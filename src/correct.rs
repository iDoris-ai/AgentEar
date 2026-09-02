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

    /// 一批送给模型的目标字数。
    ///
    /// ## 为什么必须分批（这条是实测逼出来的）
    ///
    /// 2026-09-03 实测：同一份术语表、同一个边车，**短句稳定纠对，
    /// 700 字长文连跑 5 次 0 次通过**——模型在长输入上倾向于原样输出，
    /// 术语表压不过它（`docs/benchmarks-m2.md` §8.1）。
    ///
    /// 120 字是个折中：太小则调用次数多、耗时线性上升；太大则回到长文
    /// 那个失效区间。短句实测在几十字量级稳定，留一倍余量。
    const BATCH_CHARS: usize = 120;

    /// 超过这个长度才分批。低于它的一次送完——分批本身有开销
    /// （每批一次 HTTP + 一次推理），短文本不值得。
    const SPLIT_THRESHOLD: usize = 200;

    /// 整段长文纠错的总时限。
    ///
    /// 单批 20 秒 × N 批，最坏能到一两分钟，而用户按完录音键正等着上屏。
    /// 45 秒是按实测（700 字 ~7 秒）留了六倍余量。
    const TOTAL_BUDGET: std::time::Duration = std::time::Duration::from_secs(45);

    /// 纠正一段转写。返回 `None` 表示**没有可用的纠错结果**，调用方应当用原文。
    ///
    /// 注意签名：错误不往外抛。纠错是尽力而为的增强，它的任何失败都不该
    /// 变成调用方需要处理的分支——调用方只有「有更好的版本」和「没有」两种情况。
    pub fn correct(&self, text: &str) -> Option<String> {
        if text.trim().is_empty() {
            return None;
        }
        if text.chars().count() <= Self::SPLIT_THRESHOLD {
            return match self.try_correct(text) {
                Ok(fixed) => Some(fixed),
                Err(e) => {
                    log::warn!("术语纠错不可用，按原文上屏: {e:#}");
                    None
                }
            };
        }

        // —— 长文：分批纠错 ——
        //
        // **单批失败不放弃整体**：那一批用原文，其余照常纠。
        // 整体放弃的话，一次网络抖动就让整段长录音都拿不到纠正，
        // 而长录音恰恰是术语最多、最需要纠正的。
        let batches = split_into_batches(text, Self::BATCH_CHARS);
        log::debug!("长文分 {} 批纠错（共 {} 字）", batches.len(), text.chars().count());

        // **整段有一个总时限。** 每批 20 秒 × N 批，最坏情况能到一两分钟——
        // 而用户按完录音键正等着上屏。超时就整体放弃，用原文。
        let deadline = std::time::Instant::now() + Self::TOTAL_BUDGET;

        let mut out = String::with_capacity(text.len());
        for (i, b) in batches.iter().enumerate() {
            if std::time::Instant::now() >= deadline {
                log::warn!("长文纠错超出总时限（{:?}），按原文上屏", Self::TOTAL_BUDGET);
                return None;
            }
            // **批边界的空白要原样保住。**
            //
            // 模型的输出必然被 trim（它爱加前后空行），而批的边界恰恰
            // 可能落在一个空格或换行上——英文在 `". "` 处切分时，
            // 下一批以空格开头，trim 掉就拼成了 `sentence.Next`。
            // 所以只把**正文**送模型，前后空白留在外面，拼接时原样恢复。
            let core = b.trim();
            if core.is_empty() {
                out.push_str(b);
                continue;
            }
            let lead = &b[..b.len() - b.trim_start().len()];
            let trail = &b[b.trim_end().len()..];

            // 失败重试一次：本地边车的失败多半是瞬时的（模型正在换页、
            // 上一次请求还没释放）。重试一次比整段放弃划算。
            let r = self
                .try_correct_batch(core)
                .or_else(|first| self.try_correct_batch(core).map_err(|_| first));
            match r {
                Ok(fixed) => {
                    out.push_str(lead);
                    out.push_str(&fixed);
                    out.push_str(trail);
                }
                Err(e) => {
                    // **一批失败就整体回退，不返回混合结果。**
                    //
                    // 部分成功看着划算,实际很危险:用户拿到的是「大部分纠正过、
                    // 中间某一段没纠」的文本,读起来完全正常,而那一段里
                    // 恰恰可能有术语错误。他没有任何线索知道该怀疑哪一段。
                    // 「要么全纠、要么不动」是可预测的,而可预测比多纠几句重要。
                    log::warn!("第 {} 批纠错失败（已重试一次），整体按原文上屏: {e:#}", i + 1);
                    return None;
                }
            }
        }
        Some(out)
    }

    /// 单批纠错。**和整体纠错走不同的输出清理**——见 `try_correct` 的说明。
    fn try_correct_batch(&self, text: &str) -> Result<String> {
        self.request(text, /* single_line_only = */ false)
    }

    fn try_correct(&self, text: &str) -> Result<String> {
        self.request(text, /* single_line_only = */ true)
    }

    /// `single_line_only`：整体纠错时只取最后一行非空（剥掉模型可能吐的
    /// 推理过程）；**分批时不能这么做**——一批里本来就可能含换行
    /// （ASR 的长转写里有），只取最后一行会把前面的内容整段吃掉。
    /// 分批的每批都短，模型吐推理的风险本来就低，`--no-thinking` 也在挡。
    fn request(&self, text: &str, single_line_only: bool) -> Result<String> {
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

        let fixed = if single_line_only {
            tidy(content)
        } else {
            tidy_keep_lines(content)
        };
        if fixed.is_empty() {
            bail!("模型返回空");
        }
        Ok(fixed)
    }
}

