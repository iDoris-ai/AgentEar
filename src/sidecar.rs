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
use std::time::Duration;

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
        let mut cmd = Command::new("/usr/bin/curl");
        cmd.arg("-fsS")
            // curl 自己的超时**只覆盖它认得的阶段**（连接、传输）。
            // 父进程那一侧另有 deadline，见 run_with_deadline。
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
            .arg(format!("{url}/v1/chat/completions"));
        // 父进程的墙钟给到 curl 超时之上再加 5 秒：正常情况下该由 curl
        // 自己先退出，父进程这层只兜住「curl 根本没在按预期推进」的情况。
        run_with_deadline(cmd, body, Duration::from_secs(timeout_secs + 5))
    }
}

/// 起一个子进程，喂 stdin，收 stdout，**并且由父进程强制墙钟上限**。
///
/// ## 为什么不能只靠 `curl --max-time`
///
/// 那个参数覆盖的是 curl 自己认得的阶段（连接、传输）。它**不保证**
/// 「父子之间的管道交互」也在时限内：
///
/// - curl 卡在读 stdin（对端半开、内核缓冲满）
/// - curl 因为某种原因根本没进入传输阶段
///
/// 长录音分批之后，一次纠错要起 N 个子进程，**任何一个卡住都会拖住整段**，
/// 而用户按完录音键正等着上屏。
///
/// ## 为什么写 stdin 和读输出必须并发
///
/// 先写完再读会死锁：请求体大到填满管道缓冲时，父进程阻塞在写，
/// 而子进程正阻塞在写它自己的 stdout（没人读）——两边互等。
/// `download.rs` 那次踩的是同一类坑（stderr 管道写满让 curl 永不退出）。
fn run_with_deadline(mut cmd: Command, stdin_body: &str, deadline: Duration) -> Result<String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("启动子进程失败")?;

    // 三个方向各自一个线程，谁也别等谁。
    let mut stdin = child.stdin.take().context("拿不到 stdin")?;
    let body = stdin_body.to_string();
    let writer = std::thread::spawn(move || {
        // 写失败通常意味着对端已经退出（BrokenPipe），不是我们要报的错——
        // 真正的原因会在退出码和 stderr 里。
        let _ = stdin.write_all(body.as_bytes());
        drop(stdin); // 必须显式关掉，否则 curl 一直等 EOF
    });

    let mut out_pipe = child.stdout.take().context("拿不到 stdout")?;
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        use std::io::Read;
        let _ = out_pipe.read_to_string(&mut buf);
        buf
    });

    let mut err_pipe = child.stderr.take().context("拿不到 stderr")?;
    let err_reader = std::thread::spawn(move || {
        let mut buf = String::new();
        use std::io::Read;
        // 只留尾部：错误信息有用的在末尾，而无上限地攒可能吃掉大量内存
        let _ = err_pipe.read_to_string(&mut buf);
        let n = buf.chars().count();
        buf.chars().skip(n.saturating_sub(4096)).collect::<String>()
    });

    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if start.elapsed() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // 线程会因为管道关闭而自然结束，不 join 也不会泄漏；
                    // 但 join 一下让资源回收更确定。
                    let _ = writer.join();
                    let _ = reader.join();
                    let _ = err_reader.join();
                    bail!("子进程超过墙钟上限 {deadline:?}，已终止");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e).context("等待子进程失败");
            }
        }
    };

    let _ = writer.join();
    let out = reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();

    if !status.success() {
        bail!("子进程退出码 {:?}: {}", status.code(), err.trim());
    }
    Ok(out)
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

    /// **一个不读 stdin、也不退出的子进程，必须被父进程按时掐掉。**
    ///
    /// 这是 FU-5 的验收：`curl --max-time` 只覆盖它自己认得的阶段，
    /// 卡在父子管道交互上时不保证。长录音分批后一次纠错要起 N 个子进程，
    /// 任何一个卡住都会拖住整段，而用户正等着上屏。
    #[test]
    fn a_hung_child_is_killed_at_the_deadline() {
        let mut cmd = Command::new("/bin/sh");
        // 既不读 stdin 也不输出，睡 60 秒——正是要防的形状
        cmd.arg("-c").arg("sleep 60");

        let start = std::time::Instant::now();
        let r = run_with_deadline(cmd, "请求体", Duration::from_millis(600));
        let took = start.elapsed();

        assert!(r.is_err(), "卡住的子进程应该被判失败");
        assert!(
            r.unwrap_err().to_string().contains("墙钟上限"),
            "错误里要说清是超时"
        );
        assert!(
            took < Duration::from_secs(5),
            "应该在 deadline 附近就返回，实际花了 {took:?}"
        );
    }

    /// **请求体大到填满管道缓冲时也不能死锁。**
    ///
    /// 先写完再读会互等：父进程阻塞在写 stdin，子进程阻塞在写 stdout。
    /// 三个方向各一个线程才不会。
    #[test]
    fn a_large_body_does_not_deadlock() {
        let mut cmd = Command::new("/bin/cat");
        cmd.arg("-"); // 回显 stdin 到 stdout

        // 1 MB，远超典型管道缓冲（64 KB）
        let big = "x".repeat(1 << 20);
        let out = run_with_deadline(cmd, &big, Duration::from_secs(20))
            .expect("cat 回显不该失败");
        assert_eq!(out.len(), big.len(), "回显的内容长度应该和输入一致");
    }

    /// 子进程非零退出时，错误里要带上它的 stderr——否则排障没有线索。
    #[test]
    fn nonzero_exit_surfaces_stderr() {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("echo '出错了：连接被拒' >&2; exit 7");

        let e = run_with_deadline(cmd, "", Duration::from_secs(10))
            .unwrap_err()
            .to_string();
        assert!(e.contains("7"), "要带退出码: {e}");
        assert!(e.contains("连接被拒"), "要带 stderr 内容: {e}");
    }

    /// 正常退出时拿到 stdout。
    #[test]
    fn normal_child_returns_stdout() {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("cat -");
        let out = run_with_deadline(cmd, "回显这段", Duration::from_secs(10)).unwrap();
        assert_eq!(out, "回显这段");
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
