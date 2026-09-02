//! 技术术语纠错：把 ASR 的输出送本地 LLM 过一遍。
//!
//! ## 为什么必须有这一层
//!
//! M0 横比四个 ASR 模型时发现：**`raw` 这个词四个模型全错**
//! （row / road / ro / roll），`Docker` → `doocca`，
//! `Kubernetes` → `cuubber needs`。而 jason 的使用场景几乎全是技术术语。
//!
//! ADR-0001 的结论是**换 ASR 模型解决不了**——那是声学层面的同音问题，
//! 只有结合上下文和术语表才能还原。M0 的 M2 基准验证了这一点：
//! 同样那 11 个靶子，本地 LLM **11/11 全部纠回**（`docs/benchmarks-m2.md` §1）。
//!
//! ## 三条不能违反的约束
//!
//! 1. **保留原始转写。** 纠错是有损操作——模型可能改错、可能过度改写。
//!    `derived/transcripts/` 里必须同时存得下纠正前后两版，
//!    原始那份是可以拿来重算的唯一依据。
//! 2. **纠错失败不能挡住上屏。** LLM 是独立进程，可能没起、可能崩了、
//!    可能正在加载模型。任何失败都退回**未纠正的文字**继续上屏，
//!    而不是什么都不出——用户宁可拿到一句有错别字的话，
//!    也不想按完键什么都没有。
//! 3. **必须确认对端是谁。** 见 `probe` 的说明，这条不是洁癖。
//!
//! ## 为什么用 curl 子进程而不是 HTTP 客户端库
//!
//! 和 `download.rs` 同一个理由：一个 reqwest 会带进上百个传递依赖和一整套
//! TLS 栈。这里连的是 `127.0.0.1`，连 TLS 都不需要。
//! 单次推理本身要 1–3 秒，起个进程那几毫秒可以忽略。

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

/// 边车的默认地址。
///
/// **端口是 8793 不是 8791。** `docs/benchmarks-m2.md` 原来记的是 8791，
/// 实测那个端口在本机被另一个项目的 node 服务占着。
pub const DEFAULT_URL: &str = "http://127.0.0.1:8793";

/// 单次纠错的超时。
///
/// 32.7 tok/s、输出几十到一百多 token，正常在 1–3 秒（`benchmarks-m2.md` §5）。
/// 给到 20 秒是为了容忍**模型冷加载**：服务刚起来时第一次请求要等权重进内存。
/// 再长就不值得了——用户按完录音键正等着上屏。
const TIMEOUT: Duration = Duration::from_secs(20);

/// 给模型的指令。
///
/// 措辞上有三处是刻意的，改之前先想清楚：
///
/// 1. **「只输出修正后的文本」** —— Ornith 默认会先吐 `Thinking Process:`。
///    服务端已经用 `--no-thinking` 关掉了，这里再要求一次是双保险：
///    万一有人手动起了服务忘了带那个参数，至少提示词还在挡。
/// 2. **「不要解释、不要加引号」** —— 模型很爱在结果外面裹一层
///    「修正后：xxx」。那些字会被原样粘到用户的光标处。
/// 3. **「如果没有需要修正的，原样输出」** —— 不给这句的话，
///    模型会对本来就正确的句子做「润色」，把口语改成书面语。
///    用户要的是逐字记录，不是作文。
const PROMPT_HEAD: &str = "\
你是语音转写的后处理器。下面这段文字是语音识别的输出，其中的**技术术语**可能被识别错了\
（同音或近音）。

下面是本项目的术语表。请按表里的规则还原，**严格照右边的写法输出（包括大小写）**：

";

/// 术语表和正文之间那一段。
///
/// 「表里没有的词一律不动」这句是本轮新增的**要害**：M2a 那次失败
/// （`ro的目录` 被纠成 `repo`）恰恰是模型自由发挥的产物——`repo` 在
/// 那个语境里完全说得通，只是不在我们的词汇表里。给了表还不约束范围，
/// 等于白给。
const PROMPT_TAIL: &str = "
先看两个例子，注意**同一个词在不同上下文里的处理完全相反**：

