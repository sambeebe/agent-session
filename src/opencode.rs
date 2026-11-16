//! OpenCode sessions: a SQLite database (OpenCode 1.2+), opened read-only.
//! `$OPENCODE_DB`, else the newest `opencode.db` / `opencode-*.db` under
//! `$XDG_DATA_HOME/opencode` or `~/.local/share/opencode` (where OpenCode
//! keeps its data on Windows too). Resumed with `opencode --session <id>`.
//! Ported from upstream ccsession's internal/opencode.

use crate::grep::Matcher;
use crate::session::Session;
use crate::util;
use rusqlite::{params, Connection, OpenFlags};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const NAME: &str = "opencode";

fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(x) = util::env_nonempty("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(x).join("opencode"));
    }
    if let Some(h) = util::home_dir() {
        dirs.push(h.join(".local").join("share").join("opencode"));
        if cfg!(target_os = "macos") {
            dirs.push(h.join("Library").join("Application Support").join("opencode"));
        }
    }
    dirs.dedup();
    dirs
}

/// The database to read, or None when OpenCode has none here.
pub fn db_path() -> Option<PathBuf> {
    if let Some(p) = util::env_nonempty("OPENCODE_DB") {
        let p = std::path::absolute(&p).unwrap_or_else(|_| PathBuf::from(p));
        return p.is_file().then_some(p);
    }
    let mut best: Option<(i64, PathBuf)> = None;
    for dir in candidate_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let is_db = name == "opencode.db" || (name.starts_with("opencode-") && name.ends_with(".db"));
            let Ok(meta) = e.metadata() else { continue };
            if !is_db || !meta.is_file() {
                continue;
            }
            let mt = util::mtime_ns(&meta);
            if best.as_ref().map(|(b, _)| mt > *b).unwrap_or(true) {
                best = Some((mt, e.path()));
            }
        }
    }
    best.map(|(_, p)| p)
}

fn open(path: &Path) -> Result<Connection, String> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let c = Connection::open_with_flags(path, flags).map_err(|e| format!("opencode db {}: {e}", path.display()))?;
    let _ = c.busy_timeout(std::time::Duration::from_secs(5));
    let _ = c.pragma_update(None, "query_only", 1);
    Ok(c)
}

fn connect() -> Result<Option<Connection>, String> {
    match db_path() {
        Some(p) => open(&p).map(Some),
        None => Ok(None),
    }
}

fn session_of(id: String, title: Option<String>, dir: Option<String>, updated_ms: i64) -> Session {
    let dir = dir.unwrap_or_default();
    let mut s = Session {
        source: NAME.to_string(),
        label: util::sanitize_label(title.as_deref().unwrap_or("")),
        id,
        last_ms: updated_ms,
        ..Default::default()
    };
    if s.label.is_empty() {
        s.label = "(untitled)".into();
    }
    if dir.is_empty() {
        s.cwd_unknown = true;
    } else {
        s.cwd_basename = Path::new(&dir).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| dir.clone());
        s.cwd_exists = Path::new(&dir).is_dir();
        s.cwd = dir;
    }
    s
}

const SCAN: &str = "SELECT id, title, directory, time_updated FROM session \
                    WHERE parent_id IS NULL AND time_archived IS NULL \
                    ORDER BY time_updated DESC, id DESC";

/// Top-level, unarchived sessions (subagent children are left out).
pub fn scan() -> Result<Vec<Session>, String> {
    let Some(c) = connect()? else { return Ok(Vec::new()) };
    let mut st = c.prepare(SCAN).map_err(|e| format!("opencode: {e}"))?;
    let rows = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get::<_, Option<i64>>(3)?.unwrap_or(0))))
        .map_err(|e| format!("opencode: {e}"))?;
    Ok(rows
        .flatten()
        .filter(|(id, ..)| !id.contains(['\t', '\n', '\r']))
        .map(|(id, title, dir, ms)| session_of(id, title, dir, ms))
        .collect())
}

