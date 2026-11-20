//! Claude Code transcripts: where they are, what one session looks like in
//! the list, and a cache so only changed files are parsed again.

use crate::util;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// How much of a transcript's end the list reads.
pub const TAIL_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Session {
    /// Which harness wrote it: "claude", "codex" or "opencode".
    #[serde(default = "claude_name")]
    pub source: String,
    pub id: String,
    pub project_dir: String,
    pub jsonl_path: String,
    pub cwd: String,
    pub cwd_basename: String,
    pub label: String,
    /// Last user/assistant timestamp (else the file's mtime), epoch ms.
    pub last_ms: i64,
    /// When the session was started (its first message), epoch ms.
    #[serde(default)]
    pub created_ms: i64,
    #[serde(default)]
    pub cwd_exists: bool,
    /// No cwd recorded and the directory-name fallback did not exist:
    /// resume refuses these.
    #[serde(default)]
    pub cwd_unknown: bool,
}

pub fn claude_name() -> String {
    "claude".to_string()
}

/// $CLAUDE_CONFIG_DIR/projects, else ~/.claude/projects.
pub fn projects_dir() -> Option<PathBuf> {
    if let Some(base) = util::env_nonempty("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(base).join("projects"));
    }
    util::home_dir().map(|h| h.join(".claude").join("projects"))
}

/// Every `<project>/<id>.jsonl`, leaving out subagents' `agent-*.jsonl`.
pub fn collect_jsonl_paths(base: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(dirs) = fs::read_dir(base) else {
        return out;
    };
    for pd in dirs.flatten() {
        if !pd.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(files) = fs::read_dir(pd.path()) else {
            continue;
        };
        for f in files.flatten() {
            let name = f.file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(".jsonl") || name.starts_with("agent-") {
                continue;
            }
            if f.file_type().map(|t| t.is_file()).unwrap_or(false) {
                out.push(f.path());
            }
        }
    }
    out
}

// ---------------------------------------------------------------- JSONL rows