例 1（该替换）：
  输入：然后把内容存到 road 目录里面
  输出：然后把内容存到 raw 目录里面
  理由：在谈文件目录，说的是技术术语 raw

例 2（**不该替换**）：
  输入：这条 road 很宽，可以走两辆车
  输出：这条 road 很宽，可以走两辆车
  理由：在谈道路和车，road 就是它字面的意思，与本项目术语无关

规则：
- **表里没有的词一律不动。** 不要凭上下文猜测别的术语，不要替换成你觉得更常见的词
- 只输出修正后的文本，不要解释，不要加引号，不要写「修正后：」之类的前缀
- 如果没有需要修正的地方，把原文原样输出
- **除术语替换之外，不要增删或修改任何内容**，包括标点、空格和语气词
- 不要润色，不要把口语改成书面语

待修正的文字：
";

/// 边车此刻在不在。给菜单显示用。
///
/// **带 3 秒缓存**：菜单每次展开都会调它，而每次都起一个 curl 去探测的话，
/// 打开菜单会有肉眼可见的停顿。3 秒足够让「刚起好服务」在下次展开时反映出来。
pub fn service_reachable() -> bool {
    use std::sync::Mutex;
    use std::time::Instant;
    static CACHE: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, ok)) = *c {
        if at.elapsed() < Duration::from_secs(3) {
            return ok;
        }
    }
    let url = crate::config::get()
        .llm_url
        .unwrap_or_else(|| DEFAULT_URL.to_string());
    // 探测要快：菜单在等它。1 秒还连不上 127.0.0.1 就是没起。
    let ok = Command::new("/usr/bin/curl")
        .args(["-sS", "--max-time", "1", &format!("{url}/v1/models")])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("mlx-dspark"))
        .unwrap_or(false);
    *c = Some((Instant::now(), ok));
    ok
}

pub struct Corrector {
    url: String,
    /// 渲染好的术语清单。
    ///
    /// **每次纠错前重新加载**（见 `main` 的调用点），不缓存到进程生命周期——
    /// 用户改完术语表，下一次录音就该生效，不必重启守护进程。
    /// 表只有几 KB，读一次的代价远小于一次推理。
    terms_block: String,
}

