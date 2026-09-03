//! 知识库投递：`routes/` → `kb/**/*.md`（ADR-0003 §3.3、§4.1、§7）。
//!
//! ## 这一层做什么
//!
//! `routes/` 是**机器读**的权威记录（L0 事实层）；`kb/` 是**人读**的文档层（L1）。
//! 本模块只负责把前者渲染成后者。分层的分界线是**「能不能从语音重算」**：
//! L1 完全可以从 L0 全量重放，所以它可以被删掉、被重建、被换成别的形态——
//! 这正是 `--replay-kb` 存在的意义。
//!
//! ## 为什么是文件而不是某个 App
//!
//! ADR-0003 §3.3：Obsidian / Logseq / foam / silverbullet / OpenKnowledge
//! **全都读纯 Markdown 目录**。选文件 = 同时兼容所有这些，且不被任何一个锁定，
//! 也不接触它们的许可（ADR-0006 §3）。
//!
//! ## 幂等是怎么做到的
//!
//! 文件名里带**人读的 slug**，而 slug 来自正文——正文一改（重新转写、
//! 开了术语纠错再跑一遍），文件名就跟着变。所以「路径是纯函数」这条不成立，
//! 幂等要靠三件事一起：
//!
//! 1. **文件名带 `content_hash` 前缀** —— 不同的记录永远不会撞到同一个文件名，
//!    所以 `rename` 不会静默覆盖别人的文档；
//! 2. **front matter 里的 `id` 是完整 hash** —— 投递前扫当天目录，
//!    把 `id` 相同但文件名不同的旧文件删掉。**不能用短前缀当身份**：
//!    12 位十六进制只有 48 bit，拿它做删除判据等于允许「撞前缀就删对方」；
//! 3. **`previous_location`** —— 同一条记录若在**另一天**被重投（`Route::new`
//!    取的是当时的时间），旧文档在旧日期目录里，扫当天目录看不见它。
//!    上一次的落点记在 `routes/` 里，直接按它删。
//!
//! 整段「扫描 → 写 → 删旧」用 `kb/.lock` 做跨进程互斥。没有它，守护进程和
//! `--replay-kb` 并发投同一条时，两边会各写一份再互删对方，最后一份不剩。

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crate::label::{Label, Source};
use crate::route::Route;

/// 文件名里带多少位哈希。64 bit 前缀，足以让「同一秒 + 同标签 + 同 slug」
/// 的两条不同记录也不会撞名。**身份判定用的是完整 hash，不是这一段。**
const HASH_IN_NAME: usize = 16;

/// 投递给知识库的一条内容。**两个适配器（文件 / 组织档）共享同一个模型**，
/// 这是 ADR-0003 §4.1「自由切换」的前提。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KbEntry {
    /// **完整** `content_hash`。这是身份，不是显示用的短码——
    /// 去重、删除都按它判断。
    pub id: String,
    /// RFC 3339 本地时间，直接取自 `Route::created_at`。
    pub created: String,
    pub label: Label,
    /// 是否来自用户显式标记。**下游据此决定信不信这个标签**
    /// （ADR-0002 §3.3：显式标记不得被模型推断覆盖）。
    pub explicit_label: bool,
    pub tags: Vec<String>,
    pub text: String,
    /// 上一次投到哪了（相对数据根）。用于清掉**跨日重投**留下的旧文档。
    pub previous_location: Option<String>,
}

impl KbEntry {
    pub fn from_route(r: &Route) -> Self {
        Self {
            id: r.content_hash.clone(),
            created: r.created_at.clone(),
            label: r.label,
            explicit_label: r.label_source == Source::Explicit,
            tags: r.secondary.clone(),
            text: r.text.clone(),
            previous_location: r.delivery.location.clone(),
        }
    }
}

/// 知识库投递接口。`routes/` 是权威记录，本接口只负责把内容送出去
/// ——**它的任何失败都不影响已经落好的 `routes/`**。
pub trait KbSink {
    /// 投递一条，返回落点（用于日志与 `routes/` 里的 `location` 字段）。
    ///
    /// **必须幂等**：同一个 `id` 重复投递不得产生第二条。
    fn deliver(&self, entry: &KbEntry) -> Result<String>;

    /// 健康检查。文件适配器几乎不会不健康，但组织档会——
    /// 远程 server 可能不在线、笔记本可能休眠。
    fn health(&self) -> Result<()>;
}

/// 哪些标签该投进知识库。
///
/// - `unknown`：ADR-0002 §3.1 明文规定**只落 `routes/`，不投递下游**。
///   判成 unknown 的记录本身有价值，但把它写进知识库只是污染。
/// - `command`：它的下游动作是「触发对应动作」，不是「存入知识库」。
///   动作层（L3）还没有，**在那之前宁可不投**——`routes/` 里记录完整，
///   等 L3 落地用 `--replay-kb` 就能补上。
///
/// `task` 的下游本该是 L3 的「建任务」，但 L3 不存在时把它排除会让任务
/// 在下游彻底消失，所以**暂时仍投进 `kb/`**（ADR-0003 §7，过渡安排）。
pub fn should_deliver(label: Label) -> bool {
    !matches!(label, Label::Unknown | Label::Command)
}

