//! L2 全文索引：`kb/**/*.md` → `derived/index.sqlite`（ADR-0003 §7）。
//!
//! ## 它在分层里的位置
//!
//! L2 的硬约束是**必须能从 L1 全量重建**。所以这个文件里没有任何
//! 别处没有的信息——删掉 `index.sqlite` 跑一次 `--reindex` 就回来了。
//! 这条约束不是洁癖：它意味着索引可以随便换实现、换库、换 schema，
//! 不需要迁移，也不怕写坏。
//!
//! ## 为什么中文要逐字切开
//!
//! FTS5 自带的分词器**都处理不好中文**（实测，见 `docs/agent/tasks.md` T2.4.4）：
//!
//! | 查询 | unicode61 | trigram | unigram（本模块） |
//! |---|---|---|---|
//! | 「录音」（2 字） | ✗ | ✗ | ✅ |
//! | 「录音 设备」跨词边界 | ✗ | ✗ | ✅ |
//! | 英文前缀 `know*` | ✅ | ✗ | ✅ |
//! | 索引体积 | 1.0× | 3.5× | 1.55× |
//!
//! `unicode61` 把一整句中文当成**一个 token**——搜「录音笔」一条都搜不到。
//! `trigram` 要求查询至少 3 个字符，「录音」这种两字词搜不了，索引还大 3.5 倍。
//!
//! 所以写入前把 CJK **逐字用空格切开**（`segment`），查询时走同样的切分再
//! 拼成短语查询。代价是索引大约 1.55 倍，换来的是中文按子串搜得到。
//!
//! ## 不上向量库
//!
//! jason 2026-09-03 定的。语音笔记是短文本、量级几千到几万条，
//! 关键词检索够用；向量库要嵌入模型、要常驻内存、要处理维度迁移，
//! 和「装上就能用」冲突。

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// 一条命中。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub id: String,
    /// 相对数据根的路径，例如 `kb/2026/09/03/…md`。
    pub path: String,
    pub created: String,
    pub label: String,
    /// 带高亮标记的片段，`snippet()` 生成。
    pub snippet: String,
}

pub struct Index {
    db: Connection,
}

impl Index {
    /// 打开（必要时创建）索引。
    ///
    /// 放在 `derived/` 下是有意的：那一层的语义就是「可以从上一层重算」，
    /// 备份时可以整个跳过，磁盘紧张时可以直接删。
    pub fn open(data_root: &Path) -> Result<Self> {
        let dir = data_root.join("derived");
        std::fs::create_dir_all(&dir).with_context(|| format!("建 {} 失败", dir.display()))?;
        let db = Connection::open(dir.join("index.sqlite")).context("打开索引数据库失败")?;
        let ix = Self { db };
        ix.migrate()?;
        Ok(ix)
    }

    #[cfg(test)]
    fn in_memory() -> Result<Self> {
        let ix = Self { db: Connection::open_in_memory()? };
        ix.migrate()?;
        Ok(ix)
    }

    /// 建表。**`if not exists` + 幂等**，每次启动都跑一遍。
    ///
    /// FTS 表用 `content='docs'`（external content）：正文只存一份在 `docs`
    /// 里，FTS 只存倒排索引。触发器负责同步——手写同步代码迟早会漏掉
    /// 某条路径，而触发器是 SQLite 自己保证的。
    fn migrate(&self) -> Result<()> {
        self.db
            .execute_batch(
                "
            pragma journal_mode = wal;
            create table if not exists docs (
                id       text primary key,
                path     text not null,
                created  text not null,
                label    text not null,
                explicit integer not null,
                tags     text not null,
                body     text not null,
                seg      text not null   -- CJK 逐字切开后的正文，只给 FTS 用
            );
            create virtual table if not exists docs_fts using fts5(
                seg, tags,
                content='docs', content_rowid='rowid',
                tokenize='unicode61'
            );
            create trigger if not exists docs_ai after insert on docs begin
                insert into docs_fts(rowid, seg, tags) values (new.rowid, new.seg, new.tags);
            end;
            create trigger if not exists docs_ad after delete on docs begin
                insert into docs_fts(docs_fts, rowid, seg, tags) values('delete', old.rowid, old.seg, old.tags);
            end;
            create trigger if not exists docs_au after update on docs begin
                insert into docs_fts(docs_fts, rowid, seg, tags) values('delete', old.rowid, old.seg, old.tags);
                insert into docs_fts(rowid, seg, tags) values (new.rowid, new.seg, new.tags);
            end;
            ",
            )
            .context("初始化索引 schema 失败")
    }

