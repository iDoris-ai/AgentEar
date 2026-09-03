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

// ─────────────────────────────────────────────────────────────────────
// 边车的生命周期
// ─────────────────────────────────────────────────────────────────────

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Condvar, Mutex};

/// **连接优先，拉起是兜底。**
///
/// jason 2026-09-03 定的原则，值得原样记下来：
///
/// > 未来我们有自己独立的模型调用入口。现在是写死的，希望做成标准的配置。
/// > 我们约定好服务的访问地址，按配置文件去访问就行。如果没有，
/// > 再尝试按配置去拉起。
///
/// 行为顺序是死的：先按 `llm_url` 连 → 连不上且配置允许才拉起 →
/// 仍不行就降级。`llm_autostart: false` 得到的就是那个未来：**只连不拉**。
///
/// ## 为什么这不违反 ADR-0002 的边界
///
/// 那条约束要的是「独立进程 + 明确协议边界，可独立重启、独立崩溃」。
/// 拉起一个外部进程不改变这三条。**「拉起」和「内嵌」是两回事**——
/// 内嵌是把 Python 运行时塞进我们的二进制，那才是被排除的东西。

/// 生命周期状态。
///
/// ⚠️ **必须是状态机，不能只是 `Option<Child>`**（codex High 1）：
/// 启动线程和菜单点击线程可以同时探测到「没起来」、同时 spawn，
/// 后一个赋值会 **drop 掉前一个 `Child` 句柄却不杀那个进程**——
/// 于是留下一个没人管、占着几 GB 内存、`shutdown()` 也杀不掉的孤儿。
///
/// `Starting` 这一档就是为此存在的：**先在锁里占住位子再去 spawn**。
enum Lifecycle {
    Idle,
    /// 有人正在拉起，别的调用者等着就行，不要再起一个。
    Starting,
    Running(std::process::Child),
    /// 正在退出。此时才 spawn 完成的进程要**立刻杀掉**，
    /// 否则它会活过 AgentEar。
    ShuttingDown,
}

static STATE: Mutex<Lifecycle> = Mutex::new(Lifecycle::Idle);
static STATE_CV: Condvar = Condvar::new();

/// 子进程的 pid，给**信号处理函数**用。
///
/// 信号处理函数里能做的事极少（必须 async-signal-safe），
/// 碰不了 `Mutex`、更碰不了 `Child`。而 `kill(2)` 是安全的，
/// 所以单独存一个原子 pid。0 表示没有。
static SPAWNED_PID: AtomicI32 = AtomicI32::new(0);

/// 边车此刻的可用状态。**这是给 correct/label 做门控用的**，
/// 不只是显示。
static HEALTH: Mutex<Health> = Mutex::new(Health::Down);

/// 拉起后等它就绪的上限。模型要从磁盘加载 7.8 GB，实测冷启动几十秒。
const READY_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// 连得上，且确认是我们要的服务。
    Up,
    /// 连不上——**没有任何东西在那个地址上**。只有这一档才允许拉起。
    Down,
    /// 连得上但**不是**我们的服务，或者身份验不出来。
    ///
    /// ⚠️ 这一档要**尽量宽**（codex High 4）：HTTP 4xx/5xx、返回了内容
    /// 但没有 mlx-dspark 标记、探测超时——全算这一档。
    /// 因为它的后果是「不拉起」，而误判成 `Down` 的后果是
    /// **往一个已被占用的端口再起一个服务**，真正的问题会被
    /// 「启动失败」的日志盖住。保守方向是不拉。
    WrongService,
}

/// 现在能不能用。**correct / label 在发请求前必须问这个**。
///
/// codex High 3：早先 `ensure_available` 的结果只写进日志，
/// 而 `correct`/`label` 仍然只看配置开关——于是一个**已经被识别为
/// 「端口被别人占了」的服务，照样会收到用户的转写文本**。
pub fn is_ready() -> bool {
    *HEALTH.lock().unwrap_or_else(|e| e.into_inner()) == Health::Up
}

pub fn health() -> Health {
    *HEALTH.lock().unwrap_or_else(|e| e.into_inner())
}

fn set_health(h: Health) {
    *HEALTH.lock().unwrap_or_else(|e| e.into_inner()) = h;
}

