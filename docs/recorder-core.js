/* ── AgentEar 录音核心 ─────────────────────────────────
   从 docs/thai-recorder.html 抽出来的 **DOM 无关** 那一半，供多个页面共用。
   这一段经过五轮评审（PR #2 三轮 codex + PR-Daemon 两轮 + 合并后补审），
   每一处非显然的写法都带着「上一版为什么错」的注释 —— **不要凭直觉改**。

   抽取时只做了两处参数化，其余逐字未动：
   1. 原来 `toWav16k` 把目标采样率写死成 16000（那个页面是 ASR 语料工具），
      现在是 `toWav(pcm, srcRate, dstRate, opt)`；
   2. 原来 `MAX_TAKE_SECONDS` 是模块常量，现在是 `startCapture` 的可选项。

   **「逐字未动」是验出来的，不是读出来的**（2026-08-31）：
   - `resampleSinc`：七组采样率对（48k/44.1k/22.05k/16k/8k→16k、48k→24k、
     44.1k→48k）喂同一段多频混合信号，最大逐样本差全为 0。
     特意覆盖非整数比与上采样 —— 只测 48k→16k 的话，整数比下所有输出中心
     都落在整数相位上，恰好避开相位表量化误差最大的那段。
   - `encodeWav`：五组（不同采样率/长度/削波/单样本）逐字节相同。
   - `analyseTake`：五种波形 × 三组参数 = 15 组，每个数值字段与问题列表
     逐字段相同。
   - `startCapture` / `CAP_WORKLET` / `sha256hex16`：需要真实 AudioContext，
     没法像纯函数那样对拍，改为**文本对拍** —— 后两者逐字节相同，
     `startCapture` 仅 5 行差异，正是上面第 2 条那处参数化本身。

   ⚠️ **16 kHz 是 ASR 语料的采样率，不是声音克隆的。** 拿 16 kHz 的参考音
   去做 voice clone，音色会明显变闷 —— 主流 TTS 栈原生是 22.05/24/32 kHz。
   所以目标采样率由调用方传，核心不替你决定。 */