    /// 写入一条。按 `id` 幂等——同一条重复索引不会产生第二行。
    pub fn upsert(&self, doc: &crate::kb::ParsedDoc, path: &str) -> Result<()> {
        let tags = doc.tags.join(" ");
        self.db
            .execute(
                "insert into docs(id, path, created, label, explicit, tags, body, seg)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 on conflict(id) do update set
                   path=excluded.path, created=excluded.created, label=excluded.label,
                   explicit=excluded.explicit, tags=excluded.tags,
                   body=excluded.body, seg=excluded.seg",
                rusqlite::params![
                    doc.id,
                    path,
                    doc.created,
                    doc.label,
                    doc.explicit_label as i32,
                    tags,
                    doc.body,
                    segment(&doc.body),
                ],
            )
            .context("写索引失败")?;
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        self.db.execute("delete from docs where id = ?1", [id])?;
        Ok(())
    }

    pub fn count(&self) -> Result<usize> {
        Ok(self.db.query_row("select count(*) from docs", [], |r| r.get::<_, i64>(0))? as usize)
    }

    /// 检索。空查询返回空，不返回全库。
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let expr = fts_query(query);
        if expr.is_empty() {
            return Ok(Vec::new());
        }
        let mut st = self.db.prepare(
            "select d.id, d.path, d.created, d.label,
                    snippet(docs_fts, 0, '«', '»', '…', 12)
             from docs_fts f join docs d on d.rowid = f.rowid
             where docs_fts match ?1
             order by bm25(docs_fts) limit ?2",
        )?;
        let rows = st.query_map(rusqlite::params![expr, limit as i64], |r| {
            Ok(Hit {
                id: r.get(0)?,
                path: r.get(1)?,
                created: r.get(2)?,
                label: r.get(3)?,
                // 片段是切分过的正文，把逐字空格还原回去才好看
                snippet: unsegment(&r.get::<_, String>(4)?),
            })
        })?;
        let mut out = Vec::new();
        for h in rows {
            out.push(h?);
        }
        Ok(out)
    }

    /// 从 `kb/` 全量重建。返回 `(索引条数, 跳过条数)`。
    ///
    /// **这是 L2「可从 L1 重建」的可执行证明。** 先清空再灌，
    /// 这样 `kb/` 里删掉的文档不会在索引里留成幽灵。
    pub fn rebuild(&self, data_root: &Path, kb_root: &Path) -> Result<(usize, usize)> {
        self.db.execute("delete from docs", [])?;
        let (mut n, mut skipped) = (0usize, 0usize);
        for path in walk_markdown(kb_root) {
            let Ok(text) = std::fs::read_to_string(&path) else {
                skipped += 1;
                continue;
            };
            match crate::kb::parse_document(&text) {
                Some(doc) => {
                    let rel = path
                        .strip_prefix(data_root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .to_string();
                    self.upsert(&doc, &rel)?;
                    n += 1;
                }
                // 用户自己往 kb/ 里放的别的 Markdown。不是错误，跳过就好。
                None => skipped += 1,
            }
        }
        Ok((n, skipped))
    }
}