/// 探测一次，不拉起。**会更新全局健康状态。**
///
/// ## 分类的依据（改过一次，见 Health::WrongService）
///
/// 不能用 `curl -f`：那会把 4xx/5xx 变成「失败」，与「连不上」
/// 混为一谈，于是一个**返回 500 的占用者**会被判成 `Down` 并触发拉起。
/// 改成不带 `-f`，靠 curl 的退出码区分：
///
/// | curl 退出码 | 含义 | 判成 |
/// |---|---|---|
/// | 0 | 连上且拿到响应 | 看内容有没有 mlx-dspark |
/// | 7 | 连接被拒 / 没人监听 | `Down` |
/// | 其余（28 超时、52 空回复……） | 有东西但不对劲 | `WrongService` |
pub fn probe(url: &str) -> Health {
    let out = Command::new("/usr/bin/curl")
        .args(["-sS", "--max-time", "2", &format!("{url}/v1/models")])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    let h = match out {
        Ok(o) if o.status.success() => {
            if String::from_utf8_lossy(&o.stdout).contains("mlx-dspark") {
                Health::Up
            } else {
                Health::WrongService
            }
        }
        // 7 = Failed to connect：那个地址上确实没人
        Ok(o) if o.status.code() == Some(7) => Health::Down,
        // 其余非零：连上了但不对劲（超时、空回复、协议错）。
        // 保守判成 WrongService——后果是不拉起，比误拉起安全。
        Ok(_) => Health::WrongService,
        // curl 都起不来：算不可用，但不该去拉服务（问题在本机）
        Err(_) => Health::WrongService,
    };
    set_health(h);
    h
}

/// 确保边车可用：先连，连不上再按配置拉起。
///
/// 返回 `true` 表示现在可用。任何失败都只记日志——调用方据此降级。
pub fn ensure_available(url: &str, autostart: bool, start_command: &[String]) -> bool {
    match probe(url) {
        Health::Up => return true,
        Health::WrongService => {
            log::error!("{url} 上有东西但不是 mlx-dspark（或验不出身份）——不会尝试拉起");
            return false;
        }
        Health::Down => {}
    }

    if !autostart {
        log::info!("边车不可用，且配置里关掉了自动拉起（llm_autostart=false）");
        return false;
    }
    let Some((prog, args)) = start_command.split_first() else {
        log::info!("边车不可用，且没有配置拉起命令（llm_start_command 为空）");
        return false;
    };

    // —— 在锁里占位，然后才 spawn ——
    {
        let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match &mut *st {
                Lifecycle::ShuttingDown => return false,
                Lifecycle::Starting => {
                    // 别人正在拉，等它拉完再看结果，**不要再起一个**
                    let (g, timeout) = STATE_CV
                        .wait_timeout(st, READY_TIMEOUT)
                        .unwrap_or_else(|e| e.into_inner());
                    st = g;
                    if timeout.timed_out() {
                        log::warn!("等别的线程拉起边车超时");
                        return false;
                    }
                    continue;
                }
                Lifecycle::Running(child) => {
                    if matches!(child.try_wait(), Ok(None)) {
                        log::warn!("已经拉起过边车且进程还在，但它没有响应——不重复拉起");
                        return false;
                    }
                    // 之前那个已经退出了
                    SPAWNED_PID.store(0, Ordering::SeqCst);
                    *st = Lifecycle::Idle;
                    continue;
                }
                Lifecycle::Idle => {
                    *st = Lifecycle::Starting;
                    break;
                }
            }
        }
    }

    log::info!("边车不可用，按配置拉起：{prog} {}", args.join(" "));
    let spawned = Command::new(prog)
        .args(args)
        // 输出丢弃：边车自己写日志，而把它的 stdout 接进没人读的管道，
        // 管道写满就会把它卡死（download.rs 踩过同类坑）
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            log::error!("拉起边车失败（{prog}）: {e}");
            let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
            *st = Lifecycle::Idle;
            STATE_CV.notify_all();
            return false;
        }
    };

    // 拿回锁登记。**如果这期间进入了退出流程，立刻把刚起的杀掉**——
    // 否则它会活过 AgentEar。
    {
        let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*st, Lifecycle::ShuttingDown) {
            log::info!("拉起完成时已在退出，立刻收掉刚起的边车");
            let _ = child.kill();
            let _ = child.wait();
            STATE_CV.notify_all();
            return false;
        }
        SPAWNED_PID.store(child.id() as i32, Ordering::SeqCst);
        *st = Lifecycle::Running(child);
        STATE_CV.notify_all();
    }

    // 等就绪
    let start = std::time::Instant::now();
    while start.elapsed() < READY_TIMEOUT {
        std::thread::sleep(Duration::from_secs(2));
        if probe(url) == Health::Up {
            log::info!("边车已就绪（等了 {:.0}s）", start.elapsed().as_secs_f32());
            return true;
        }
        let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
        if let Lifecycle::Running(c) = &mut *st {
            if let Ok(Some(code)) = c.try_wait() {
                log::error!("边车启动后立刻退出了（{code:?}），检查 {prog} 能不能单独跑通");
                SPAWNED_PID.store(0, Ordering::SeqCst);
                *st = Lifecycle::Idle;
                STATE_CV.notify_all();
                return false;
            }
        } else {
            // 被别人收走了（退出流程）
            return false;
        }
    }
    log::error!("边车启动后 {READY_TIMEOUT:?} 内没有就绪");
    false
}

