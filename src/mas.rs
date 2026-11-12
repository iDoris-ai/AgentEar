//! Mac App Store 沙箱版（cargo feature `app-store`）专用代码。
//!
//! 这个文件只在 `--features app-store` 下编译；默认构建不含它，行为一个字节不变。
//! 内容：
//! - `apply_limits`：把配置压到 MAS 子集（见 `config::load` / `config::update`）。
//!
//! 沙箱探针（`--mas-probe` / `--mas-probe-run`）在 `mas_probe.rs`，另有 feature `mas-probe`，提审构建不含。

use crate::config::{Config, InputTranslate, TalkMode, Trigger};
use crate::engine::AsrBackend;

/// MAS 子集不提供的功能一律在这里压回安全值。配置文件里写了也不生效。
///
/// 为什么压配置而不是到处加 `cfg`：这些功能的入口（菜单、命令行、启动对账）
/// 都读配置，压在读取处，下游所有分支一次性变成「功能关闭」的既有行为，
/// 不需要在几十处各开一个新分支。
pub fn apply_limits(c: &mut Config) {
    // 右 Command 手势要 CGEventTap（输入监控/辅助功能）；MAS 初期版只给组合键。
    c.trigger = Trigger::CtrlShiftR;
    // 自动 ⌘V 靠 CGEventPost + 辅助功能；MAS 版只写剪贴板。
    c.auto_paste = false;
    // 对话模式要 Python 边车（下载并执行代码，审核指南 2.5.2）。
    c.talk_mode = TalkMode::InputMethod;
    // Qwen3 要下载运行时可执行文件（审核指南 2.5.2）。
    c.asr_backend = AsrBackend::Builtin;
    c.qwen3_resident = false;
    // 开机自启靠写 ~/Library/LaunchAgents。
    c.launch_at_login = false;
    // 翻译输入走本机 LLM 边车。
    c.input_translate = InputTranslate::Off;
    // 理解层 LLM 边车（自动拉起）。
    c.llm_autostart = false;
    c.llm_start_command.clear();
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_force_the_mas_subset_whatever_the_config_says() {
        let mut c = Config::default();
        c.trigger = Trigger::RightCommand;
        c.auto_paste = true;
        c.talk_mode = TalkMode::Conversation;
        c.asr_backend = AsrBackend::SpeechSwift;
        c.qwen3_resident = true;
        c.launch_at_login = true;
        c.input_translate = InputTranslate::En;
        c.llm_autostart = true;
        c.llm_start_command = vec!["x".into()];
        apply_limits(&mut c);
        assert_eq!(c.trigger, Trigger::CtrlShiftR);
        assert!(!c.auto_paste);
        assert_eq!(c.talk_mode, TalkMode::InputMethod);
        assert_eq!(c.asr_backend, AsrBackend::Builtin);
        assert!(!c.qwen3_resident);
        assert!(!c.launch_at_login);
        assert_eq!(c.input_translate, InputTranslate::Off);
        assert!(!c.llm_autostart);
        assert!(c.llm_start_command.is_empty());
    }

    #[test]
    fn a_config_file_asking_for_the_removed_features_is_clamped() {
        let mut c: Config = serde_json::from_str(
            r#"{"trigger":"right_command","auto_paste":true,"talk_mode":"conversation"}"#,
        )
        .unwrap();
        assert_eq!(c.trigger, Trigger::RightCommand, "前提：文件里确实写了");
        apply_limits(&mut c);
        assert_eq!(c.trigger, Trigger::CtrlShiftR);
        assert!(!c.auto_paste);
        assert_eq!(c.talk_mode, TalkMode::InputMethod);
    }
}
