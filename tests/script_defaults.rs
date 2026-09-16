//! 钉住 `scripts/` 里那几条**不能靠代码评审记住**的默认值。
//!
//! 为什么要有这个文件：这里的三条都不是「逻辑」，而是**默认档与失败模式**，
//! 写错了编译能过、跑起来也不报错，只是用户拿到的东西不对：
//!
//! 1. **默认量化档必须是 4bit**（jason 2026-09-15 拍板）。8bit 权重 3.22 GB、
//!    边车峰值约 3.3 GB，而 4bit 是 2.30 GB / 约 2.4 GB —— **发布的普通人
//!    电脑没这么大内存**。默认档一旦被谁顺手改掉，受影响的是不知道这件事的人。
//! 2. **8bit 的体积守卫不能沿用 4bit 的下界**：只下了一半的 8bit 目录
//!    （约 1.6 GB）借 4bit 的下界（1500 MB）就能过关，然后当成完整模型加载。
//! 3. **TTS 边车必须拿到音色库**。没有参考音频时 VoxCPM2 每次生成**随机换
//!    说话人**（边车自己会告警：F0 极差 65%、音量差 4.5 倍）；而句子级流水线
//!    会把一次回答切成好几句、每句各发一次请求，于是**一句话里换好几个人**。

use std::fs;

fn read(rel: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("读不了 {}: {e}", path.display()))
}

/// 默认档**只能**是 4bit，而且必须是显式写出来的默认值（不是「碰巧为空」）。
#[test]
fn the_default_tts_quant_is_4bit() {
    for script in ["scripts/setup-talk.sh", "scripts/serve-tts.sh"] {
        let body = read(script);
        assert!(
            body.contains("AGENTEAR_TTS_QUANT:-4bit"),
            "{script} 的默认量化档必须显式写成 4bit（发布默认档，普通人电脑内存不够 8bit）"
        );
        assert!(
            !body.contains("AGENTEAR_TTS_QUANT:-8bit"),
            "{script} 不许把 8bit 变成默认档"
        );
    }
}

/// 每个档位有自己的下载完成下界：8bit 不能借 4bit 的下界混过去。
#[test]
fn each_quant_has_its_own_size_floor() {
    let body = read("scripts/setup-talk.sh");
    assert!(
        body.contains("4bit) TTS_MIN_MB=") && body.contains("8bit) TTS_MIN_MB="),
        "setup-talk.sh 要按档位分别给下载完成的下界"
    );
    // 8bit 权重实测 3.22 GB；下界必须明显高于「只下了一半」的量级
    let eight = body
        .lines()
        .find(|l| l.contains("8bit) TTS_MIN_MB="))
        .expect("找不到 8bit 的下界");
    let value: u32 = eight
        .split('=')
        .next_back()
        .and_then(|v| v.trim().trim_end_matches(";;").trim().parse().ok())
        .unwrap_or_else(|| panic!("8bit 的下界解析不出来：{eight}"));
    assert!(
        value >= 2000,
        "8bit 的下界只给了 {value} MB，半份权重会被当成完整的放过去"
    );
}

/// 边车启停路径必须带上音色库，否则用户听到的是「每句换一个人」。
#[test]
fn the_tts_sidecar_is_pointed_at_a_voice_library() {
    let body = read("scripts/serve-tts.sh");
    assert!(
        body.contains("AGENTEAR_TTS_VOICES_DIR"),
        "serve-tts.sh 要能指定音色库目录"
    );
    assert!(
        body.contains("--voices-dir"),
        "serve-tts.sh 必须把音色库传给边车——不传就没有参考音频，每次生成随机换说话人"
    );
    assert!(
        body.contains("--voice"),
        "serve-tts.sh 要把音色钉住（不钉就由库的排序决定，换一次目录顺序就换一个人）"
    );
    assert!(
        body.contains("assets/talk-voices"),
        "数据目录还没有音色库时要回退到仓库里那份（clone 出来就有，不用下载）"
    );
}