/// 把长文按**句子边界**切成若干批，每批约 `target` 字。
///
/// ## 为什么在句子边界切，而不是按固定字数硬切
///
/// 硬切会把一个词劈成两半，而术语纠错恰恰依赖上下文判断
/// （「road 目录」要还原、「road 很宽」不能动）。切在句中等于毁掉判据。
///
/// ## 为什么攒到 120 字才发，而不是一句一发
///
/// 一句一发对 700 字的录音意味着二三十次推理，耗时线性上升。
/// 而且太短的片段（「对。」「嗯。」）本身没有上下文，模型反而容易乱改。
/// 攒批既省调用又保住上下文。
///
/// **末批不足 target 也照发**——不要为了凑够字数把它并进上一批，
/// 那会让上一批超出稳定区间。
fn split_into_batches(text: &str, target: usize) -> Vec<String> {
    // 句子结束的标记。中英文都要：转写里两种都会出现。
    // 注意**不包含逗号**——逗号切出来的片段太碎，上下文不够。
    const ENDERS: [char; 10] = ['。', '！', '？', '\n', '!', '?', '；', ';', '…', '.'];
    // 退而求其次的切点：找不到句末标点时用它们，总比硬切在词中间强。
    const SOFT: [char; 4] = ['，', ',', '、', ' '];

    // **硬上限**。`target` 只是「到这儿就可以切了」，不是「不许超过」——
    // 一段几千字没有句号的 ASR 输出（口述时不停顿就会这样）在只看
    // `target` 的实现里仍然是**一整批**，于是回到 700 字长文那个失效区间，
    // 更长还会撑爆上下文。所以必须有一个谁都突破不了的上限。
    let hard = target * 3;

    let mut batches = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;

    let chars: Vec<char> = text.chars().collect();
    for (i, &ch) in chars.iter().enumerate() {
        cur.push(ch);
        cur_len += 1;

        // ASCII 句点要谨慎：`v1.2`、`mushroom.cv`、`0.2 秒` 里的点不是句末。
        // 只有后面跟空白或到结尾时才算。
        let is_end = if ch == '.' {
            chars.get(i + 1).is_none_or(|n| n.is_whitespace())
        } else {
            ENDERS.contains(&ch)
        };

        if (is_end && cur_len >= target) || cur_len >= hard {
            batches.push(std::mem::take(&mut cur));
            cur_len = 0;
            continue;
        }
        // 逼近硬上限时，遇到软切点就先切，避免真的硬切在词中间
        if cur_len >= hard.saturating_sub(target / 2) && SOFT.contains(&ch) {
            batches.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
    }
    if !cur.trim().is_empty() {
        batches.push(cur);
    } else if let Some(last) = batches.last_mut() {
        // 尾部只剩空白：并进上一批，别产生一个空批次
        last.push_str(&cur);
    }
    batches
}

/// 分批时用的输出清理：**保留换行**，只剥掉外围包裹。
///
/// 和 `tidy` 的区别只在「取不取最后一行」。分批的每批本来就可能含换行，
/// 取最后一行会把前面的内容整段丢掉（codex 抓到的 High 1）。
fn tidy_keep_lines(s: &str) -> String {
    let mut t = s.trim();
    for p in ["修正后：", "修正后:", "修正结果：", "输出：", "Corrected:"] {
        if let Some(rest) = t.strip_prefix(p) {
            t = rest.trim();
        }
    }
    for (a, b) in [('"', '"'), ('「', '」'), ('“', '”')] {
        if t.starts_with(a) && t.ends_with(b) && t.chars().count() >= 2 {
            t = &t[a.len_utf8()..t.len() - b.len_utf8()];
        }
    }
    t.trim().to_string()
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
    /// 分批切分必须切在句子边界，且不产生空批。
    #[test]
    fn batches_split_on_sentence_boundaries() {
        let text = "第一句话在这里。第二句话也在这里。第三句稍微长一点点内容。第四句。";
        let b = split_into_batches(text, 10);
        assert!(b.len() > 1, "应该切成多批");
        for x in &b {
            assert!(!x.trim().is_empty(), "不该有空批次");
        }
        // 每批（除末批外）都该以句末标点结尾
        for x in &b[..b.len() - 1] {
            let last = x.trim_end().chars().last().unwrap();
            assert!(
                "。！？.!?；".contains(last),
                "批次没有切在句子边界，结尾是 {last:?}：{x:?}"
            );
        }
        assert_eq!(b.concat(), text, "拼回来必须和原文一字不差");
    }

    /// **拼回来必须和原文完全一致**——一个字都不能丢。
    ///
    /// 分批是为了纠错更准，不是为了改内容。切分本身若丢字，
    /// 后面纠得再准也是错的。
    #[test]
    fn concatenating_batches_reproduces_the_input() {
        for text in [
            "没有任何标点的一长串文字就这样一直写下去也不换行",
            "短。",
            "换行\n分隔的\n内容\n也要处理",
            "混合 English sentences. 和中文句子。Together.",
            "结尾没有标点",
            "。。。连续标点。。。",
        ] {
            let b = split_into_batches(text, 5);
            assert_eq!(b.concat(), text, "分批丢字了：{text:?}");
        }
    }

    /// **没有句末标点的长文本必须被硬上限切开。**
    ///
    /// 这条测试原来断言的是相反的事（「整段一批」），codex 指出那
    /// **固化了风险**：口述时不停顿，ASR 就会吐出几千字没有句号的文本，
    /// 而整段一批正好回到 700 字长文那个「模型原样输出」的失效区间，
    /// 更长还会撑爆上下文。
    #[test]
    fn text_without_enders_is_still_capped() {
        let text = "这段话完全没有句号也没有问号就这么一直说下去".repeat(20);
        let target = 20;
        let b = split_into_batches(&text, target);
        assert!(b.len() > 1, "无标点长文本必须被硬上限切开，否则回到失效区间");
        for x in &b {
            assert!(
                x.chars().count() <= target * 3,
                "有批次突破了硬上限：{} 字",
                x.chars().count()
            );
        }
        assert_eq!(b.concat(), text, "硬切也不能丢字");
    }

    /// ASCII 句点在版本号、域名里不是句末，不能在那儿切。
    #[test]
    fn ascii_dot_inside_versions_and_domains_is_not_a_boundary() {
        // 点号后面不是空白 → 不算句末，所以整段不会在 v1.2 或 .cv 处被切
        let text = format!("{}升级到 v1.2 以后那篇博客搬到了 blog.mushroom.cv 上面去了", "填充".repeat(60));
        let b = split_into_batches(&text, 100);
        for x in &b {
            assert!(!x.ends_with("v1."), "切在版本号中间了：{x:?}");
            assert!(!x.ends_with("blog."), "切在域名中间了：{x:?}");
            assert!(!x.ends_with("mushroom."), "切在域名中间了：{x:?}");
        }
        assert_eq!(b.concat(), text);
    }

    /// 短文本不走分批路径（省掉多次调用的开销）。
    #[test]
    fn short_text_is_not_batched() {
        let short = "把内容存到 ro 的目录";
        assert!(short.chars().count() <= Corrector::SPLIT_THRESHOLD);
    }

    /// 长文回归。**T2.1.4 分批之后已经稳定通过**（3/3），
    /// 在那之前是 0/5——模型在 700 字长输入上倾向原样输出，
    /// 术语表压不过它。历史见 `docs/benchmarks-m2.md` §8.1。
    #[test]
    #[ignore = "需要 LLM 边车在跑：scripts/serve-llm.sh"]
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
        // ⚠️ **这里刻意不断言 MacBook。**
        // fixture 里有两处：`我的妈 book`（实测分批后不纠，已知局限）
        // 和 `macbook`（纠对）。断言「MacBook 出现」会被后者满足，
        // 于是这条断言看着在测前者、其实什么都没测到。
        // 宁可不断言，也不要一条自我满足的断言——见 benchmarks-m2.md §8.1。
        for (want, why) in [
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
