# Fake Claude Code, Codex and OpenCode session data for tests/e2e.sh.
# Nothing here is real: every id, title and message is made up.
#
#   python3 make_fixtures.py <root> <cwd>
#
# <cwd> must be an existing folder: it is the folder the fake sessions
# "ran in", which resume changes into.
import json
import os
import re
import sqlite3
import sys
import time

root = sys.argv[1]
cwd = sys.argv[2]
now_ms = int(time.time() * 1000)
MIN, HOUR, DAY = 60_000, 3_600_000, 86_400_000
ESC = '\x1b'

CLAUDE_ID = 'aaaaaaaa-1111-4222-8333-444455556666'
HOSTILE_ID = '11111111-2222-4333-8444-555555555555'
CODEX_A = '0199aaaa-1111-7222-8333-444455556666'
CODEX_B = '0199bbbb-1111-7222-8333-444455556666'


def iso(ms):
    return time.strftime('%Y-%m-%dT%H:%M:%S', time.gmtime(ms / 1000)) + 'Z'


# Compact JSON, one object per line, as the tools themselves write it.
def write_jsonl(path, rows, mtime_ms):
    with open(path, 'w', encoding='utf-8') as f:
        for r in rows:
            f.write(json.dumps(r, separators=(',', ':')) + '\n')
    os.utime(path, (mtime_ms / 1000, mtime_ms / 1000))


# ---------------------------------------------------------------- Claude Code
# ~/.claude/projects/<cwd with every non-alphanumeric as '-'>/<id>.jsonl
proj = os.path.join(root, 'claude', 'projects', re.sub(r'[^A-Za-z0-9]', '-', cwd))
os.makedirs(proj, exist_ok=True)


def claude_user(text, t, where=cwd):
    return {'type': 'user', 'timestamp': iso(t), 'cwd': where, 'message': {'role': 'user', 'content': text}}


def claude_assistant(text, t):
    return {'type': 'assistant', 'timestamp': iso(t),
            'message': {'role': 'assistant', 'content': [{'type': 'text', 'text': text}]}}


t = now_ms - 10 * MIN
write_jsonl(os.path.join(proj, CLAUDE_ID + '.jsonl'), [
    claude_user('the login page redirects twice', t),
    claude_assistant('Removed the duplicate redirect in the auth middleware', t + 5000),
    {'type': 'ai-title', 'aiTitle': 'Fix the login redirect'},
], t + 5000)

# Hostile transcripts: a file name a harness would read as an option, and
# escape sequences in the folder, the title and the messages.
t = now_ms - 5 * DAY
write_jsonl(os.path.join(proj, '--dangerously-skip-permissions.jsonl'), [
    claude_user('planted', t),
    {'type': 'ai-title', 'aiTitle': 'planted flag id'},
], t)
write_jsonl(os.path.join(proj, HOSTILE_ID + '.jsonl'), [
    claude_user('hello ' + ESC + '[2J' + ESC + ']52;c;ZXZpbA==\x07 quokka', t),
    claude_assistant('reply ' + ESC + '[31mred', t + 5000),
    {'type': 'ai-title', 'aiTitle': 'title ' + ESC + '[5mblink\tTAB'},
], t + 5000)

# ---------------------------------------------------------------- Codex
# One session resumed (the same id in two rollouts), each with the AGENTS.md
# preamble Codex adds as a user turn, and one wrapped request.
d = os.path.join(root, 'codex-home', 'sessions', '2026', '11', '15')
os.makedirs(d, exist_ok=True)


def rollout(name, sid, t0, user_msgs, reply):
    def msg(role, kind, text, at):
        return {'timestamp': iso(at), 'type': 'response_item',
                'payload': {'type': 'message', 'role': role, 'content': [{'type': kind, 'text': text}]}}
    rows = [{'timestamp': iso(t0), 'type': 'session_meta', 'payload': {'id': sid, 'timestamp': iso(t0), 'cwd': cwd}},
            msg('user', 'input_text', '# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\nsecret rule\n</INSTRUCTIONS>', t0 + 1000)]
    for i, m in enumerate(user_msgs):
        rows.append(msg('user', 'input_text', m, t0 + 2000 + i * 1000))
    rows.append(msg('assistant', 'output_text', reply, t0 + 9000))
    write_jsonl(os.path.join(d, name), rows, t0 + 9000)