/// 挑 Python 解释器**必须过版本闸**，不能只看命令名存不存在。
///
/// 踩点（jason 这台机器，2026-09-15）：默认 `python3` 是 pyenv 的 **3.11.9**，
/// 但 **pyenv 只在交互式 shell 里生效**——launchd / GUI / 非交互 shell 里
/// `python3` 落到 `/usr/bin/python3` = **Xcode 3.9.6**，而 mlx 要 3.11+。
/// 所以「名字存在」和「能不能装 mlx」是两件事，必须真的问一次版本。
#[test]
fn the_interpreter_picker_checks_the_version() {
    let body = read("scripts/setup-talk.sh");
    assert!(
        body.contains("3, 11"),
        "setup-talk.sh 要真的验 Python ≥ 3.11，而不是只看命令名"
    );
    assert!(
        body.contains("sys.version_info"),
        "版本闸应该问解释器自己（sys.version_info），不要解析 `python3 -V` 的字符串"
    );
    // `python3` 必须进候选：只有 pyenv、没有 python3.11 这种别名的机器上，
    // 早先的清单会直接报「找不到 Python 3.11+」——明明有可用的。
    assert!(
        body.contains("python3.11 python3"),
        "候选里要有裸 `python3`（有些机器只有它）"
    );
}

/// 随包的默认音色库必须真的在仓库里，而且成对（`.wav` + `.json` 缺一不可）。
///
/// `.json` 里的 `ref_text` 是克隆模式**必须**的：参考音频和它的文本对不上，
/// 音色会被带偏——所以「有 wav 没 json」不是「少个附件」，是「这条不能用」。
#[test]
fn the_bundled_voice_library_is_complete() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/talk-voices");
    let mut wavs: Vec<String> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("读不了 {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".wav"))
        .collect();
    wavs.sort();
    assert!(!wavs.is_empty(), "assets/talk-voices/ 里一条参考音频都没有");
    for wav in &wavs {
        let json = format!("{}.json", wav.trim_end_matches(".wav"));
        assert!(
            dir.join(&json).exists(),
            "{} 缺配套的 {json}（没有 ref_text 这条音色不能用）",
            wav
        );
    }
    // 实测挑过的那条必须在：它才是钉住的默认音色
    assert!(
        wavs.iter().any(|n| n == "female_zh_02.wav"),
        "随包的音色库里必须有 female_zh_02（实测挑的那条，F0 174.5 Hz / 半音起伏 4.22）"
    );
}

/// **被源码引用的实测工具必须真的在仓库里。**
///
/// 这条钉的是一次真实的错：`services/tts/backends.py` 与
/// `services/tts/make_voice.py` 都引用 `measure_f0.py` 的数字当实测来源
/// （「不归一时 RMS 差 4.54 倍」），而它当时在 `vendor/models/talk/` 下
/// ——**那个目录被 gitignore**。于是新克隆的仓库里没有这个文件，
/// 那些数字**没有可复现的来源**，而编译、测试、CI 全都不会报错。
/// 2026-09-16 把它搬进 `scripts/`。
///
/// ⚠️ 关键点是**引用的路径必须落在仓库内**：只断言「文件存在」不够，
/// 因为 `vendor/` 下的文件在开发机上确实存在、在别人机器上不存在。
#[test]
fn cited_measurement_tools_are_in_the_repo() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let tool = "scripts/measure-f0.py";
    assert!(
        root.join(tool).is_file(),
        "{tool} 不在仓库里 —— 引用它的源码就成了不可复现的声明"
    );
    for src in ["services/tts/backends.py", "services/tts/make_voice.py"] {
        let body = read(src);
        assert!(body.contains(tool), "{src} 引用实测来源时必须写仓库内的路径 {tool}");
        for ignored in ["vendor/models/talk/measure", "vendor/models/talk/release"] {
            assert!(
                !body.contains(ignored),
                "{src} 不许把 gitignore 的 {ignored} 当实测来源/文档路径"
            );
        }
    }
}
