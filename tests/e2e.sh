#!/usr/bin/env bash
# End-to-end test: runs the built binary against fake Claude Code, Codex and
# OpenCode data, with stand-in `claude`, `codex` and `opencode` commands that
# only record how they were started.
#
#   tests/e2e.sh [path/to/agent-session]
#
# On Linux and macOS it also drives the real picker inside tmux: fzf with the
# picker's own arguments, keys sent as a person would press them. That part
# needs fzf and tmux; without them it is skipped, or fails when
# E2E_REQUIRE_PICKER=1. On Windows (Git Bash) only the commands are checked.
#
# It reads and writes nothing outside a temporary folder.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
bin=${1:-$here/../target/release/agent-session}

windows=0
case "$(uname -s)" in MINGW* | MSYS* | CYGWIN*) windows=1 ;; esac

if [ "$windows" = 1 ]; then
  case "$bin" in *.exe) ;; *) bin="$bin.exe" ;; esac
  py=$(command -v python || command -v python3)
else
  py=$(command -v python3 || command -v python)
fi
[ -f "$bin" ] || { echo "no binary at $bin (build it first: cargo build --release)" >&2; exit 2; }
bin="$(cd "$(dirname "$bin")" && pwd -P)/$(basename "$bin")"

W=$(mktemp -d)
W=$(cd "$W" && pwd -P)
if [ "$windows" = 1 ]; then
  W=$(cygpath -m "$W")
  bin=$(cygpath -m "$bin")
fi
sock="agent-session-e2e-$$"
cleanup() {
  tmux -L "$sock" kill-server 2>/dev/null || true
  rm -rf "$W"
}
trap cleanup EXIT

proj="$W/proj"
mkdir -p "$proj" "$W/bin"
"$py" "$here/make_fixtures.py" "$W" "$proj"

export CLAUDE_CONFIG_DIR="$W/claude"
export CODEX_HOME="$W/codex-home"
export OPENCODE_DB="$W/opencode.db"
export AGENT_SESSION_CACHE_DIR="$W/cache"
unset AGENT_SESSION_EXCLUDE_DIR AGENT_SESSION_PREVIEW_MESSAGES XDG_DATA_HOME XDG_CACHE_HOME
unset AGENT_SESSION_BIND_GREP AGENT_SESSION_BIND_DIR AGENT_SESSION_BIND_FUZZY
unset AGENT_SESSION_BIND_COPY AGENT_SESSION_BIND_WIDEN AGENT_SESSION_BIND_NARROW

CLAUDE_ID=aaaaaaaa-1111-4222-8333-444455556666
HOSTILE_ID=11111111-2222-4333-8444-555555555555
CODEX_A=0199aaaa-1111-7222-8333-444455556666
CODEX_B=0199bbbb-1111-7222-8333-444455556666
OC_ID=ses_7Hq2mKpLx9
OC_GONE=ses_Ab3dEf9GhJ

checks=0
ok() {
  checks=$((checks + 1))
  echo "ok    $1"
}
fail() {
  echo "FAIL  $1" >&2
  shift
  for extra in "$@"; do printf '%s\n' "$extra" >&2; done
  exit 1
}
# contains <haystack> <needle> <what>
contains() {
  case "$1" in *"$2"*) ok "$3" ;; *) fail "$3" "wanted: $2" "got:" "$1" ;; esac
}
# lacks <haystack> <needle> <what>
lacks() {
  case "$1" in *"$2"*) fail "$3" "did not want: $2" "got:" "$1" ;; *) ok "$3" ;; esac
}
# equals <got> <want> <what>
equals() {
  [ "$1" = "$2" ] && ok "$3" || fail "$3" "wanted: $2" "got:    $1"
}

# ------------------------------------------------------------------ commands

out=$("$bin" --version)
contains "$out" "agent-session" "--version runs"

rows=$("$bin" list --color=never)
ids=$(printf '%s\n' "$rows" | cut -f1 | tr '\n' ' ')
equals "$ids" "$CLAUDE_ID $OC_ID $CODEX_A $CODEX_B $OC_GONE $HOSTILE_ID " \
  "list: all three harnesses, newest first, one row per Codex session"
