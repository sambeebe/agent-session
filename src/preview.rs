//! The preview pane: who/where/when, the resume command, and the first
//! and last messages (how it started, where it ended), with the query
//! highlighted.

use crate::grep::FileKind;
use crate::session::{extract_text, Entry, Session};
use crate::sources::{self, Src};
use crate::{codex, opencode};
use crate::util::{self, BOLD, CYAN, DIM, GREEN, HIGHLIGHT, RESET, YELLOW};
use regex::Regex;
use std::collections::VecDeque;
use std::fs::File;
use serde::{Deserialize, Serialize};
use std::io::{self, BufReader, Seek, SeekFrom, Write};
use std::path::Path;

/// Messages shown from each end of a session.
const DEFAULT_MESSAGES: usize = 6;
/// Bumped when `Scanned` changes meaning, so older cache files are re-read.
const PREVIEW_CACHE_V: u32 = 2;
const BODY_MAX: usize = 200;
const LINE_CAP: usize = 16 * 1024 * 1024;

pub struct Options {
    pub source: Option<Src>,
    pub query: Option<String>,
    pub regex: bool,
    pub locator: Option<String>,
    pub color: bool,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct Message {
    role: String,
    ms: i64,
    body: String,
}

fn message_limit() -> usize {
    util::env_nonempty("AGENT_SESSION_PREVIEW_MESSAGES")
        .and_then(|v| v.trim().parse().ok())
        .filter(|n: &usize| *n > 0)
        .unwrap_or(DEFAULT_MESSAGES)
}

/// What the preview knows about one transcript, kept in the cache so moving
/// the cursor back onto a session (or onto one that only grew) does not
/// read the whole file again. Bodies are stored already shortened.
#[derive(Serialize, Deserialize, Default)]
struct Scanned {
    #[serde(default)]
    v: u32,
    path: String,
    size: u64,
    mtime_ns: i64,
    /// Just past the last whole line read; a grown file continues here.
    offset: u64,
    limit: usize,
    started: i64,
    total: usize,
    /// The first `limit` messages.
    #[serde(default)]
    head: Vec<Message>,
    /// The last `limit` messages.
    ring: VecDeque<Message>,
}

/// The first and last messages of a session, its start, and its count.
struct Excerpt {
    head: Vec<Message>,
    tail: Vec<Message>,
    started: i64,
    total: usize,
}

impl From<Scanned> for Excerpt {
    fn from(s: Scanned) -> Excerpt {
        Excerpt { head: s.head, tail: s.ring.into(), started: s.started, total: s.total }
    }
}

fn cache_file(path: &str) -> Option<std::path::PathBuf> {
    util::cache_dir().map(|d| d.join("preview").join(format!("{:016x}.json", util::fnv64(path))))
}

/// The first and last `limit` messages with text, the first message's
/// time, and how many messages with text there are in all; from the cache
/// when it holds this file, read on from where it stopped when it grew.
fn load_messages(path: &Path, limit: usize, kind: FileKind) -> io::Result<Excerpt> {
    let meta = std::fs::metadata(path)?;
    let (size, mtime) = (meta.len(), util::mtime_ns(&meta));
    let key = path.to_string_lossy().to_string();
    let file = cache_file(&key);
    let cached: Option<Scanned> = file
        .as_ref()
        .and_then(|f| std::fs::read(f).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(|s: &Scanned| s.v == PREVIEW_CACHE_V && s.path == key && s.limit == limit);

    let mut state = match cached {
        Some(s) if s.size == size && s.mtime_ns == mtime => return Ok(s.into()),
        Some(s) if size > s.size && s.offset <= size => s,
        _ => Scanned { v: PREVIEW_CACHE_V, path: key, limit, ..Default::default() },
    };
    scan_from(path, &mut state, kind)?;
    state.size = size;
    state.mtime_ns = mtime;
    if let (Some(f), Ok(bytes)) = (&file, serde_json::to_vec(&state)) {
        let _ = util::write_atomic(f, &bytes);
    }
    Ok(state.into())
}

fn scan_from(path: &Path, st: &mut Scanned, kind: FileKind) -> io::Result<()> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(st.offset))?;
    let mut r = BufReader::with_capacity(1 << 20, f);
    let mut buf = Vec::new();
    loop {
        let line = util::read_line_capped(&mut r, &mut buf, LINE_CAP)?;
        if line.len == 0 || !line.whole {
            break; // a half-written last line waits for the next look
        }
        st.offset += line.len as u64;
        if line.over {
            continue;
        }
        if kind == FileKind::Codex {
            let Ok(s) = std::str::from_utf8(&buf) else { continue };
            let (ts, msg) = codex::line_message(s);
            if st.started == 0 && ts > 0 {
                st.started = ts;
            }
            if let Some((role, ms, body)) = msg {
                st.push(Message { role, ms, body: short_body(&body) });
            }
            continue;
        }
        let head = String::from_utf8_lossy(&buf[..buf.len().min(400)]);
        if head.contains("\"tool_result\"") || head.starts_with("{\"type\":\"api-request") {
            continue; // never has text of its own
        }
        let Ok(s) = std::str::from_utf8(&buf) else { continue };
        if !s.contains("\"type\":\"user\"") && !s.contains("\"type\":\"assistant\"") {
            continue;
        }
        let Ok(e) = serde_json::from_str::<Entry>(s.trim()) else { continue };
        let role = match e.kind.as_deref() {
            Some(k @ ("user" | "assistant")) => k.to_string(),
            _ => continue,
        };
        let Some(m) = &e.message else { continue };
        let body = extract_text(&m.content, "\n");
        if body.trim().is_empty() {
            continue;
        }
        let ms = util::parse_ts_ms(e.timestamp.as_deref().unwrap_or(""));
        if st.started == 0 && ms > 0 {
            st.started = ms;
        }
        st.push(Message { role, ms, body: short_body(&body) });
    }
    Ok(())
}