/// 退出时收拾**我们自己拉起的**那个进程。
///
/// 不是我们拉起的一律不动——用户可能自己开着终端跑服务，
/// AgentEar 退出把它杀了是很难排查的越权。
///
/// **幂等**：多条退出路径都会调它（菜单 Quit、信号、restart、worker 失败）。
pub fn shutdown() {
    let child = {
        let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::mem::replace(&mut *st, Lifecycle::ShuttingDown);
        STATE_CV.notify_all();
        match prev {
            Lifecycle::Running(c) => Some(c),
            _ => None,
        }
    };
    // **杀和 wait 都在锁外做**：wait 可能要等一会儿，
    // 持着全局锁等于把菜单的 Quit 处理和所有可用性检查一起冻住。
    if let Some(mut c) = child {
        log::info!("退出：关掉我们拉起的边车");
        let _ = c.kill();
        let _ = c.wait();
    }
    SPAWNED_PID.store(0, Ordering::SeqCst);
}

/// 注册 SIGINT / SIGTERM，让它们也能收拾边车。
///
/// ## 为什么不能在 handler 里调 `shutdown()`
///
/// 信号处理函数必须 **async-signal-safe**：不能锁 `Mutex`、不能分配内存、
/// 不能碰 `Child`。所以 handler 里只做一件事——对着原子里存的 pid
/// 调 `kill(2)`，那是明确安全的。
///
/// ⚠️ **SIGKILL 覆盖不到**（内核不给机会）。那种情况下边车会变成孤儿，
/// 下次启动时 `probe` 会发现端口上已经有一个能用的服务、直接复用它，
/// 所以不会重复起——但那个进程不再受 AgentEar 管理。
/// 要彻底解决得给边车套一个看门狗（监视父进程 PID），
/// 那是更大的工程，现在只把限制记在这里。
pub fn install_signal_handlers() {
    unsafe extern "C" fn on_signal(sig: i32) {
        let pid = SPAWNED_PID.load(Ordering::SeqCst);
        if pid > 0 {
            // SIGTERM 给它机会自己收尾
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        // 恢复默认行为再把信号发给自己，保持正常的退出语义
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }
    unsafe {
        libc::signal(libc::SIGINT, on_signal as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as libc::sighandler_t);
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    /// 关掉自动拉起时**只连不拉**——这就是「未来有独立模型入口」的形态。
    #[test]
    fn autostart_disabled_never_spawns() {
        // 1 端口不会有服务；给一个会明显留下痕迹的命令，断言它没被执行
        let marker = std::env::temp_dir().join(format!("agentear-should-not-exist-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let cmd = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("touch {}", marker.display()),
        ];

        let ok = ensure_available("http://127.0.0.1:1", false, &cmd);
        assert!(!ok, "连不上时应该返回不可用");
        assert!(!marker.exists(), "autostart=false 时绝不该执行拉起命令");
    }

    /// 没有配置拉起命令时也不拉。
    #[test]
    fn empty_command_never_spawns() {
        assert!(!ensure_available("http://127.0.0.1:1", true, &[]));
    }

    /// **端口被别的服务占着时不拉起**，而且要和「连不上」区分开。
    ///
    /// 这一条是 2026-09-02 真实踩到的：8793 被本机另一个 node 服务占用，
    /// 不区分的话 AgentEar 会把转写文本发给它，而它只要返回一个形状合法的
    /// 响应就会被采信（实测拿到过一句关于产品配色的话）。
    #[test]
    fn wrong_service_is_distinguished_from_down() {
        let port = 39217;
        // 用 python 起一个返回合法 HTTP 但不是 mlx-dspark 的服务。
        // 不用 `nc -l`：macOS 的 nc 在监听模式下行为不稳，连不上就测了个寂寞。
        let mut server = Command::new("python3")
            .arg("-c")
            .arg(format!(
                r#"
import http.server
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        b = b'{{"who":"not-us"}}'
        self.send_response(200)
        self.send_header('Content-Length', str(len(b)))
        self.end_headers()
        self.wfile.write(b)
    def log_message(self, *a): pass
http.server.HTTPServer(('127.0.0.1', {port}), H).serve_forever()
"#
            ))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("起假服务失败");

        let url = format!("http://127.0.0.1:{port}");
        // **等它真的起来再测**，否则测到的是 Down，这条断言就没意义了
        let mut ready = false;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(100));
            if probe(&url) != Health::Down {
                ready = true;
                break;
            }
        }

        let h = probe(&url);
        let _ = server.kill();
        let _ = server.wait();

        assert!(ready, "假服务没起来，这条测试无从判断");
        assert_eq!(h, Health::WrongService, "占着端口的别家服务必须判成 WrongService");
    }

    /// **返回 HTTP 错误的占用者也必须判成 WrongService，不能判成 Down。**
    ///
    /// codex High 4：早先用 `curl -f`，4xx/5xx 会变成「命令失败」，
    /// 与「连不上」混为一谈——于是一个返回 500 的占用者会被判成 `Down`
    /// 并触发拉起，而端口其实已经被占着。原来的测试只覆盖了返回 200 的
    /// 假服务，正好漏掉这一整类。
    #[test]
    fn an_http_error_from_an_occupier_is_wrong_service_not_down() {
        let port = 39218;
        let mut server = Command::new("python3")
            .arg("-c")
            .arg(format!(
                r#"
import http.server
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_error(500, 'boom')
    def log_message(self, *a): pass
http.server.HTTPServer(('127.0.0.1', {port}), H).serve_forever()
"#
            ))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("起假服务失败");

        let url = format!("http://127.0.0.1:{port}");
        let mut ready = false;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(100));
            if probe(&url) != Health::Down {
                ready = true;
                break;
            }
        }
        let h = probe(&url);
        let _ = server.kill();
        let _ = server.wait();

        assert!(ready, "假服务没起来，这条测试无从判断");
        assert_eq!(
            h,
            Health::WrongService,
            "返回 500 的占用者被判成了 Down —— 那会触发一次不该发生的拉起"
        );
    }

    /// **健康状态是门控用的，不只是显示。**
    ///
    /// codex High 3：`ensure_available` 的结果早先只写日志，
    /// 而 correct/label 只看配置开关——于是已知「端口被别人占了」的服务
    /// 照样会收到转写文本。
    #[test]
    fn health_gates_whether_we_send_anything() {
        probe("http://127.0.0.1:1"); // 置成 Down
        assert!(!is_ready(), "Down 时不该判为就绪");
        set_health(Health::WrongService);
        assert!(!is_ready(), "WrongService 时更不能发东西过去");
        set_health(Health::Up);
        assert!(is_ready());
        set_health(Health::Down); // 收拾干净，别影响别的测试
    }

    /// 连不上时是 Down。
    #[test]
    fn unreachable_is_down() {
        assert_eq!(probe("http://127.0.0.1:1"), Health::Down);
    }
}

