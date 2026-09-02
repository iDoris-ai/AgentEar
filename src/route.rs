//! `routes/` 记录：一次转写的下游决策。
//!
//! ## 它在存储语义里的位置
//!
//! ```text
//! raw/audio/          原始字节，丢了不可重建
//! derived/transcripts/ 模型输出，可从 raw 重算
//! routes/             下游决策，可重算   ← 这里
//! ```
//!
//! **可重算不等于可有可无**：在重算之前，它就是「这段话被判成了什么、
//! 投递到哪了」的唯一记录。CLAUDE.md 的存储语义要求 `routes/` 是
//! **本地权威记录**，先落盘再投递（架构边界 B6）——
//! 知识库挂了、投递失败，都不影响这里已经写好的东西。
//!
//! ## 为什么投递状态也在这里
//!
//! `delivery` 字段现在只会是 `pending`——本轮不做实际投递
//! （ADR-0003 的双适配器排在后面）。但它必须**现在就存在**：
//! 等投递做出来再往已有记录里加字段，意味着要处理「老记录没有这个字段」
//! 的迁移，而那是完全可以避免的。

use serde::{Deserialize, Serialize};

use crate::label::{Label, Source};

/// 投递状态。本轮只会产生 `Pending`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    #[default]
    Pending,
    Delivered,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delivery {
    pub state: DeliveryState,
    pub attempts: u32,
    /// 最后一次失败的原因。**留着不清空**——投递成功后它仍然是
    /// 「这条曾经失败过几次、为什么」的证据，排查时有用。
    pub last_error: Option<String>,
}

impl Default for Delivery {
    fn default() -> Self {
        Self { state: DeliveryState::Pending, attempts: 0, last_error: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    /// 内容寻址的 sha256，**与 `raw/audio/` 和 `derived/transcripts/` 对齐**。
    /// 三层用同一个 key，才能从任意一层找回另外两层。
    pub content_hash: String,
    /// RFC 3339 本地时间。用本地时间而不是 UTC：这些记录是给人看的，
    /// 「我昨天下午说的那条」要能对上。
    pub created_at: String,
    pub label: Label,
    pub label_source: Source,
    /// 模型推断时的置信度。显式标记时为 `None`——
    /// 用户明说的事情没有「置信度」可言。
    ///
    /// ⚠️ 当前边车不返回 logprobs，所以模型推断时**也是 `None`**。
    /// 字段先留着（见模块文档「为什么投递状态也在这里」的同一个理由）。
    pub confidence: Option<f32>,
    /// 二级标签（自由词表）。**本轮不抽取**，恒为空。
    #[serde(default)]
    pub secondary: Vec<String>,
    /// 纠错后的转写文本。
    pub text: String,
    #[serde(default)]
    pub delivery: Delivery,
}

impl Route {
    pub fn new(content_hash: impl Into<String>, label: Label, source: Source, text: impl Into<String>) -> Self {
        Self {
            content_hash: content_hash.into(),
            created_at: now_rfc3339(),
            label,
            label_source: source,
            confidence: None,
            secondary: Vec::new(),
            text: text.into(),
            delivery: Delivery::default(),
        }
    }

    /// 归档用的月份目录名，`2026-09`。
    ///
    /// 从 `created_at` 的前 7 个字符取。**不重新算当前时间**——
    /// 记录的归属月份必须和它自己的时间戳一致，否则跨月的那一刻
    /// 会出现「文件在 9 月目录、时间戳是 8 月」的错位。
    pub fn month(&self) -> String {
        self.created_at.chars().take(7).collect()
    }
}

/// 本地时区的 RFC 3339 时间戳。
///
/// 不引 chrono：为一个时间戳拉进一个日期库不划算，而 `date` 是系统自带的。
/// 拿不到就退回一个明确标记的占位——**记录不能因为取不到时间就不写**。
fn now_rfc3339() -> String {
    std::process::Command::new("/bin/date")
        .arg("+%Y-%m-%dT%H:%M:%S%z")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| s.len() >= 7)
        .unwrap_or_else(|| "0000-00-00T00:00:00+0000".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_comes_from_the_records_own_timestamp() {
        let mut r = Route::new("abc", Label::Idea, Source::Explicit, "内容");
        r.created_at = "2026-08-31T23:59:59+0700".into();
        assert_eq!(r.month(), "2026-08", "跨月时不能按当前时间归档");
    }

    /// 序列化出来的字段名要和 spec.md §3 一字不差——
    /// 这些文件是**长期存档**，改名字会让历史记录读不出来。
    #[test]
    fn field_names_match_the_spec() {
        let r = Route::new("h", Label::Idea, Source::Explicit, "t");
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        for k in [
            "content_hash",
            "created_at",
            "label",
            "label_source",
            "confidence",
            "secondary",
            "text",
            "delivery",
        ] {
            assert!(v.get(k).is_some(), "spec.md 要求有字段 {k}");
        }
        assert_eq!(v["delivery"]["state"], "pending", "投递状态初始必须是 pending");
        assert_eq!(v["delivery"]["attempts"], 0);
        assert!(v["confidence"].is_null(), "显式标记没有置信度");
    }

    /// 显式与推断两种来源都要能正确落进 JSON。
    #[test]
    fn label_source_roundtrips() {
        for src in [Source::Explicit, Source::Model] {
            let r = Route::new("h", Label::Task, src, "t");
            let back: Route = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
            assert_eq!(back.label_source, src);
            assert_eq!(back.label, Label::Task);
        }
    }

    /// 老记录缺 `secondary` / `delivery` 时要能读——
    /// 这两个字段是后加的语义，`#[serde(default)]` 必须真的生效。
    #[test]
    fn older_records_without_optional_fields_still_parse() {
        let json = r#"{
            "content_hash": "h",
            "created_at": "2026-09-01T10:00:00+0700",
            "label": "note",
            "label_source": "model",
            "confidence": null,
            "text": "旧记录"
        }"#;
        let r: Route = serde_json::from_str(json).expect("缺可选字段的老记录必须能读");
        assert_eq!(r.label, Label::Note);
        assert!(r.secondary.is_empty());
        assert_eq!(r.delivery.state, DeliveryState::Pending);
    }

    /// 时间戳格式至少要能被 `month()` 用。
    #[test]
    fn timestamp_is_usable() {
        let r = Route::new("h", Label::Unknown, Source::Model, "t");
        assert!(r.created_at.len() >= 7, "时间戳太短: {:?}", r.created_at);
        assert_eq!(r.month().len(), 7, "月份应该是 yyyy-mm");
        assert!(r.month().contains('-'));
    }
}
