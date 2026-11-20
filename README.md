# agent-session

> **Credit:** this is a port of **[ccsession](https://github.com/sorafujitani/ccsession) by [fujitani sora](https://github.com/sorafujitani)**. The idea, the picker's design, the command line, and the knowledge of each tool's session format are all from the original project. This repository only re-implements it in Rust so it runs on Windows. If you are on macOS or Linux, use the original. It is the maintained project and supports more tools.

An fzf picker for **Claude Code, Codex and OpenCode** sessions: every project's sessions from all three in one list, with a harness column, a live preview and full-text grep. Enter resumes a session in its own folder with its own tool. It's a Rust port of [sorafujitani/ccsession](https://github.com/sorafujitani/ccsession) (MIT) that runs natively on Windows. The original has no Windows build, and its resume step uses `syscall.Exec`, which Windows doesn't have.

| Harness | Read from | Resumed with |
|---|---|---|
| claude | `$CLAUDE_CONFIG_DIR/projects`, default `~/.claude/projects` | `claude --resume <id>` |
| codex | `$CODEX_HOME/sessions`, default `~/.codex/sessions` | `codex resume <id>` |
| opencode | `$OPENCODE_DB`, else the newest `opencode*.db` in `~/.local/share/opencode`, opened read-only | `opencode --session <id>` |

`--source claude|codex|opencode|all` (default `all`) picks which ones to list. A harness that isn't set up on the machine has no rows.

```
agent-session
```

The list has a row per session: when it was created, when the last message was sent, the harness, the folder it ran in, and its name. The name is the one you gave it with `/rename` in Claude Code or OpenCode, else the tool's own title, else your last message. The preview sits under the list at full width. It shows the session's id, folder and resume command, then its first and last few messages, so you can see what it started with and where it ended.

| Key | |
|---|---|
| type | fuzzy-filter on created, last message, harness, folder and name |
| `ctrl-g` | grep mode: every keystroke searches all user and assistant text |
| `ctrl-o` | folder mode: match the folder column only |
| `ctrl-f` | back to fuzzy mode |
| `<` / `>` | make the preview smaller / bigger (remembered between runs) |
| `shift-up` / `shift-down` | scroll the preview |
| `?` / `f1` | show the keys in the preview pane |
| `ctrl-y` | copy the session id |
| `enter` | resume: `cd` to the session's folder, then run its harness's resume command |

`?`, `<` and `>` act only while the query is empty. In a query they are typed like any other character.

`AGENT_SESSION_PREVIEW_MESSAGES` sets how many messages the preview shows from each end (default 6).

Other commands: `agent-session list [--grep Q] [--json]`, `agent-session preview <id>`, `agent-session resume <id-or-prefix>`, `agent-session resume-spec <id>`. `agent-session --help` has the rest.

## Changes from upstream

- **Windows:** fzf is started directly, with no bash script. Its preview and reload commands always go through `cmd.exe`, even under Git Bash, so the quoting always matches. Resume runs `claude` as a child process and ignores Ctrl+C while it waits, since Windows has no `exec`.
- **Speed:** agent-session is fzf's shell (`--with-shell=agent-session __sh`), so a preview or reload is one process rather than `cmd.exe` plus a process. That saves about 20 ms per cursor move and keystroke. Reloads use `reload-sync`, so the list doesn't blank and redraw on each keystroke. Previews are cached per transcript, append-aware, so a cursor move back onto a 150 MB session takes about 10 ms.
- **Long sessions are listed.** Upstream reads only the last 64 KiB of each transcript and drops a session when that tail has no user message, which is common after big tool output or images. Here the tail grows until it finds one.
- **`/rename` names** from Claude Code (`custom-title`) and OpenCode (the session title) are used first.
- Rows show when a session was created as well as its last message. `ctrl-y` copies the session id, and the preview shows the exact resume command and the session's first messages as well as its last.
- **Grep ignores `<system-reminder>` blocks**, so CLAUDE.md and memory text copied into every session doesn't match everything. Its text cache is append-aware: a session that grew gets only its new lines read.
- `resume` and `preview` take a unique id prefix (`agent-session resume 7b6ae3ee`).
- Left out: upstream's other sources (Pi, Grok, Kiro, Cortex, oh-my-pi) and the TOML config file. Keys come from `--bind-*` flags or `AGENT_SESSION_BIND_*` env vars.

## Build

```
cargo install --path .
```

It needs a C compiler, because SQLite is compiled in for the OpenCode reader. On Windows without Visual Studio Build Tools, rustup's GNU toolchain with a MinGW gcc on PATH works:

```
cargo +stable-x86_64-pc-windows-gnu install --path .
```

At run time it needs `fzf` 0.58+ and whichever of `claude`, `codex`, `opencode` you resume.

## Platforms

Developed on Windows 11. It also runs on Linux and macOS: CI builds it on all three and runs an end-to-end test, `tests/e2e.sh`. On Linux and macOS that test drives the real picker in a terminal with made-up session data: the list, the preview, grep and folder modes, the split and copy keys, and Enter resuming each tool in the session's own folder. It has had little everyday use on those two systems, where the original ccsession is the maintained choice.

To run the test yourself (it needs `python3`, and `fzf` and `tmux` for the picker part):

```
cargo build --release && tests/e2e.sh
```

## Security and privacy

- **Read-only.** It never writes to any tool's session data. The OpenCode database is opened read-only.
- **No network.** It makes no network connections.
- **Cache.** To be fast it keeps a cache in `%LOCALAPPDATA%\agent-session` (`~/.cache/agent-session` elsewhere, or `$AGENT_SESSION_CACHE_DIR`). The cache holds session titles, folders and the text of your conversations, so treat it like the transcripts themselves. On Unix it is created owner-only (0700 folders, 0600 files). Deleting the folder is always safe.
- **Transcripts are untrusted input.** They contain whatever a tool printed or a web page said. Control characters are stripped before anything is shown, so a transcript cannot send escape sequences to your terminal. A session id that is not a plain id is not listed or resumed, so a planted file name cannot become a command-line option to the tool.
- **Resume runs a program.** Enter runs `claude`, `codex` or `opencode` as found on your `PATH`, in the folder the session recorded. On Windows the current folder and relative `PATH` entries are never searched.