#[cfg(test)]
mod live_lifecycle_tests {
    use super::*;

    /// **真的拉起一次边车。**
    ///
    /// 前置：边车没在跑（测试会先确认）。它会执行配置里的启动脚本、
    /// 等模型加载（最多 120 秒）、确认就绪，然后**留着它**——
    /// 不 shutdown，因为后面的集成测试还要用。
    ///
    /// 标 ignore：要下好模型的机器才跑得动。
    #[test]
    #[ignore = "会真的拉起边车，需要 ~/.agentear/llm 已备好"]
    fn really_starts_the_sidecar() {
        let url = "http://127.0.0.1:8793";
        // 边车已经在跑时**这条测试验证不了「拉起」**——它需要一个干净的起点。
        // 明确跳过并说清原因，而不是断言失败：那会让人以为功能坏了，
        // 而实际上只是环境不满足前置条件。
        if probe(url) != Health::Down {
            eprintln!("跳过：边车已在运行。要测拉起，先 pkill -f 'mlx-dspark serve'");
            return;
        }

        let cmd = vec!["/Users/jason/Dev/tools/AgentEar/scripts/serve-llm.sh".to_string()];
        let t0 = std::time::Instant::now();
        let ok = ensure_available(url, true, &cmd);
        println!("拉起耗时 {:.0}s", t0.elapsed().as_secs_f32());

        assert!(ok, "应该能拉起并就绪");
        assert_eq!(probe(url), Health::Up);
    }
}
