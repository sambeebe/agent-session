//! Small shared helpers: paths, ANSI, labels, time, base64url, a thread pool.

use chrono::{DateTime, Local, TimeZone};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const CYAN: &str = "\x1b[36m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
/// Reverse video: marks grep-query matches, like fzf's own highlight.
pub const HIGHLIGHT: &str = "\x1b[7m";

pub fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

#[cfg(windows)]
fn platform_home() -> Option<PathBuf> {
    env_nonempty("USERPROFILE").map(PathBuf::from)
}
#[cfg(not(windows))]
fn platform_home() -> Option<PathBuf> {
    None
}

pub fn home_dir() -> Option<PathBuf> {
    platform_home().or_else(|| env_nonempty("HOME").map(PathBuf::from))
}

#[cfg(windows)]
fn platform_cache_base() -> Option<PathBuf> {
    env_nonempty("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join("AppData").join("Local")))
}
#[cfg(not(windows))]
fn platform_cache_base() -> Option<PathBuf> {
    env_nonempty("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".cache")))
}

/// Where the scan and grep caches live: $AGENT_SESSION_CACHE_DIR, else
/// %LOCALAPPDATA%\agent-session on Windows and $XDG_CACHE_HOME/agent-session elsewhere.
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(d) = env_nonempty("AGENT_SESSION_CACHE_DIR") {
        return Some(PathBuf::from(d));
    }
    platform_cache_base().map(|b| b.join("agent-session"))
}

pub fn fnv64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

pub fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// RFC3339 timestamp to epoch milliseconds; 0 when empty or unparseable.
pub fn parse_ts_ms(raw: &str) -> i64 {
    DateTime::parse_from_rfc3339(raw)
        .map(|t| t.timestamp_millis())
        .unwrap_or(0)
}

pub fn local_fmt(ms: i64, fmt: &str) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(t) if ms > 0 => t.format(fmt).to_string(),
        _ => "?".to_string(),
    }
}

pub fn rfc3339(ms: i64) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(t) if ms > 0 => t.to_rfc3339(),
        _ => String::new(),
    }
}

/// Compact age: "just now", "5m ago", "3h ago", "2d ago", else the date.
pub fn relative(ms: i64, now: i64) -> String {
    if ms <= 0 {
        return "?".into();
    }
    let secs = (now - ms.min(now)) / 1000;
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 7 * 86_400 {
        format!("{}d ago", secs / 86_400)
    } else {
        local_fmt(ms, "%Y-%m-%d")
    }
}

pub const LABEL_MAX: usize = 200;

/// Collapses whitespace and control characters (pasted ANSI codes would
/// otherwise hijack fzf's rendering) and cuts to LABEL_MAX characters.
pub fn sanitize_label(s: &str) -> String {
    let mut out = String::new();
    let mut n = 0usize;
    let mut space = false;
    for c in s.chars() {
        if (c as u32) < 0x20 || c as u32 == 0x7f || c.is_whitespace() {
            space = n > 0;
            continue;
        }
        if space {
            out.push(' ');
            n += 1;
            space = false;
        }
        out.push(c);
        n += 1;
        if n > LABEL_MAX {
            break;
        }
    }
    truncate(&out, LABEL_MAX)
}

/// Cuts `s` to at most `n` characters, the last one an ellipsis when cut.
pub fn truncate(s: &str, n: usize) -> String {
    if n == 0 || s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n - 1).collect();
    out.push('…');
    out
}

pub fn pad_right(s: &str, n: usize) -> String {
    let len = s.chars().count();
    if len >= n {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(n - len))
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// base64url without padding: the locator column, so a path with spaces or
/// tabs never breaks the TSV row or the shell fzf hands it to.
pub fn b64url_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 4 / 3 + 3);
    for chunk in data.chunks(3) {
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = ((chunk[0] as u32) << 16) | (b1 << 8) | b2;
        for i in 0..=chunk.len() {
            out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.trim().as_bytes();
    if bytes.len() % 4 == 1 {
        return None;
    }
    let val = |c: u8| B64.iter().position(|&x| x == c).map(|p| p as u32);
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        for i in 0..chunk.len() - 1 {
            out.push(((n >> (16 - 8 * i)) & 0xff) as u8);
        }
    }
    Some(out)
}

/// Runs `f` over `items` on every core and returns the results in order.
pub fn parallel_map<T, R, F>(items: &[T], f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }
    let workers = std::thread::available_parallelism()
        .map(|p| p.get() * 2)
        .unwrap_or(4)
        .clamp(1, n);
    let next = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<R>>> = (0..n).map(|_| Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let r = f(&items[i]);
                *slots[i].lock().unwrap() = Some(r);
            });
        }
    });
    slots
        .into_iter()
        .map(|m| m.into_inner().unwrap().expect("every slot is filled"))
        .collect()
}