(function (global) {
"use strict";

const normText = v => (v || "").normalize("NFC").trim();

/* 已下载的答案文件会过期：下载完再改文本、再重录、改个署名，磁盘上那份
   就对不上了，而清单还绿着 —— 于是他按着绿勾把旧文本发出来，
   WAV 和参考文本静默错配。这正是本页想避免的事，只是从内存搬到了磁盘。
   所以把**上次导出的内容原样留着**，每次刷新逐字符比。
   （不用 32 位哈希：这文件才几 KB，直接存字符串是精确比较，
     省掉一份没必要承担的碰撞风险。） */

function encodeWav(samples, sampleRate) {
  const buf = new ArrayBuffer(44 + samples.length * 2);
  const v = new DataView(buf);
  const str = (o, s) => { for (let i = 0; i < s.length; i++) v.setUint8(o + i, s.charCodeAt(i)); };
  str(0, "RIFF"); v.setUint32(4, 36 + samples.length * 2, true); str(8, "WAVE");
  str(12, "fmt "); v.setUint32(16, 16, true); v.setUint16(20, 1, true);
  v.setUint16(22, 1, true); v.setUint32(24, sampleRate, true);
  v.setUint32(28, sampleRate * 2, true); v.setUint16(32, 2, true); v.setUint16(34, 16, true);
  str(36, "data"); v.setUint32(40, samples.length * 2, true);
  let o = 44;
  for (let i = 0; i < samples.length; i++, o += 2) {
    const s = Math.max(-1, Math.min(1, samples[i]));
    v.setInt16(o, s < 0 ? s * 0x8000 : s * 0x7FFF, true);
  }
  return new Blob([buf], { type: "audio/wav" });
}

/* ── 重采样：必须先抗混叠 ──
   录出来的容器格式各浏览器不同（Chrome 多为 webm/opus，Safari 多为 mp4/aac），
   所以不假设容器，一律解码后转成 16 kHz 单声道。

   **不能把 48 kHz 的 AudioBuffer 直接喂进 16 kHz 的 OfflineAudioContext。**
   AudioBufferSourceNode 只做插值、不做低通，8 kHz 以上的能量会几乎无衰减地
   折回语音带（实测折回点幅度 0.90，正确实现应 ≈0.0001）。折回的擦音落在
   F1/F2/F3 上，15.6–15.9 kHz 更是直接折进泰语声调所在的 F0 带 —— 而这批
   录音存在的唯一意义就是拿去和 FLEURS 基线比，混叠会把「code-switch 难」
   和「这批音频本身坏」永久搅在一起。而且下载下来的 WAV 听着完全正常。

   浏览器自己的重采样器**也不用** —— 它虽然抗混叠，但各家实现不同：实测
   Chrome 的离线解码在 9 kHz 折回点上还残留 0.097，而下面的 sinc 是 0.00016。
   这批录音是要拿去和 FLEURS 比的测量样本，同一段声音在不同浏览器上得出不同
   结果，会让语料本身失去可比性。宁可多花这点时间，换一份确定的结果 ——
   实测代价见提交说明，别在注释里写死数字，改了参数就会对不上。 */
/* ── 采集原始 PCM ──
   **不用 MediaRecorder。** 各家浏览器用不同的**有损**编码器（Chrome 多为
   Opus，Safari 多为 AAC），解码回来只是把有损结果装进 PCM，恢复不了原波形；
   码率、预回声、高频截止、编码器内部的 DSP 都不同。这批录音是要拿去和
   FLEURS 比 CER 的**测量样本**，采集端夹一层各家不同的有损编码，就没法把
   CER 的差异归因给「泰语夹英文词」而不是「浏览器用了哪个编码器」。
   直接从 AudioContext 取 Float32 就没有这一层。 */

const CAP_WORKLET = `
class Cap extends AudioWorkletProcessor {
  constructor() {
    super();
    this.buf = new Float32Array(4096); this.n = 0; this.done = false;
    this.frames = 0; this.startFrame = -1;
    // 停止协议：主线程发 stop，音频线程把不满一块的残留发出来，再发 ACK。
    // 同一个 port 保证消息顺序，所以收到 ACK 时它之前投递的块都已到齐。
    // 少了这一步，停止时最多丢 4095 个样本（48 kHz 85 ms / 44.1 kHz 93 ms），
    // 外加已投递但主线程还没处理的完整块。
    this.port.onmessage = e => {
      if (e.data && e.data.type === 'stop') {
        this.done = true;
        if (this.n) { this.port.postMessage(this.buf.slice(0, this.n)); this.n = 0; }
        // 起止帧号取自**音频线程**的 currentFrame。用主线程的 ctx.currentTime
        // 的话，ACK 投递的延迟（调度/GC/切后台，30–300 ms）会被算成「丢了音频」，
        // 直接变成不可人工放行的假硬失败。
        this.port.postMessage({ type: 'flushed', frames: this.frames,
                                startFrame: this.startFrame, stopFrame: currentFrame });
      }
    };
  }
  process(inputs) {
    if (this.done) return false;
    const inp = inputs[0];
    if (inp && inp.length) {
      if (this.startFrame < 0) this.startFrame = currentFrame;
      const len = inp[0].length, ch = inp.length;
      for (let i = 0; i < len; i++) {
        // **平均全部声道**。只取 inputs[0][0] 的话，设备真给了立体声时
        // 这条路拿左声道、ScriptProcessor 那条路拿的是降混，同一台设备
        // 会产出两种不同的语料。
        let acc = 0;
        for (let c = 0; c < ch; c++) acc += inp[c][i];
        this.buf[this.n++] = acc / ch; this.frames++;
        if (this.n === this.buf.length) { this.port.postMessage(this.buf.slice(0)); this.n = 0; }
      }
    }
    return true;
  }
}
registerProcessor('agentear-cap', Cap);
`;

// 4 分钟。第 4 段的提示语要「2–3 分钟」，上限若也是 3 分钟，照着上限念
// 就会被截断 —— 那个冲突是提示语自己造的。留一分钟余量。
// 缓冲和拼接各占一份内存，48 kHz float 下峰值约 92 MB。
const DEFAULT_MAX_TAKE_SECONDS = 240;

/* 答案文件里要能指名道姓地绑到某一份 WAV。只写「时长 12.3 秒、峰值 0.412」
   是不够的：两次不同的录音只要这些四舍五入过的数落进同一个桶，写出来就一模一样。 */
async function sha256hex16(blob) {
  try {
    const d = await crypto.subtle.digest("SHA-256", await blob.arrayBuffer());
    return [...new Uint8Array(d)].slice(0, 8).map(b => b.toString(16).padStart(2, "0")).join("");
  } catch (_) { return null; }        // 非安全上下文没有 subtle，不因此挡住录音
}

async function startCapture(ctx, source, opts) {
  const maxSeconds = (opts && opts.maxSeconds) || DEFAULT_MAX_TAKE_SECONDS;
  const chunks = [];
  let total = 0, dropped = 0;
  const limit = Math.round(ctx.sampleRate * maxSeconds);
  const push = a => {
    if (total >= limit) { dropped += a.length; return; }
    chunks.push(a); total += a.length;
  };

  let node = null, worklet = false;
  if (ctx.audioWorklet && typeof AudioWorkletNode === "function") {
    try {
      const url = URL.createObjectURL(new Blob([CAP_WORKLET], { type: "text/javascript" }));
      try { await ctx.audioWorklet.addModule(url); } finally { URL.revokeObjectURL(url); }
      node = new AudioWorkletNode(ctx, "agentear-cap",
        { channelCount: 1, channelCountMode: "explicit", channelInterpretation: "speakers" });
      node.port.onmessage = e => { if (e.data instanceof Float32Array) push(e.data); };
      worklet = true;
    } catch (_) { node = null; }
  }
  // ScriptProcessorNode 已废弃，但老 Safari 只有它。它在主线程上跑，
  // 主线程一卡就可能延迟/跳过回调 —— 所以下面靠 playbackTime 查缺口。
  let gaps = 0, gapSeconds = 0, lastPlayback = null;
  if (!node) {
    node = ctx.createScriptProcessor(4096, 1, 1);
    const expect = 4096 / ctx.sampleRate;
    node.onaudioprocess = e => {
      if (lastPlayback !== null) {
        const d = e.playbackTime - lastPlayback;
        if (d > expect * 1.5) { gaps++; gapSeconds += d - expect; }
      }
      lastPlayback = e.playbackTime;
      push(new Float32Array(e.inputBuffer.getChannelData(0)));
    };
  }
  // 两种节点都要有下游才会被拉动；增益置 0，免得把麦克风的声音放出来。
  // （接 destination 会激活输出设备，在 iOS/蓝牙上可能触发路由切换 ——
  //   所以下面把 track 的实际参数一并记下来，事后能看出有没有被改过。）
  const sink = ctx.createGain();
  sink.gain.value = 0;
  source.connect(node); node.connect(sink); sink.connect(ctx.destination);

  const backend = worklet ? "audioworklet" : "scriptprocessor";
  const t0 = ctx.currentTime;
  let taken = false;

  return {
    backend,
    get seconds() { return total / ctx.sampleRate; },
    async take() {
      if (taken) return null;
      taken = true;
      // **先读时钟再等 flush。** 等待期间 ctx.currentTime 照走，而 worklet
      // 早已停止采集 —— 把等待时间算进去就是凭空多出一段「缺失」。
      const elapsed = ctx.currentTime - t0;
      let ack = null, flushTimedOut = false;
      if (worklet && node.port) {
        ack = await new Promise(resolve => {
          let settled = false;
          const done = v => { if (!settled) { settled = true; resolve(v); } };
          const prev = node.port.onmessage;
          node.port.onmessage = e => {
            if (e.data && e.data.type === "flushed") done(e.data);
            else prev(e);
          };
          node.port.postMessage({ type: "stop" });
          // worklet 崩了也不能把页面卡死。但超时意味着「ACK 之前的块都已到齐」
          // 这个保证失效了 —— 那就得说出来，不能当无事发生。
          setTimeout(() => done(null), 300);
        });
        if (!ack) flushTimedOut = true;
        node.port.onmessage = null;
      }
      node.onaudioprocess = null;
      try { source.disconnect(node); } catch (_) {}
      try { node.disconnect(); } catch (_) {}
      try { sink.disconnect(); } catch (_) {}
      const pcm = new Float32Array(total);
      let o = 0;
      for (const c of chunks) { pcm.set(c, o); o += c.length; }
      chunks.length = 0;
      // 拿到的样本数对不上流逝的时间 = 中间掉过块。这是**必须能看见**的：
      // 时间线有洞的录音在科学上不可用，而听起来往往只是「有点跳」。
      // 有 ACK 就用音频线程自己的帧号，没有（ScriptProcessor 或超时）才退回时钟
      const audioFrames = ack ? Math.max(0, ack.stopFrame - ack.startFrame)
                              : Math.round(elapsed * ctx.sampleRate);
      const expectedFrames = Math.max(0, audioFrames - dropped);
      const missing = Math.max(0, expectedFrames - total);
      // 判「有洞」要先扣掉后端固有的量化，否则会误报。
      // ScriptProcessor 没有 flush 协议，最后不满一块的部分本来就交不出来：
      // 4096 样本在 44.1 kHz 是 92.9 ms —— 比任何合理的告警阈值都大。
      // AudioWorklet 走了 flush，只需容忍 currentTime 的抖动。
      // ScriptProcessor 置空 onaudioprocess 时，还有**一块在途**没交出来，
      // 所以要留两块。时间线是硬失败、没有人工出口 —— 容差算小了，
      // 老 Safari 的志愿者会被永远告知「必须重录」，而他什么也没做错。
      const tolerance = worklet ? 0.02
        : 2 * 4096 / ctx.sampleRate + (ctx.baseLatency || 0) + 0.02;
      return {
        pcm, sampleRate: ctx.sampleRate, dropped, backend,
        timeline: { elapsed, expectedFrames, gotFrames: total,
                    producedFrames: ack ? ack.frames : null, flushTimedOut,
                    missingSeconds: missing / ctx.sampleRate, gaps, gapSeconds, tolerance },
      };
    },
  };
}

/* Kaiser 窗 sinc 重采样，纯 JS，唯一的重采样路径。
   不用 BiquadFilterNode 补救 —— 规范里 lowpass 的 Q 是**按 dB 计**的，实测
   四级串联在 5 kHz 上有 +6.8 dB 的通带增益，正是那类实现差异。这段不依赖
   任何浏览器行为，抗混叠靠核函数自身的截止频率与阶数。
   实测数字见提交说明，别在这里复述——改了参数就会对不上。 */

function besselI0(x) {
  let sum = 1, term = 1;
  for (let k = 1; k < 64; k++) {
    const r = x / (2 * k);
    term *= r * r; sum += term;
    if (term < 1e-14 * sum) break;
  }
  return sum;
}

function resampleSinc(x, srcRate, dstRate) {
  if (srcRate === dstRate) return x;
  const ratio = dstRate / srcRate;
  const n = Math.max(1, Math.round(x.length * ratio));
  const out = new Float32Array(n);

  // **指标先定死，阶数由指标反解**，不是先拍个阶数再看能滤成什么样。
  // 上一版是 `cutoff = ratio * 0.95` 配 103 taps —— 那只是把 −6 dB 点放在
  // 7.6 kHz，和「过渡带有多宽」没有任何关系，实测 8.1 kHz 折回 7.9 kHz 时
  // 还有 −17.9 dB。只测 9 kHz 以上的话正好避开这一段，看不出来。
  //
  //   下采样：阻带边界只能压在**目标 Nyquist** 上 —— 超过它的任何能量都
  //           会折回带内。通带取 0.875×目标 Nyquist（16 kHz 时是 7 kHz）。
  //   上采样：镜像出现在**源 Nyquist** 以上，故以源 Nyquist 为中心，
  //           通带 0.95×源 Nyquist（不能像下采样那样砍到 0.875，
  //           那是在削真实存在的源高频）。
  const nyq = Math.min(srcRate, dstRate) / 2;      // Hz
  let fPass, fStop;
  if (srcRate < dstRate) {
    // 上采样：镜像出现在 srcRate − f，离通带最近的一个在 2·nyq − fPass。
    // 所以通带/阻带该**对称地夹住源 Nyquist**，而不是把阻带压在它上面
    // ——后者会把 0.95·nyq 到 nyq 之间真实存在的源高频削掉。
    fPass = nyq * 0.95;
    fStop = 2 * nyq - fPass;
  } else {
    // 下采样：超过目标 Nyquist 的能量全都会折回带内，阻带边界只能压在它上面
    fPass = nyq * 0.875;
    fStop = nyq;
  }
  const fc = (fPass + fStop) / 2;                  // −6 dB 点
  const trans = (fStop - fPass) / srcRate;         // 归一化过渡带宽

  // Kaiser 窗的经验公式：给定阻带衰减 A(dB) 反解 β 和阶数。
  // A=80 → 48 kHz→16 kHz 得 239 taps。阶数随过渡带收窄而增长，
  // 这正是「指标决定成本」而不是反过来。
  // 留 5 dB 余量：经验公式本身是近似，逐相位归一化又会动一点点
  const A = 85;
  const beta = A > 50 ? 0.1102 * (A - 8.7) : 0.5842 * Math.pow(A - 21, 0.4) + 0.07886 * (A - 21);
  const taps = Math.max(16, Math.ceil((A - 8) / (2.285 * 2 * Math.PI * trans) / 2));
  const width = 2 * taps + 1;
  const cut = 2 * fc / srcRate;                    // 归一化截止（周期/采样）
  const i0b = besselI0(beta);

  // 相位核表。直接在内层循环里算 sin/Bessel 的话，一分钟录音要几十万次
  // 超越函数，手机上会把主线程卡死好几秒；把小数偏移量量化成 PH 档，
  // 核只算一次。48 kHz→16 kHz 是精确 3:1，所有输出中心都落在整数上，
  // 这一档的量化误差为零；非整数比时用最近相位（不是向下取整）。
  const PH = 512;
  const kern = new Float32Array(PH * width);
  for (let ph = 0; ph < PH; ph++) {
    const frac = ph / PH, base = ph * width;
    let sum = 0;
    for (let j = 0; j < width; j++) {
      const t = (j - taps) - frac;
      const arg = Math.PI * cut * t;
      const sinc = Math.abs(arg) < 1e-9 ? cut : cut * Math.sin(arg) / arg;
      // Kaiser 窗，自变量归一到 [-1, 1]。用 t/taps 而不是 t/(taps+1)：
      // 后者不是长度 2·taps+1 的标准定义，端点窗值会高约 2 dB，
      // 实际阻带就比设计值差一截。
      const u = t / taps;
      const w = Math.abs(u) >= 1 ? 0 : besselI0(beta * Math.sqrt(1 - u * u)) / i0b;
      const v = sinc * w;
      kern[base + j] = v; sum += v;
    }
    const g = sum > 1e-9 ? 1 / sum : 1;             // 归一化，保证 DC 增益为 1
    for (let j = 0; j < width; j++) kern[base + j] *= g;
  }

  const len = x.length;
  // 分块 + 让出主线程。259 抽头 × 16000 样本/秒是实打实的算力：
  // 桌面上 180 秒音频要 ~950 ms，手机会更久，一次跑完整段就是把页面冻住。
  // 每块之后 yield 一次，让「正在转换」的动画还能动。
  const BLOCK = 1 << 15;
  const step = (from, to) => {
  for (let i = from; i < to; i++) {
    const center = i / ratio;
    const i0 = Math.floor(center);
    // 最近相位而不是向下取整：非整数比时误差直接少约 6 dB
    let ph = Math.round((center - i0) * PH), off = i0;
    if (ph >= PH) { ph = 0; off = i0 + 1; }
    const base = ph * width;
    const from = off - taps;
    let acc = 0;
    if (from >= 0 && from + width <= len) {
      for (let j = 0; j < width; j++) acc += x[from + j] * kern[base + j];
    } else {
      // 两端不足半个窗：越界的抽头按静音处理。首尾各约 (taps/srcRate) 秒内
      // 增益不准（48 kHz / 119 taps 时约 2.5 ms），正常录音这一段是静音。
      for (let j = 0; j < width; j++) {
        const idx = from + j;
        if (idx >= 0 && idx < len) acc += x[idx] * kern[base + j];
      }
    }
    out[i] = acc;
  }
  };
  return (async () => {
    for (let i = 0; i < n; i += BLOCK) {
      step(i, Math.min(n, i + BLOCK));
      if (i + BLOCK < n) await new Promise(r => setTimeout(r, 0));
    }
    return out;
  })();
}

/* 落盘前的质量体检。
   电平表只在录音那几秒有效，还要求他一直盯着看；真正会被发出去的是这份
   数据，所以在这里按整段重新判一次。单看全局峰值不行——整段静音里混一个
   0.02 的咔哒声就能过。 */

function analyseTake(x, sr, opt) {
  opt = opt || {};
  const n = x.length, seconds = n / sr;
  let peak = 0, sumsq = 0, clipped = 0;
  for (let i = 0; i < n; i++) {
    const a = Math.abs(x[i]);
    if (a > peak) peak = a;
    if (a > 0.99) clipped++;
    sumsq += x[i] * x[i];
  }
  const rms = n ? Math.sqrt(sumsq / n) : 0;

  // 20 ms 分帧
  const fr = Math.max(1, Math.round(sr * 0.02));
  const fRms = [];
  for (let i = 0; i + fr <= n; i += fr) {
    let ss = 0;
    for (let j = i; j < i + fr; j++) ss += x[j] * x[j];
    fRms.push(Math.sqrt(ss / fr));
  }
  // 门限走**分位数**，不走全局峰值。用 peak*0.1 的话：一次碰麦的瞬态就能
  // 把门限拉到 0.1，正常语音帧全被判成无声；反过来，稳定的背景噪声会让
  // 有声比接近 100%，纯噪声照样过关。
  const sorted = fRms.slice().sort((p, q) => p - q);
  const pct = q => sorted.length ? sorted[Math.min(sorted.length - 1,
      Math.max(0, Math.round(q * (sorted.length - 1))))] : 0;
  const noise = pct(0.20), speech = pct(0.90);
  // noise 为 0 时分两种：确实有语音（动态范围极大，记 99）和整段全静音（记 0）。
  // 都记 99 的话，被人工放行的全静音样本会在答案文件里留下「SNR 99 dB」。
  const snrDb = noise > 1e-9 ? 20 * Math.log10(Math.max(speech, 1e-9) / noise)
                             : (speech > 1e-9 ? 99 : 0);
  const gate = Math.max(noise * 3, 0.004);
  const voiced = sorted.length ? fRms.filter(v => v > gate).length / fRms.length : 0;
  const clipRatio = n ? clipped / n : 0;

  // 「软问题」他听完可以自己放行；「硬问题」不行 —— 时间线有洞的录音
  // 听起来往往只是有点跳，但在科学上不可用，不该由主观判断放行。
  const problems = [], hard = [];
  const minSeconds = opt.minSeconds || 2;
  if (seconds < minSeconds) problems.push({
    th: `สั้นเกินไป (${seconds.toFixed(1)} วิ, ควรอย่างน้อย ${minSeconds} วิ)`,
    en: `too short (${seconds.toFixed(1)} s, expected at least ${minSeconds} s)` });
  else if (opt.expectSeconds && seconds < opt.expectSeconds * 0.6) problems.push({
    th: `สั้นกว่าที่ควรมาก (${seconds.toFixed(1)} วิ ต่อข้อความยาวขนาดนี้) — อาจอ่านไม่จบ`,
    en: `much shorter than expected for this much text (${seconds.toFixed(1)} s) — possibly cut off` });
  if (peak < 0.01) problems.push({
    th: `แทบไม่มีเสียง (peak ${peak.toFixed(4)}) — ไมโครโฟนอาจถูกปิดอยู่`,
    en: `virtually silent (peak ${peak.toFixed(4)}) — the mic may be muted or the wrong device` });
  else if (snrDb < 10) problems.push({
    th: `เสียงพูดแทบไม่ดังกว่าเสียงรบกวน (SNR ${snrDb.toFixed(1)} dB) — ลองหาที่เงียบกว่านี้`,
    en: `speech barely rises above the noise floor (SNR ${snrDb.toFixed(1)} dB) — try a quieter room` });
  else if (voiced < 0.25) problems.push({
    th: `มีเสียงพูดแค่ ${(voiced * 100).toFixed(0)}% ของความยาว — ส่วนใหญ่เป็นความเงียบ`,
    en: `only ${(voiced * 100).toFixed(0)}% of the take has speech-level energy — mostly silence` });
  if (clipRatio > 0.005) problems.push({
    th: `เสียงดังเกินจนตัดยอด ${(clipRatio * 100).toFixed(1)}% — ขยับไมค์ให้ห่างขึ้น`,
    en: `${(clipRatio * 100).toFixed(1)}% of samples are clipped — move further from the mic` });

  const tl = opt.timeline;
  if (tl && (tl.missingSeconds > (tl.tolerance != null ? tl.tolerance : 0.05) || tl.gaps > 0)) hard.push({
    th: `เสียงขาดหายระหว่างอัด (~${tl.missingSeconds.toFixed(2)} วิ) — ต้องอัดใหม่`,
    en: `the recording has gaps (~${tl.missingSeconds.toFixed(2)} s missing) — must re-record` });
  if (tl && tl.flushTimedOut) hard.push({
    // 超时后「ACK 之前的块都已到齐」这个保证就没了，小于容差的尾部丢失
    // 也检不出来 —— 不能因为「看起来没问题」就放行
    th: "ปิดการอัดไม่สมบูรณ์ ท้ายไฟล์อาจขาด — ต้องอัดใหม่",
    en: "the recorder did not shut down cleanly; the tail may be truncated — must re-record" });

  return { peak, rms, seconds, voiced, snrDb, noise, speech, clipRatio, problems, hard };
}

/* 采到的就是 Float32 PCM，只剩重采样这一步。
   **目标采样率由调用方决定** —— 见文件头那条关于 16 kHz 与声音克隆的警告。
   dstRate 等于 srcRate 时 resampleSinc 会短路，不做任何处理。 */
async function toWav(pcm, srcRate, dstRate, opt) {
  const mono = await resampleSinc(pcm, srcRate, dstRate);
  const q = analyseTake(mono, dstRate, opt);
  return { blob: encodeWav(mono, dstRate), quality: q, seconds: q.seconds,
           srcRate, dstRate };
}

/* ── 环境自检 ──
   「没弹窗就失败」和「用户拒绝了」是两回事，得分清楚，否则会给出
   「去浏览器里允许」这种在嵌套 iframe 下根本无效的建议。 */

function diagnose() {
  const d = {
    secure: window.isSecureContext,
    framed: window.self !== window.top,
    gum: !!(navigator.mediaDevices && navigator.mediaDevices.getUserMedia),

    ac:  !!(window.AudioContext || window.webkitAudioContext),
    // 没有 download 属性就存不下文件；那样让他直接走手机那条路，
    // 别让他录完一轮才发现点「保存」毫无反应
    dl:  "download" in document.createElement("a"),
    policy: null,
  };
  try {
    // featurePolicy 是旧名，permissionsPolicy 是新名；两个都可能不存在
    const pol = document.permissionsPolicy || document.featurePolicy;
    if (pol && pol.allowsFeature) d.policy = pol.allowsFeature("microphone");
  } catch (_) {}
  return d;
}

global.RecorderCore = {
  encodeWav, resampleSinc, analyseTake, toWav,
  startCapture, DEFAULT_MAX_TAKE_SECONDS,
  sha256hex16, diagnose, normText,
};
})(typeof window !== "undefined" ? window : globalThis);