lacks "$rows" "dangerously" "list: a file named like an option is not a session"
lacks "$rows" "subagent task" "list: OpenCode child sessions are left out"
lacks "$rows" "archived one" "list: OpenCode archived sessions are left out"
contains "$rows" "Fix the login redirect" "list: Claude title"
contains "$rows" "now cap retries at five" "list: Codex label is the last request"
contains "$rows" "why is the zebra test flaky" "list: Codex request wrapper is removed"
contains "$rows" "[gone] Benchmark the tokenizer" "list: a session whose folder is gone is marked"
harnesses=$(printf '%s\n' "$rows" | cut -f3 | sort | uniq -c | tr -s ' \n' ' ')
equals "$harnesses" " 2 claude 2 codex 2 opencode " "list: harness column"

colored=$("$bin" list --color=always)
# Whatever the transcripts held, a row carries only this program's own
# colour codes: ESC [ digits m. No other escape sequence and no BEL.
stripped=$(printf '%s' "$colored" | sed $'s/\x1b\\[[0-9;]*m//g')
if printf '%s' "$stripped" | LC_ALL=C grep -q $'[\x01-\x08\x0b-\x1f\x7f]'; then
  fail "list: control characters from a transcript reached the output"
fi
ok "list: escape sequences in transcripts are removed"

json=$("$bin" list --json)
n=$(printf '%s' "$json" | "$py" -c 'import json,sys; print(len(json.load(sys.stdin)))')
equals "$n" 6 "list --json parses"

only=$("$bin" list --color=never --source codex | cut -f3 | sort -u)
equals "$only" codex "list --source codex"

grep_ids() { "$bin" list --color=never --grep "$1" | cut -f1 | tr '\n' ' '; }
equals "$(grep_ids 'duplicate redirect')" "$CLAUDE_ID " "grep: Claude message text"
equals "$(grep_ids backoff)" "$CODEX_A " "grep: Codex message text"
codex_row=$(printf '%s\n' "$rows" | awk -F '\t' -v id="$CODEX_A" '$1 == id')
equals "$("$bin" list --color=never --grep backoff)" "$codex_row" \
  "grep: an older Codex rollout match keeps the newest session row"
equals "$("$bin" list --color=never --grep 'add retry')" "$codex_row" \
  "grep: matching both Codex rollouts still returns one newest row"
equals "$("$bin" list --color=never --grep 'Capped at 5')" "$codex_row" \
  "grep: a newest Codex rollout match keeps the newest session row"
equals "$(grep_ids 'explicit LOD')" "$OC_ID " "grep: OpenCode message text"
equals "$(grep_ids 'no such text anywhere')" "" "grep: no match, no rows"

out=$("$bin" preview --color=never --source claude "$CLAUDE_ID")
contains "$out" "$CLAUDE_ID" "preview: session id"
contains "$out" "claude --resume $CLAUDE_ID" "preview: resume command"
contains "$out" "Removed the duplicate redirect" "preview: Claude messages"
out=$("$bin" preview --color=never 0199aaaa)
contains "$out" "Capped at 5 retries" "preview: Codex messages, by id prefix"
lacks "$out" "secret rule" "preview: Codex's AGENTS.md turn is hidden"
out=$("$bin" preview --color=never "$OC_ID")
contains "$out" "Bloom pass ported" "preview: OpenCode messages"
out=$("$bin" preview --color=never "$HOSTILE_ID")
if printf '%s' "$out" | LC_ALL=C grep -q $'[\x01-\x08\x0b-\x1f\x7f]'; then
  fail "preview: control characters from a transcript reached the output"
fi
ok "preview: escape sequences in transcripts are removed"

spec=$("$bin" resume-spec "$OC_ID")
contains "$spec" '"args":["opencode","--session","ses_7Hq2mKpLx9"]' "resume-spec: OpenCode command"
spec=$("$bin" resume-spec "$CODEX_A")
contains "$spec" '"args":["codex","resume","0199aaaa-1111-7222-8333-444455556666"]' "resume-spec: Codex command"

if out=$("$bin" resume "$OC_GONE" 2>&1); then
  fail "resume: a session whose folder is gone must be refused" "$out"
