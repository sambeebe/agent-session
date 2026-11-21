//! The picker: rows into fzf, the same binary for preview and reloads, and
//! the pick resumed in-process once fzf has exited (no bash script, so it
//! runs the same from PowerShell, cmd or a POSIX shell).

use crate::sources::{self, Src};
use crate::{list, resume, util};
use std::io::Write;
use std::process::{Command, Stdio};

pub struct Keys {
    pub grep: String,
    pub dir: String,
    pub fuzzy: String,
    pub copy: String,
    /// A taller preview (the preview is under the list).
    pub bigger: String,
    /// A shorter preview.
    pub smaller: String,
}

impl Keys {
    /// Flags win, then AGENT_SESSION_BIND_* env vars, then the defaults.
    pub fn resolve(grep: Option<String>, dir: Option<String>, fuzzy: Option<String>) -> Keys {
        let pick = |flag: Option<String>, env: &str, default: &str| {
            flag.filter(|v| !v.trim().is_empty())
                .or_else(|| util::env_nonempty(env))
                .unwrap_or_else(|| default.to_string())
        };
        Keys {
            grep: pick(grep, "AGENT_SESSION_BIND_GREP", "ctrl-g"),
            dir: pick(dir, "AGENT_SESSION_BIND_DIR", "ctrl-o"),
            fuzzy: pick(fuzzy, "AGENT_SESSION_BIND_FUZZY", "ctrl-f"),
            copy: pick(None, "AGENT_SESSION_BIND_COPY", "ctrl-y"),
            bigger: pick(None, "AGENT_SESSION_BIND_BIGGER", ">"),
            smaller: pick(None, "AGENT_SESSION_BIND_SMALLER", "<"),
        }
    }
}

/// The executable as fzf's shell will read it: cmd.exe on Windows (the
/// picker hides $SHELL from fzf there), $SHELL -c elsewhere.
fn quote_exe(exe: &str) -> String {
    if cfg!(windows) {
        format!("\"{exe}\"")
    } else {
        format!("'{}'", exe.replace('\'', r"'\''"))
    }
}

/// An fzf action with an argument, in a bracket pair the argument does
/// not contain (an exe path with parentheses would end `reload(...)` early).
fn action(name: &str, arg: &str) -> String {
    for (open, close) in [('(', ')'), ('[', ']'), ('{', '}'), ('<', '>'), ('~', '~')] {
        if !arg.contains(close) {
            return format!("{name}{open}{arg}{close}");
        }
    }
    format!("{name}:{arg}")
}

fn exe() -> Result<String, String> {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .map_err(|e| format!("cannot find this executable: {e}"))
}

/// Whether agent-session can be fzf's shell (`--with-shell` splits on spaces).
fn self_shell(exe: &str) -> bool {
    !exe.chars().any(char::is_whitespace)
}

/// What starts one of our own subcommands inside an fzf command string:
/// nothing when agent-session is the shell, else the quoted exe and a space.
fn prefix(exe: &str) -> String {
    if self_shell(exe) { String::new() } else { format!("{} ", quote_exe(exe)) }
}

// ---------------------------------------------------------------- the split
// The preview sits under the list at full width, so neither is ever cramped
// side by side; the split keys set how much of the height it takes.

const SPLIT_MIN: u32 = 20;
const SPLIT_MAX: u32 = 80;
const SPLIT_DEFAULT: u32 = 50;
const SPLIT_STEP: i32 = 10;
/// The key in ui.json. Not `preview_pct`: that was a share of the width,
/// when the preview sat beside the list.
const SPLIT_KEY: &str = "preview_height_pct";

fn ui_state_file() -> Option<std::path::PathBuf> {
    util::cache_dir().map(|d| d.join("ui.json"))
}

/// The preview's share of the height, as last set with the split keys.
fn preview_pct() -> u32 {
    ui_state_file()
        .and_then(|f| std::fs::read(f).ok())
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v[SPLIT_KEY].as_u64())
        .map(|v| (v as u32).clamp(SPLIT_MIN, SPLIT_MAX))
        .unwrap_or(SPLIT_DEFAULT)
}

fn preview_window(pct: u32) -> String {
    format!("down,{pct}%,wrap")
}

/// The character a key types, when it is a plain one like `<`: such a key
/// moves the split only on an empty query, and is typed otherwise.
fn typed_char(key: &str) -> Option<char> {
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_control() && !c.is_whitespace() => Some(c),
        _ => None,
    }
}