impl Scanned {
    fn push(&mut self, m: Message) {
        if self.head.len() < self.limit {
            self.head.push(m.clone());
        }
        if self.ring.len() == self.limit {
            self.ring.pop_front();
        }
        self.ring.push_back(m);
        self.total += 1;
    }
}

/// First two non-empty lines, joined, cut to BODY_MAX characters.
fn short_body(s: &str) -> String {
    let kept: Vec<&str> = s
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .take(2)
        .collect();
    util::truncate(&kept.join(" | "), BODY_MAX)
}

fn highlight(s: &str, re: Option<&Regex>) -> String {
    let Some(re) = re else { return s.to_string() };
    let mut out = String::with_capacity(s.len() + 16);
    let mut last = 0;
    for m in re.find_iter(s) {
        if m.start() == m.end() {
            continue;
        }
        out.push_str(&s[last..m.start()]);
        out.push_str(HIGHLIGHT);
        out.push_str(m.as_str());
        out.push_str(RESET);
        last = m.end();
    }
    out.push_str(&s[last..]);
    out
}

pub fn run(id: &str, opts: &Options) -> Result<(), String> {
    let s = sources::resolve(opts.source, id, opts.locator.as_deref())?;
    let limit = message_limit();
    let excerpt = match Src::of(&s) {
        Src::OpenCode => {
            let (head, tail, started, total) = opencode::messages(&s.id, limit)?;
            let short = |v: Vec<(String, i64, String)>| -> Vec<Message> {
                v.into_iter().map(|(role, ms, body)| Message { role, ms, body: short_body(&body) }).collect()
            };
            Excerpt { head: short(head), tail: short(tail), started, total }
        }
        Src::Codex => load_messages(Path::new(&s.jsonl_path), limit, FileKind::Codex).map_err(|e| e.to_string())?,
        Src::Claude => load_messages(Path::new(&s.jsonl_path), limit, FileKind::Claude).map_err(|e| e.to_string())?,
    };
    render(&s, &excerpt, opts).map_err(|e| e.to_string())
}

/// What to show, in order: the first messages, then how many are left out
/// (`Some(n)`, only when there is a gap), then the last ones. Where the two
/// ends overlap, a short session, each message is shown once.
fn arrange(e: &Excerpt) -> (&[Message], Option<usize>, &[Message]) {
    let tail_start = e.total.saturating_sub(e.tail.len());
    let skip = e.head.len().saturating_sub(tail_start).min(e.tail.len());
    let gap = tail_start.saturating_sub(e.head.len());
    (&e.head, (gap > 0).then_some(gap), &e.tail[skip..])
}