fi
contains "$out" "gone" "resume: a session whose folder is gone is refused"

# ------------------------------------------------------------------ renaming
# Claude Code's /rename appends a custom-title row, and keeps re-appending
# it as the session goes on; the newest one is the name. OpenCode's
# /rename (or ctrl+r in its session list) sets the session's title. Both
# are undone afterwards, so the checks below see the original fixtures.

claude_file=$(find "$W/claude/projects" -name "$CLAUDE_ID.jsonl")
cp -p "$claude_file" "$W/claude-before-rename"
cp -p "$OPENCODE_DB" "$W/opencode-before-rename"
"$py" - "$claude_file" "$CLAUDE_ID" "$OPENCODE_DB" "$OC_ID" << 'EOF'
import json, sqlite3, sys
path, sid, db, oc_id = sys.argv[1:]
with open(path, 'a', encoding='utf-8') as f:
    for name in ('First name', 'Login bug, renamed'):
        f.write(json.dumps({'type': 'custom-title', 'customTitle': name, 'sessionId': sid}, separators=(',', ':')) + '\n')
c = sqlite3.connect(db)
c.execute("UPDATE session SET title = 'WGSL port, renamed' WHERE id = ?", (oc_id,))
c.commit()
EOF
label_of() { "$bin" list --color=never | awk -F '\t' -v id="$1" '$1 == id { print $NF }' | sed 's/ *$//'; }
equals "$(label_of "$CLAUDE_ID")" "Login bug, renamed" "rename: a Claude Code /rename is the title, the newest one"
equals "$(label_of "$OC_ID")" "WGSL port, renamed" "rename: an OpenCode /rename is the title"
equals "$("$bin" list --color=never --grep 'renamed' | cut -f1 | sort | tr '\n' ' ')" "$CLAUDE_ID $OC_ID " \
  "rename: grep finds sessions by their new names"
cp -p "$W/claude-before-rename" "$claude_file"
cp -p "$W/opencode-before-rename" "$OPENCODE_DB"
equals "$(label_of "$CLAUDE_ID")" "Fix the login redirect" "rename: undone for the checks that follow"

if [ "$windows" = 1 ]; then
  echo
  echo "$checks checks passed (Windows: commands only)"
  exit 0
fi

# ------------------------------------------------------------------ resume
# Stand-ins for the three tools: each appends "name|folder|arguments".

for tool in claude codex opencode; do
  cat > "$W/bin/$tool" << EOF
#!/bin/sh
printf '%s|%s|%s\n' "$tool" "\$(pwd -P)" "\$*" >> "$W/resumed"
EOF
  chmod +x "$W/bin/$tool"
done
# The clipboard commands the copy key tries, as stand-ins too.
for tool in wl-copy pbcopy; do
  printf '#!/bin/sh\ncat > "%s/clip"\n' "$W" > "$W/bin/$tool"
  chmod +x "$W/bin/$tool"
done
export PATH="$W/bin:$PATH"

resumed() { cat "$W/resumed" 2>/dev/null || true; }
reset_resumed() { rm -f "$W/resumed"; }

(cd / && "$bin" resume aaaaaaaa 2>/dev/null)
equals "$(resumed)" "claude|$proj|--resume $CLAUDE_ID" "resume: Claude, in the session's folder, by id prefix"
reset_resumed
(cd / && "$bin" resume "$CODEX_A" 2>/dev/null)
equals "$(resumed)" "codex|$proj|resume $CODEX_A" "resume: Codex"
reset_resumed
(cd / && "$bin" resume "$OC_ID" 2>/dev/null)
equals "$(resumed)" "opencode|$proj|--session $OC_ID" "resume: OpenCode"
reset_resumed

# ------------------------------------------------------------------ cache

loose=$(find "$W/cache" \( -type d ! -perm 700 \) -o \( -type f ! -perm 600 \))
equals "$loose" "" "cache: folders are 0700 and files 0600"

# ------------------------------------------------------------------ defaults
# No environment variables: everything is found under the home folder.

