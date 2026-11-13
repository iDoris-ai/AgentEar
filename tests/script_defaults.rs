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

/// App Store 沙箱只声明这个应用真正用到的权限。扬声器没有对应的 entitlement。
/// 随包 helper（识别引擎）只能是「沙箱 + inherit」，不许带别的 entitlement。
#[test]
fn app_store_helper_entitlements_are_sandbox_plus_inherit_only() {
    let ent = read("assets/entitlements/AppStoreHelper.entitlements");
    assert!(ent.contains("com.apple.security.app-sandbox"));
    assert!(ent.contains("com.apple.security.inherit"));
    assert_eq!(ent.matches("<key>").count(), 2, "inherit 不能和别的 entitlement 同时出现");
    let bundle = read("scripts/bundle.sh");
    assert!(bundle.contains("AppStoreHelper.entitlements"), "bundle.sh 要逐个签 helper");
    assert!(!bundle.contains("--deep --options"), "App Store 签名不能用 --deep（会抹掉 helper 的 entitlements）");
}

#[test]
fn app_store_entitlements_enable_sandbox_and_microphone() {
    let ent = read("assets/entitlements/AppStore.entitlements");
    for key in [
        "com.apple.security.app-sandbox",
        "com.apple.security.device.microphone",
        "com.apple.security.device.audio-input",
        "com.apple.security.network.client",
        "com.apple.security.network.server",
    ] {
        assert!(ent.contains(key), "App Store entitlements 缺少 {key}");
    }
    assert!(
        !ent.contains("speaker") && !ent.contains("audio-output"),
        "播放不需要单独的扬声器 entitlement"
    );
    let bundle = read("scripts/bundle.sh");
    assert!(
        bundle.contains("AGENTEAR_DISTRIBUTION:-}") && bundle.contains("app-store"),
        "沙箱只在 App Store 打包时签上"
    );
    assert!(
        bundle.contains("assets/entitlements/AppStore.entitlements"),
        "App Store 签名要带上这份 entitlements"
    );
    // #133 评审 L1：探针能在沙箱里跑任意命令，提审构建不能带。只有显式 AGENTEAR_MAS_PROBE=1 才开。
    assert!(
        bundle.contains("\"${AGENTEAR_MAS_PROBE:-}\" = \"1\"")
            && bundle.matches("--features mas-probe").count() == 1,
        "mas-probe feature 只能在 AGENTEAR_MAS_PROBE=1 分支里打开"
    );
    // App Store 每次上传 CFBundleVersion 必须递增：构建号与版本号分开。
    assert!(
        bundle.contains("<key>CFBundleVersion</key>\n    <string>$BUILD_NUMBER</string>"),
        "CFBundleVersion 要用独立的构建号"
    );
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

/// 从 `src/talk.rs` 里读出某个 `DEFAULT_*_URL` 常量的端口。
fn rust_default_port(constant: &str) -> u16 {
    let src = read("src/talk.rs");
    let needle = format!("pub const {constant}: &str = \"");
    let start = src.find(&needle).unwrap_or_else(|| panic!("src/talk.rs 里找不到 {constant}")) + needle.len();
    let url = &src[start..start + src[start..].find('"').unwrap()];
    url.rsplit(':').next().unwrap().trim_end_matches('/').parse().unwrap_or_else(|_| panic!("{constant} = {url} 读不出端口"))
}

/// 从脚本里读 `${VAR:-NNNN}` 的默认端口。
fn script_default_port(rel: &str, var: &str) -> u16 {
    let body = read(rel);
    let needle = format!("${{{var}:-");
    let start = body.find(&needle).unwrap_or_else(|| panic!("{rel} 里找不到 {needle}")) + needle.len();
    body[start..start + body[start..].find('}').unwrap()].parse().unwrap()
}

/// **Rust 默认地址与拉起脚本的默认端口必须一致**（v0.22.0 换 TTS 端口时加）。
///
/// 两边漂移的症状是「请求一个端口、边车起在另一个」：守护进程探活永远是
/// 「连接被拒」→ 拉起 → 边车在另一个端口上好好地跑着 → 90 秒后报「没就绪」。
/// 编译、单测都不会报错，因为两边各自都是「对的」。
#[test]
fn rust_and_script_default_ports_agree() {
    let tts = rust_default_port("DEFAULT_TTS_URL");
    assert_eq!(tts, script_default_port("scripts/serve-tts.sh", "AGENTEAR_TTS_PORT"), "TTS：Rust 与 serve-tts.sh 默认端口不一致");
    let server = read("services/tts/server.py");
    assert!(
        server.contains(&format!("\"--port\", type=int, default={tts})")),
        "TTS：services/tts/server.py 的 --port 默认值与 Rust 的 DEFAULT_TTS_URL（{tts}）不一致"
    );
    let llm = rust_default_port("DEFAULT_LLM_URL");
    assert_eq!(llm, script_default_port("scripts/serve-talk-llm.sh", "AGENTEAR_TALK_LLM_PORT"), "LLM：Rust 与 serve-talk-llm.sh 默认端口不一致");
    assert_ne!(tts, 8765, "8765 是 jason 机器上被别的 app 占过的端口（2026-09-26），换掉了就别换回去");
}

/// 自动换端口靠环境变量把端口交给拉起脚本——两个脚本都必须认这个变量。
#[test]
fn both_sidecar_scripts_read_their_port_from_the_env() {
    assert!(read("scripts/serve-tts.sh").contains("${AGENTEAR_TTS_PORT:-"));
    assert!(read("scripts/serve-talk-llm.sh").contains("${AGENTEAR_TALK_LLM_PORT:-"));
    let talk = read("src/talk.rs");
    assert!(talk.contains("\"AGENTEAR_TTS_PORT\"") && talk.contains("\"AGENTEAR_TALK_LLM_PORT\""),
        "src/talk.rs 的 port_env_var 与脚本读的变量名要一致");
}

// ---- bundle.sh 干跑行为测试（FU-29 / FU-30）----
// 上面那些断言只 grep 脚本文本，行为改坏仍绿；这里真跑 `AGENTEAR_BUNDLE_DRY_RUN=1`
// （不编译、不拷贝、不签名）并断言它打印的 key=value。

fn dry_run(script: &std::path::Path, envs: &[(&str, &str)]) -> (i32, std::collections::HashMap<String, String>, String) {
    let mut cmd = std::process::Command::new("/bin/bash");
    cmd.arg(script).env("AGENTEAR_BUNDLE_DRY_RUN", "1");
    for k in ["AGENTEAR_DISTRIBUTION", "AGENTEAR_MAS_PROBE", "AGENTEAR_BUILD_NUMBER", "AGENTEAR_BUNDLE_ID"] {
        cmd.env_remove(k);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("跑不起 bundle.sh");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let map = stdout
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect();
    (out.status.code().unwrap_or(-1), map, String::from_utf8_lossy(&out.stderr).into_owned())
}

fn real_bundle_script() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/bundle.sh")
}

fn cargo_version() -> String {
    read("Cargo.toml")
        .lines()
        .find(|l| l.starts_with("version"))
        .and_then(|l| l.split('"').nth(1))
        .expect("Cargo.toml 没有 version")
        .to_string()
}

#[test]
fn bundle_dry_run_direct_distribution() {
    let v = cargo_version();
    let (rc, m, _) = dry_run(&real_bundle_script(), &[]);
    assert_eq!(rc, 0);
    assert_eq!(m["cargo_features"], "");
    assert_eq!(m["bundle_id"], "ai.idoris.agentear");
    assert_eq!(m["app_name"], "AgentEar.app");
    assert_eq!(m["build_number"], v);
    assert_eq!(m["short_version"], v);
    assert_eq!(m["probe_assert"], "none");
}

#[test]
fn bundle_dry_run_app_store_has_no_probe_by_default() {
    let (rc, m, _) = dry_run(&real_bundle_script(), &[("AGENTEAR_DISTRIBUTION", "app-store")]);
    assert_eq!(rc, 0);
    assert_eq!(m["cargo_features"], "--features app-store");
    assert_eq!(m["bundle_id"], "ai.idoris.agentear.mas");
    assert_eq!(m["app_name"], "AgentEar-MAS.app");
    assert_eq!(m["probe_assert"], "must-not-have");
    // 未设构建号：退回版本号
    assert_eq!(m["build_number"], cargo_version());
}

#[test]
fn bundle_dry_run_app_store_probe_only_when_asked() {
    let (rc, m, _) = dry_run(
        &real_bundle_script(),
        &[("AGENTEAR_DISTRIBUTION", "app-store"), ("AGENTEAR_MAS_PROBE", "1")],
    );
    assert_eq!(rc, 0);
    assert_eq!(m["cargo_features"], "--features mas-probe");
    assert_eq!(m["probe_assert"], "must-have");
    // 直接分发版里 AGENTEAR_MAS_PROBE 不起作用
    let (_, m, _) = dry_run(&real_bundle_script(), &[("AGENTEAR_MAS_PROBE", "1")]);
    assert_eq!(m["cargo_features"], "");
    assert_eq!(m["probe_assert"], "none");
}

#[test]
fn bundle_dry_run_build_number_is_separate_from_version() {
    let (rc, m, _) = dry_run(
        &real_bundle_script(),
        &[("AGENTEAR_DISTRIBUTION", "app-store"), ("AGENTEAR_BUILD_NUMBER", "7")],
    );
    assert_eq!(rc, 0);
    assert_eq!(m["build_number"], "7");
    assert_eq!(m["short_version"], cargo_version());
}

#[test]
fn bundle_dry_run_rejects_malformed_build_number() {
    let (rc, m, err) = dry_run(
        &real_bundle_script(),
        &[("AGENTEAR_DISTRIBUTION", "app-store"), ("AGENTEAR_BUILD_NUMBER", "1.2.3.4")],
    );
    assert_eq!(rc, 1, "四段构建号必须 exit 1");
    assert!(m.is_empty(), "校验失败时不该打印任何 key=value");
    assert!(err.contains("AGENTEAR_BUILD_NUMBER"));
}

/// FU-30：vendor 是符号链接（或含指向包外的链接）时拒绝打包。在临时 ROOT 里造结构，不碰真实 vendor。
#[test]
fn bundle_refuses_symlinked_vendor() {
    use std::os::unix::fs::symlink;
    let root = std::env::temp_dir().join(format!("agentear-bundle-fu30-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("scripts")).unwrap();
    fs::copy(real_bundle_script(), root.join("scripts/bundle.sh")).unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\nversion = \"0.0.1\"\n").unwrap();
    let script = root.join("scripts/bundle.sh");

    // 没有 vendor：干跑照常通过并报 missing
    let (rc, m, _) = dry_run(&script, &[]);
    assert_eq!((rc, m["vendor"].as_str()), (0, "missing"));

    // 真实目录：ok
    let real = root.join("elsewhere");
    fs::create_dir_all(real.join("bin")).unwrap();
    fs::create_dir_all(root.join("vendor/bin")).unwrap();
    let (rc, m, _) = dry_run(&script, &[]);
    assert_eq!((rc, m["vendor"].as_str()), (0, "ok"));

    // 内部含指向包外的链接
    symlink(real.join("bin"), root.join("vendor/bin/asr")).unwrap();
    let (rc, m, err) = dry_run(&script, &[]);
    assert_eq!((rc, m["vendor"].as_str()), (1, "external-symlink"));
    assert!(err.contains("实拷"));
    fs::remove_file(root.join("vendor/bin/asr")).unwrap();

    // vendor 本身是链接
    fs::remove_dir_all(root.join("vendor")).unwrap();
    symlink(&real, root.join("vendor")).unwrap();
    let (rc, m, _) = dry_run(&script, &[]);
    assert_eq!((rc, m["vendor"].as_str()), (1, "symlink"));

    let _ = fs::remove_dir_all(&root);
}
