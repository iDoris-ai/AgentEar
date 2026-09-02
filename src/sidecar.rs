//! LLM 边车的共享客户端：HTTP 传输 + 响应解析。
//!
//! ## 为什么要有这个模块
//!
//! `correct` 和 `label` 各自持有一份 curl 调用和一份响应解析，两处的
//! `finish_reason` 检查、身份校验、错误分类逻辑几乎一样——而**几乎一样
//! 是最坏的情况**：改一处忘另一处，谁也发现不了（codex 两轮评审里
//! 各指出过一次同类问题只修了一边）。
//!
//! ## 为什么要抽 `Transport`
//!
//! 更实在的理由是**可测性**。这些分支现在只能靠真实边车碰运气覆盖：
//!
//! - 响应是垃圾 JSON / 缺字段 / `content` 为空
//! - HTTP 500 但响应体形状恰好合法
//! - `finish_reason: "length"`（输出被截断的半句话）
//! - 长文分批时中间某一批失败
//!
//! 它们全都是「静默出错」型——不会崩，只会把错的东西粘进用户的窗口。
//! 靠真实边车测不出来，因为你没法让它按需返回垃圾。
//! 注入一个假的传输层就能确定性地覆盖每一条。

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};

/// 一次 HTTP POST。
///
/// 实现要保证：**非 2xx 一律返回 `Err`**。把错误页当正文交给调用方，
/// 会让「HTTP 500 但响应体恰好合法」变成一个静默的错误结果
/// （codex 在 label 那轮抓到过）。
pub trait Transport: Send + Sync {
    fn post_json(&self, url: &str, body: &str, timeout_secs: u64) -> Result<String>;
}

/// 生产实现：调系统的 curl。
///
/// 不引 HTTP 客户端库的理由和 `download.rs` 一样——一个 reqwest 会带进
/// 上百个传递依赖和一整套 TLS 栈，而这里连的是 127.0.0.1，连 TLS 都不需要。
pub struct Curl;