/// 个人档适配器：写 Markdown 文件树（ADR-0003 §3.3）。
pub struct FileSink {
    /// 数据根目录。用于把 `source` / `transcript` 写成**相对路径**——
    /// 绝对路径会在换机器、改 `AGENTEAR_DATA` 后全部失效。
    data_root: PathBuf,
    kb_root: PathBuf,
}

/// 持有 `kb/.lock` 的独占 flock，drop 时随 fd 关闭自动释放。
///
/// 这个 `File` 从不被读写——它存在的唯一目的就是让 fd 活着，
/// 因为 flock 是绑在 fd 上的，fd 一关锁就没了。
pub struct Guard(#[allow(dead_code)] File);

impl FileSink {
    pub fn new(data_root: impl AsRef<Path>, kb_root: impl AsRef<Path>) -> Self {
        Self {
            data_root: data_root.as_ref().to_path_buf(),
            kb_root: kb_root.as_ref().to_path_buf(),
        }
    }

    /// 跨进程互斥。守护进程和 `--replay-kb` 是两个进程，共享同一个 `kb/`，
    /// 而投递不是单次系统调用——中间那段「扫描 → 写 → 删旧」必须串行，
    /// 否则两边会各写一份再互相把对方删掉。
    ///
    /// 用 `flock` 而不是自己造锁文件：进程被 kill 时内核自动释放，
    /// 不会留下一把没人解得开的锁。
    fn lock(&self) -> Result<Guard> {
        lock_kb(&self.kb_root)
    }
}

/// 取知识库的独占锁。`FileSink` 的投递和 L2 的 `--reindex` 都要经过它——
/// **两边必须是同一把**，否则重建会把并发投递的增量更新覆盖掉。
pub fn lock_kb(kb_root: &Path) -> Result<Guard> {
    {
        fs::create_dir_all(kb_root)
            .with_context(|| format!("建 {} 失败", kb_root.display()))?;
        let p = kb_root.join(".lock");
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&p)
            .with_context(|| format!("打开锁文件失败: {}", p.display()))?;
        // SAFETY: fd 来自上面刚打开的 File，在 Guard 存活期间有效
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error()).context("获取知识库锁失败");
        }
        Ok(Guard(f))
    }
}

impl FileSink {

    /// 一条记录该落在哪个目录。
    ///
    /// `journal` 走 `kb/private/` **独立子树**（ADR-0003 §7.1）：
    /// ADR-0002 §3.1 要求 journal 进「私有区」，而在文件适配器上，
    /// 「私有」的可执行含义就是**能被单独排除在 git / 分享之外**。
    /// 靠 front matter 标记做不到这一点——`git add kb/` 会把它一起带走。
    fn dir_for(&self, entry: &KbEntry) -> PathBuf {
        let (y, m, d, _) = date_parts(&entry.created);
        let mut p = self.kb_root.clone();
        if entry.label == Label::Journal {
            p.push("private");
        }
        p.push(y);
        p.push(m);
        p.push(d);
        p
    }

    /// `103022-idea-给录音笔加-wifi-deadbeef00112233.md`。
    ///
    /// 末尾那段哈希不是给人看的，是**防撞名**：没有它，同一秒里两条不同的
    /// 记录只要标签相同、正文前 32 个字符也相同，就会 rename 到同一个路径，
    /// 后一条静默覆盖前一条。
    fn file_name(&self, entry: &KbEntry) -> String {
        let (_, _, _, hms) = date_parts(&entry.created);
        let short: String = entry.id.chars().take(HASH_IN_NAME).collect();
        format!("{hms}-{}-{}-{short}.md", entry.label.as_str(), slug(&entry.text))
    }