impl Corrector {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into(), terms_block: String::new() }
    }

    /// 带术语表构造。没有术语表时（探测、测试）用 `new`。
    pub fn with_terms(url: impl Into<String>, terms: &crate::terms::Terms) -> Self {
        Self { url: url.into(), terms_block: terms.to_prompt_block() }
    }

    /// 确认对端**确实是我们的模型服务**。
    ///
    /// ## 这条不是洁癖，是 2026-09-02 真实踩到的
    ///
    /// 原计划用 8791，而本机另一个项目的 node 服务正好占着它。
    /// 连上去之后 `/v1/models` 返回的是那个服务的业务响应
    /// （一句关于产品配色的话）。如果不校验，AgentEar 会把**别人服务的回答**
    /// 当成纠错结果，直接粘进用户正在打字的窗口。
    ///
    /// 判据取 mlx-dspark 的两个特征字段，不是随便找个 200 就算数。
    pub fn probe(&self) -> Result<()> {
        let out = curl(&[&format!("{}/v1/models", self.url)], None)?;
        // 不做 JSON 解析：只要这两个标记同时出现就足够区分「是不是我们的服务」，
        // 而多引一层结构定义反而会因为上游改字段而脆。
        if !out.contains("mlx-dspark") {
            bail!(
                "{}上的服务不是 mlx-dspark（可能是别的程序占了这个端口）：{}",
                self.url,
                out.chars().take(200).collect::<String>()
            );
        }
        Ok(())
    }

    /// 纠正一段转写。返回 `None` 表示**没有可用的纠错结果**，调用方应当用原文。
    ///
    /// 注意签名：错误不往外抛。纠错是尽力而为的增强，它的任何失败都不该
    /// 变成调用方需要处理的分支——调用方只有「有更好的版本」和「没有」两种情况。
    pub fn correct(&self, text: &str) -> Option<String> {
        if text.trim().is_empty() {
            return None;
        }
        match self.try_correct(text) {
            Ok(fixed) => Some(fixed),
            Err(e) => {
                log::warn!("术语纠错不可用，按原文上屏: {e:#}");
                None
            }
        }
    }

    fn try_correct(&self, text: &str) -> Result<String> {
        let body = serde_json::json!({
            "model": "ornith",
            "messages": [{
                "role": "user",
                "content": format!("{PROMPT_HEAD}{}{PROMPT_TAIL}{text}", self.terms_block),
            }],
            // 纠错要的是确定性，不是创造力
            "temperature": 0.0,
            // 输出长度按输入给，留 2 倍余量。不设上限的话，模型跑飞时
            // 会一直生成到上下文用完，把 1–3 秒变成几十秒。
            "max_tokens": (text.chars().count() * 2 + 64).min(2048),
        })
        .to_string();

        let out = curl(
            &[
                "-X", "POST",
                "-H", "Content-Type: application/json",
                "--data-binary", "@-",
                &format!("{}/v1/chat/completions", self.url),
            ],
            Some(&body),
        )?;

        let v: serde_json::Value = serde_json::from_str(&out)
            .with_context(|| format!("解析响应失败: {}", out.chars().take(200).collect::<String>()))?;
        // **必须检查结束原因。** `finish_reason: "length"` 表示输出被
        // max_tokens 截断了——`content` 里是**半句话**，而它一样非空。
        // 不检查的话，半句话会被当成纠错成功，自动上屏并写进 transcript，
        // 覆盖掉本来完整的原文。宁可不纠错，也不能上屏半句。
        let finish = v["choices"][0]["finish_reason"].as_str().unwrap_or("");
        if finish != "stop" {
            bail!("响应未正常结束（finish_reason={finish:?}），按原文处理");
        }
        let content = v["choices"][0]["message"]["content"]
            .as_str()
            .context("响应里没有 choices[0].message.content")?;

        let fixed = tidy(content);
        if fixed.is_empty() {
            bail!("模型返回空");
        }
        Ok(fixed)
    }
}

/// 收拾模型输出。
///
/// **只取最后一行非空**——这是 M0 那次踩出来的判据
/// （`docs/benchmarks-m2.md` §2.1）：Ornith 会先输出推理过程再给答案，
/// 取整段的话推理文字会被粘到用户光标处。服务端 `--no-thinking` 已经关了它，
/// 这里是第二道。
///
/// 顺带剥掉模型爱加的包裹：前缀「修正后：」、成对的引号。
fn tidy(s: &str) -> String {
    let line = s.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let mut t = line;
    for p in ["修正后：", "修正后:", "修正结果：", "输出：", "Corrected:"] {
        if let Some(rest) = t.strip_prefix(p) {
            t = rest.trim();
        }
    }
    // 成对引号才剥。单边的引号可能是内容本身的一部分。
    for (a, b) in [('"', '"'), ('「', '」'), ('“', '”'), ('\'', '\'')] {
        if t.starts_with(a) && t.ends_with(b) && t.chars().count() >= 2 {
            t = &t[a.len_utf8()..t.len() - b.len_utf8()];
        }
    }
    t.trim().to_string()
}