impl Transport for Curl {
    fn post_json(&self, url: &str, body: &str, timeout_secs: u64) -> Result<String> {
        let mut child = Command::new("/usr/bin/curl")
            // `-f`：4xx/5xx 以非零退出，而不是把错误页当正文交上来
            .arg("-fsS")
            .arg("--max-time")
            .arg(timeout_secs.to_string())
            .arg("-X")
            .arg("POST")
            .arg("-H")
            .arg("Content-Type: application/json")
            // 请求体走 stdin：转写内容可能很长，也可能含引号和换行，
            // 塞进 argv 既有长度上限又容易被 shell 语义咬到
            .arg("--data-binary")
            .arg("@-")
            .arg(format!("{url}/v1/chat/completions"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("启动 curl 失败")?;

        // 写失败也要把子进程收掉：直接 `?` 返回会让 Child 没 wait 就 drop，
        // 留下僵尸。守护进程一开就是几周，每次录音漏一个，积少成多。
        let write_result = (|| -> Result<()> {
            child
                .stdin
                .take()
                .context("拿不到 curl 的 stdin")?
                .write_all(body.as_bytes())
                .context("写请求体失败")
        })();
        if let Err(e) = write_result {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }

        let out = child.wait_with_output().context("等待 curl 失败")?;
        if !out.status.success() {
            bail!(
                "curl 退出码 {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// 从 OpenAI 风格的 chat completion 响应里取出正文。
///
/// **两道检查缺一不可**：
///
/// 1. `finish_reason` 必须是 `stop`。`length` 表示被 `max_tokens` 截断，
///    `content` 里是**半句话**——而它一样非空。不检查的话，半句话会被
///    当成成功结果自动上屏，覆盖掉本来完整的原文。
/// 2. `content` 必须存在且非空。
pub fn extract_content(raw: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(raw)
        .with_context(|| format!("解析响应失败: {}", raw.chars().take(200).collect::<String>()))?;

    let finish = v["choices"][0]["finish_reason"].as_str().unwrap_or("");
    if finish != "stop" {
        bail!("响应未正常结束（finish_reason={finish:?}）");
    }

    let content = v["choices"][0]["message"]["content"]
        .as_str()
        .context("响应里没有 choices[0].message.content")?;
    if content.trim().is_empty() {
        bail!("模型返回空内容");
    }
    Ok(content.to_string())
}

#[cfg(test)]
pub mod test_support {
    use super::*;
    use std::sync::Mutex;

    /// 按调用顺序吐出预设响应的假传输层。
    ///
    /// 这就是抽 `Transport` 的全部理由：让「垃圾 JSON」「HTTP 500」
    /// 「截断的输出」这些**静默出错**的分支变成可以确定性复现的测试，
    /// 而不是靠真实边车碰运气。
    pub struct Fake {
        /// 每次调用弹出一个。`Ok` 是响应体，`Err` 是传输层失败（如 HTTP 500）。
        pub responses: Mutex<Vec<Result<String, String>>>,
        pub calls: Mutex<usize>,
    }

    impl Fake {
        /// 所有调用都返回同一个响应体。
        pub fn always(body: &str) -> Self {
            Self {
                responses: Mutex::new(vec![Ok(body.to_string()); 64]),
                calls: Mutex::new(0),
            }
        }

        /// 按顺序返回。用于测「第几批失败」。
        pub fn sequence(items: Vec<Result<String, String>>) -> Self {
            Self { responses: Mutex::new(items), calls: Mutex::new(0) }
        }

        /// 包一段正文成合法的 chat completion 响应。
        pub fn ok_body(content: &str) -> String {
            serde_json::json!({
                "choices": [{
                    "finish_reason": "stop",
                    "message": { "content": content }
                }]
            })
            .to_string()
        }

        pub fn call_count(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    impl Transport for Fake {
        fn post_json(&self, _url: &str, _body: &str, _t: u64) -> Result<String> {
            *self.calls.lock().unwrap() += 1;
            let mut r = self.responses.lock().unwrap();
            if r.is_empty() {
                bail!("Fake：响应用完了（调用次数超出预设）");
            }
            match r.remove(0) {
                Ok(s) => Ok(s),
                Err(e) => bail!("Fake 传输层失败: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::Fake;

    /// 正常响应能取出正文。
    #[test]
    fn extracts_content_from_a_normal_response() {
        assert_eq!(extract_content(&Fake::ok_body("纠正后的文本")).unwrap(), "纠正后的文本");
    }

    /// **截断的响应必须被拒。**
    ///
    /// `finish_reason: "length"` 时 `content` 是半句话，而它一样非空——
    /// 不检查就会把半句话当成功结果上屏。
    #[test]
    fn truncated_response_is_rejected() {
        let raw = serde_json::json!({
            "choices": [{ "finish_reason": "length", "message": { "content": "这是半句" } }]
        })
        .to_string();
        let e = extract_content(&raw).unwrap_err().to_string();
        assert!(e.contains("length"), "错误里要说清是被截断了: {e}");
    }

    /// 垃圾 JSON、缺字段、空正文，全都要报错而不是返回可疑内容。
    #[test]
    fn malformed_responses_are_rejected() {
        for (raw, why) in [
            ("这不是 JSON", "垃圾"),
            ("{}", "缺 choices"),
            (r#"{"choices":[]}"#, "choices 为空"),
            (r#"{"choices":[{"finish_reason":"stop"}]}"#, "缺 message"),
            (
                r#"{"choices":[{"finish_reason":"stop","message":{"content":"   "}}]}"#,
                "content 全是空白",
            ),
        ] {
            assert!(extract_content(raw).is_err(), "应该拒绝（{why}）: {raw}");
        }
    }

    /// 缺 `finish_reason` 也要拒——不能默认当成功。
    #[test]
    fn missing_finish_reason_is_rejected() {
        let raw = r#"{"choices":[{"message":{"content":"内容"}}]}"#;
        assert!(extract_content(raw).is_err(), "没有 finish_reason 时不该当成正常结束");
    }

    /// 假传输层按顺序吐响应，且记得调用次数。
    #[test]
    fn fake_transport_serves_in_order_and_counts() {
        let f = Fake::sequence(vec![
            Ok(Fake::ok_body("第一次")),
            Err("HTTP 500".into()),
            Ok(Fake::ok_body("第三次")),
        ]);
        assert_eq!(extract_content(&f.post_json("u", "b", 1).unwrap()).unwrap(), "第一次");
        assert!(f.post_json("u", "b", 1).is_err(), "第二次应该是传输失败");
        assert_eq!(extract_content(&f.post_json("u", "b", 1).unwrap()).unwrap(), "第三次");
        assert_eq!(f.call_count(), 3);
    }
}