rollout(f'rollout-2026-11-15T01-00-00-{CODEX_A}.jsonl', CODEX_A, now_ms - 5 * HOUR,
        ['add retry to the uploader'], 'Added exponential backoff')
rollout(f'rollout-2026-11-15T02-00-00-{CODEX_A}.jsonl', CODEX_A, now_ms - 2 * HOUR,
        ['add retry to the uploader', 'now cap retries at five'], 'Capped at 5 retries')
rollout(f'rollout-2026-11-15T03-00-00-{CODEX_B}.jsonl', CODEX_B, now_ms - 26 * HOUR,
        ['# Files mentioned by the user:\n\n## My request for Codex:\nwhy is the zebra test flaky'],
        'A race in the fixture teardown')

# ---------------------------------------------------------------- OpenCode
# One root session stored as parts, one stored in the projection table and
# whose folder is gone, one child and one archived (neither is listed).
db = os.path.join(root, 'opencode.db')
if os.path.exists(db):
    os.remove(db)
c = sqlite3.connect(db)
c.executescript('''
CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT NOT NULL DEFAULT '', title TEXT NOT NULL DEFAULT '', time_created INTEGER NOT NULL DEFAULT 0, time_updated INTEGER NOT NULL DEFAULT 0, time_archived INTEGER);
CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL DEFAULT 0, data TEXT NOT NULL);
CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, data TEXT NOT NULL);
CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, data TEXT NOT NULL, seq INTEGER NOT NULL);
''')
t1, t2 = now_ms - 30 * MIN, now_ms - 3 * DAY
gone = os.path.join(root, 'no', 'such', 'folder')
c.execute("INSERT INTO session VALUES ('ses_7Hq2mKpLx9',NULL,?,?,?,?,NULL)", (cwd, 'Port the shader graph to WGSL', t1, t1))
c.execute("INSERT INTO session VALUES ('ses_child00001','ses_7Hq2mKpLx9',?,'subagent task',?,?,NULL)", (cwd, t1, t1))
c.execute("INSERT INTO session VALUES ('ses_Archived01',NULL,?,'archived one',?,?,?)", (cwd, t2, t2, t2))
c.execute("INSERT INTO session VALUES ('ses_Ab3dEf9GhJ',NULL,?,?,?,?,NULL)", (gone, 'Benchmark the tokenizer', t2, t2))
c.execute("INSERT INTO message VALUES ('m1','ses_7Hq2mKpLx9',?,?)", (t1 - MIN, json.dumps({'role': 'user', 'time': {'created': t1 - MIN}})))
c.execute("INSERT INTO part VALUES ('p1','m1','ses_7Hq2mKpLx9',?)", (json.dumps({'type': 'text', 'text': 'port the bloom pass first'}),))
c.execute("INSERT INTO message VALUES ('m2','ses_7Hq2mKpLx9',?,?)", (t1, json.dumps({'role': 'assistant', 'time': {'created': t1}})))
c.execute("INSERT INTO part VALUES ('p2','m2','ses_7Hq2mKpLx9',?)", (json.dumps({'type': 'text', 'text': 'Bloom pass ported; textureSample needs explicit LOD'}),))
c.execute("INSERT INTO session_message VALUES ('sm1','ses_Ab3dEf9GhJ','user',?,1)", (json.dumps({'text': 'benchmark the BPE tokenizer on 1 GB', 'time': {'created': t2}}),))
c.execute("INSERT INTO session_message VALUES ('sm2','ses_Ab3dEf9GhJ','assistant',?,2)", (json.dumps({'text': '410 MB/s single-threaded', 'time': {'created': t2 + 1000}}),))
c.commit()
c.close()
