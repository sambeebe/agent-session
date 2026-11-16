//! Resuming: start the session's own harness (`claude --resume <id>`,
//! `codex resume <id>`, `opencode --session <id>`) in its original cwd.
//!
//! Unix replaces this process with it (exec), as upstream does. Windows
//! has no exec, so it runs as a child with the console handed over and
//! this process waits, ignoring Ctrl+C so the first Ctrl+C reaches the
//! harness alone instead of also killing the parent out from under it.

use crate::session::Session;
use crate::sources::{self, Src};
use crate::util::{self, DIM, RESET};
use std::io::IsTerminal;
use std::process::Command;

fn checked(src: Option<Src>, id: &str, locator: Option<&str>) -> Result<Session, String> {
    let s = sources::resolve(src, id, locator)?;
    if !util::safe_id(&s.id) {
        return Err("refusing to resume: the session's id is not a plain id".to_string());
    }
    if s.cwd_unknown || s.cwd.is_empty() {
        return Err(format!(
            "cwd unknown for session {id}; refusing to resume (the session recorded no folder)"
        ));
    }
    if !s.cwd_exists {
        return Err(format!("original cwd is gone: {}", s.cwd));
    }
    Ok(s)
}

/// `agent-session resume-spec`: what resume would run, as JSON, running nothing.
pub fn spec(src: Option<Src>, id: &str, locator: Option<&str>) -> Result<(), String> {
    let s = sources::resolve(src, id, locator)?;
    let argv = Src::of(&s).resume_argv(&s.id);
    let v = serde_json::json!({
        "source": s.source,
        "id": s.id,
        "locator": crate::session::locator_for(&s),
        "cwd": s.cwd,
        "cwd_exists": s.cwd_exists,
        "cwd_unknown": s.cwd_unknown,
        "bin": argv[0],
        "args": argv,
    });
    println!("{v}");
    Ok(())
}

pub fn run(src: Option<Src>, id: &str, locator: Option<&str>) -> Result<i32, String> {
    let s = checked(src, id, locator)?;
    let argv = Src::of(&s).resume_argv(&s.id);
    if std::io::stderr().is_terminal() {
        eprintln!("{DIM}→ {}\n  cd \"{}\" && {}{RESET}", util::printable(&s.label), util::printable(&s.cwd), argv.join(" "));
    }
    launch(&s, &argv)
}

fn start_error(bin: &str, e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        format!("{bin} not found in PATH")
    } else {
        format!("starting {bin}: {e}")
    }
}

/// `name` on PATH, trying PATHEXT's extensions in order as cmd.exe does:
/// npm installs CLIs as `.cmd` shims, which `Command::new("codex")` (it
/// adds only `.exe`) would not find. Unlike cmd.exe it never looks in the
/// current folder, and an empty or relative PATH entry (a stray `;;`, a
/// `.`) is skipped: either would let a `claude.cmd` sitting in whatever
/// folder agent-session was started from run in place of the real one.
#[cfg(windows)]
fn find_program(name: &str) -> Option<std::path::PathBuf> {
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|e| !e.is_empty())
        .map(str::to_string)
        .collect();
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if !dir.is_absolute() {
            continue;
        }
        for ext in &exts {
            let p = dir.join(format!("{name}{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(windows)]
fn launch(s: &Session, argv: &[String]) -> Result<i32, String> {
    let bin = &argv[0];
    let program = find_program(bin).ok_or_else(|| format!("{bin} not found in PATH"))?;
    ignore_ctrl_c();
    let status = Command::new(program)
        .args(&argv[1..])
        .current_dir(&s.cwd)
        .status()
        .map_err(|e| start_error(bin, e))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(unix)]
fn launch(s: &Session, argv: &[String]) -> Result<i32, String> {
    use std::os::unix::process::CommandExt;
    std::env::set_current_dir(&s.cwd).map_err(|e| format!("chdir to {}: {e}", s.cwd))?;
    let err = Command::new(&argv[0]).args(&argv[1..]).exec();
    Err(start_error(&argv[0], err))
}

/// A handler that swallows Ctrl+C and Ctrl+Break for this process only. A
/// handler function, unlike `SetConsoleCtrlHandler(NULL, TRUE)`, is not
/// inherited, so the harness still gets Ctrl+C as usual.
#[cfg(windows)]
fn ignore_ctrl_c() {
    type Handler = unsafe extern "system" fn(u32) -> i32;
    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<Handler>, add: i32) -> i32;
    }
    unsafe extern "system" fn swallow(ctrl_type: u32) -> i32 {
        // CTRL_C_EVENT = 0, CTRL_BREAK_EVENT = 1; anything else (close,
        // logoff, shutdown) keeps its default.
        (ctrl_type <= 1) as i32
    }
    unsafe {
        SetConsoleCtrlHandler(Some(swallow), 1);
    }
}
