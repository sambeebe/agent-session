//! agent-session: an fzf picker for Claude Code sessions.
//!
//! A Rust port of sorafujitani/ccsession (MIT) for Claude Code, Codex and
//! OpenCode that runs natively on Windows as well as macOS and Linux.

mod codex;
mod grep;
mod list;
mod opencode;
mod picker;
mod preview;
mod resume;
mod session;
mod sources;
mod util;

use sources::Src;
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;

const USAGE: &str = "\
agent-session - fzf picker for Claude Code, Codex and OpenCode sessions

USAGE:
  agent-session [--source S] [--exclude-dir D] [--bind-grep K] [--bind-dir K] [--bind-fuzzy K]
                                                   list -> fzf -> resume
  agent-session list [--source S] [--grep Q] [--regex] [--exclude-dir D] [--json] [--limit N] [--color M]
  agent-session preview [--source S] [--query Q] [--regex] [--locator L] [--color M] <id>
  agent-session resume [--source S] [--locator L] <id|prefix>      resume in the session's folder
  agent-session resume-spec [--source S] [--locator L] <id|prefix> what resume would run, as JSON
  agent-session keys                                               the picker's keys
  agent-session copy <text>                                        put text on the clipboard

  --source: claude, codex, opencode or all (default all). A harness that is
  not set up on this machine simply has no rows.

PICKER KEYS (? or f1 in the picker shows them too):
  typing      fuzzy-filter created / last message / harness / folder / name
  ctrl-g      grep mode: every keystroke searches all message text
  ctrl-o      folder mode     ctrl-f     back to fuzzy
  <           smaller preview >          bigger preview (remembered)
              (<, > and ? act on an empty query; otherwise they are typed)
  ctrl-y      copy the id     enter      resume
  Rebind: --bind-* flags, or AGENT_SESSION_BIND_GREP / _DIR / _FUZZY / _COPY / _BIGGER / _SMALLER.

WHERE SESSIONS ARE READ FROM:
  claude    $CLAUDE_CONFIG_DIR/projects, default ~/.claude/projects
  codex     $CODEX_HOME/sessions, default ~/.codex/sessions
  opencode  $OPENCODE_DB, else the newest opencode*.db in $XDG_DATA_HOME/opencode
            or ~/.local/share/opencode (opened read-only)

ENVIRONMENT:
  AGENT_SESSION_EXCLUDE_DIR       same as --exclude-dir
  AGENT_SESSION_PREVIEW_MESSAGES  messages the preview shows from each end of a session (default 6)
  AGENT_SESSION_CACHE_DIR         cache folder (default %LOCALAPPDATA%\\agent-session or ~/.cache/agent-session)

REQUIRES: fzf 0.58+, and the harness you resume (claude / codex / opencode).
";

#[derive(Default)]
struct Args {
    values: HashMap<String, String>,
    flags: HashSet<String>,
    positional: Vec<String>,
}

impl Args {
    fn value(&self, name: &str) -> Option<String> {
        self.values.get(name).cloned()
    }
    fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }
}

/// `--name value`, `--name=value` and `--flag`. A value flag always takes
/// the next argument, so a query of "" or "--x" from fzf is still a value.
fn parse(args: &[String], value_flags: &[&str], bool_flags: &[&str]) -> Result<Args, String> {
    let mut out = Args::default();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if let Some(rest) = arg.strip_prefix("--") {
            let (name, inline) = match rest.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (rest, None),
            };
            if value_flags.contains(&name) {
                let v = match inline {
                    Some(v) => v,
                    None => {
                        i += 1;
                        args.get(i).cloned().ok_or_else(|| format!("--{name} needs a value"))?
                    }
                };
                out.values.insert(name.to_string(), v);
            } else if bool_flags.contains(&name) {
                out.flags.insert(name.to_string());
            } else {
                return Err(format!("unknown flag: --{name} (see agent-session --help)"));
            }
        } else {
            out.positional.push(arg.clone());
        }
        i += 1;
    }
    Ok(out)
}

/// always / never, else on when stdout is a terminal and NO_COLOR is unset.
fn color_on(a: &Args) -> bool {
    match a.value("color").map(|c| c.to_lowercase()).as_deref() {
        Some("always") => true,
        Some("never") => false,
        _ => !a.flag("no-color") && util::env_nonempty("NO_COLOR").is_none() && std::io::stdout().is_terminal(),
    }
}

fn one_id(a: &Args, cmd: &str) -> Result<String, String> {
    match a.positional.as_slice() {
        [id] if !id.trim().is_empty() => Ok(id.trim().to_string()),
        _ => Err(format!("usage: agent-session {cmd} [flags] <sessionId>")),
    }
}

