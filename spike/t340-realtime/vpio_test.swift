// T3.4.0 spike：测 macOS Voice Processing I/O 的 AEC 能不能消掉扬声器回灌。
// 用法: swift vpio_test.swift <要播放的wav> <录音输出wav> [--no-vpio]
import AVFoundation
import Foundation

let args = CommandLine.arguments
guard args.count >= 3 else {
    FileHandle.standardError.write("用法: vpio_test.swift <play.wav> <out.wav> [--no-vpio]\n".data(using: .utf8)!)
    exit(2)
}
let playPath = args[1]
let outPath  = args[2]
let useVPIO  = !args.contains("--no-vpio")

let engine = AVAudioEngine()
let player = AVAudioPlayerNode()

// 关键：必须先 touch inputNode 再设置，否则 VPIO 开不起来
let input = engine.inputNode
if useVPIO {
    do {
        try input.setVoiceProcessingEnabled(true)
        // 输出侧也开，让 VPIO 拿到播放参考信号
        try engine.outputNode.setVoiceProcessingEnabled(true)
        print("VPIO: 已开启")
    } catch {
        print("VPIO: 开启失败 \(error)")
        exit(3)
    }
} else {
    print("VPIO: 关闭（对照组）")
}

guard let playFile = try? AVAudioFile(forReading: URL(fileURLWithPath: playPath)) else {
    print("无法读取播放文件"); exit(4)
}

engine.attach(player)
engine.connect(player, to: engine.mainMixerNode, format: playFile.processingFormat)

// 录 VPIO 处理之后的麦克风信号
let inFormat = input.outputFormat(forBus: 0)
print("输入格式: \(inFormat.sampleRate) Hz, \(inFormat.channelCount) ch")

let outSettings: [String: Any] = [
    AVFormatIDKey: kAudioFormatLinearPCM,
    AVSampleRateKey: inFormat.sampleRate,
    AVNumberOfChannelsKey: inFormat.channelCount,
    AVLinearPCMBitDepthKey: 16,
    AVLinearPCMIsFloatKey: false,
    AVLinearPCMIsBigEndianKey: false,
]
guard let outFile = try? AVAudioFile(forWriting: URL(fileURLWithPath: outPath), settings: outSettings) else {
    print("无法创建输出文件"); exit(5)
}

var writeErrors = 0
input.installTap(onBus: 0, bufferSize: 1024, format: inFormat) { buffer, _ in
    do { try outFile.write(from: buffer) } catch { writeErrors += 1 }
}

do {
    try engine.start()
} catch {
    print("engine 启动失败: \(error)"); exit(6)
}

player.scheduleFile(playFile, at: nil, completionHandler: nil)
player.play()

// 录满 播放时长 + 1.2 秒
let dur = Double(playFile.length) / playFile.processingFormat.sampleRate
Thread.sleep(forTimeInterval: dur + 1.2)

player.stop()
input.removeTap(onBus: 0)
engine.stop()
if writeErrors > 0 { print("⚠️ 写入失败 \(writeErrors) 次") }
print("完成：播放 \(String(format: "%.2f", dur))s，录音已写入 \(outPath)")