/// Creates a cache folder (and its parents). The caches hold conversation
/// text, so on Unix a folder made here is the owner's alone (0700); Windows
/// keeps %LOCALAPPDATA%'s per-user ACL.
pub fn private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

/// Writes `data` to `path` through a temporary file and a rename, so a
/// concurrent reader (another fzf reload) never sees half a cache file.
/// The file is created new (never through an existing file or link) and,
/// on Unix, readable by its owner only (0600).
pub fn write_atomic(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        private_dir(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let res = opts
        .open(&tmp)
        .and_then(|mut f| f.write_all(data))
        .and_then(|_| std::fs::rename(&tmp, path));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Text that is safe to print: every control character (ESC above all, so
/// no terminal escape sequence survives; also tab, CR, LF) becomes a space.
/// Transcripts hold whatever a tool printed or a web page said, and both
/// fzf (`--ansi`) and the terminal would otherwise act on it.
pub fn printable(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// Whether `id` is safe as a row field and as an argument to the harness:
/// letters, digits, `-`, `_`, `.`, not starting with `-` (which a harness
/// would read as an option, e.g. a transcript planted under a name like
/// `--some-flag.jsonl`). Real ids (UUIDs, `ses_…`) all pass.
pub fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('-')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Whether `path` really lies under `root`, links and `..` resolved.
pub fn is_under(path: &std::path::Path, root: &std::path::Path) -> bool {
    match (path.canonicalize(), root.canonicalize()) {
        (Ok(p), Ok(r)) => p.starts_with(r),
        _ => false,
    }
}

/// One line read into `buf`, holding at most `cap` bytes of it in memory.
pub struct Line {
    /// Bytes consumed from the reader, the newline included; 0 at the end.
    pub len: usize,
    /// The line ended with a newline (false: a half-written last line).
    pub whole: bool,
    /// It was longer than `cap`; `buf` is empty and the line is skipped.
    pub over: bool,
}

/// `read_until(b'\n')` that cannot be made to buffer a giant line: past
/// `cap` the rest of the line is consumed and dropped.
pub fn read_line_capped<R: std::io::BufRead>(r: &mut R, buf: &mut Vec<u8>, cap: usize) -> std::io::Result<Line> {
    buf.clear();
    let mut line = Line { len: 0, whole: false, over: false };
    loop {
        let avail = r.fill_buf()?;
        if avail.is_empty() {
            return Ok(line);
        }
        let (take, done) = match avail.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (avail.len(), false),
        };
        if !line.over {
            if buf.len() + take > cap {
                line.over = true;
                buf.clear();
            } else {
                buf.extend_from_slice(&avail[..take]);
            }
        }
        r.consume(take);
        line.len += take;
        if done {
            line.whole = true;
            return Ok(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        for s in ["", "a", "ab", "abc", "abcd", r"C:\Users\me\.claude\projects\x\y.jsonl"] {
            let enc = b64url_encode(s.as_bytes());
            assert_eq!(b64url_decode(&enc).unwrap(), s.as_bytes(), "{s}");
        }
    }

    #[test]
    fn ids_and_text_are_made_safe() {
        assert!(safe_id("7b6ae3ee-52c7-478b-a0fc-e586a02e82b2"));
        assert!(safe_id("ses_7Hq2mKpLx9"));
        for bad in ["", "--dangerous-flag", "a b", "a\tb", "a&calc", "a%PATH%", "a;b", "a\"b", "../x"] {
            assert!(!safe_id(bad), "{bad:?}");
        }
        assert_eq!(printable("a\x1b[31mb\tc\r\nd\u{9b}e"), "a [31mb c  d e");
    }

    #[test]
    fn capped_lines_skip_giants_without_holding_them() {
        let data = format!("short\n{}\nlast", "x".repeat(5000));
        let mut r = std::io::BufReader::with_capacity(64, data.as_bytes());
        let mut buf = Vec::new();
        let l = read_line_capped(&mut r, &mut buf, 100).unwrap();
        assert_eq!((l.len, l.whole, l.over, buf.as_slice()), (6, true, false, &b"short\n"[..]));
        let l = read_line_capped(&mut r, &mut buf, 100).unwrap();
        assert_eq!((l.len, l.whole, l.over, buf.len()), (5001, true, true, 0));
        let l = read_line_capped(&mut r, &mut buf, 100).unwrap();
        assert_eq!((l.len, l.whole, l.over, buf.as_slice()), (4, false, false, &b"last"[..]));
        assert_eq!(read_line_capped(&mut r, &mut buf, 100).unwrap().len, 0);
    }

    #[test]
    fn labels_collapse_and_cut() {
        // A control character (here ESC) counts as whitespace, as upstream.
        assert_eq!(sanitize_label("  a\n\tb\x1b[31m c "), "a b [31m c");
        let long = "x".repeat(500);
        let cut = sanitize_label(&long);
        assert_eq!(cut.chars().count(), LABEL_MAX);
        assert!(cut.ends_with('…'));
    }
}