    /// 渲染整篇 Markdown。front matter 字段照 ADR-0003 §3.3。
    ///
    /// **每一个动态值都要么是枚举、要么经过校验、要么加引号转义。**
    /// `created` 与 `tags` 都可能是从磁盘读进来的脏数据，一个裸的换行
    /// 就能在 front matter 里凭空多出一个字段。
    fn render(&self, entry: &KbEntry) -> String {
        let mut s = String::from("---\n");
        s.push_str(&format!("id: {}\n", entry.id));
        s.push_str(&format!("created: {}\n", yaml_timestamp(&entry.created)));
        s.push_str(&format!("label: {}\n", entry.label.as_str()));
        s.push_str(&format!("tags: {}\n", yaml_list(&entry.tags)));
        s.push_str(&format!("source: raw/audio/{}.wav\n", entry.id));
        s.push_str(&format!("transcript: derived/transcripts/{}.txt\n", entry.id));
        // 纠错前的原文**只在真的存在时才写**。指向一个不存在的文件比不写更糟：
        // 排障的人会以为文件被误删了。它只在纠错真的改动了文本时才生成
        // （见 `store::write_raw_transcript`）。
        let raw = self
            .data_root
            .join("derived/transcripts")
            .join(format!("{}.raw.txt", entry.id));
        if raw.exists() {
            s.push_str(&format!(
                "transcript_raw: derived/transcripts/{}.raw.txt\n",
                entry.id
            ));
        }
        s.push_str(&format!("explicit_label: {}\n", entry.explicit_label));
        s.push_str("---\n\n");
        s.push_str(entry.text.trim_end());
        s.push('\n');
        s
    }

    /// 扫一遍目标目录，同时得到两样东西：**同 id 的旧文档**（待删）
    /// 和**已被别的 id 占用的文件名**（避让）。一次 `read_dir` 拿全。
    fn survey(&self, dir: &Path, id: &str) -> (Vec<PathBuf>, HashSet<OsString>) {
        let mut mine = Vec::new();
        let mut taken = HashSet::new();
        let Ok(rd) = fs::read_dir(dir) else {
            return (mine, taken);
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            match fs::read_to_string(&p).ok().and_then(|s| front_matter_id(&s)) {
                Some(found) if found == id => mine.push(p),
                _ => {
                    taken.insert(e.file_name());
                }
            }
        }
        (mine, taken)
    }
}