/// `agent-session split <+N|-N> [query]` (fzf's transform runs it): makes
/// the preview N% of the height bigger or smaller, remembers that for next
/// time, and tells fzf. When the key is a plain character and the query is
/// not empty, it types that character instead, so a search can contain it.
pub fn split(delta: &str, query: Option<&str>, keys: &Keys) -> Result<(), String> {
    let delta: i32 = delta.trim().parse().map_err(|_| format!("split: not a number: {delta}"))?;
    let key = if delta > 0 { &keys.bigger } else { &keys.smaller };
    if let (Some(_), Some(c)) = (query.filter(|q| !q.is_empty()), typed_char(key)) {
        println!("{}", action("put", &c.to_string()));
        return Ok(());
    }
    let pct = (preview_pct() as i32 + delta).clamp(SPLIT_MIN as i32, SPLIT_MAX as i32) as u32;
    if let Some(f) = ui_state_file() {
        let _ = util::write_atomic(&f, serde_json::json!({ SPLIT_KEY: pct }).to_string().as_bytes());
    }
    println!("change-preview-window({})", preview_window(pct));
    Ok(())
}

// ---------------------------------------------------------------- help

pub fn keys_text(keys: &Keys) -> String {
    let Keys { grep, dir, fuzzy, copy, bigger, smaller } = keys;
    let rows = [
        ("type", "filter: created, last, msgs, harness, folder, name (fuzzy)"),
        (grep.as_str(), "grep mode: search all message text"),
        (dir.as_str(), "folder mode: match the folder only"),
        (fuzzy.as_str(), "back to fuzzy mode"),
        ("enter", "resume, in the session's own folder"),
        (copy.as_str(), "copy the session id"),
        (bigger.as_str(), "bigger preview"),
        (smaller.as_str(), "smaller preview"),
        ("shift-up/down", "scroll the preview"),
        ("up/down", "move; the preview follows"),
        ("? / f1", "these keys"),
        ("esc", "quit"),
    ];
    let mut out = String::from("\x1b[1mKEYS\x1b[0m\n\n");
    for (k, what) in rows {
        out.push_str(&format!("  \x1b[36m{}\x1b[0m {what}\n", util::pad_right(k, 15)));
    }
    out.push_str(
        "\n\x1b[2mHarness: claude, codex, opencode (--source to pick one).\n\
         ?, < and > act only while the query is empty; otherwise they are typed.\n\
         The split is remembered. Move the cursor to go back to the preview.\x1b[0m\n",
    );
    out
}

/// `?` types itself into a query, but on an empty prompt it is the help
/// key: what fzf's transform runs for it.
pub fn help_or_type(query: &str) -> Result<(), String> {
    if query.is_empty() {
        println!("{}", action("preview", &format!("{}keys", prefix(&exe()?))));
    } else {
        println!("put(?)");
    }
    Ok(())
}

// ---------------------------------------------------------------- fzf

/// The fzf arguments. Every preview, reload and copy is a command fzf hands
/// to a shell. Starting cmd.exe for each one costs about 20 ms on Windows,
/// on top of the process itself, per cursor move and per keystroke. So when
/// the exe path has no spaces (`--with-shell` splits on whitespace), agent-session
/// is itself the shell: fzf runs `agent-session __sh "<command>"` and the command
/// runs in that one process. Otherwise it falls back to cmd.exe / $SHELL.
///
/// Reloads use `reload-sync`: the old list stays up until the new one has
/// loaded, rather than blanking and redrawing on every keystroke.
pub fn fzf_args(exe: &str, keys: &Keys, srcs: &[Src]) -> Vec<String> {
    let me = prefix(exe);
    let filter = sources::filter_label(srcs);
    let list = format!("{me}list --source {filter} --color=always --column-names");
    let Keys { grep, dir, fuzzy, copy, bigger, smaller } = keys;
    let mut args: Vec<String> = Vec::new();
    if self_shell(exe) {
        args.push(format!("--with-shell={exe} __sh"));
    }
    let reload = |cmd: &str| action("reload-sync", cmd);
    let transform = |cmd: &str| action("transform", cmd);
    args.extend([
        // The whole terminal, whatever FZF_DEFAULT_OPTS says: a `--height
        // 40%` there (common on Linux) would draw the picker inline in part
        // of the screen, and `--tmux` would put it in a popup.
        "--no-height".into(),
        "--no-tmux".into(),
        "--ansi".into(),
        "--delimiter=\t".into(),
        // Shown: created, last message, messages, harness, folder, name. Each is padded
        // to a fixed width and a tab is one cell, so the columns line up.
        // The first row holds the column names: fzf keeps it as a header,
        // next to the rows whichever way the list is laid out.
        "--with-nth=4,5,6,7,8,9".into(),
        "--header-lines=1".into(),
        "--nth=1,2,3,4,5,6".into(),
        "--tabstop=1".into(),
        "--no-sort".into(),
        "--no-hscroll".into(),
        "--color=hl:-1:reverse,hl+:-1:reverse".into(),
        format!("--preview={me}preview --source {{3}} --color=always --locator {{2}} --query {{q}} {{1}}"),
        format!("--preview-window={}", preview_window(preview_pct())),
        format!("--header=[{filter}] ? keys · {grep} grep · {dir} folder · {fuzzy} fuzzy · {smaller}/{bigger} preview size · enter resume"),
        "--bind=start:unbind(change)".into(),
        format!("--bind=change:{}", reload(&format!("{list} --delay 40 --grep {{q}}"))),
        format!(
            "--bind={grep}:change-prompt(grep> )+disable-search+{}+rebind(change)",
            reload(&format!("{list} --grep {{q}}"))
        ),
        format!(
            "--bind={dir}:change-prompt(dir> )+enable-search+change-nth(5)+{}+unbind(change)",
            reload(&list)
        ),
        format!(
            "--bind={fuzzy}:change-prompt(> )+enable-search+change-nth(1,2,3,4,5,6)+{}+unbind(change)",
            reload(&list)
        ),
        format!("--bind={copy}:{}", action("execute-silent", &format!("{me}copy {{1}}"))),
        format!("--bind={bigger}:{}", transform(&format!("{me}split +{SPLIT_STEP} {{q}}"))),
        format!("--bind={smaller}:{}", transform(&format!("{me}split -{SPLIT_STEP} {{q}}"))),
        format!("--bind=?:{}", transform(&format!("{me}help-or-type {{q}}"))),
        format!("--bind=f1:{}", action("preview", &format!("{me}keys"))),
        "--bind=shift-up:preview-up,shift-down:preview-down".into(),
    ]);
    args
}

