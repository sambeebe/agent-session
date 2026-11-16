//! Grep mode: which transcripts mention the query in what you read (user and
//! assistant text, titles, last prompts). Each transcript's searchable text is
//! cached lowercased; transcripts only grow, so a grown file is read from
//! where the cache stopped instead of from the start.

use crate::session::{extract_text, Entry};
use crate::util;
use regex::Regex;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Rows longer than this are skipped, as upstream does: they are tool
/// payloads, not conversation.
const LINE_CAP: usize = 4 * 1024 * 1024;
const CACHE_MAGIC: &str = "ccsg1";

const SKIP_PREFIX: [&str; 3] = ["{\"type\":\"api-request", "{\"type\":\"file-history", "{\"type\":\"attachment\""];
const WANT: [&str; 5] = [
    "\"type\":\"user\"",
    "\"type\":\"assistant\"",
    "\"type\":\"ai-title\"",
    "\"type\":\"last-prompt\"",
    "\"type\":\"custom-title\"",
];

/// Which file format a transcript is, for reading its rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileKind {
    Claude,
    Codex,
}

pub enum Matcher {
    Fixed(String),
    Re(Regex),
}

impl Matcher {
    pub fn new(query: &str, regex: bool) -> Result<Matcher, String> {
        if regex {
            Regex::new(&format!("(?i){query}"))
                .map(Matcher::Re)
                .map_err(|e| format!("invalid regex: {e}"))
        } else {
            Ok(Matcher::Fixed(query.to_lowercase()))
        }
    }

    /// One piece of raw text (an OpenCode title or message).
    pub fn hits_one(&self, text: &str) -> bool {
        match self {
            Matcher::Fixed(needle) => text.to_lowercase().contains(needle.as_str()),
            Matcher::Re(re) => re.is_match(text),
        }
    }

    /// `text` is the cache's form: lowercased fragments, each ended by NUL.
    fn hits(&self, text: &str) -> bool {
        match self {
            Matcher::Fixed(needle) => text.contains(needle.as_str()),
            Matcher::Re(re) => text.split('\0').any(|f| re.is_match(f)),
        }
    }
}

/// The transcripts among `paths` whose text matches `query`.
pub fn filter(paths: &[PathBuf], query: &str, regex: bool, kind: FileKind) -> Result<HashSet<PathBuf>, String> {
    let matcher = Matcher::new(query, regex)?;
    let dir = util::cache_dir().map(|d| d.join("grep"));
    if let Some(d) = &dir {
        let _ = util::private_dir(d);
    }
    let hits = util::parallel_map(paths, |p| {
        cached_text(p, dir.as_deref(), kind)
            .map(|t| matcher.hits(&t))
            .unwrap_or(false)
    });
    Ok(paths
        .iter()
        .zip(hits)
        .filter(|(_, hit)| *hit)
        .map(|(p, _)| p.clone())
        .collect())
}

struct Header {
    size: u64,
    mtime_ns: i64,
    offset: u64,
    path: String,
}

fn parse_cache(bytes: &[u8]) -> Option<(Header, &str)> {
    let nl = bytes.iter().position(|&b| b == b'\n')?;
    let head = std::str::from_utf8(&bytes[..nl]).ok()?;
    let mut f = head.splitn(5, '\t');
    if f.next()? != CACHE_MAGIC {
        return None;
    }
    let header = Header {
        size: f.next()?.parse().ok()?,
        mtime_ns: f.next()?.parse().ok()?,
        offset: f.next()?.parse().ok()?,
        path: f.next()?.to_string(),
    };
    let body = std::str::from_utf8(&bytes[nl + 1..]).ok()?;
    Some((header, body))
}

fn write_cache(file: &Path, path: &str, size: u64, mtime_ns: i64, offset: u64, text: &str) {
    let mut data = format!("{CACHE_MAGIC}\t{size}\t{mtime_ns}\t{offset}\t{path}\n").into_bytes();
    data.extend_from_slice(text.as_bytes());
    let _ = util::write_atomic(file, &data);
}

fn cached_text(path: &Path, dir: Option<&Path>, kind: FileKind) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    let (size, mtime) = (meta.len(), util::mtime_ns(&meta));
    let Some(dir) = dir else {
        return extract(path, 0, kind).ok().map(|(t, _)| t);
    };
    let key = path.to_string_lossy().to_string();
    let file = dir.join(format!("{:016x}.txt", util::fnv64(&key)));

    if let Ok(bytes) = fs::read(&file) {
        if let Some((h, body)) = parse_cache(&bytes) {
            if h.path == key {
                if h.size == size && h.mtime_ns == mtime {
                    return Some(body.to_string());
                }
                if size > h.size && h.offset <= size {
                    if let Ok((more, offset)) = extract(path, h.offset, kind) {
                        let text = format!("{body}{more}");
                        write_cache(&file, &key, size, mtime, offset, &text);
                        return Some(text);
                    }
                }
            }
        }
    }
    let (text, offset) = extract(path, 0, kind).ok()?;
    write_cache(&file, &key, size, mtime, offset, &text);
    Some(text)
}

/// Reads whole lines from `from` on and returns their searchable text and
/// the offset after the last whole line (a half-written last line waits).
fn extract(path: &Path, from: u64, kind: FileKind) -> io::Result<(String, u64)> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(from))?;
    let mut r = BufReader::with_capacity(1 << 20, f);
    let mut buf = Vec::new();
    let mut offset = from;
    let mut out = String::new();
    loop {
        let line = util::read_line_capped(&mut r, &mut buf, LINE_CAP)?;
        if line.len == 0 || !line.whole {
            break;
        }
        offset += line.len as u64;
        if line.over {
            continue;
        }
        let texts = match kind {
            FileKind::Claude => line_texts(&buf),
            FileKind::Codex => std::str::from_utf8(&buf).map(crate::codex::line_texts).unwrap_or_default(),
        };
        for frag in texts {
            out.push_str(&frag.to_lowercase());
            out.push('\0');
        }
    }
    Ok((out, offset))
}

fn line_texts(line: &[u8]) -> Vec<String> {
    let head = String::from_utf8_lossy(&line[..line.len().min(400)]);
    if SKIP_PREFIX.iter().any(|p| head.starts_with(p))
        || head.contains("\"tool_result\"")
        || head.contains("\"attachment\":{")
    {
        return Vec::new();
    }
    let Ok(s) = std::str::from_utf8(line) else {
        return Vec::new();
    };
    if !WANT.iter().any(|w| s.contains(w)) {
        return Vec::new();
    }
    let Ok(e) = serde_json::from_str::<Entry>(s.trim()) else {
        return Vec::new();
    };
    let text = match e.kind.as_deref() {
        Some("user") | Some("assistant") => e
            .message
            .as_ref()
            .map(|m| extract_text(&m.content, " "))
            .unwrap_or_default(),
        Some("ai-title") => e.ai_title.unwrap_or_default(),
        Some("custom-title") => e.custom_title.unwrap_or_default(),
        Some("last-prompt") => e.last_prompt.unwrap_or_default(),
        _ => String::new(),
    };
    if text.trim().is_empty() {
        Vec::new()
    } else {
        vec![text]
    }
}