/// 跑一次 curl，返回 stdout。
fn curl(args: &[&str], stdin_body: Option<&str>) -> Result<String> {
    let mut cmd = Command::new("/usr/bin/curl");
    cmd.arg("-sS")
        .arg("--max-time")
        .arg(TIMEOUT.as_secs().to_string())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin_body.is_some() {
        cmd.stdin(Stdio::piped());
    }

    let mut child = cmd.spawn().context("启动 curl 失败")?;
    if let Some(b) = stdin_body {
        // 请求体走 stdin（`--data-binary @-`），不进命令行参数：
        // 转写内容可能很长，也可能包含引号和换行，塞进 argv 既有长度上限
        // 又容易被 shell 语义咬到。
        child
            .stdin
            .take()
            .context("拿不到 curl 的 stdin")?
            .write_all(b.as_bytes())
            .context("写请求体失败")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 只取最后一行非空。
    ///
    /// 这条挡的是 M0 踩过的坑：模型输出推理过程 + 答案，取整段就会把
    /// 「Thinking Process: 用户说的 road 应该是 raw……」整段粘给用户。
    #[test]
    fn takes_only_the_last_nonempty_line() {
        assert_eq!(
            tidy("Thinking Process:\n用户说的 road 其实是 raw。\n\n把内容存到 raw 目录里面。\n"),
            "把内容存到 raw 目录里面。"
        );
    }

    /// 模型爱加的前缀和包裹引号要剥掉——它们会被原样敲进用户的窗口。
    #[test]
    fn strips_wrappers() {
        assert_eq!(tidy("修正后：把内容存到 raw 目录"), "把内容存到 raw 目录");
        assert_eq!(tidy("「把内容存到 raw 目录」"), "把内容存到 raw 目录");
        assert_eq!(tidy("\"把内容存到 raw 目录\""), "把内容存到 raw 目录");
    }

    /// **单边引号不能剥。** 用户真的可能说出带引号的内容，
    /// 剥掉就是篡改。
    #[test]
    fn keeps_unpaired_quotes() {
        assert_eq!(tidy("他说\"这个不行"), "他说\"这个不行");
        assert_eq!(tidy("引号「在中间」不动"), "引号「在中间」不动");
    }

    /// 空输入不该发起请求。
    #[test]
    fn empty_input_returns_none() {
        let c = Corrector::new("http://127.0.0.1:1");
        assert!(c.correct("").is_none());
        assert!(c.correct("   \n ").is_none());
    }

    /// **服务不可达时返回 None，不是 panic、不是 Err。**
    ///
    /// 这条钉住「纠错失败不挡上屏」这个约束：调用方只该看到
    /// 「有更好的版本」或「没有」，不该被迫处理错误分支。
    #[test]
    fn unreachable_service_degrades_quietly() {
        // 1 端口不会有人监听
        let c = Corrector::new("http://127.0.0.1:1");
        assert_eq!(c.correct("把内容存到 road 目录"), None);
    }

    /// 中文标点两侧的引号剥离不能把多字节字符切坏。
    #[test]
    fn multibyte_slicing_is_safe() {
        assert_eq!(tidy("“中文引号”"), "中文引号");
        assert_eq!(tidy("「」"), "");
    }

    /// **短句上的术语纠错是可靠的**——这是长文那条失败时的对照组。
    ///
    /// 2026-09-03 直接调边车验证：「然后把内容存到 ro 的目录里面」和
    /// 「都话先有一个ro的目录，然后就存储下来」两条都正确还原成 raw。
    /// 所以问题出在**长度**，不是术语表本身。
    ///
    /// 真实的 `road`（道路）和 `ID`（身份标识）**不能**被术语表改成
    /// `raw` / `idea`。
    ///
    /// codex 评审指出：默认表里恰好有 `road → raw`、`ID → idea` 两条，
    /// 写成无条件替换就会误伤。改成「由上下文决定」之后需要真的验一次。
    ///
    /// **要真调边车，所以标 ignore**：`cargo test -- --ignored`，
    /// 前提是 scripts/serve-llm.sh 在跑。
    #[test]
    #[ignore = "需要 LLM 边车在跑：scripts/serve-llm.sh"]
    fn real_road_and_id_survive() {
        let terms = crate::terms::Terms::default();
        let c = Corrector::with_terms(DEFAULT_URL, &terms);

        let cases = [
            ("这条 road 很宽，可以走两辆车", "road"),
            ("他的 ID 是 12345，不要弄错", "ID"),
            ("今天中午吃的肉很好吃", "肉"),
            ("帮我写个日报交上去", "日报"),
        ];
        for (input, must_keep) in cases {
            let out = c.correct(input).unwrap_or_else(|| input.to_string());
            assert!(
                out.contains(must_keep),
                "术语表把真实的 {must_keep} 改掉了：{input:?} → {out:?}"
            );
        }
    }

    /// **长文回归：把 `benchmarks-m2.md` §8.1 那次真实失败钉死。**
    ///
    /// 那次的症状：`ro的目录` 在孤立句里纠对（raw），放进 700 字的真实录音
    /// 转写里就变成了 `repo`——而「先有一个 repo 的目录」读起来毫无破绽，
    /// 不对着原音频根本发现不了。
    ///
    /// ## 为什么用转写文本当输入，不重跑 ASR
    ///
    /// 测的是**纠错层**。重跑 ASR 会把 SenseVoice 的随机性混进来：
    /// 哪天 ASR 的输出变了一个字，这条测试就会以一种和纠错无关的方式失败，
    /// 而排查的人得先花时间才能发现「不是纠错坏了」。
    /// fixture 是一次真实转写的快照，内容固定。
    ///
    /// **要真调边车，所以标 ignore**：`cargo test --release -- --ignored`。
    /// ⚠️ **这条目前稳定失败，是已知缺陷不是环境问题。**
    ///
    /// 2026-09-03 实测：同一份 fixture 连跑 5 次，**0 次通过**。
    /// 而同样的术语表在**短句**上稳定有效（`短句纠错是可靠的` 那条测试，
    /// 以及直接调边车验证：「然后把内容存到 ro 的目录里面」→ raw，两次都对）。
    ///
    /// 也就是说：**术语表解决了短句，没解决长文。** T2.1.2 当时报「已修复」
    /// 是基于**单次通过**——那次是运气。700 字的输入里，模型倾向于
    /// 原样输出（只调整空格），术语表没能压过它。
    ///
    /// 正解是**先分句再逐句纠错**，那是独立的一块工作（tasks.md 的 T2.1.4）。
    /// 在那之前保留这条测试并让它失败，比删掉或放宽阈值诚实——
    /// 一个假装通过的回归测试比没有更糟。
    #[test]
    #[ignore = "已知失败：长文纠错不可靠，见 T2.1.4。跑法 cargo test --release -- --ignored"]
    fn longform_regression_ro_becomes_raw_not_repo() {
        let input = include_str!("../tests/fixtures/sample02-asr-raw.txt");
        // fixture 得真的含有那个触发条件，否则这条测试是空转
        assert!(input.contains("ro的目录"), "fixture 里应该有触发这次回归的原始错误");

        let terms = crate::terms::Terms::default();
        let out = Corrector::with_terms(DEFAULT_URL, &terms)
            .correct(input)
            .expect("边车在跑时应该返回结果");

        // —— 主靶：这次回归的核心 ——
        assert!(
            out.contains("raw 的目录") || out.contains("raw的目录"),
            "`ro的目录` 应该还原成 raw：\n{out}"
        );
        // ⚠️ 判据不能写成 `!out.contains("repo")`——**`report` 里就含 `repo`**，
        // 而这段录音里恰好有「给你写个 report」，会一直误报。
        // 要匹配的是「repo 后面跟着的/目录」这个具体形态。
        assert!(
            !out.contains("repo 的") && !out.contains("repo的"),
            "**这正是 §8.1 那次失败**：长上下文里被纠成了 repo：\n{out}"
        );

        // —— 其余已知正确的纠正不许退化 ——
        // 每一条都是真实录音里实际出现过的错误形式，不是编的
        for (want, why) in [
            ("MacBook", "`我的妈 book`"),
            ("knowledge base", "`notice base`"),
            ("24 小时", "`24R`"),
            ("Mac mini", "`mac mini` 大小写"),
            ("WiFi", "`wifi` 大小写"),
        ] {
            assert!(out.contains(want), "{why} 应该纠成 {want}，退化了：\n{out}");
        }

        // —— 不该被动的东西 ——
        // 长度别差太多：纠错只该替换术语，不该润色或删减。
        // 给 20% 余量（大小写规范化和空格会让长度略变）。
        let (a, b) = (input.chars().count(), out.chars().count());
        assert!(
            b as f64 > a as f64 * 0.8 && (b as f64) < a as f64 * 1.2,
            "长度变化过大（{a} → {b}），模型可能在润色或删减而不只是替换术语"
        );
    }
}