fn source_of(a: &Args) -> Result<Option<Src>, String> {
    match a.value("source") {
        None => Ok(None),
        Some(v) if v.trim().is_empty() || v.trim().eq_ignore_ascii_case("all") => Ok(None),
        Some(v) => Src::parse(&v).map(Some).ok_or_else(|| format!("unknown --source {v}: claude, codex, opencode or all")),
    }
}

fn real_main() -> Result<i32, String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // `agent-session __sh "<command>"`: the picker makes this binary fzf's shell
    // (`--with-shell`), so a preview or reload is one process, not cmd.exe
    // starting a second one. The command is only ever this binary's own
    // subcommands; nothing else runs.
    if args.first().map(String::as_str) == Some("__sh") {
        args = args.get(1).map(|line| picker::shell_words(line)).unwrap_or_default();
        if args.is_empty() {
            return Ok(0);
        }
    }

    // Global flags come before the subcommand.
    const GLOBAL_VALUES: [&str; 5] = ["exclude-dir", "bind-grep", "bind-dir", "bind-fuzzy", "source"];
    let mut split = 0;
    while split < args.len() && args[split].starts_with("--") {
        let name = args[split].trim_start_matches("--");
        split += if GLOBAL_VALUES.contains(&name) { 2 } else { 1 };
    }
    let split = split.min(args.len());
    let global = parse(&args[..split], &GLOBAL_VALUES, &["help", "version"])?;
    if global.flag("help") {
        print!("{USAGE}");
        return Ok(0);
    }
    if global.flag("version") {
        println!("agent-session {} (Rust port)", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    let exclude = global
        .value("exclude-dir")
        .or_else(|| util::env_nonempty("AGENT_SESSION_EXCLUDE_DIR"));
    let keys = || picker::Keys::resolve(global.value("bind-grep"), global.value("bind-dir"), global.value("bind-fuzzy"));

    let rest = &args[split..];
    let Some(cmd) = rest.first() else {
        let srcs = sources::parse_filter(global.value("source").as_deref())?;
        return picker::run(exclude, keys(), srcs);
    };
    let sub = &rest[1..];
    match cmd.as_str() {
        "list" => {
            let a = parse(
                sub,
                &["grep", "exclude-dir", "limit", "color", "delay", "source"],
                &["regex", "json", "no-color", "column-names"],
            )?;
            list::run(&list::Options {
                sources: sources::parse_filter(a.value("source").or(global.value("source")).as_deref())?,
                grep: a.value("grep"),
                regex: a.flag("regex"),
                exclude: a.value("exclude-dir").or(exclude),
                json: a.flag("json"),
                limit: a.value("limit").map(|v| v.parse().map_err(|_| "--limit must be a number >= 0")).transpose()?.unwrap_or(0),
                color: color_on(&a),
                column_names: a.flag("column-names"),
                delay_ms: a.value("delay").and_then(|v| v.parse().ok()).unwrap_or(0),
            })?;
            Ok(0)
        }
        "preview" => {
            let a = parse(sub, &["query", "locator", "color", "source"], &["regex", "no-color"])?;
            let id = one_id(&a, "preview")?;
            preview::run(
                &id,
                &preview::Options {
                    source: source_of(&a)?,
                    query: a.value("query"),
                    regex: a.flag("regex"),
                    locator: a.value("locator"),
                    color: color_on(&a),
                },
            )?;
            Ok(0)
        }
        "resume" => {
            let a = parse(sub, &["locator", "source"], &[])?;
            resume::run(source_of(&a)?, &one_id(&a, "resume")?, a.value("locator").as_deref())
        }
        "resume-spec" => {
            let a = parse(sub, &["locator", "source"], &[])?;
            resume::spec(source_of(&a)?, &one_id(&a, "resume-spec")?, a.value("locator").as_deref())?;
            Ok(0)
        }
        "keys" => {
            print!("{}", picker::keys_text(&keys()));
            Ok(0)
        }
        "copy" => {
            picker::copy(&sub.join(" "))?;
            Ok(0)
        }
        // Run by the picker's key bindings, not meant to be typed.
        "split" => {
            picker::split(sub.first().map(String::as_str).unwrap_or("0"), sub.get(1).map(String::as_str), &keys())?;
            Ok(0)
        }
        "help-or-type" => {
            picker::help_or_type(sub.first().map(String::as_str).unwrap_or(""))?;
            Ok(0)
        }
        // Prints the picker's fzf arguments, for checking them.
        "fzf-args" => {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let srcs = sources::parse_filter(global.value("source").as_deref())?;
            println!("{}", serde_json::to_string(&picker::fzf_args(&exe.to_string_lossy(), &keys(), &srcs)).unwrap());
            Ok(0)
        }
        "help" => {
            print!("{USAGE}");
            Ok(0)
        }
        other => Err(format!("unknown command: {other} (see agent-session --help)")),
    }
}

fn main() {
    let code = match real_main() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("agent-session: {e}");
            1
        }
    };
    std::process::exit(code);
}
