//! The harnesses agent-session reads, side by side in one list: Claude Code,
//! Codex and OpenCode. A harness that is not set up on this machine simply
//! contributes no rows.

use crate::grep::{self, FileKind};
use crate::session::{self, Session};
use crate::{codex, opencode, util};
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Src {
    Claude,
    Codex,
    OpenCode,
}

impl Src {
    pub const ALL: [Src; 3] = [Src::Claude, Src::Codex, Src::OpenCode];

    pub fn name(self) -> &'static str {
        match self {
            Src::Claude => "claude",
            Src::Codex => codex::NAME,
            Src::OpenCode => opencode::NAME,
        }
    }

    pub fn parse(s: &str) -> Option<Src> {
        match s.trim().to_lowercase().as_str() {
            "claude" | "claude-code" | "cc" => Some(Src::Claude),
            "codex" => Some(Src::Codex),
            "opencode" | "oc" => Some(Src::OpenCode),
            _ => None,
        }
    }

    pub fn of(s: &Session) -> Src {
        Src::parse(&s.source).unwrap_or(Src::Claude)
    }

    /// The command that reopens a session of this harness.
    pub fn resume_argv(self, id: &str) -> Vec<String> {
        let v: [&str; 3] = match self {
            Src::Claude => ["claude", "--resume", id],
            Src::Codex => ["codex", "resume", id],
            Src::OpenCode => ["opencode", "--session", id],
        };
        v.iter().map(|s| s.to_string()).collect()
    }
}

/// `--source`: one harness, or `all` (the default).
pub fn parse_filter(v: Option<&str>) -> Result<Vec<Src>, String> {
    match v.map(str::trim).filter(|v| !v.is_empty()) {
        None | Some("all") => Ok(Src::ALL.to_vec()),
        Some(v) => Src::parse(v)
            .map(|s| vec![s])
            .ok_or_else(|| format!("unknown --source {v}: claude, codex, opencode or all")),
    }
}

/// The filter's label in the picker header.
pub fn filter_label(srcs: &[Src]) -> &'static str {
    if srcs.len() == 1 { srcs[0].name() } else { "all" }
}

fn load_one(src: Src, query: Option<&str>, regex: bool) -> Result<Vec<Session>, String> {
    let keep = |paths: Vec<PathBuf>, kind: FileKind| -> Result<Vec<PathBuf>, String> {
        let Some(q) = query else { return Ok(paths) };
        let allow = grep::filter(&paths, q, regex, kind)?;
        Ok(paths.into_iter().filter(|p| allow.contains(p)).collect())
    };
    match src {
        Src::Claude => {
            let Some(base) = session::projects_dir() else { return Ok(Vec::new()) };
            let paths = keep(session::collect_jsonl_paths(&base), FileKind::Claude)?;
            Ok(session::scan(&paths, query.is_none()))
        }
        Src::Codex => {
            let paths = keep(codex::collect_paths(), FileKind::Codex)?;
            Ok(codex::scan(&paths, query.is_none()))
        }
        Src::OpenCode => {
            let all = opencode::scan()?;
            match query {
                None => Ok(all),
                Some(q) => {
                    let ids = opencode::grep(q, regex)?;
                    Ok(all.into_iter().filter(|s| ids.contains(&s.id)).collect())
                }
            }
        }
    }
}

/// Every session of `srcs`, newest first; with a query, only those whose
/// text matches. With several harnesses, one that fails to read (a locked
/// or unfamiliar OpenCode database, say) is left out instead of failing
/// the list, since an error printed under fzf would garble the screen.
pub fn load(srcs: &[Src], query: Option<&str>, regex: bool) -> Result<Vec<Session>, String> {
    let query = query.filter(|q| !q.trim().is_empty());
    let mut out = Vec::new();
    for &src in srcs {
        match load_one(src, query, regex) {
            Ok(mut v) => out.append(&mut v),
            Err(e) if srcs.len() > 1 => {
                let _ = e;
            }
            Err(e) => return Err(e),
        }
    }
    // An id becomes a row field and, on resume, an argument to the harness:
    // one that is not plainly an id (see `util::safe_id`) is not listed.
    out.retain(|s| util::safe_id(&s.id));
    let now = util::now_ms();
    let key = |s: &Session| if s.last_ms > now { 0 } else { s.last_ms };
    out.sort_by(|a, b| key(b).cmp(&key(a)).then_with(|| a.id.cmp(&b.id)));
    Ok(out)
}

/// One session by id (or a unique prefix): from `src` when given, else
/// from whichever harness has it.
pub fn resolve(src: Option<Src>, id: &str, locator: Option<&str>) -> Result<Session, String> {
    let locator_path = locator
        .and_then(util::b64url_decode)
        .and_then(|b| String::from_utf8(b).ok())
        .map(PathBuf::from);
    let one = |s: Src| match s {
        Src::Claude => session::resolve(id, locator),
        Src::Codex => codex::find(id, locator_path.as_deref()),
        Src::OpenCode => opencode::find(id),
    };
    match src {
        Some(s) => one(s),
        None => Src::ALL
            .iter()
            .find_map(|&s| one(s).ok())
            .ok_or_else(|| format!("session not found: {id}")),
    }
}