pub fn find(id: &str) -> Result<Session, String> {
    let c = connect()?.ok_or("no opencode database found")?;
    let mut st = c
        .prepare("SELECT id, title, directory, time_updated FROM session WHERE id = ?1 OR id LIKE ?2 ESCAPE '\\' LIMIT 2")
        .map_err(|e| format!("opencode: {e}"))?;
    let prefix = format!("{}%", escape_like(id));
    let found: Vec<Session> = st
        .query_map(params![id, prefix], |r| Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get::<_, Option<i64>>(3)?.unwrap_or(0))))
        .map_err(|e| format!("opencode: {e}"))?
        .flatten()
        .map(|(id, t, d, ms)| session_of(id, t, d, ms))
        .collect();
    if let Some(exact) = found.iter().find(|s| s.id == id) {
        return Ok(exact.clone());
    }
    match found.len() {
        1 => Ok(found.into_iter().next().unwrap()),
        0 => Err(format!("opencode session not found: {id}")),
        _ => Err(format!("several opencode sessions start with {id}; give more of the id")),
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Time {
    created: i64,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct MessageData {
    role: String,
    time: Time,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct PartData {
    #[serde(rename = "type")]
    kind: String,
    text: String,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct ProjectionData {
    text: String,
    time: Time,
}

fn has_table(c: &Connection, name: &str) -> bool {
    c.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1", [name], |_| Ok(()))
        .is_ok()
}

/// Every user/assistant message with text, oldest first: (role, ms, body).
/// Newer databases keep a `session_message` projection; older ones only
/// messages and their parts.
fn all_messages(c: &Connection, id: &str) -> Result<Vec<(String, i64, String)>, String> {
    let err = |e: rusqlite::Error| format!("opencode: {e}");
    if has_table(c, "session_message") {
        let mut st = c
            .prepare("SELECT type, data FROM session_message WHERE session_id = ?1 AND type IN ('user','assistant') ORDER BY seq, id")
            .map_err(err)?;
        let out: Vec<_> = st
            .query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(err)?
            .flatten()
            .filter_map(|(role, data)| {
                let d: ProjectionData = serde_json::from_str(&data).ok()?;
                (!d.text.trim().is_empty()).then_some((role, d.time.created, d.text))
            })
            .collect();
        if !out.is_empty() {
            return Ok(out);
        }
    }
    let mut st = c
        .prepare(
            "SELECT m.id, m.data, p.data FROM message m LEFT JOIN part p ON p.message_id = m.id \
             WHERE m.session_id = ?1 ORDER BY m.time_created, m.id, p.id",
        )
        .map_err(err)?;
    let rows = st
        .query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?)))
        .map_err(err)?;
    let mut out: Vec<(String, i64, String)> = Vec::new();
    let mut cur: Option<(String, String, i64, String)> = None; // (msg id, role, ms, body)
    let flush = |cur: Option<(String, String, i64, String)>, out: &mut Vec<(String, i64, String)>| {
        if let Some((_, role, ms, body)) = cur {
            if (role == "user" || role == "assistant") && !body.trim().is_empty() {
                out.push((role, ms, body));
            }
        }
    };
    for (mid, mdata, pdata) in rows.flatten() {
        if cur.as_ref().map(|c| c.0 != mid).unwrap_or(true) {
            flush(cur.take(), &mut out);
            let md: MessageData = serde_json::from_str(&mdata).unwrap_or_default();
            cur = Some((mid, md.role, md.time.created, String::new()));
        }
        if let (Some(c), Some(p)) = (cur.as_mut(), pdata) {
            if let Ok(part) = serde_json::from_str::<PartData>(&p) {
                if part.kind == "text" && !part.text.is_empty() {
                    if !c.3.is_empty() {
                        c.3.push('\n');
                    }
                    c.3.push_str(&part.text);
                }
            }
        }
    }
    flush(cur.take(), &mut out);
    Ok(out)
}

/// The last `limit` messages, the first message's time, and the count.
pub fn messages(id: &str, limit: usize) -> Result<(Vec<(String, i64, String)>, i64, usize), String> {
    let c = connect()?.ok_or("no opencode database found")?;
    let all = all_messages(&c, id)?;
    let started = all.first().map(|m| m.1).unwrap_or(0);
    let total = all.len();
    let tail = all.into_iter().skip(total.saturating_sub(limit)).collect();
    Ok((tail, started, total))
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// SQLite's LIKE folds ASCII case only; for anything else (and regexes)
/// every session is checked in Rust instead of prefiltered in SQL.
fn prefilter_usable(query: &str, regex: bool) -> bool {
    !regex && query.is_ascii() && !query.contains(['"', '\\', '\n', '\r'])
}

/// Ids of the sessions whose title or message text matches.
pub fn grep(query: &str, regex: bool) -> Result<HashSet<String>, String> {
    let matcher = Matcher::new(query, regex)?;
    let Some(c) = connect()? else { return Ok(HashSet::new()) };
    let err = |e: rusqlite::Error| format!("opencode: {e}");
    let roots: Vec<(String, String)> = if prefilter_usable(query, regex) {
        let like = format!("%{}%", escape_like(query));
        let projection = if has_table(&c, "session_message") {
            " OR EXISTS (SELECT 1 FROM session_message sm WHERE sm.session_id = s.id AND sm.data LIKE ?1 ESCAPE '\\')"
        } else {
            ""
        };
        let sql = format!(
            "SELECT id, COALESCE(title,'') FROM session s WHERE s.parent_id IS NULL AND s.time_archived IS NULL AND ( \
             s.title LIKE ?1 ESCAPE '\\' \
             OR EXISTS (SELECT 1 FROM part p JOIN message m ON p.message_id = m.id WHERE m.session_id = s.id AND p.data LIKE ?1 ESCAPE '\\'){projection})"
        );
        let mut st = c.prepare(&sql).map_err(err)?;
        let rows: Vec<_> = st.query_map([like], |r| Ok((r.get(0)?, r.get(1)?))).map_err(err)?.flatten().collect();
        rows
    } else {
        let mut st = c
            .prepare("SELECT id, COALESCE(title,'') FROM session WHERE parent_id IS NULL AND time_archived IS NULL")
            .map_err(err)?;
        let rows: Vec<_> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(err)?.flatten().collect();
        rows
    };
    let mut hits = HashSet::new();
    for (id, title) in roots {
        if matcher.hits_one(&title) {
            hits.insert(id);
            continue;
        }
        if all_messages(&c, &id)?.iter().any(|(_, _, body)| matcher.hits_one(body)) {
            hits.insert(id);
        }
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(with_projection: bool) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ccs-oc-{}-{}.db", std::process::id(), with_projection));
        let _ = std::fs::remove_file(&p);
        let c = Connection::open(&p).unwrap();
        c.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT NOT NULL DEFAULT '', title TEXT NOT NULL DEFAULT '', time_created INTEGER NOT NULL DEFAULT 0, time_updated INTEGER NOT NULL DEFAULT 0, time_archived INTEGER);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL DEFAULT 0, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, data TEXT NOT NULL);
             INSERT INTO session VALUES ('ses_aaa', NULL, '/tmp', 'Refactor parser', 1, 2000, NULL);
             INSERT INTO session VALUES ('ses_child', 'ses_aaa', '/tmp', 'subagent', 1, 3000, NULL);
             INSERT INTO session VALUES ('ses_old', NULL, '/tmp', 'Archived one', 1, 1000, 5);
             INSERT INTO message VALUES ('m1', 'ses_aaa', 10, '{\"role\":\"user\",\"time\":{\"created\":10}}');
             INSERT INTO message VALUES ('m2', 'ses_aaa', 20, '{\"role\":\"assistant\",\"time\":{\"created\":20}}');
             INSERT INTO part VALUES ('p1', 'm1', 'ses_aaa', '{\"type\":\"text\",\"text\":\"split the tokenizer\"}');
             INSERT INTO part VALUES ('p2', 'm2', 'ses_aaa', '{\"type\":\"tool\",\"tool\":\"bash\"}');
             INSERT INTO part VALUES ('p3', 'm2', 'ses_aaa', '{\"type\":\"text\",\"text\":\"Done: Lexer is separate\"}');",
        )
        .unwrap();
        if with_projection {
            c.execute_batch(
                "CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, data TEXT NOT NULL, seq INTEGER NOT NULL);
                 INSERT INTO session_message VALUES ('s1','ses_aaa','user','{\"text\":\"projected question\",\"time\":{\"created\":11}}',1);",
            )
            .unwrap();
        }
        p
    }

    // One test: OPENCODE_DB is process-wide, so the cases run in sequence.
    #[test]
    fn reads_sessions_messages_and_grep() {
        let p = fixture(false);
        std::env::set_var("OPENCODE_DB", &p);
        let ss = scan().unwrap();
        assert_eq!(ss.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["ses_aaa"]);
        assert_eq!(ss[0].label, "Refactor parser");
        let (msgs, started, total) = messages("ses_aaa", 30).unwrap();
        assert_eq!((started, total), (10, 2));
        assert_eq!(msgs[1].2, "Done: Lexer is separate");
        assert!(grep("lexer", false).unwrap().contains("ses_aaa")); // case-insensitive
        assert!(grep("nothing like this", false).unwrap().is_empty());
        assert!(grep("tokeni[sz]er", true).unwrap().contains("ses_aaa"));
        assert_eq!(find("ses_a").unwrap().id, "ses_aaa");

        let p2 = fixture(true);
        std::env::set_var("OPENCODE_DB", &p2);
        let (msgs, _, total) = messages("ses_aaa", 30).unwrap();
        assert_eq!((total, msgs[0].2.as_str()), (1, "projected question"));
        std::env::remove_var("OPENCODE_DB");
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_file(p2);
    }
}
