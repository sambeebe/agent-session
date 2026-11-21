//! `agent-session list`: one TSV row per session, the rows fzf shows.
//!
//! Columns: id, locator, source | created, last message, messages, harness,
//! folder, name. fzf shows the last six (`--with-nth=4..9`); the id stays hidden
//! (ctrl-y copies it, the preview shows it). Each shown column is padded
//! to a fixed width and fzf draws a tab as one cell (`--tabstop=1`), so the
//! columns line up whatever a folder name or an age's length.

use crate::session::{self, Session};
use crate::sources::{self, Src};
use crate::util::{self, DIM, RESET, YELLOW};
use std::io::{self, BufWriter, Write};

pub struct Options {
    pub sources: Vec<Src>,
    pub grep: Option<String>,
    pub regex: bool,
    pub exclude: Option<String>,
    pub json: bool,
    pub limit: usize,
    pub color: bool,
    /// Start with a row of column names (the picker's header line).
    pub column_names: bool,
    /// Wait this long first: fzf kills a reload when the next keystroke
    /// starts another, so a short wait keeps typing from scanning per key.
    pub delay_ms: u64,
}

const AGE_W: usize = 10;
const MSGS_W: usize = 5;
const HARNESS_W: usize = 8;
const DIR_W: usize = 20;

const FOLDER: &str = "\x1b[36m";

fn harness_color(src: Src) -> &'static str {
    match src {
        Src::Claude => "\x1b[38;5;209m",
        Src::Codex => "\x1b[38;5;114m",
        Src::OpenCode => "\x1b[38;5;141m",
    }
}

/// `text` cut and padded to exactly `w` cells.
fn fit(text: &str, w: usize) -> String {
    util::pad_right(&util::truncate(text, w), w)
}

/// A row of column names, shaped like a session row (with empty id,
/// locator and source) so fzf's `--header-lines=1` lines it up with them.
pub fn column_names(color: bool) -> String {
    let (dim, reset) = if color { (DIM, RESET) } else { ("", "") };
    let names = [
        fit("created", AGE_W),
        fit("last msg", AGE_W),
        format!("{:>MSGS_W$}", "msgs"),
        fit("harness", HARNESS_W),
        fit("folder", DIR_W),
        "name".into(),
    ];
    let shown: Vec<String> = names.iter().map(|n| format!("{dim}{n}{reset}")).collect();
    format!("\t\t\t{}", shown.join("\t"))
}

pub fn load(srcs: &[Src], grep: Option<&str>, regex: bool, exclude: Option<&str>) -> Result<Vec<Session>, String> {
    let mut sessions = sources::load(srcs, grep, regex)?;
    if let Some(x) = exclude.map(str::trim).filter(|x| !x.is_empty()) {
        sessions = session::filter_out_by_dir(sessions, x);
    }
    Ok(sessions)
}

pub fn format_line(s: &Session, now: i64, color: bool) -> String {
    let src = Src::of(s);
    let created = fit(&util::relative(s.created_ms, now), AGE_W);
    let last = fit(&util::relative(s.last_ms, now), AGE_W);
    // Right-aligned so the digits line up; 99999+ would not fit, and no
    // session gets near it.
    let msgs = format!("{:>MSGS_W$}", s.messages.min(99_999));
    let harness = fit(src.name(), HARNESS_W);
    // A folder name comes out of the transcript: a tab in it would shift
    // the row's fields and an escape sequence would reach fzf's --ansi.
    let folder_name = util::printable(&s.cwd_basename);
    let dir = fit(if folder_name.trim().is_empty() { "(unknown)" } else { &folder_name }, DIR_W);
    // [cwd?]: never knew the cwd. [gone]: the recorded cwd no longer exists.
    let marker = if s.cwd_unknown {
        "[cwd?] "
    } else if !s.cwd_exists {
        "[gone] "
    } else {
        ""
    };
    let healthy = marker.is_empty();
    let shown = if color {
        let folder = if healthy { FOLDER } else { YELLOW };
        let marker = if healthy { String::new() } else { format!("{YELLOW}{marker}{RESET}") };
        format!(
            "{DIM}{created}{RESET}\t{last}\t{DIM}{msgs}{RESET}\t{}{harness}{RESET}\t{folder}{dir}{RESET}\t{marker}{}",
            harness_color(src),
            s.label
        )
    } else {
        format!("{created}\t{last}\t{msgs}\t{harness}\t{dir}\t{marker}{}", s.label)
    };
    format!("{}\t{}\t{}\t{shown}", s.id, session::locator_for(s), src.name())
}

pub fn run(opts: &Options) -> Result<(), String> {
    if opts.delay_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(opts.delay_ms));
    }
    let mut sessions = load(&opts.sources, opts.grep.as_deref(), opts.regex, opts.exclude.as_deref())?;
    if opts.limit > 0 {
        sessions.truncate(opts.limit);
    }
    let stdout = io::stdout();
    let mut w = BufWriter::new(stdout.lock());
    // A write error is fzf closing the pipe (a newer reload took over): not ours to report.
    if opts.json {
        let rows: Vec<serde_json::Value> = sessions
            .iter()
            .map(|s| {
                serde_json::json!({
                    "source": s.source,
                    "id": s.id,
                    "locator": session::locator_for(s),
                    "cwd": s.cwd,
                    "cwd_basename": s.cwd_basename,
                    "label": s.label,
                    "messages": s.messages,
                    "created_epoch": s.created_ms / 1000,
                    "created": util::rfc3339(s.created_ms),
                    "last_epoch": s.last_ms / 1000,
                    "last_activity": util::rfc3339(s.last_ms),
                    "cwd_exists": s.cwd_exists,
                    "cwd_unknown": s.cwd_unknown,
                })
            })
            .collect();
        let _ = serde_json::to_writer(&mut w, &rows);
        let _ = writeln!(w);
    } else {
        let now = util::now_ms();
        if opts.column_names && writeln!(w, "{}", column_names(opts.color)).is_err() {
            return Ok(());
        }
        for s in &sessions {
            if writeln!(w, "{}", format_line(s, now, opts.color)).is_err() {
                break;
            }
        }
    }
    let _ = w.flush();
    Ok(())
}