/// The rule under the header: 60 wide, or narrower in a preview pane that
/// is (fzf says how wide in FZF_PREVIEW_COLUMNS), so it never wraps.
fn rule_width() -> usize {
    std::env::var("FZF_PREVIEW_COLUMNS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|cols| cols.saturating_sub(1).clamp(1, 60))
        .unwrap_or(60)
}

fn render(s: &Session, e: &Excerpt, opts: &Options) -> io::Result<()> {
    let c = |code: &'static str| if opts.color { code } else { "" };
    let (bold, reset, dim) = (c(BOLD), c(RESET), c(DIM));
    let now = util::now_ms();
    let started = if e.started > 0 { e.started } else { s.last_ms };
    let total = e.total;

    let out = io::stdout();
    let mut w = io::BufWriter::new(out.lock());
    // Everything that comes out of a transcript is printed through
    // `printable`: a folder name or message holding terminal escape
    // sequences must not reach the terminal as such.
    let id = util::printable(&s.id);
    let cwd_text = util::printable(&s.cwd);
    writeln!(w, "{bold}session{reset} : {id}  {dim}({}){reset}", Src::of(s).name())?;
    let cwd = if cwd_text.is_empty() { "(unknown)" } else { &cwd_text };
    if s.cwd_exists {
        writeln!(w, "{bold}project{reset} : {}{cwd}{reset}", c(CYAN))?;
    } else {
        writeln!(w, "{bold}project{reset} : {}{cwd} [gone]{reset}", c(YELLOW))?;
    }
    let age = |ms: i64| if ms > now { "in the future".to_string() } else { util::relative(ms, now) };
    writeln!(w, "{bold}started{reset} : {}  {dim}({}){reset}", util::local_fmt(started, "%Y-%m-%d %H:%M"), age(started))?;
    writeln!(w, "{bold}last{reset}    : {}  {dim}({total} msgs){reset}", util::local_fmt(s.last_ms, "%Y-%m-%d %H:%M"))?;
    if s.cwd_exists {
        let cmd = Src::of(s).resume_argv(&id).join(" ");
        writeln!(w, "{bold}resume{reset}  : {dim}cd \"{cwd}\" && {cmd}{reset}")?;
    }
    writeln!(w, "{dim}{}{reset}", "─".repeat(rule_width()))?;

    let re = opts
        .query
        .as_deref()
        .filter(|q| opts.color && !q.trim().is_empty())
        .and_then(|q| {
            let pattern = if opts.regex { q.to_string() } else { regex::escape(q) };
            Regex::new(&format!("(?i){pattern}")).ok()
        });
    // A message from another day than the session's last one shows its date.
    let last_day = util::local_fmt(s.last_ms, "%Y-%m-%d");
    let line = |w: &mut io::BufWriter<_>, m: &Message| -> io::Result<()> {
        let (role, color) = if m.role == "assistant" { ("asst", c(CYAN)) } else { ("user", c(GREEN)) };
        let stamp = if m.ms <= 0 {
            "--:--".to_string()
        } else if util::local_fmt(m.ms, "%Y-%m-%d") == last_day {
            util::local_fmt(m.ms, "%H:%M")
        } else {
            util::local_fmt(m.ms, "%m-%d %H:%M")
        };
        writeln!(w, "{color}[{role} {stamp}]{reset} {}", highlight(&util::printable(&m.body), re.as_ref()))
    };
    let (head, gap, tail) = arrange(e);
    for m in head {
        line(&mut w, m)?;
    }
    if let Some(n) = gap {
        let what = if n == 1 { "message" } else { "messages" };
        writeln!(w, "{dim}  ··· {n} more {what} ···{reset}")?;
    }
    for m in tail {
        line(&mut w, m)?;
    }
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::{arrange, Excerpt, Message};

    fn msgs(range: std::ops::Range<usize>) -> Vec<Message> {
        range.map(|i| Message { role: "user".into(), ms: i as i64, body: i.to_string() }).collect()
    }
    fn bodies(m: &[Message]) -> Vec<String> {
        m.iter().map(|m| m.body.clone()).collect()
    }
    fn shown(head: std::ops::Range<usize>, tail: std::ops::Range<usize>, total: usize) -> (Vec<String>, Option<usize>, Vec<String>) {
        let e = Excerpt { head: msgs(head), tail: msgs(tail), started: 0, total };
        let (h, gap, t) = arrange(&e);
        (bodies(h), gap, bodies(t))
    }

    #[test]
    fn first_and_last_messages_once_each() {
        // 20 messages, 3 from each end: a gap of 14 between them.
        assert_eq!(shown(0..3, 17..20, 20), (bodies(&msgs(0..3)), Some(14), bodies(&msgs(17..20))));
        // 7: the ends touch, nothing left out, nothing twice.
        assert_eq!(shown(0..3, 4..7, 7), (bodies(&msgs(0..3)), Some(1), bodies(&msgs(4..7))));
        assert_eq!(shown(0..3, 3..6, 6), (bodies(&msgs(0..3)), None, bodies(&msgs(3..6))));
        // 4: the ends overlap; the overlap is shown once.
        assert_eq!(shown(0..3, 1..4, 4), (bodies(&msgs(0..3)), None, bodies(&msgs(3..4))));
        // 2, shorter than one end: shown once.
        assert_eq!(shown(0..2, 0..2, 2), (bodies(&msgs(0..2)), None, vec![]));
        assert_eq!(shown(0..0, 0..0, 0), (vec![], None, vec![]));
    }
}