home="$W/home"
mkdir -p "$home/.local/share/opencode"
cp -Rp "$W/claude" "$home/.claude"
cp -Rp "$W/codex-home" "$home/.codex"
cp -p "$W/opencode.db" "$home/.local/share/opencode/opencode.db"
ids=$(env -u CLAUDE_CONFIG_DIR -u CODEX_HOME -u OPENCODE_DB -u AGENT_SESSION_CACHE_DIR HOME="$home" \
  "$bin" list --color=never | cut -f1 | tr '\n' ' ')
equals "$ids" "$CLAUDE_ID $OC_ID $CODEX_A $CODEX_B $OC_GONE $HOSTILE_ID " \
  "defaults: ~/.claude, ~/.codex and ~/.local/share/opencode are found"
[ -f "$home/.cache/agent-session/scan-claude.json" ] || fail "defaults: cache in ~/.cache/agent-session"
ok "defaults: cache in ~/.cache/agent-session"

# ------------------------------------------------------------------ picker

if ! command -v fzf > /dev/null || ! command -v tmux > /dev/null; then
  if [ "${E2E_REQUIRE_PICKER:-0}" = 1 ]; then
    fail "the picker test needs fzf and tmux on PATH"
  fi
  echo
  echo "$checks checks passed; picker not tested (needs fzf and tmux)"
  exit 0
fi
echo "      fzf $(fzf --version), $(tmux -V)"

tm() { tmux -L "$sock" -f /dev/null "$@"; }
screen() { tm capture-pane -p -t picker 2>/dev/null || true; }
# open_picker [arguments]: the picker in a terminal of $COLS x $ROWS,
# 240 x 40 unless set.
open_picker() {
  tm kill-server 2>/dev/null || true
  rm -f "$W/exit"
  reset_resumed
  # The sleep keeps the terminal up after the picker ends, so what it
  # printed on the way out can still be read.
  tm new-session -d -s picker -x "${COLS:-240}" -y "${ROWS:-40}" \
    "'$bin' $*; echo \$? > '$W/exit'; sleep 120"
}
# line_of <text>: the screen line it is first on, counting from 1.
line_of() { screen | grep -n -F -m1 -- "$1" | cut -d: -f1; }
press() { tm send-keys -t picker "$@"; }
type_text() { tm send-keys -t picker -l "$1"; }
# until_true <seconds> <command...>
until_true() {
  local tries=$(($1 * 10))
  shift
  while [ "$tries" -gt 0 ]; do
    if "$@"; then return 0; fi
    sleep 0.1
    tries=$((tries - 1))
  done
  return 1
}
on_screen() { case "$(screen)" in *"$1"*) return 0 ;; *) return 1 ;; esac; }
off_screen() { ! on_screen "$1"; }
picker_failed() {
  fail "$1" "--- screen:" "$(screen)"
}
# see <text> <what> / gone <text> <what>: wait for it to show or to go.
see() { until_true 15 on_screen "$1" && ok "$2" || picker_failed "$2 (waited for: $1)"; }
gone() { until_true 15 off_screen "$1" && ok "$2" || picker_failed "$2 (still shown: $1)"; }
file_has() { [ -f "$1" ] && [ "$(cat "$1")" = "$2" ]; }
# closed <exit code> <what>: the picker has ended with this exit code.
closed() {
  until_true 15 test -s "$W/exit" || picker_failed "$2 (the picker did not exit)"
  equals "$(cat "$W/exit")" "$1" "$2"
}

open_picker
see "login redirect" "picker: opens and lists sessions"
see "session : $CLAUDE_ID" "picker: preview of the first row"
s=$(screen)
contains "$s" "Port the shader graph to WGSL" "picker: OpenCode row"
contains "$s" "now cap retries at five" "picker: Codex row"
contains "$s" "Removed the duplicate redirect" "picker: preview shows messages"
contains "$s" "? keys · ctrl-g grep" "picker: header"

# The list is drawn bottom-up, newest at the bottom: Up goes to the next row.
press Up
see "Bloom pass ported" "picker: the preview follows the cursor"
press Down
see "Removed the duplicate redirect" "picker: and back"

type_text "?"
see "KEYS" "picker: ? shows the keys"
press Up
press Down
see "Removed the duplicate redirect" "picker: moving the cursor brings the preview back"

