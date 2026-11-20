//! Codex CLI sessions: `$CODEX_HOME/sessions/**/*.jsonl` (default ~/.codex),
//! one "rollout" file per run, resumed with `codex resume <id>`. Ported
//! from upstream ccsession's internal/codex.

use crate::session::{self, Session};
use crate::util;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

pub const NAME: &str = "codex";
pub const SCAN_CACHE: &str = "scan-codex.json";
const LINE_CAP: usize = 64 * 1024 * 1024;

pub fn home() -> Option<PathBuf> {
    match util::env_nonempty("CODEX_HOME") {
        Some(h) => Some(std::path::absolute(&h).unwrap_or_else(|_| PathBuf::from(h))),
        None => util::home_dir().map(|h| h.join(".codex")),
    }
}

/// Every rollout file, oldest path first.
pub fn collect_paths() -> Vec<PathBuf> {
    let Some(root) = home().map(|h| h.join("sessions")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                stack.push(e.path());
            } else if t.is_file() && e.file_name().to_string_lossy().ends_with(".jsonl") {
                out.push(e.path());
            }
        }
    }
    out.sort();
    out
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Line {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    payload: Option<Payload>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Payload {
    id: Option<String>,
    timestamp: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    role: Option<String>,
    content: Option<Content>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<Block>),
    Other(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
struct Block {
    #[serde(default)]
    text: Option<String>,
}

fn content_text(c: &Option<Content>) -> String {
    match c {
        Some(Content::Text(s)) => s.clone(),
        Some(Content::Blocks(bs)) => bs
            .iter()
            .filter_map(|b| b.text.as_deref().filter(|t| !t.is_empty()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// What Codex injects as a user turn (AGENTS.md, environment, reminders)
/// is not something you typed; a wrapped request keeps only the request.
fn user_visible(body: &str) -> String {
    let body = body.trim();
    if body.is_empty()
        || body.starts_with("# AGENTS.md instructions for ")
        || body.contains("\n<INSTRUCTIONS>")
        || body.starts_with("<environment_context>")
        || body.starts_with("<system_reminder>")
    {
        return String::new();
    }
    match body.split_once("## My request for Codex:\n") {
        Some((_, after)) => after.trim().to_string(),
        None => body.to_string(),
    }
}

/// One row, cheaply skipped unless it can matter.
fn parse_line(line: &str) -> Option<Line> {
    if !line.contains("\"response_item\"") && !line.contains("\"session_meta\"") {
        return None;
    }
    serde_json::from_str(line.trim()).ok()
}

/// A user/assistant message row: (role, epoch ms, body).
fn message_of(l: &Line) -> Option<(String, i64, String)> {
    if l.kind.as_deref() != Some("response_item") {
        return None;
    }
    let p = l.payload.as_ref()?;
    if p.kind.as_deref() != Some("message") {
        return None;
    }
    let role = p.role.as_deref()?;
    if role != "user" && role != "assistant" {
        return None;
    }
    let mut body = content_text(&p.content);
    if role == "user" {
        body = user_visible(&body);
    }
    if body.trim().is_empty() {
        return None;
    }
    Some((role.to_string(), util::parse_ts_ms(l.timestamp.as_deref().unwrap_or("")), body))
}

/// The session id inside a rollout name (`rollout-2026-…-<uuid>.jsonl`).
fn id_from_path(path: &Path) -> String {
    let name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let re = regex::Regex::new(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}").unwrap();
    re.find_iter(&name).last().map(|m| m.as_str().to_string()).unwrap_or(name)
}

/// Reads one rollout. `Ok(None)`: nothing you typed, so nothing to label.
pub fn parse_file(path: &Path) -> io::Result<Option<Session>> {
    let mut r = BufReader::with_capacity(256 << 10, File::open(path)?);
    let (mut id, mut cwd, mut label, mut last_ms) = (String::new(), String::new(), String::new(), 0i64);
    let mut created_ms = 0i64;
    let mut buf = Vec::new();
    loop {
        let line = util::read_line_capped(&mut r, &mut buf, LINE_CAP)?;
        if line.len == 0 {
            break;
        }
        if line.over {
            continue;
        }
        let Ok(s) = std::str::from_utf8(&buf) else { continue };
        let Some(l) = parse_line(s) else { continue };
        let ts = util::parse_ts_ms(l.timestamp.as_deref().unwrap_or(""));
        last_ms = last_ms.max(ts);
        if created_ms == 0 {
            created_ms = ts;
        }
        if l.kind.as_deref() == Some("session_meta") {
            if let Some(p) = &l.payload {
                if let Some(v) = p.id.as_deref().filter(|v| !v.is_empty()) {
                    id = v.to_string();
                }
                if let Some(v) = p.cwd.as_deref().filter(|v| !v.is_empty()) {
                    cwd = v.to_string();
                }
                last_ms = last_ms.max(util::parse_ts_ms(p.timestamp.as_deref().unwrap_or("")));
            }
        } else if let Some((role, _, body)) = message_of(&l) {
            if role == "user" {
                label = util::sanitize_label(&body); // the last thing you asked
            }
        }
    }
    if id.is_empty() {
        id = id_from_path(path);
    }
    if id.is_empty() || id.contains(['\t', '\n', '\r']) || label.is_empty() {
        return Ok(None);
    }
    if last_ms == 0 {
        last_ms = fs::metadata(path).map(|m| util::mtime_ns(&m) / 1_000_000).unwrap_or(0);
    }
    let mut s = Session {
        source: NAME.to_string(),
        id,
        project_dir: path.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
        jsonl_path: path.to_string_lossy().to_string(),
        label,
        last_ms,
        created_ms: if created_ms > 0 { created_ms } else { last_ms },
        ..Default::default()
    };
    if cwd.is_empty() {
        s.cwd_unknown = true;
    } else {
        s.cwd_basename = Path::new(&cwd).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| cwd.clone());
        s.cwd_exists = Path::new(&cwd).is_dir();
        s.cwd = cwd;
    }
    Ok(Some(s))
}

/// Every Codex session, newest first, one per id: a resumed session writes
/// a new rollout with the same id, and the newest one is the live one. It
/// was started when its oldest rollout was.
pub fn scan(paths: &[PathBuf], is_full: bool) -> Vec<Session> {
    let all = session::scan_with(paths, is_full, SCAN_CACHE, parse_file);
    let mut started: HashMap<String, i64> = HashMap::new();
    for s in &all {
        let first = started.entry(s.id.clone()).or_insert(s.created_ms);
        if s.created_ms > 0 && (*first == 0 || s.created_ms < *first) {
            *first = s.created_ms;
        }
    }
    let mut seen: HashMap<String, ()> = HashMap::new();
    all.into_iter()
        .filter(|s| seen.insert(s.id.clone(), ()).is_none())
        .map(|mut s| {
            s.created_ms = started.get(&s.id).copied().unwrap_or(s.created_ms);
            s
        })
        .collect()
}

pub fn find(id: &str, locator_path: Option<&Path>) -> Result<Session, String> {
    // A locator is one of our own rows' paths: only a file under Codex's
    // sessions folder is read through it.
    let sessions_root = home().map(|h| h.join("sessions"));
    let inside = |p: &&Path| sessions_root.as_deref().map(|r| util::is_under(p, r)).unwrap_or(false);
    if let Some(p) = locator_path.filter(|p| p.is_file()).filter(inside) {
        if let Some(s) = session::cached_session(p, SCAN_CACHE).or_else(|| parse_file(p).ok().flatten()) {
            if s.id == id {
                return Ok(s);
            }
        }
    }
    let found: Vec<Session> = scan(&collect_paths(), true)
        .into_iter()
        .filter(|s| s.id == id || (id.len() >= 4 && s.id.starts_with(id)))
        .collect();
    match found.len() {
        0 => Err(format!("codex session not found: {id}")),
        1 => Ok(found.into_iter().next().unwrap()),
        n => Err(format!("{n} codex sessions start with {id}; give more of the id")),
    }
}

/// Grep and preview read rows through these, so Codex shares their
/// append-aware caches with Claude.
pub fn line_texts(line: &str) -> Vec<String> {
    parse_line(line).and_then(|l| message_of(&l)).map(|(_, _, b)| vec![b]).unwrap_or_default()
}

/// (first-message time candidate, message) for the preview.
pub fn line_message(line: &str) -> (i64, Option<(String, i64, String)>) {
    let Some(l) = parse_line(line) else { return (0, None) };
    if l.kind.as_deref() == Some("session_meta") {
        let ts = l.payload.as_ref().and_then(|p| p.timestamp.as_deref()).map(util::parse_ts_ms).unwrap_or(0);
        return (ts, None);
    }
    let m = message_of(&l);
    (m.as_ref().map(|m| m.1).unwrap_or(0), m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollout_parses_like_upstream() {
        let dir = std::env::temp_dir().join(format!("ccs-codex-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let id = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
        let path = dir.join(format!("rollout-2026-06-14T00-00-00-{id}.jsonl"));
        let cwd = dir.to_string_lossy().replace('\\', "\\\\");
        let rows = [
            format!(r#"{{"timestamp":"2026-06-14T00:00:00Z","type":"session_meta","payload":{{"id":"{id}","timestamp":"2026-06-14T00:00:00Z","cwd":"{cwd}"}}}}"#),
            r##"{"timestamp":"2026-06-14T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\nrule\n</INSTRUCTIONS>"}]}}"##.to_string(),
            r##"{"timestamp":"2026-06-14T00:00:02Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# Files mentioned by the user:\n\n## My request for Codex:\nfix the flaky test"}]}}"##.to_string(),
            r##"{"timestamp":"2026-06-14T00:00:03Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done, it was a race"}]}}"##.to_string(),
        ];
        fs::write(&path, rows.join("\n") + "\n").unwrap();
        let s = parse_file(&path).unwrap().expect("a session");
        assert_eq!(s.id, id);
        assert_eq!(s.label, "fix the flaky test");
        assert!(s.cwd_exists);
        assert_eq!(s.source, "codex");
        let texts: Vec<String> = rows.iter().flat_map(|r| line_texts(r)).collect();
        assert_eq!(texts, ["fix the flaky test", "done, it was a race"]);
        let _ = fs::remove_dir_all(&dir);
    }
}