/// 递归收集 `*.md`。跳过点开头的目录（`.lock` 所在处、`.git`）。
fn walk_markdown(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let hidden = p
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'));
            if hidden {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|s| s.to_str()) == Some("md") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// 是不是需要逐字切开的表意文字。
///
/// 覆盖 CJK 统一表意文字（含扩展 A、兼容区）与日文假名。**不含谚文**——
/// 韩文是有空格分词的，切开反而会破坏它本来的词边界。
fn is_ideograph(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30ff   // 平假名 / 片假名
        | 0x3400..=0x4dbf // 扩展 A
        | 0x4e00..=0x9fff // 统一表意文字
        | 0xf900..=0xfaff // 兼容表意文字
        | 0x20000..=0x2ebef // 扩展 B–F
    )
}

/// 把 CJK 逐字用空格切开，ASCII 词保持原样。
///
/// `"给录音笔加 WiFi"` → `"给 录 音 笔 加 WiFi"`。
/// 这样 `unicode61` 就能把每个汉字当成一个 token，查询侧走同样的切分
/// 再拼成**短语**，得到的就是子串语义——而中文用户搜东西要的正是子串。
pub fn segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    let mut prev: Option<char> = None;
    for c in s.chars() {
        // 只在**两个都是字母数字**、且至少一个是表意文字时插空格。
        //
        // 标点旁边不插：`unicode61` 本来就在标点处断词，插了不改变分词结果，
        // 只是白白撑大索引，还会让 `snippet()` 的输出多出「idea， 给」
        // 这种还原不回去的空格。
        if let Some(p) = prev {
            if (is_ideograph(p) || is_ideograph(c)) && p.is_alphanumeric() && c.is_alphanumeric() {
                out.push(' ');
            }
        }
        out.push(c);
        prev = Some(c);
    }
    out
}