press M-Left
until_true 15 file_has "$W/cache/ui.json" '{"preview_pct":70}' || picker_failed "picker: alt-left widens the preview"
ok "picker: alt-left widens the preview"
press M-Right
until_true 15 file_has "$W/cache/ui.json" '{"preview_pct":60}' || picker_failed "picker: alt-right narrows it"
ok "picker: alt-right narrows it"

press C-y
until_true 15 file_has "$W/clip" "$CLAUDE_ID" || picker_failed "picker: ctrl-y copies the session id"
ok "picker: ctrl-y copies the session id"

press C-g
see "grep>" "picker: ctrl-g switches to grep mode"
type_text "backoff"
gone "login redirect" "picker: grep hides sessions without the text"
see "now cap retries at five" "picker: grep keeps the session with the text"
see "session : $CODEX_A" "picker: grep result is previewed"
see "Capped at 5 retries" "picker: grep keeps the newest Codex preview"

press C-u
press C-f
see "login redirect" "picker: ctrl-f goes back to the full list"

press C-o
see "dir>" "picker: ctrl-o switches to folder mode"
type_text "proj"
gone "Benchmark the tokenizer" "picker: folder mode matches the folder only"
see "login redirect" "picker: folder mode keeps matching folders"

press C-u
press C-f
see "Benchmark the tokenizer" "picker: back to fuzzy mode"
type_text "login"
gone "WGSL" "picker: typing filters the list"
press Enter
closed 0 "picker: enter on a Claude session exits cleanly"
equals "$(resumed)" "claude|$proj|--resume $CLAUDE_ID" "picker: enter resumes Claude in the session's folder"

open_picker --source codex
see "now cap retries at five" "picker --source codex: opens"
lacks "$(screen)" "login redirect" "picker --source codex: only Codex sessions"
press Enter
closed 0 "picker: enter on a Codex session exits cleanly"
equals "$(resumed)" "codex|$proj|resume $CODEX_A" "picker: enter resumes Codex"

open_picker
see "login redirect" "picker: opens again"
type_text "WGSL"
gone "login redirect" "picker: filtered to the OpenCode session"
press Enter
closed 0 "picker: enter on an OpenCode session exits cleanly"
equals "$(resumed)" "opencode|$proj|--session $OC_ID" "picker: enter resumes OpenCode"

open_picker
see "login redirect" "picker: opens again"
type_text "tokenizer"
gone "login redirect" "picker: filtered to the session whose folder is gone"
press Enter
closed 1 "picker: enter on a session whose folder is gone fails"
equals "$(resumed)" "" "picker: and starts nothing"
see "original cwd is gone" "picker: and says why"

open_picker
see "login redirect" "picker: opens again"
press Escape
closed 0 "picker: esc quits"
equals "$(resumed)" "" "picker: esc starts nothing"

# A small terminal, as over SSH. At 96 columns a 60% side preview would
# be under 60 columns wide, so it goes under the list; widened to 70% it
# is wide enough to go back to the side. The "6/6" count line marks where the
# list is: at the side split its titles are cut off.
preview_below() { [ "$(line_of "session : $CLAUDE_ID")" -gt "$(line_of "6/6")" ] 2>/dev/null; }
preview_beside() { [ "$(line_of "session : $CLAUDE_ID")" -lt "$(line_of "6/6")" ] 2>/dev/null; }
COLS=96 ROWS=30 open_picker
see "Fix the login redirect" "small terminal: opens"
see "session : $CLAUDE_ID" "small terminal: shows the preview"
until_true 15 preview_below || picker_failed "small terminal: the preview is under the list"
ok "small terminal: the preview is under the list"
lacks "$(screen)" "↳ ─" "small terminal: the rule under the header does not wrap"
press M-Left
until_true 15 preview_beside || picker_failed "small terminal: a wider split puts the preview back beside the list"
ok "small terminal: a wider split puts the preview back beside the list"
lacks "$(screen)" "↳ ─" "small terminal: the rule fits the side preview too"
press M-Right
until_true 15 preview_below || picker_failed "small terminal: narrowing it moves the preview under the list again"
ok "small terminal: narrowing it moves the preview under the list again"
press Escape
closed 0 "small terminal: esc quits"

echo
echo "$checks checks passed"