impl KbSink for FileSink {
    fn deliver(&self, entry: &KbEntry) -> Result<String> {
        anyhow::ensure!(
            crate::route::is_content_hash(&entry.id),
            "id 形状不对，拒绝投递: {:?}",
            entry.id
        );
        let dir = self.dir_for(entry);
        fs::create_dir_all(&dir).with_context(|| format!("建 {} 失败", dir.display()))?;
        let _guard = self.lock()?;

        let (mine, taken) = self.survey(&dir, &entry.id);

        // 撞名了就往后排。**这不该发生**（文件名带 64 bit 哈希前缀），
        // 但「不该发生」和「不会发生」之间的差距正好是一次静默覆盖。
        let base = self.file_name(entry);
        let mut name = base.clone();
        let mut n = 2;
        while taken.contains(&OsString::from(&name)) {
            name = format!("{}-{n}.md", base.trim_end_matches(".md"));
            n += 1;
        }
        let path = dir.join(&name);

        // 临时文件名带 pid + 纳秒：两个进程同时投同一条时，共用一个
        // `.<id>.tmp` 会互相截断。锁已经挡住了大部分，但临时文件名
        // 不该依赖锁的正确性。
        let tmp = dir.join(format!(
            ".{}.{}.{}.tmp",
            &entry.id[..entry.id.len().min(8)],
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        // 临时文件 + rename：崩在写一半不会留下半截文档，
        // 让 Obsidian 那边读到一个残缺的 front matter。
        //
        // **刻意不 fsync**：`kb/` 是 L1 派生层，掉电丢了可以用 `--replay-kb`
        // 从 `routes/` 重建。这和 `derived/transcripts/` 的取舍一致
        // （见 `store::write_transcript_named`）——只有 `raw/audio/`
        // 那份不可重建的才值得走完整提交协议。
        fs::write(&tmp, self.render(entry)).with_context(|| format!("写 {} 失败", tmp.display()))?;
        if let Err(e) = fs::rename(&tmp, &path) {
            let _ = fs::remove_file(&tmp);
            return Err(e).context("rename 知识库文档失败");
        }

        // 同 id 的旧文档：正文改了导致 slug 变、文件名变，留着就是重复。
        for p in mine {
            if p != path {
                if let Err(e) = fs::remove_file(&p) {
                    log::warn!("删除同 id 的旧文档失败 {}: {e}", p.display());
                } else {
                    log::debug!("同 id 的旧文档已替换: {}", p.display());
                }
            }
        }
        // 跨日重投：旧文档在**旧日期目录**里，上面那一轮扫不到它。
        self.drop_previous(entry, &path);

        Ok(rel_to(&self.data_root, &path))
    }

    fn health(&self) -> Result<()> {
        fs::create_dir_all(&self.kb_root)
            .with_context(|| format!("知识库目录不可写: {}", self.kb_root.display()))
    }
}

impl FileSink {
    /// 删掉上一次投递的落点（如果它不是这次的落点）。
    ///
    /// **删之前必须确认那篇文档的 `id` 确实是我们的**：`location` 是从
    /// `routes/` 读进来的字符串，指到别处去了不该由它说了算。
    fn drop_previous(&self, entry: &KbEntry, current: &Path) {
        let Some(prev) = entry.previous_location.as_deref() else {
            return;
        };
        let p = self.data_root.join(prev);
        if p == current || !p.is_file() {
            return;
        }
        // 不允许 location 把我们带出数据根
        if !p.starts_with(&self.data_root) {
            log::warn!("上一次的落点指向数据根之外，忽略: {prev}");
            return;
        }
        match fs::read_to_string(&p).ok().and_then(|s| front_matter_id(&s)) {
            Some(found) if found == entry.id => {
                if let Err(e) = fs::remove_file(&p) {
                    log::warn!("删除上一次的落点失败 {}: {e}", p.display());
                } else {
                    log::debug!("跨日重投，旧落点已清理: {}", p.display());
                }
            }
            _ => log::debug!("上一次的落点已不属于本记录，不动: {}", p.display()),
        }
    }
}

/// 相对数据根的路径；不在根下就退回绝对路径。
fn rel_to(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .to_string()
}

/// 从 `2026-09-03T10:30:22+0800` 拆出 `("2026","09","03","103022")`。
///
/// **不引日期库**（同 `route::now_rfc3339` 的理由），按位置切。
/// 格式不对时退回 `unknown/00/00` 而不是 panic——投递失败可以重来，
/// 但因为一个坏时间戳把守护进程带崩不行。
fn date_parts(created: &str) -> (String, String, String, String) {
    if !looks_like_timestamp(created) {
        log::warn!("时间戳格式无法识别，归档到 unknown/: {created:?}");
        return ("unknown".into(), "00".into(), "00".into(), "000000".into());
    }
    let b: Vec<char> = created.chars().collect();
    let at = |r: std::ops::Range<usize>| -> String { b[r].iter().collect() };
    (
        at(0..4),
        at(5..7),
        at(8..10),
        format!("{}{}{}", at(11..13), at(14..16), at(17..19)),
    )
}

/// `YYYY-MM-DDTHH:MM:SS` 加一个合法时区后缀。
///
/// **必须校验到字符串末尾。** 只看前 19 个字符的话，
/// `"2026-09-03T10:30:22\nadmin: true"` 会被判成合法时间戳，
/// 然后原样写进 front matter——那就是一次 YAML 注入。
fn looks_like_timestamp(s: &str) -> bool {
    let b: Vec<char> = s.chars().collect();
    let head_ok = b.len() >= 19
        && b[4] == '-'
        && b[7] == '-'
        && b[10] == 'T'
        && b[13] == ':'
        && b[16] == ':'
        && b[..19]
            .iter()
            .enumerate()
            .all(|(i, c)| matches!(i, 4 | 7 | 10 | 13 | 16) || c.is_ascii_digit());
    if !head_ok {
        return false;
    }
    // 尾巴只允许：空、`Z`、`+0800`、`+08:00`
    let tail = &b[19..];
    match tail.len() {
        0 => true,
        1 => tail[0] == 'Z',
        5 => matches!(tail[0], '+' | '-') && tail[1..].iter().all(|c| c.is_ascii_digit()),
        6 => {
            matches!(tail[0], '+' | '-')
                && tail[3] == ':'
                && tail[1..].iter().enumerate().all(|(i, c)| i == 2 || c.is_ascii_digit())
        }
        _ => false,
    }
}

/// front matter 里的 `created`。
///
/// 时间戳来自磁盘上的 JSON，可能被编辑过。**裸写进 YAML 是注入**——
/// 一个换行就能凭空多出一个字段。所以：形状对的规范化后裸写
/// （ISO 日期不加引号才能被 dataview 之类当日期解析），形状不对的
/// 一律加引号转义，让它老老实实当一个字符串。
fn yaml_timestamp(created: &str) -> String {
    if looks_like_timestamp(created) {
        normalize_offset(created)
    } else {
        yaml_quote(created)
    }
}

/// `+0800` → `+08:00`。`date +%z` 给的是不带冒号的形式，而 RFC 3339
/// 与 ADR-0003 §3.3 的示例都要带冒号——Obsidian 的 dataview 之类
/// 按 ISO 解析日期，差这个冒号就当成普通字符串了。
fn normalize_offset(created: &str) -> String {
    let b: Vec<char> = created.chars().collect();
    if b.len() >= 5 {
        let tail = &b[b.len() - 5..];
        if (tail[0] == '+' || tail[0] == '-') && tail[1..].iter().all(|c| c.is_ascii_digit()) {
            let head: String = b[..b.len() - 5].iter().collect();
            return format!(
                "{head}{}{}{}:{}{}",
                tail[0], tail[1], tail[2], tail[3], tail[4]
            );
        }
    }
    created.to_string()
}

/// YAML 双引号标量。**控制字符必须转义**，尤其是换行——
/// 二级标签是自由词表（ADR-0002 §3.2），里面出现什么都不奇怪，
/// 而一个裸换行就能把后面的内容变成新的 front matter 字段。
fn yaml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// YAML 行内序列。空表写 `[]`。
fn yaml_list(items: &[String]) -> String {
    if items.is_empty() {
        return "[]".into();
    }
    let inner: Vec<String> = items.iter().map(|s| yaml_quote(s)).collect();
    format!("[{}]", inner.join(", "))
}

/// 文件名里那段人读的短标题。
///
/// 保留中日韩文字与字母数字，其余一律折成连字符。**按字符截断而不是字节**，
/// 否则会把一个汉字切成半截无效 UTF-8。
fn slug(text: &str) -> String {
    const MAX_CHARS: usize = 32;
    let mut out = String::new();
    let mut dash = false;
    for c in text.chars() {
        if out.chars().count() >= MAX_CHARS {
            break;
        }
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let s = out.trim_matches('-').to_string();
    if s.is_empty() {
        "untitled".into()
    } else {
        s
    }
}

/// 一篇 `kb/` 文档解析回来的样子。**L2 索引从这里重建**
/// （ADR-0003 §7：L2 必须能从 L1 全量重建）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDoc {
    pub id: String,
    pub created: String,
    pub label: String,
    pub tags: Vec<String>,
    pub explicit_label: bool,
    pub body: String,
}

/// 解析一篇我们自己写的文档。**不做通用 YAML 解析**——
/// 格式是 `render` 写出来的，字段固定、值都经过转义，
/// 引一个 YAML 库来读自己刚写的东西不划算。
///
/// 认不出来返回 `None`（比如用户手工往 `kb/` 里放了别的 Markdown）——
/// 那种文件不该进索引，也不该让重建整体失败。
pub fn parse_document(doc: &str) -> Option<ParsedDoc> {
    let rest = doc.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let (fm, body) = rest.split_at(end);
    let body = body.trim_start_matches("\n---").trim_start().to_string();

    let get = |key: &str| -> Option<String> {
        fm.lines()
            .find_map(|l| l.strip_prefix(&format!("{key}:")))
            .map(|v| v.trim().to_string())
    };
    let id = get("id").filter(|s| !s.is_empty())?;
    let label = get("label").filter(|s| !s.is_empty())?;
    Some(ParsedDoc {
        id,
        created: parse_yaml_scalar(&get("created").unwrap_or_default()),
        label,
        tags: parse_yaml_list(&get("tags").unwrap_or_default()),
        explicit_label: get("explicit_label").as_deref() == Some("true"),
        body,
    })
}

/// `["a", "b\"c"]` → `["a", "b\"c"]`。`yaml_list` 的逆。
fn parse_yaml_list(s: &str) -> Vec<String> {
    let inner = s.trim().strip_prefix('[').and_then(|x| x.strip_suffix(']')).unwrap_or("");
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut it = inner.chars().peekable();
    let mut in_q = false;
    while let Some(c) = it.next() {
        match c {
            '\\' if in_q => unescape_one(&mut it, &mut cur),
            '"' => {
                if in_q { out.push(std::mem::take(&mut cur)); }
                in_q = !in_q;
            }
            _ => if in_q { cur.push(c) },
        }
    }
    out
}

/// 解一个反斜杠转义。**必须和 `yaml_quote` 一一对应**——
/// 它会把控制字符写成 `\xNN`，这里不解就会把 `d\x7fe` 读成 `dx7fe`，
/// 标签被永久改掉且没人发现。两个函数改一个就得改另一个。
fn unescape_one(it: &mut std::iter::Peekable<std::str::Chars>, out: &mut String) {
    match it.next() {
        Some('n') => out.push('\n'),
        Some('r') => out.push('\r'),
        Some('t') => out.push('\t'),
        Some('x') => {
            // 恰好两位十六进制。凑不齐就原样保留——宁可留下可见的怪字符，
            // 也不要吞掉内容。
            let h: String = it.clone().take(2).collect();
            match u8::from_str_radix(&h, 16) {
                Ok(b) if h.len() == 2 => {
                    it.next();
                    it.next();
                    out.push(b as char);
                }
                _ => out.push_str("\\x"),
            }
        }
        Some(o) => out.push(o),
        None => out.push('\\'),
    }
}

/// 解一个 YAML 双引号标量：`"a\nb"` → `a<换行>b`。不带引号的原样返回。
///
/// `created` 在时间戳形状不对时会被 `yaml_quote` 加引号存起来
/// （那是为了挡住 front matter 注入）。读回来不解，就会拿到带字面
/// 反斜杠和引号的字符串。
fn parse_yaml_scalar(s: &str) -> String {
    let inner = match s.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
        Some(i) => i,
        None => return s.to_string(),
    };
    let mut out = String::with_capacity(inner.len());
    let mut it = inner.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' { unescape_one(&mut it, &mut out); } else { out.push(c); }
    }
    out
}