#[derive(Deserialize, Default)]
#[serde(default)]
pub(crate) struct Entry {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    #[serde(rename = "aiTitle")]
    pub ai_title: Option<String>,
    /// What `/rename` sets; the upstream tool does not read it.
    #[serde(rename = "customTitle")]
    pub custom_title: Option<String>,
    #[serde(rename = "lastPrompt")]
    pub last_prompt: Option<String>,
    pub timestamp: Option<String>,
    pub cwd: Option<String>,
    pub message: Option<Msg>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(crate) struct Msg {
    pub content: Option<Content>,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum Content {
    Text(String),
    Blocks(Vec<Block>),
    Other(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
pub(crate) struct Block {
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
}

const REMINDER_OPEN: &str = "<system-reminder>";
const REMINDER_CLOSE: &str = "</system-reminder>";

/// Drops `<system-reminder>` blocks: the CLAUDE.md and memory copies the
/// harness puts in user turns would otherwise match every grep.
pub(crate) fn strip_reminders(s: &str) -> String {
    if !s.contains(REMINDER_OPEN) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(REMINDER_OPEN) {
        out.push_str(&rest[..i]);
        match rest[i..].find(REMINDER_CLOSE) {
            Some(j) => rest = &rest[i + j + REMINDER_CLOSE.len()..],
            None => rest = "",
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// The text of a message: a bare string, or its `text` blocks joined by `sep`.
pub(crate) fn extract_text(content: &Option<Content>, sep: &str) -> String {
    match content {
        Some(Content::Text(s)) => strip_reminders(s),
        Some(Content::Blocks(blocks)) => {
            let mut out = String::new();
            for b in blocks {
                if b.kind.as_deref() != Some("text") {
                    continue;
                }
                let Some(t) = &b.text else { continue };
                let t = strip_reminders(t);
                if t.is_empty() {
                    continue;
                }
                if !out.is_empty() {
                    out.push_str(sep);
                }
                out.push_str(&t);
            }
            out
        }
        _ => String::new(),
    }
}

// ---------------------------------------------------------------- cwd <-> project dir

/// Claude Code names a project's folder after its start cwd with every
/// character that is not an ASCII letter or digit turned into '-':
/// C:\Users\me\dev\x -> C--Users-me-dev-x, /home/me/x -> -home-me-x.
pub fn encode_cwd(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The lossy inverse of `encode_cwd`, the last resort for a transcript
/// that recorded no cwd at all.
#[cfg(windows)]
pub fn restore_cwd_from_dir(base: &str) -> String {
    let b = base.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && &b[1..3] == b"--" {
        format!("{}:\\{}", &base[..1], base[3..].replace('-', "\\"))
    } else {
        base.replace('-', "\\")
    }
}
#[cfg(not(windows))]
pub fn restore_cwd_from_dir(base: &str) -> String {
    format!("/{}", base.trim_start_matches('-').replace('-', "/"))
}

// ---------------------------------------------------------------- parsing

fn read_tail(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let mut f = File::open(path)?;
    let size = f.metadata()?.len();
    if size == 0 {
        return Ok(Vec::new());
    }
    let offset = size.saturating_sub(max);
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::with_capacity((size - offset) as usize);
    f.read_to_end(&mut buf)?;
    if offset > 0 {
        // The first line is most likely cut.
        if let Some(i) = buf.iter().position(|&b| b == b'\n') {
            buf.drain(..=i);
        }
    }
    Ok(buf)
}

/// The cwd of the first row that records one, in the first 64 KiB: the
/// project folder is named after it, so it is what `claude --resume` finds.
fn read_start_cwd(path: &Path) -> Option<String> {
    let mut buf = Vec::new();
    File::open(path).ok()?.take(64 * 1024).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(e) = serde_json::from_str::<Entry>(line) {
            if let Some(c) = e.cwd.filter(|c| !c.is_empty()) {
                return Some(c);
            }
        }
    }
    None
}

fn nonempty(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.trim().is_empty())
}

/// Reads the end of one transcript. `Ok(None)`: the file holds no user
/// message or nothing to label it with.
pub fn parse_tail(path: &Path, max: u64) -> io::Result<Option<Session>> {
    let buf = read_tail(path, max)?;
    let project_dir = path.parent().unwrap_or(Path::new(""));
    let projects_base = project_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut sess = Session {
        source: claude_name(),
        id: path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        project_dir: project_dir.to_string_lossy().to_string(),
        jsonl_path: path.to_string_lossy().to_string(),
        ..Default::default()
    };

    let mut has_user = false;
    let (mut ai_title, mut custom_title, mut last_prompt, mut last_user) =
        (None::<String>, None::<String>, None::<String>, None::<String>);
    let mut earliest_cwd = String::new();
    let mut matching_cwd = String::new();
    let mut last_ms = 0i64;
    let mut first_ms = 0i64;

    let text = String::from_utf8_lossy(&buf);
    for line in text.lines().rev() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("{\"type\":\"api-request") || line.starts_with("{\"type\":\"file-history") {
            continue; // request blobs run to megabytes and carry nothing read here
        }
        let Ok(e) = serde_json::from_str::<Entry>(line) else {
            continue;
        };
        // A session that `cd`-ed records several cwds; only the one whose
        // encoding names this project folder is safe to resume from.
        if let Some(c) = e.cwd.as_deref().filter(|c| !c.is_empty()) {
            earliest_cwd = c.to_string();
            if matching_cwd.is_empty() && encode_cwd(c) == projects_base {
                matching_cwd = c.to_string();
            }
        }
        let kind = e.kind.clone().unwrap_or_default();
        match kind.as_str() {
            "ai-title" if ai_title.is_none() => ai_title = nonempty(e.ai_title),
            "custom-title" if custom_title.is_none() => custom_title = nonempty(e.custom_title),
            "last-prompt" if last_prompt.is_none() => last_prompt = nonempty(e.last_prompt),
            "user" | "assistant" => {
                if kind == "user" {
                    has_user = true;
                    if last_user.is_none() {
                        if let Some(m) = &e.message {
                            last_user = nonempty(Some(extract_text(&m.content, " ")));
                        }
                    }
                }
                let ts = util::parse_ts_ms(e.timestamp.as_deref().unwrap_or(""));
                last_ms = last_ms.max(ts);
                if ts > 0 {
                    first_ms = if first_ms == 0 { ts } else { first_ms.min(ts) };
                }
            }
            _ => {}
        }
    }

    let mut cwd = matching_cwd;
    if cwd.is_empty() {
        cwd = read_start_cwd(path).unwrap_or_default();
    }
    if cwd.is_empty() {
        let guess = restore_cwd_from_dir(&projects_base);
        if Path::new(&guess).is_dir() {
            cwd = guess;
        }
    }
    if cwd.is_empty() {
        cwd = earliest_cwd;
    }

    if !has_user {
        return Ok(None);
    }
    let Some(label) = [custom_title, ai_title, last_prompt, last_user]
        .into_iter()
        .flatten()
        .next()
    else {
        return Ok(None);
    };
    sess.label = util::sanitize_label(&label);
    if sess.label.is_empty() {
        return Ok(None);
    }

    if cwd.is_empty() {
        sess.cwd_unknown = true;
    } else {
        sess.cwd_basename = Path::new(&cwd)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| cwd.clone());
        sess.cwd_exists = Path::new(&cwd).is_dir();
    }
    sess.cwd = cwd;

    if last_ms == 0 {
        if let Ok(m) = fs::metadata(path) {
            last_ms = util::mtime_ns(&m) / 1_000_000;
        }
    }
    sess.last_ms = last_ms;
    // When it was started: the first message near the top of the file, else
    // the earliest one in the tail just read, else its last activity.
    sess.created_ms = read_start_ms(path)
        .or((first_ms > 0).then_some(first_ms))
        .unwrap_or(last_ms);
    Ok(Some(sess))
}

/// The time of the first user or assistant row in the first 64 KiB.
fn read_start_ms(path: &Path) -> Option<i64> {
    let mut buf = Vec::new();
    File::open(path).ok()?.take(64 * 1024).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines() {
        let line = line.trim();
        if !line.contains("\"type\":\"user\"") && !line.contains("\"type\":\"assistant\"") {
            continue;
        }
        let Ok(e) = serde_json::from_str::<Entry>(line) else { continue };
        if !matches!(e.kind.as_deref(), Some("user" | "assistant")) {
            continue;
        }
        let ms = util::parse_ts_ms(e.timestamp.as_deref().unwrap_or(""));
        if ms > 0 {
            return Some(ms);
        }
    }
    None
}

/// `parse_tail` over a growing tail until it finds a session. A long session
/// can end in megabytes of tool output with no user row in the last 64 KiB;
/// upstream reads only that much and drops such a session from the list.
pub fn parse_session(path: &Path, first: u64) -> io::Result<Option<Session>> {
    // The tail is held in memory, so it stops growing at TAIL_MAX however
    // large the file: a session with nothing usable in its last 256 MiB
    // is left out rather than read whole.
    const TAIL_MAX: u64 = 256 << 20;
    let limit = fs::metadata(path)?.len().min(TAIL_MAX);
    let mut max = first;
    loop {
        if let Some(s) = parse_tail(path, max)? {
            return Ok(Some(s));
        }
        if max >= limit {
            return Ok(None);
        }
        max = max.saturating_mul(16).min(TAIL_MAX);
    }
}

// ---------------------------------------------------------------- scanning with a cache

const SCAN_CACHE_VERSION: u32 = 4;

#[derive(Serialize, Deserialize, Default)]
struct ScanCache {
    v: u32,
    records: HashMap<String, ScanRecord>,
}

#[derive(Serialize, Deserialize, Clone)]
struct ScanRecord {
    size: u64,
    mtime_ns: i64,
    session: Option<Session>,
}

fn load_scan_cache(path: Option<&Path>) -> ScanCache {
    path.and_then(|p| fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<ScanCache>(&b).ok())
        .filter(|c| c.v == SCAN_CACHE_VERSION)
        .unwrap_or(ScanCache { v: SCAN_CACHE_VERSION, records: HashMap::new() })
}

/// Parses each transcript (reusing the cache for files whose size and
/// mtime have not changed) and sorts newest first. `is_full` says `paths`
/// is every transcript, so cache records for deleted files can go.
pub fn scan(paths: &[PathBuf], is_full: bool) -> Vec<Session> {
    scan_with(paths, is_full, CLAUDE_SCAN_CACHE, |p| parse_session(p, TAIL_BYTES))
}

pub const CLAUDE_SCAN_CACHE: &str = "scan-claude.json";

/// `scan` for any file-per-session source: `parse` reads one file, and
/// `cache_name` keeps each source's records apart (pruning is per source).
pub fn scan_with<F>(paths: &[PathBuf], is_full: bool, cache_name: &str, parse: F) -> Vec<Session>
where
    F: Fn(&Path) -> io::Result<Option<Session>> + Sync,
{
    let cache_path = util::cache_dir().map(|d| d.join(cache_name));
    let mut cache = load_scan_cache(cache_path.as_deref());
    let mut changed = false;

    let metas: Vec<(PathBuf, String, u64, i64)> = paths
        .iter()
        .filter_map(|p| {
            let m = fs::metadata(p).ok()?;
            Some((p.clone(), p.to_string_lossy().to_string(), m.len(), util::mtime_ns(&m)))
        })
        .collect();

    let mut results: Vec<Option<Session>> = vec![None; metas.len()];
    let mut todo = Vec::new();
    for (i, (_, key, size, mt)) in metas.iter().enumerate() {
        match cache.records.get(key) {
            Some(r) if r.size == *size && r.mtime_ns == *mt => results[i] = r.session.clone(),
            _ => todo.push(i),
        }
    }
    let parsed = util::parallel_map(&todo, |&i| parse(&metas[i].0));
    for (&i, res) in todo.iter().zip(parsed) {
        if let Ok(s) = res {
            let (_, key, size, mt) = &metas[i];
            cache.records.insert(
                key.clone(),
                ScanRecord { size: *size, mtime_ns: *mt, session: s.clone() },
            );
            results[i] = s;
            changed = true;
        }
    }
    if is_full {
        let live: HashSet<&str> = metas.iter().map(|m| m.1.as_str()).collect();
        let before = cache.records.len();
        cache.records.retain(|k, _| live.contains(k.as_str()));
        changed |= cache.records.len() != before;
    }
    if changed {
        if let (Some(p), Ok(bytes)) = (&cache_path, serde_json::to_vec(&cache)) {
            let _ = util::write_atomic(p, &bytes);
        }
    }

    let mut out: Vec<Session> = results.into_iter().flatten().collect();
    for s in &mut out {
        // Folders come and go between runs; the cache must not vouch for one.
        if !s.cwd_unknown {
            s.cwd_exists = Path::new(&s.cwd).is_dir();
        }
    }
    let now = util::now_ms();
    // A future timestamp is clock skew or a pasted transcript: sink it.
    let key = |s: &Session| if s.last_ms > now { 0 } else { s.last_ms };
    out.sort_by(|a, b| key(b).cmp(&key(a)).then_with(|| a.id.cmp(&b.id)));
    out
}

/// Drops sessions whose cwd (or basename) contains `needle`, ignoring case.
pub fn filter_out_by_dir(sessions: Vec<Session>, needle: &str) -> Vec<Session> {
    let needle = needle.to_lowercase();
    sessions
        .into_iter()
        .filter(|s| {
            let target = if s.cwd.is_empty() { &s.cwd_basename } else { &s.cwd };
            target.is_empty() || !target.to_lowercase().contains(&needle)
        })
        .collect()
}

/// The transcript's path (an OpenCode session's id, as it has no file),
/// base64url so it is one safe TSV field and shell word.
pub fn locator_for(s: &Session) -> String {
    if s.jsonl_path.is_empty() {
        util::b64url_encode(s.id.as_bytes())
    } else {
        util::b64url_encode(s.jsonl_path.as_bytes())
    }
}

// ---------------------------------------------------------------- one session by id

/// The list's own parse of `path`, when the scan cache holds it for the
/// file as it is now: the preview runs per cursor move and should not
/// re-read a tail the list just read.
pub fn cached_session(path: &Path, cache_name: &str) -> Option<Session> {
    let meta = fs::metadata(path).ok()?;
    let cache = load_scan_cache(util::cache_dir().map(|d| d.join(cache_name)).as_deref());
    let rec = cache.records.get(path.to_string_lossy().as_ref())?;
    if rec.size != meta.len() || rec.mtime_ns != util::mtime_ns(&meta) {
        return None;
    }
    let mut s = rec.session.clone()?;
    if !s.cwd_unknown {
        s.cwd_exists = Path::new(&s.cwd).is_dir();
    }
    Some(s)
}

fn parse_for_resolve(path: &Path, id: &str) -> Result<Session, String> {
    if let Some(s) = cached_session(path, CLAUDE_SCAN_CACHE) {
        return Ok(s);
    }
    match parse_session(path, TAIL_BYTES * 4) {
        Ok(Some(s)) => Ok(s),
        Ok(None) => Err(format!("session has no usable content: {id}")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(format!("session not found: {id}")),
        Err(e) => Err(e.to_string()),
    }
}

/// Finds a session by its id, or by a unique prefix of it (`7b6ae3ee`).
pub fn find_by_id(id: &str) -> Result<Session, String> {
    let base = projects_dir().ok_or("cannot find the home directory")?;
    let want = format!("{id}.jsonl");
    let mut prefixed = Vec::new();
    if let Ok(dirs) = fs::read_dir(&base) {
        for pd in dirs.flatten() {
            let exact = pd.path().join(&want);
            if exact.is_file() {
                return parse_for_resolve(&exact, id);
            }
            if id.len() >= 4 {
                if let Ok(files) = fs::read_dir(pd.path()) {
                    for f in files.flatten() {
                        let name = f.file_name().to_string_lossy().to_string();
                        if name.starts_with(id) && name.ends_with(".jsonl") && !name.starts_with("agent-") {
                            prefixed.push(f.path());
                        }
                    }
                }
            }
        }
    }
    match prefixed.len() {
        1 => parse_for_resolve(&prefixed[0], id),
        0 => Err(format!("session not found: {id}")),
        n => Err(format!("{n} sessions start with {id}; give more of the id")),
    }
}

/// The session `id` names, through its locator (the transcript's path,
/// base64url) when one is given and still points at that id's file.
pub fn resolve(id: &str, locator: Option<&str>) -> Result<Session, String> {
    if let Some(raw) = locator
        .filter(|l| !l.trim().is_empty())
        .and_then(util::b64url_decode)
        .and_then(|b| String::from_utf8(b).ok())
    {
        let p = PathBuf::from(raw);
        let stem_matches = p.file_stem().map(|s| s.to_string_lossy() == id).unwrap_or(false);
        // A locator is only ever one of our own rows' paths: anything that
        // does not resolve to a transcript under the projects folder is
        // ignored, so it cannot be used to make agent-session read another file.
        let inside = projects_dir().map(|base| util::is_under(&p, &base)).unwrap_or(false);
        if stem_matches && inside && p.is_file() {
            return parse_for_resolve(&p, id);
        }
    }
    find_by_id(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_like_claude_code() {
        assert_eq!(encode_cwd(r"C:\Users\me\dev\app"), "C--Users-me-dev-app");
        assert_eq!(
            encode_cwd(r"C:\Users\me\dev\app\my files (1)\sub_dir"),
            "C--Users-me-dev-app-my-files--1--sub-dir"
        );
        assert_eq!(encode_cwd("/home/me/my.app"), "-home-me-my-app");
    }

    #[test]
    fn strips_reminders() {
        assert_eq!(strip_reminders("hi <system-reminder>noise</system-reminder> there"), "hi  there");
        assert_eq!(strip_reminders("plain"), "plain");
    }
}