#[cfg(windows)]
fn use_cmd_for_fzf_commands(cmd: &mut Command) {
    // Only for the fallback without the self-shell: fzf runs commands
    // through $SHELL when set (Git Bash sets it), else cmd.exe; pin cmd.exe
    // so the quoting above always matches.
    cmd.env_remove("SHELL");
}
#[cfg(not(windows))]
fn use_cmd_for_fzf_commands(_: &mut Command) {}

pub fn run(exclude: Option<String>, keys: Keys, srcs: Vec<Src>) -> Result<i32, String> {
    let exe = exe()?;
    let sessions = list::load(&srcs, None, false, exclude.as_deref())?;
    let now = util::now_ms();
    let mut rows = list::column_names(true);
    rows.push('\n');
    for s in &sessions {
        rows.push_str(&list::format_line(s, now, true));
        rows.push('\n');
    }

    let mut cmd = Command::new("fzf");
    cmd.args(fzf_args(&exe, &keys, &srcs))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(x) = &exclude {
        cmd.env("AGENT_SESSION_EXCLUDE_DIR", x);
    }
    use_cmd_for_fzf_commands(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "fzf is required but not found in PATH".to_string()
        } else {
            format!("starting fzf: {e}")
        }
    })?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(rows.as_bytes());
    });
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let _ = writer.join();

    let picked = String::from_utf8_lossy(&out.stdout);
    let line = picked.lines().next().unwrap_or("").trim_end_matches('\r');
    if line.trim().is_empty() {
        return Ok(0); // Esc, Ctrl+C or nothing matched
    }
    let mut fields = line.split('\t');
    let id = fields.next().unwrap_or("").trim();
    let locator = fields.next();
    let src = fields.next().and_then(Src::parse);
    resume::run(src, id, locator)
}

/// Splits a command line the way a POSIX shell would (whitespace, '...',
/// "...", backslash escapes): what fzf builds for a shell it does not know,
/// which is how it treats `agent-session __sh`, single-quoting each placeholder.
pub fn shell_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    cur.push(q);
                }
            }
            '"' => {
                in_word = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' if matches!(chars.peek(), Some('"' | '\\' | '$' | '`')) => cur.push(chars.next().unwrap()),
                        _ => cur.push(q),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

#[cfg(windows)]
const CLIPBOARD: &[&[&str]] = &[&["clip"]];
#[cfg(target_os = "macos")]
const CLIPBOARD: &[&[&str]] = &[&["pbcopy"]];
#[cfg(all(unix, not(target_os = "macos")))]
const CLIPBOARD: &[&[&str]] = &[&["wl-copy"], &["xclip", "-selection", "clipboard"], &["xsel", "-b"]];

/// `agent-session copy <text>`: puts the text on the system clipboard.
pub fn copy(text: &str) -> Result<(), String> {
    for argv in CLIPBOARD {
        let Ok(mut child) = Command::new(argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        if child.wait().map(|s| s.success()).unwrap_or(false) {
            return Ok(());
        }
    }
    Err("no clipboard command found".into())
}

#[cfg(test)]
mod tests {
    use super::shell_words;

    #[test]
    fn splits_what_fzf_builds() {
        // fzf single-quotes each placeholder, a quote inside as '\''.
        assert_eq!(
            shell_words(r#"preview --color=always --locator 'QzpcVX' --query 'it'\''s "x" & 50%' 'abc'"#),
            ["preview", "--color=always", "--locator", "QzpcVX", "--query", r#"it's "x" & 50%"#, "abc"]
        );
        assert_eq!(shell_words("list --grep ''"), ["list", "--grep", ""]);
        assert_eq!(shell_words(r#"a "b \"c\"" d\ e"#), ["a", r#"b "c""#, "d e"]);
    }
}