/// 读一篇已有文档 front matter 里的 `id`。用于去重，不做完整 YAML 解析——
/// 这些文件是我们自己写的，格式已知。
fn front_matter_id(doc: &str) -> Option<String> {
    let rest = doc.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    rest[..end]
        .lines()
        .find_map(|l| l.strip_prefix("id:"))
        .map(|v| v.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "agentear-kb-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn route(hash: &str, label: Label, text: &str) -> Route {
        let mut r = Route::new(hash, label, Source::Model, text);
        r.created_at = "2026-09-03T10:30:22+0800".into();
        r
    }

    #[test]
    fn document_lands_in_date_directory() {
        let d = tmpdir();
        let sink = FileSink::new(&d, d.join("kb"));
        let e = KbEntry::from_route(&route("abc123def4567890", Label::Idea, "给录音笔加 WiFi"));
        let loc = sink.deliver(&e).unwrap();

        assert!(loc.starts_with("kb/2026/09/03/103022-idea-"), "落点不对: {loc}");
        assert!(d.join(&loc).exists());
        fs::remove_dir_all(&d).ok();
    }

    /// `journal` 必须落在**独立子树**里，这样才能单独排除在 git / 分享之外。
    /// 靠 front matter 标记做不到——`git add kb/` 会把它一起带走。
    #[test]
    fn journal_goes_to_a_separate_private_subtree() {
        let d = tmpdir();
        let sink = FileSink::new(&d, d.join("kb"));
        let e = KbEntry::from_route(&route("0a0000000000", Label::Journal, "今天有点累"));
        let loc = sink.deliver(&e).unwrap();
        assert!(loc.starts_with("kb/private/2026/09/03/"), "journal 未进私有区: {loc}");
        fs::remove_dir_all(&d).ok();
    }

    /// **正文改了也不能产生第二篇。** 重新转写、开了纠错再跑一遍都会改正文，
    /// 而正文决定文件名——这正是最容易堆出重复的场景。
    #[test]
    fn redelivering_the_same_id_replaces_rather_than_duplicates() {
        let d = tmpdir();
        let sink = FileSink::new(&d, d.join("kb"));
        let mut r = route("d00000000000", Label::Note, "第一版文字");
        let loc1 = sink.deliver(&KbEntry::from_route(&r)).unwrap();

        r.text = "纠错之后完全不同的文字".into();
        let loc2 = sink.deliver(&KbEntry::from_route(&r)).unwrap();

        assert_ne!(loc1, loc2, "正文变了文件名就该跟着变");
        assert!(!d.join(&loc1).exists(), "旧文件必须被删掉");
        let dir = d.join("kb/2026/09/03");
        let n = fs::read_dir(&dir).unwrap().filter(|e| {
            e.as_ref().is_ok_and(|e| e.path().extension().is_some_and(|x| x == "md"))
        }).count();
        assert_eq!(n, 1, "同一条记录只该有一篇文档");
        fs::remove_dir_all(&d).ok();
    }

    /// **跨日重投**：同一段音频隔天被重新路由时，`Route::new` 会给一个新的
    /// 时间戳，旧文档留在旧日期目录里——扫当天目录看不见它。
    /// 靠 `routes/` 里记着的上一次落点来清。
    #[test]
    fn a_redelivery_on_another_day_cleans_up_yesterdays_document() {
        let d = tmpdir();
        let sink = FileSink::new(&d, d.join("kb"));
        let mut r = route("beef00000000", Label::Note, "同一段音频");
        let yesterday = sink.deliver(&KbEntry::from_route(&r)).unwrap();

        // 隔天重跑：新时间戳 + 上一次的落点
        r.created_at = "2026-09-04T09:00:00+0800".into();
        r.delivery.location = Some(yesterday.clone());
        let today = sink.deliver(&KbEntry::from_route(&r)).unwrap();

        assert!(!d.join(&yesterday).exists(), "昨天那篇必须清掉");
        assert!(d.join(&today).exists());
        fs::remove_dir_all(&d).ok();
    }

    /// 上一次的落点是从 JSON 读来的字符串。它指到别人的文档、或者指到
    /// 数据根外面时，**都不该由它说了算**。
    #[test]
    fn a_previous_location_cannot_delete_someone_elses_document() {
        let d = tmpdir();
        let sink = FileSink::new(&d, d.join("kb"));
        let victim = sink
            .deliver(&KbEntry::from_route(&route("aaaa11112222", Label::Note, "别人的")))
            .unwrap();

        let mut r = route("bbbb33334444", Label::Note, "我的");
        r.delivery.location = Some(victim.clone());
        sink.deliver(&KbEntry::from_route(&r)).unwrap();
        assert!(d.join(&victim).exists(), "id 不匹配就不该删");

        // 指到数据根外面
        let outside = d.join("outside.md");
        fs::write(&outside, "---\nid: bbbb33334444\n---\n").unwrap();
        r.delivery.location = Some("../outside.md".into());
        sink.deliver(&KbEntry::from_route(&r)).unwrap();
        assert!(outside.exists(), "不能顺着 ../ 走出数据根");
        fs::remove_dir_all(&d).ok();
    }

    /// **两条不同的记录不能撞到同一个文件名。** 同一秒、同标签、正文前 32 个
    /// 字符也一样——文件名里那段哈希就是为这个存在的。
    #[test]
    fn two_different_records_never_collide_on_one_file() {
        let d = tmpdir();
        let sink = FileSink::new(&d, d.join("kb"));
        let same_prefix = "完全一样的开头一模一样的开头一模一样的开头一模一样的";
        let a = sink
            .deliver(&KbEntry::from_route(&route("1111aaaa", Label::Note, same_prefix)))
            .unwrap();
        let b = sink
            .deliver(&KbEntry::from_route(&route("2222bbbb", Label::Note, same_prefix)))
            .unwrap();

        assert_ne!(a, b, "不同记录必须落在不同文件");
        assert!(d.join(&a).exists() && d.join(&b).exists(), "两篇都要在");
        fs::remove_dir_all(&d).ok();
    }

    /// 身份判定必须用**完整** hash：短前缀只有 48 bit，
    /// 拿它当删除判据等于「撞前缀就删对方」。
    #[test]
    fn identity_is_the_full_hash_not_a_prefix() {
        let d = tmpdir();
        let sink = FileSink::new(&d, d.join("kb"));
        let a = sink
            .deliver(&KbEntry::from_route(&route(
                "abcdef0123456789aaaa",
                Label::Note,
                "第一条",
            )))
            .unwrap();
        let b = sink
            .deliver(&KbEntry::from_route(&route(
                "abcdef0123456789bbbb",
                Label::Note,
                "第二条",
            )))
            .unwrap();
        assert!(d.join(&a).exists() && d.join(&b).exists(), "前缀相同不代表是同一条");

        let doc = fs::read_to_string(d.join(&a)).unwrap();
        assert!(doc.contains("id: abcdef0123456789aaaa"), "front matter 要写完整 hash");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn front_matter_carries_the_provenance_fields() {
        let d = tmpdir();
        let sink = FileSink::new(&d, d.join("kb"));
        let mut r = route("f00d000000000000", Label::Idea, "内容正文");
        r.label_source = Source::Explicit;
        r.secondary = vec!["agentear".into(), "录音笔".into()];
        let loc = sink.deliver(&KbEntry::from_route(&r)).unwrap();
        let doc = fs::read_to_string(d.join(&loc)).unwrap();

        assert!(doc.starts_with("---\n"));
        // 回溯到原始音频与转写，这是 ADR-0002 §4.3 的落地
        assert!(doc.contains("source: raw/audio/f00d000000000000.wav"));
        assert!(doc.contains("transcript: derived/transcripts/f00d000000000000.txt"));
        assert!(doc.contains("explicit_label: true"), "显式标记必须如实记录");
        assert!(doc.contains(r#"tags: ["agentear", "录音笔"]"#), "{doc}");
        // 时区带冒号，才是能被 ISO 解析器认出来的日期
        assert!(doc.contains("created: 2026-09-03T10:30:22+08:00"), "{doc}");
        assert!(doc.ends_with("内容正文\n"));
        fs::remove_dir_all(&d).ok();
    }

    /// 纠错前的原文**只在真的存在时**才写进 front matter——
    /// 指向不存在的文件比不写更糟。
    #[test]
    fn transcript_raw_is_referenced_only_when_it_exists() {
        let d = tmpdir();
        fs::create_dir_all(d.join("derived/transcripts")).unwrap();
        let sink = FileSink::new(&d, d.join("kb"));

        let e = KbEntry::from_route(&route("ffff100000000", Label::Note, "有纠错的"));
        let loc = sink.deliver(&e).unwrap();
        assert!(!fs::read_to_string(d.join(&loc)).unwrap().contains("transcript_raw"));

        fs::write(d.join("derived/transcripts/ffff100000000.raw.txt"), "原文").unwrap();
        let loc = sink.deliver(&e).unwrap();
        assert!(fs::read_to_string(d.join(&loc)).unwrap().contains("transcript_raw:"));
        fs::remove_dir_all(&d).ok();
    }

    /// `unknown` 只落 routes（ADR-0002 §3.1），`command` 等动作层做完再说。
    #[test]
    fn unknown_and_command_are_not_delivered() {
        assert!(!should_deliver(Label::Unknown));
        assert!(!should_deliver(Label::Command));
        for l in [Label::Idea, Label::Note, Label::Task, Label::Journal, Label::Question, Label::Reference] {
            assert!(should_deliver(l), "{l:?} 应该投递");
        }
    }

    /// 全是标点的一句话不能产生一个空文件名。
    #[test]
    fn slug_never_becomes_empty_or_unsafe() {
        assert_eq!(slug("。。。！！！"), "untitled");
        assert_eq!(slug(""), "untitled");
        assert!(!slug("a/b:c").contains('/'), "路径分隔符必须被折掉");
        assert!(!slug("a/b:c").contains(':'));
        assert!(!slug("...abc").starts_with('-'));
        // 按字符截断，不能切出半截 UTF-8
        let s = slug(&"汉".repeat(100));
        assert_eq!(s.chars().count(), 32);
    }

    /// front matter 里的动态值都可能是磁盘上的脏数据。
    /// **一个裸换行就能凭空多出一个字段。**
    #[test]
    fn front_matter_cannot_be_injected_through_tags_or_timestamp() {
        let d = tmpdir();
        let sink = FileSink::new(&d, d.join("kb"));
        let mut r = route("cafe00000000", Label::Note, "正文");
        r.secondary = vec!["a\nadmin: true".into(), "b\"c".into(), "d\u{7f}e".into()];
        r.created_at = "2026-09-03T10:30:22\nadmin: true".into();
        let loc = sink.deliver(&KbEntry::from_route(&r)).unwrap();
        let doc = fs::read_to_string(d.join(&loc)).unwrap();

        let fm = &doc[..doc[4..].find("\n---").unwrap() + 4];
        assert!(!fm.contains("\nadmin:"), "front matter 被注入了:\n{fm}");
        assert!(fm.lines().filter(|l| l.starts_with("created:")).count() == 1);
        assert!(doc.contains(r#"\nadmin: true"#), "换行必须被转义成字面量: {doc}");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn yaml_list_escapes_hostile_tags() {
        assert_eq!(yaml_list(&[]), "[]");
        let out = yaml_list(&[r#"a"b"#.into(), "c: d".into(), "e\nf".into()]);
        assert_eq!(out, r#"["a\"b", "c: d", "e\nf"]"#);
    }

    /// 坏时间戳不能让投递 panic——归到 `unknown/` 里，人还能找到。
    #[test]
    fn broken_timestamp_falls_back_instead_of_panicking() {
        let d = tmpdir();
        let sink = FileSink::new(&d, d.join("kb"));
        let mut r = route("bad000000000", Label::Note, "时间戳坏了");
        r.created_at = "0000-00-00T00:00:00+0000".into();
        let loc = sink.deliver(&KbEntry::from_route(&r)).unwrap();
        assert!(loc.starts_with("kb/0000/00/00/"), "{loc}");

        r.created_at = "垃圾".into();
        let loc = sink.deliver(&KbEntry::from_route(&r)).unwrap();
        assert!(loc.starts_with("kb/unknown/00/00/"), "{loc}");
        fs::remove_dir_all(&d).ok();
    }

    /// id 形状不对的一律拒投——它会变成文件名。
    #[test]
    fn a_malformed_id_is_refused() {
        let d = tmpdir();
        let sink = FileSink::new(&d, d.join("kb"));
        let mut e = KbEntry::from_route(&route("aaaa", Label::Note, "x"));
        e.id = "../../etc/passwd".into();
        assert!(sink.deliver(&e).is_err());
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn no_temp_file_is_left_behind() {
        let d = tmpdir();
        let sink = FileSink::new(&d, d.join("kb"));
        sink.deliver(&KbEntry::from_route(&route("100000000000", Label::Note, "x")))
            .unwrap();
        let leftovers: Vec<_> = fs::read_dir(d.join("kb/2026/09/03"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "留下了临时文件: {leftovers:?}");
        fs::remove_dir_all(&d).ok();
    }

    /// 并发投同一条不能出现「各写一份再互删对方」——最后必须恰好留一篇。
    #[test]
    fn concurrent_delivery_of_one_record_leaves_exactly_one_document() {
        let d = tmpdir();
        let kb = d.join("kb");
        std::thread::scope(|s| {
            for i in 0..8 {
                let (d, kb) = (d.clone(), kb.clone());
                s.spawn(move || {
                    let sink = FileSink::new(&d, &kb);
                    // 每个线程的正文不同 → 文件名不同 → 正是互删的场景
                    let r = route("cccc00001111", Label::Note, &format!("第 {i} 版正文"));
                    sink.deliver(&KbEntry::from_route(&r)).unwrap();
                });
            }
        });
        let n = fs::read_dir(kb.join("2026/09/03"))
            .unwrap()
            .filter(|e| e.as_ref().is_ok_and(|e| e.path().extension().is_some_and(|x| x == "md")))
            .count();
        assert_eq!(n, 1, "并发投同一条应恰好留一篇");
        fs::remove_dir_all(&d).ok();
    }
}