/// `segment` 的逆，用于把 `snippet()` 的输出还原成人读的样子。
///
/// 只去掉**两个表意文字之间**的那个空格——`"录 音 笔 加 WiFi"` 要还原成
/// `"录音笔加 WiFi"`，中英文之间那个空格得留着。
fn unsegment(s: &str) -> String {
    let cs: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < cs.len() {
        if cs[i] == ' '
            && i > 0
            && i + 1 < cs.len()
            && is_ideograph(cs[i - 1])
            && is_ideograph(cs[i + 1])
        {
            i += 1;
            continue;
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}

/// 用户输入 → FTS5 查询表达式。
///
/// ## 为什么不能把用户输入直接塞进 `MATCH`
///
/// FTS5 的查询串有自己的语法（`AND` / `OR` / `NOT` / `NEAR` / `"` / `*` / `(`）。
/// 直接拼进去，用户搜一个带引号的词就会得到语法错误，搜 `a OR b` 会得到
/// 意外的结果——**这是查询注入**，只是后果比 SQL 注入轻。
/// 所以这里自己切词、自己加引号、自己转义。
///
/// ## 语义
///
/// - **空格 = AND**：`录音 知识库` → 两个都要出现，不要求相邻
/// - **不带空格 = 相邻**：`录音` 切成 `"录 音"` 短语，等价于子串
/// - **结尾 `*` = 前缀**：`know*` 匹配 knowledge
fn fts_query(input: &str) -> String {
    let mut parts = Vec::new();
    for raw in input.split_whitespace() {
        let prefix = raw.ends_with('*');
        let word = raw.trim_end_matches('*');
        let seg = segment(word);
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        // FTS5 的短语里，双引号靠重复来转义
        let escaped = seg.replace('"', "\"\"");
        parts.push(if prefix {
            format!("\"{escaped}\"*")
        } else {
            format!("\"{escaped}\"")
        });
    }
    parts.join(" AND ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::ParsedDoc;

    fn doc(id: &str, body: &str) -> ParsedDoc {
        ParsedDoc {
            id: id.into(),
            created: "2026-09-03T10:00:00+08:00".into(),
            label: "note".into(),
            tags: vec![],
            explicit_label: false,
            body: body.into(),
        }
    }

    fn seeded() -> Index {
        let ix = Index::in_memory().unwrap();
        for (i, b) in [
            "这是一个 idea，给录音笔加 WiFi，自动把音频推到 Mac",
            "记一个任务：把 raw 目录的保留策略改成 30 天",
            "今天开会讨论了知识库的分层，L1 是文档层",
            "问一下 Docker 容器里怎么挂载录音设备",
            "note: the knowledge base should be plain markdown",
        ]
        .iter()
        .enumerate()
        {
            ix.upsert(&doc(&format!("{:016x}", i), b), &format!("kb/{i}.md")).unwrap();
        }
        ix
    }

    fn hits(ix: &Index, q: &str) -> usize {
        ix.search(q, 50).unwrap().len()
    }

    /// **中文两字词必须搜得到。** 这是选 unigram 而不是 trigram 的
    /// 全部理由——trigram 要求查询 ≥3 字符，「录音」搜不了。
    #[test]
    fn two_character_chinese_words_are_findable() {
        let ix = seeded();
        assert_eq!(hits(&ix, "录音"), 2, "「录音」应命中录音笔和录音设备两条");
        assert_eq!(hits(&ix, "音"), 2, "单字也要能搜");
        assert_eq!(hits(&ix, "知识库"), 1);
        assert_eq!(hits(&ix, "容器"), 1);
    }

    /// 中文是**子串**语义：查询词不必对应原文里的「词」。
    /// 「录音 设备」要能命中「挂载录音设备」。
    #[test]
    fn chinese_search_crosses_word_boundaries() {
        let ix = seeded();
        assert_eq!(hits(&ix, "录音设备"), 1);
        assert_eq!(hits(&ix, "挂载录音"), 1);
    }

    #[test]
    fn english_words_and_prefixes_work() {
        let ix = seeded();
        assert_eq!(hits(&ix, "wifi"), 1, "大小写不敏感");
        assert_eq!(hits(&ix, "raw"), 1);
        assert_eq!(hits(&ix, "know*"), 1, "前缀查询");
        assert_eq!(hits(&ix, "knowledge base"), 1);
    }

    /// 空格 = AND，两个词不要求相邻。
    #[test]
    fn spaces_mean_and_not_adjacency() {
        let ix = seeded();
        assert_eq!(hits(&ix, "录音 wifi"), 1, "同一篇里都出现");
        assert_eq!(hits(&ix, "录音 知识库"), 0, "没有一篇同时含这两个");
    }

    /// **用户输入不能当成 FTS5 语法。** 带引号、带 `NEAR`、带括号的查询
    /// 应该被当成普通文字，而不是报语法错误或改变查询含义。
    #[test]
    fn query_syntax_in_user_input_is_neutralised() {
        let ix = seeded();
        for q in [r#"a" OR "b"#, "NEAR(a b)", "录音 AND NOT 任务", "((((", "\"", "*", "  "] {
            // 不 panic、不返回 Err 就算过——这些本来就搜不到东西
            let r = ix.search(q, 10);
            assert!(r.is_ok(), "查询 {q:?} 不该报错: {:?}", r.err());
        }
        // `AND` 被当成字面量，不是运算符，所以搜不到任何东西
        assert_eq!(hits(&ix, "录音 AND 任务"), 0);
        assert_eq!(hits(&ix, ""), 0, "空查询返回空，不返回全库");
    }

    /// 同一条重复索引不产生第二行。
    #[test]
    fn upsert_is_idempotent() {
        let ix = Index::in_memory().unwrap();
        ix.upsert(&doc("aabb", "第一版"), "kb/a.md").unwrap();
        ix.upsert(&doc("aabb", "改过的正文"), "kb/b.md").unwrap();
        assert_eq!(ix.count().unwrap(), 1);
        assert_eq!(hits(&ix, "第一版"), 0, "旧正文必须从索引里消失");
        assert_eq!(hits(&ix, "改过"), 1);
        assert_eq!(ix.search("改过", 1).unwrap()[0].path, "kb/b.md");
    }

    #[test]
    fn removing_a_document_removes_it_from_the_index() {
        let ix = seeded();
        let id = ix.search("知识库", 1).unwrap()[0].id.clone();
        ix.remove(&id).unwrap();
        assert_eq!(hits(&ix, "知识库"), 0);
    }

    /// 片段要还原成人读的样子：汉字之间的空格去掉，中英文之间的留着。
    #[test]
    fn snippets_are_readable_again() {
        assert_eq!(unsegment("给 录 音 笔 加 WiFi"), "给录音笔加 WiFi");
        assert_eq!(segment("给录音笔加 WiFi"), "给 录 音 笔 加 WiFi");
        // 往返
        assert_eq!(unsegment(&segment("知识库的分层")), "知识库的分层");
        // **标点旁边不插空格**，否则 snippet 会变成「idea， 给」那样还原不回去
        assert_eq!(segment("这是一个 idea，给录音笔加 WiFi"), "这 是 一 个 idea，给 录 音 笔 加 WiFi");
        assert_eq!(unsegment(&segment("这是一个 idea，给录音笔加 WiFi")), "这是一个 idea，给录音笔加 WiFi");

        let ix = seeded();
        let h = &ix.search("录音笔", 1).unwrap()[0];
        assert!(h.snippet.contains("录音笔"), "片段里不该有逐字空格: {}", h.snippet);
        assert!(h.snippet.contains('«'), "应该带高亮标记: {}", h.snippet);
    }

    /// 韩文有空格分词，**不该**被逐字切开——切了反而破坏原有的词边界。
    #[test]
    fn korean_is_not_split_character_by_character() {
        assert_eq!(segment("안녕하세요 여러분"), "안녕하세요 여러분");
    }

    /// **L2 必须能从 L1 全量重建**（ADR-0003 §7）。
    /// 索引整个删掉、重建，结果要一样。
    #[test]
    fn the_index_rebuilds_itself_from_the_markdown_tree() {
        let root = std::env::temp_dir().join(format!(
            "agentear-ix-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let kb = root.join("kb/2026/09/03");
        std::fs::create_dir_all(&kb).unwrap();
        std::fs::write(
            kb.join("a.md"),
            "---\nid: aabbccdd\ncreated: 2026-09-03T10:00:00+08:00\nlabel: idea\ntags: [\"录音笔\", \"esp32\"]\nexplicit_label: true\n---\n\n给录音笔加 WiFi\n",
        )
        .unwrap();
        // 用户自己放进来的普通 Markdown：跳过，不该让重建失败
        std::fs::write(kb.join("random.md"), "# 我自己的笔记\n随便写的\n").unwrap();

        let ix = Index::in_memory().unwrap();
        let (n, skipped) = ix.rebuild(&root, &root.join("kb")).unwrap();
        assert_eq!((n, skipped), (1, 1));
        assert_eq!(hits(&ix, "录音笔"), 1);
        assert_eq!(hits(&ix, "随便"), 0, "不带 front matter 的不进索引");

        let h = &ix.search("录音笔", 1).unwrap()[0];
        assert_eq!(h.path, "kb/2026/09/03/a.md", "路径要相对数据根");
        assert_eq!(h.label, "idea");
        // 标签也可检索
        assert_eq!(hits(&ix, "esp32"), 1);

        // 再重建一次不该翻倍
        assert_eq!(ix.rebuild(&root, &root.join("kb")).unwrap(), (1, 1));
        assert_eq!(ix.count().unwrap(), 1);

        // kb/ 里删掉的文档不能在索引里留成幽灵
        std::fs::remove_file(kb.join("a.md")).unwrap();
        assert_eq!(ix.rebuild(&root, &root.join("kb")).unwrap(), (0, 1));
        assert_eq!(hits(&ix, "录音笔"), 0);
        std::fs::remove_dir_all(&root).ok();
    }
}
