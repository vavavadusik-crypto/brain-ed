#!/usr/bin/env python3
"""Private local transcript archive prototype (bounded, secret-masked capture).

Explicitly invoked CLI; no hooks, no background jobs, no network.

    python3 tools/session_archive.py --database PATH import --client claude|codex|hermes --source PATH
    python3 tools/session_archive.py --database PATH search QUERY [--limit N]
    python3 tools/session_archive.py --database PATH read ID
    python3 tools/session_archive.py --database PATH status
    python3 tools/session_archive.py --database PATH collect --home HOME --config PATH

Every import is a bounded full rescan of one explicitly named source with
dedupe on (source, locator, evidence sha256), where the evidence hash covers
session, role, timestamp and text. It is NOT an incremental cursor and NOT a
complete raw backup, and scans stop at fixed byte/row/record bounds.

`collect` is the bounded automatic collector: it discovers only the canonical
sources under HOME (Claude Code projects JSONL, Codex sessions JSONL, Hermes
state.db -- the top-level one plus every profiles/<name>/state.db, never the
state-snapshots/ backups; profile session ids are stored namespaced as
profiles/<name>/<id> so ids stay unique across profiles), reads them read-only
from a durable per-source cursor committed in the same transaction as the
entries, and stops at per-run source/byte/record/time budgets. It does nothing
unless the config file says `"enabled": true`.

An existing archive file must be owned by the current user with no group or
other permission bits; otherwise it is rejected before anything is written.
JSONL reads consume at most MAX_SCAN bytes per import (no probe byte; growth
past the cap is detected with fstat).
"""
import argparse
import fcntl
import hashlib
import json
import os
import re
import sqlite3
import stat
import sys
import time
from pathlib import Path

SCHEMA_MARKER = "brain_session_archive:3"
LEGACY_MARKER = "brain_session_archive:2"   # still readable; migrated on the next write
MAX_LINE = 1 << 20          # bytes per JSONL line / Hermes content value
MAX_SCAN = 64 << 20         # bytes scanned per import
HERMES_MAX_ROWS = 200000    # Hermes rows scanned per import, whatever their type/size
DEFAULT_MAX_RECORDS = 10000  # newly inserted entries per import
SEARCH_MAX = 20
EXCERPT_MAX = 300
READ_MAX = 32 << 10
CLIENTS = ("claude", "codex", "hermes")
# What the archive keeps, said in one place for every surface that reports it.
# Must stay in sync with STORED_TEXT in brain-cli/src/archive.rs.
STORED_TEXT = ("conversation plus bounded, secret-masked tool calls/results/errors"
               " and visible reasoning summaries; never system prompts, injected"
               " instructions, sidechains, hidden/encrypted reasoning, attachments"
               " or metadata events")
CODEX_INSTRUCTION_PREFIXES = (
    "<environment_context>", "<user_instructions>", "<instructions>",
    "<permissions instructions>", "# agents.md instructions",
)
# Fail closed on hidden reasoning: a message tagged with any other channel/phase
# (e.g. channel "analysis") is skipped, not archived.
CODEX_VISIBLE_CHANNELS = (None, "final", "commentary")
CODEX_VISIBLE_PHASES = (None, "final", "final_answer", "commentary")
# Codex can import another agent's session; its importer flattens that agent's tool
# calls and results into assistant output_text that starts with these markers (strings
# in the Codex binary next to "<EXTERNAL SESSION IMPORTED>"). That is tool traffic, not
# something the agent said. Assistant only: a person quoting a marker is still talking.
# Prefixes, not whole markers: real imports write "[external_agent_tool_call: Bash]" and
# "[external_agent_tool_result: error]", which the bracket-closed forms never matched
# (10 885 such rows reached search before rule 2).
CODEX_EXTERNAL_TOOL_MARKERS = ("[external_agent_tool_call", "[external_agent_tool_result")
# internal_chat_message_metadata_passthrough.content_item_kinds the client itself labels
# as injected context (besides any "*.instructions"); a message made only of them is skipped.
CODEX_INJECTED_KINDS = ("environments.environment_context",
                        "additional_content.codex_apps_open_page")
# Bump when the Codex filter changes so stored rows are judged again once.
QUARANTINE_RULE = "codex_visible_text:2"
# Bump when the extractors start capturing new kinds of records: every followed
# JSONL source is then read again from the start once, and existing rows are
# recognised by their unchanged locator/evidence, so only the new kinds are added.
# journal:2 - Hermes tool calls (assistant tool_calls JSON) and tool results
# (role='tool' rows) join the archive; reasoning stays hidden.
CAPTURE_REV = "journal:2"
# Roles beyond the conversation itself. Their text is bounded and secret-masked.
JOURNAL_ROLES = ("tool_call", "tool_result", "tool_error", "reasoning")
JOURNAL_MAX = 2048          # characters kept per tool call/result/reasoning record
# Masked at write time, in every record. Shapes of real credentials, not keywords:
# a secret that reaches the archive is copied into search results and backups.
SECRET_PATTERNS = [
    re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|$)", re.DOTALL),
    re.compile(r"\b(?:sk|rk|pk)-(?:ant-|proj-|live-|test-)?[A-Za-z0-9_\-]{16,}"),
    re.compile(r"\b(?:ghp|gho|ghu|ghs|ghr|github_pat)_[A-Za-z0-9_]{20,}"),
    re.compile(r"\bxox[abposr]-[A-Za-z0-9\-]{10,}"),
    re.compile(r"\bAKIA[0-9A-Z]{16}\b"),
    # Service keys shaped `<prefix>_live_…` / `_test_` / `_prod_` (Stripe and many others): a run of
    # 24+ letters and digits is a generated value, not an identifier in code.
    re.compile(r"\b[A-Za-z]{1,16}_(?:live|test|prod)_[A-Za-z0-9]{24,}"),
    re.compile(r"\bAIza[0-9A-Za-z_\-]{30,}"),
    re.compile(r"\beyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}"),
    re.compile(r"(?i)(?P<keep>\b(?:bearer|basic)\s+)(?!\[REDACTED\])[A-Za-z0-9_\-\.\+/=]{12,}"),
    # user:password@ in a URL; the user and host stay readable. Only the scheme and user are
    # bounded (an unbounded scheme made a long "a-a-a-…" line quadratic); the secret is not.
    re.compile(r"(?P<keep>\b[a-z][a-z0-9+.\-]{0,30}://[^\s:/@]{1,128}:)(?!\[REDACTED\]@)"
               r"[^\s/]+(?P<tail>@)"),
]
_SECRET_NAME = (r"(?:password|(?<!no)passwd|passphrase|secret|token|api[_-]?key|access[_-]?key"
                r"|private[_-]?key)")
# A value assigned to a key that names a credential, whatever its prefix: DB_PASSWORD=…,
# GITHUB_TOKEN: …, "client_secret": "…" (the JSON shape of every tool_call row), YAML
# `api_key: >-` blocks, `?access_token=` in a URL. Suffixes are limited so tokenize=,
# max_tokens= and token_count= stay readable, as do NOPASSWD: and credentials: 'include'.
# A quoted value is taken whole, spaces included; `_keyed_secret` decides what is code.
KEYED_SECRET = re.compile(
    r"(?i)(?P<keep>\b[A-Za-z0-9_]{0,64}?" + _SECRET_NAME + r"(?:[_-](?:key|value))?"
    r"['\"]?[ \t]*[:=][ \t]*(?:[|>][-+]?[ \t]*\n[ \t]*)?)"
    # A quote left open masks to the end of the line. Values are unbounded on purpose: a
    # match starts only right after a credential name, so the scan stays linear.
    r"(?P<value>(?P<quote>['\"])[^'\"\n]+(?:(?P=quote)|(?=\n|$))|(?:(?![\"'`,;&}\]\)\[\(])[\x21-\x7e])+)")
# A secret passed as a word after its name: `--password X`, `.netrc` `password X`, `token X`.
SPOKEN_SECRET = re.compile(r"(?i)(?P<keep>(?:(?<![\w-])--?)?\b" + _SECRET_NAME + r"[ \t]+)"
                           r"(?P<value>[\x21-\x7e]{4,})")
# mysql-style -p'secret' / -p"secret".
FLAG_SECRET = re.compile(r"(?P<keep>(?<!\S)-p)(?P<value>'[^'\n]+'|\"[^\"\n]+\")")
# Values that name a type or state rather than hold a secret.
NOT_SECRETS = frozenset(("true", "false", "null", "none", "nil", "undefined", "string", "str", "int",
                         "integer", "number", "bool", "boolean", "required", "optional", "example",
                         "redacted", "[redacted]", "***", "..."))
# A reference in code that names where the secret comes from: os.environ, parser.next_token.
# Digits make a dotted value look generated, so it is masked.
CODE_REFERENCE = re.compile(r"[A-Za-z_][A-Za-z_]*(?:\.[A-Za-z_][A-Za-z_]*)*")
VARIABLE_REFERENCE = re.compile(r"\$\{?[A-Za-z_]\w*\}?")
GENERATED_VALUE = re.compile(r"[A-Za-z0-9_\-+/=.!@#$%^&*~]+")
ENV_NAME = re.compile(r"[A-Z][A-Z0-9_]*")
BASE64_VALUE = re.compile(r"[A-Za-z0-9_\-+/=]+")

# Bump when the masking covers new shapes: rows already stored are masked again once.
MASK_REV = "mask:4"
# A Hermes profile directory name: conservative so a session namespace built
# from it cannot smuggle path separators or traversal.
PROFILE_NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*\Z")
THINK_OPEN = re.compile(r"\s*<think>", re.IGNORECASE)
THINK_BLOCK = re.compile(r"\s*<think>.*?</think>", re.IGNORECASE | re.DOTALL)

SCHEMA = [
    "CREATE TABLE archive_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL)",
    "CREATE TABLE sources(id INTEGER PRIMARY KEY, client TEXT NOT NULL, path TEXT NOT NULL,"
    " path_sha256 TEXT NOT NULL, UNIQUE(client, path))",
    "CREATE TABLE entries(id INTEGER PRIMARY KEY,"
    " source_id INTEGER NOT NULL REFERENCES sources(id), session_id TEXT, role TEXT NOT NULL,"
    " locator TEXT NOT NULL, source_offset INTEGER, ts TEXT, content TEXT NOT NULL,"
    " content_sha256 TEXT NOT NULL, evidence_sha256 TEXT NOT NULL,"
    " record_sha256 TEXT NOT NULL, imported_at TEXT NOT NULL,"
    " UNIQUE(source_id, locator, evidence_sha256))",
    "CREATE VIRTUAL TABLE entries_fts USING fts5(content, content='entries', content_rowid='id',"
    " tokenize=\"unicode61 remove_diacritics 0 tokenchars '_'\")",
]
# Schema 3 additions: applied to a fresh archive and as the 2 -> 3 migration.
SCHEMA_3 = [
    # Working directory the client itself recorded with the message; never inferred.
    "ALTER TABLE entries ADD COLUMN cwd TEXT",
    "CREATE INDEX entries_evidence ON entries(source_id, evidence_sha256)",
    "CREATE INDEX entries_session ON entries(source_id, session_id, id)",
    # One row per followed source. `position` is a byte offset (JSONL) or the last
    # row id (Hermes); it only moves inside the transaction that stores the entries.
    "CREATE TABLE cursors(source_id INTEGER PRIMARY KEY REFERENCES sources(id),"
    " client TEXT NOT NULL, dev INTEGER, ino INTEGER, size INTEGER,"
    " position INTEGER NOT NULL DEFAULT 0, discarding INTEGER NOT NULL DEFAULT 0,"
    " head_len INTEGER NOT NULL DEFAULT 0, head_sha256 TEXT, ctx TEXT,"
    " state TEXT NOT NULL, last_error TEXT, resets INTEGER NOT NULL DEFAULT 0,"
    " last_reset_reason TEXT, malformed INTEGER NOT NULL DEFAULT 0,"
    " oversize INTEGER NOT NULL DEFAULT 0, first_seen TEXT NOT NULL, last_visit TEXT,"
    " last_caught_up TEXT)",
    "CREATE TABLE collector_runs(id INTEGER PRIMARY KEY, started_at TEXT NOT NULL,"
    " finished_at TEXT NOT NULL, outcome TEXT NOT NULL, summary TEXT NOT NULL)",
]


# Rows an older filter stored that the current one rejects. The entry stays unchanged in
# `entries`; an active row here only hides it from search, read and counts.
QUARANTINE_TABLE = (
    "CREATE TABLE IF NOT EXISTS quarantine(entry_id INTEGER PRIMARY KEY REFERENCES entries(id),"
    " reason TEXT NOT NULL, rule TEXT NOT NULL, evidence TEXT NOT NULL,"
    " quarantined_at TEXT NOT NULL, released_at TEXT)")


class ArchiveError(Exception):
    """User-facing error; messages never contain transcript text."""


def _sha(data):
    return hashlib.sha256(data).hexdigest()


def _evidence_sha(session, role, ts, text):
    """Deterministic identity of normalized evidence; changes if any attributed field changes."""
    return _sha(json.dumps([session, role, ts, text], ensure_ascii=False).encode("utf-8"))


def _opt_str(value):
    return str(value) if isinstance(value, (str, int, float)) and not isinstance(value, bool) else None


# ---------------------------------------------------------------- destination

def _reject_symlinks(db):
    for path in (db, db + "-journal", db + "-wal", db + "-shm"):
        if os.path.islink(path):
            raise ArchiveError("refusing symlinked archive database or side file")


def _check_schema(conn, allow_init):
    """Return True if archive schema exists, False if DB is empty (and init allowed)."""
    try:
        names = {row[0] for row in conn.execute("SELECT name FROM sqlite_master")}
    except sqlite3.DatabaseError:
        raise ArchiveError("database is not a readable SQLite archive") from None
    if not names:
        if allow_init:
            return False
        raise ArchiveError("archive database is not initialized")
    if "archive_meta" not in names or "entries_fts" not in names:
        raise ArchiveError("unknown database schema; refusing to use it as an archive")
    row = conn.execute("SELECT value FROM archive_meta WHERE key = 'schema'").fetchone()
    if not row or row[0] not in (SCHEMA_MARKER, LEGACY_MARKER):
        raise ArchiveError("unknown archive schema version; refusing")
    return True


def _schema_version(conn):
    row = conn.execute("SELECT value FROM archive_meta WHERE key = 'schema'").fetchone()
    return 3 if row and row[0] == SCHEMA_MARKER else 2


def _prepare_write(conn):
    _prepare_schema(conn)
    conn.execute(QUARANTINE_TABLE)
    _quarantine_sweep(conn)


def _visible(conn):
    """SQL condition: entry `e` is not in active quarantine (true before the table exists)."""
    if conn.execute("SELECT 1 FROM sqlite_master WHERE type = 'table'"
                    " AND name = 'quarantine'").fetchone():
        return ("NOT EXISTS (SELECT 1 FROM quarantine q WHERE q.entry_id = e.id"
                " AND q.released_at IS NULL)")
    return "1"


def _source_line(handles, path, offset):
    """The raw line at `offset` of a followed source, or None if it cannot be read."""
    if offset is None:
        return None
    if path not in handles:
        try:
            handles[path] = os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC), "rb")
        except OSError:
            handles[path] = None
    handle = handles[path]
    if handle is None:
        return None
    handle.seek(offset)
    return handle.readline(MAX_LINE + 2)


def _quarantine_sweep(conn):
    """Set aside stored Codex rows the current filter would not import; never deletes.

    Runs once per QUARANTINE_RULE. A row is re-derived from its own source line when that
    line still hashes to the stored record_sha256; otherwise only the stored text is judged,
    and only by a leading tool-traffic marker on an assistant row. Released rows stay out.
    """
    done = conn.execute("SELECT value FROM archive_meta WHERE key = 'quarantine_rule'").fetchone()
    if done and done[0] == QUARANTINE_RULE:
        return
    now = _now()
    rows = conn.execute(
        "SELECT e.id, s.path, e.source_offset, e.record_sha256, e.role, e.content"
        " FROM entries e JOIN sources s ON s.id = e.source_id WHERE s.client = 'codex'"
        " AND e.id NOT IN (SELECT entry_id FROM quarantine)"
        " ORDER BY s.path, e.source_offset").fetchall()
    handles = {}
    try:
        for entry_id, path, offset, record_sha, role, content in rows:
            if role not in ("user", "assistant"):
                continue  # journal rows come from non-message lines by design
            reason = basis = None
            line = _source_line(handles, path, offset)
            if line is not None and record_sha in (_sha(line), _sha(line.rstrip(b"\r\n"))):
                basis = "source_line"
                try:
                    obj = json.loads(line.decode("utf-8"))
                    payload = obj.get("payload") if isinstance(obj, dict) else None
                    if isinstance(payload, dict):
                        reason = _codex_visible_text(payload)[1]
                except (UnicodeDecodeError, ValueError, RecursionError):
                    reason = None
            elif role == "assistant" and content.lstrip().lower().startswith(
                    CODEX_EXTERNAL_TOOL_MARKERS):
                basis, reason = "stored_text_marker", "external_agent_tool_traffic"
            if reason:
                conn.execute(
                    "INSERT INTO quarantine(entry_id, reason, rule, evidence, quarantined_at)"
                    " VALUES (?, ?, ?, ?, ?)",
                    (entry_id, reason, QUARANTINE_RULE, json.dumps(
                        {"basis": basis, "record_sha256": record_sha, "source_offset": offset}), now))
    finally:
        for handle in handles.values():
            if handle is not None:
                handle.close()
    conn.execute("INSERT OR REPLACE INTO archive_meta VALUES ('quarantine_rule', ?)",
                 (QUARANTINE_RULE,))


def _prepare_schema(conn):
    """Inside a write transaction: create a fresh archive or migrate schema 2 to 3."""
    if not _check_schema(conn, allow_init=True):
        for statement in SCHEMA + SCHEMA_3:
            conn.execute(statement)
        conn.execute("INSERT INTO archive_meta VALUES ('schema', ?)", (SCHEMA_MARKER,))
    elif _schema_version(conn) == 2:
        for statement in SCHEMA_3:
            conn.execute(statement)
        conn.execute("UPDATE archive_meta SET value = ? WHERE key = 'schema'", (SCHEMA_MARKER,))


def _ensure_private_parent(parent):
    missing = []
    probe = parent
    while not os.path.lexists(probe):
        missing.append(probe)
        probe = os.path.dirname(probe)
    for directory in reversed(missing):  # only newly created dirs get 0700
        os.mkdir(directory, 0o700)
        os.chmod(directory, 0o700)
    if not os.path.isdir(parent):
        raise ArchiveError("archive parent is not a directory")
    # SQLite creates the rollback journal next to the archive later, without O_NOFOLLOW;
    # a directory others can write to would let them plant a journal symlink.
    info = os.stat(parent)
    if info.st_uid != os.geteuid() or info.st_mode & 0o022:
        raise ArchiveError("archive directory must be owned by you and not group/other"
                           " writable; refusing to write")


def _require_owner_only(db):
    """Reject an existing archive (even empty) unless it is ours and mode has no g/o bits."""
    fd = os.open(db, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        info = os.fstat(fd)
    finally:
        os.close(fd)
    if not stat.S_ISREG(info.st_mode):
        raise ArchiveError("archive path is not a regular file")
    if info.st_uid != os.geteuid() or info.st_mode & 0o077:
        raise ArchiveError("existing archive must be owned by you with owner-only permissions"
                           " (chmod 600); refusing to write")
    return info.st_size


def _open_rw(db):
    _reject_symlinks(db)
    _ensure_private_parent(os.path.dirname(db))
    if not os.path.lexists(db):
        fd = os.open(db, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        os.close(fd)
        os.chmod(db, 0o600)
    elif not stat.S_ISREG(os.lstat(db).st_mode):
        raise ArchiveError("archive path is not a regular file")
    elif _require_owner_only(db) > 0:
        _open_ro(db).close()  # reject Brain/unknown databases before any write open
    # Default rollback journal (never WAL); SQLite gives it the DB file's 0600 mode.
    return sqlite3.connect(db, isolation_level=None)


def _open_ro(db):
    db = os.path.abspath(db)
    if not os.path.lexists(db):
        raise ArchiveError("archive database not found")
    _reject_symlinks(db)
    if not stat.S_ISREG(os.lstat(db).st_mode):
        raise ArchiveError("archive path is not a regular file")
    conn = sqlite3.connect(Path(db).as_uri() + "?mode=ro", uri=True)
    try:
        _check_schema(conn, allow_init=False)
    except BaseException:
        conn.close()
        raise
    return conn


# ---------------------------------------------------------------- sources

def _iter_lines(path, stats):
    """Yield (offset, line) for complete lines only, within MAX_LINE / MAX_SCAN.

    Never consumes more than MAX_SCAN bytes; data beyond the cap is detected via fstat.
    """
    with open(path, "rb") as handle:
        offset = 0
        discarding = False  # inside the remainder of an oversized line
        while True:
            room = MAX_SCAN - offset
            if room <= 0:
                if os.fstat(handle.fileno()).st_size > offset:
                    stats["scan_truncated"] = True
                return
            chunk = handle.readline(min(MAX_LINE + 1, room))
            if not chunk:
                return
            start = offset
            offset += len(chunk)
            complete = chunk.endswith(b"\n")
            if discarding:
                discarding = not complete
            elif complete:
                stats["scanned"] += 1
                yield start, chunk
            elif len(chunk) > MAX_LINE:
                stats["oversize"] += 1
                discarding = True
            elif offset >= MAX_SCAN and os.fstat(handle.fileno()).st_size > offset:
                stats["scan_truncated"] = True  # line cut by the scan cap, not by EOF
                return
            else:
                stats["partial_tail"] = 1  # incomplete final line; retried next import
                return


def _text_blocks(content, kinds, skip_prefixes=()):
    if isinstance(content, str):
        parts = [content]
    elif isinstance(content, list):
        parts = [b["text"] for b in content
                 if isinstance(b, dict) and b.get("type") in kinds and isinstance(b.get("text"), str)]
    else:
        return ""
    if skip_prefixes:
        parts = [p for p in parts if not p.lstrip().lower().startswith(skip_prefixes)]
    return "\n".join(parts)


def _mask_secrets(text):
    for pattern in SECRET_PATTERNS:
        text = pattern.sub(_redacted, text)
    text = KEYED_SECRET.sub(_keyed_secret, text)
    text = SPOKEN_SECRET.sub(_spoken_secret, text)
    return FLAG_SECRET.sub(lambda m: m.group("keep") + _quoted_redaction(m.group("value")), text)


def _quoted_redaction(value):
    """[REDACTED] in the value's own quotes; an unclosed quote stays unclosed."""
    if value[:1] in ("'", '"'):
        closing = value[0] if len(value) > 1 and value[-1] == value[0] else ""
        return f"{value[0]}[REDACTED]{closing}"
    return "[REDACTED]"


def _keyed_secret(match):
    value, quoted = match.group("value"), bool(match.group("quote"))
    inner = value[1:].rstrip(value[0]) if quoted else value
    after = match.string[match.end():match.end() + 1]
    if (len(inner) < 4 or inner.lower() in NOT_SECRETS or inner.isdigit()
            or VARIABLE_REFERENCE.fullmatch(inner)  # $VAR / ${VAR}: a reference, not the value
            or (not quoted and CODE_REFERENCE.fullmatch(inner) and ("." in inner or (after != "" and after in "([.")))):
        return match.group(0)
    return match.group("keep") + _quoted_redaction(value)


def _spoken_secret(match):
    value = match.group("value")
    core = value.rstrip(".,:;)]}\"'`*")
    tail = value[len(core):]
    flag = match.group("keep").lstrip().startswith("-")
    # Prose and code listings ("token usage:", "secret sauce") stay: a spoken value counts
    # after a --flag, or when it looks generated: letters and digits mixed, or long base64/hex.
    generated = ((len(core) >= 8 and any(c.isdigit() for c in core) and any(c.isalpha() for c in core)
                  and GENERATED_VALUE.fullmatch(core))
                 or (len(core) >= 16 and BASE64_VALUE.fullmatch(core)))
    if (not core or value.startswith(("-", "[REDACTED]", "$")) or "\\" in core
            or core.lower().strip("'\"") in NOT_SECRETS or not (flag or generated)
            # After a flag, an environment variable name or a <placeholder> is not the value.
            or ENV_NAME.fullmatch(core) or core.startswith(("<", "{"))):
        return match.group(0)
    return match.group("keep") + "[REDACTED]" + tail


def _redacted(match):
    parts = match.groupdict()
    return (parts.get("keep") or "") + "[REDACTED]" + (parts.get("tail") or "")


def _journal_text(text):
    text = _mask_secrets(text)
    if len(text) > JOURNAL_MAX:
        text = text[:JOURNAL_MAX] + f"\n…[{len(text) - JOURNAL_MAX} more characters not archived]"
    return text


def _json_text(value):
    if isinstance(value, str):
        return value
    try:
        return json.dumps(value, ensure_ascii=False, sort_keys=True)
    except (TypeError, ValueError):
        return ""


def _result_text(content):
    """Tool output as text: a string, or the text parts of a content list."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(b["text"] for b in content
                         if isinstance(b, dict) and isinstance(b.get("text"), str))
    if isinstance(content, dict) and isinstance(content.get("output"), str):
        return content["output"]
    return _json_text(content)


def _claude_journal(msg):
    """(suffix, role, text) for the tool calls, results and thinking in one Claude message."""
    content = msg.get("content")
    if not isinstance(content, list):
        return []
    out = []
    for index, block in enumerate(content):
        if not isinstance(block, dict):
            continue
        kind = block.get("type")
        if kind == "tool_use" and isinstance(block.get("name"), str):
            ident = block.get("id") if isinstance(block.get("id"), str) else str(index)
            out.append((f"#tool_use:{ident}", "tool_call",
                        f"{block['name']} {_json_text(block.get('input'))}"))
        elif kind == "tool_result":
            ident = block.get("tool_use_id") if isinstance(block.get("tool_use_id"), str) else str(index)
            role = "tool_error" if block.get("is_error") is True else "tool_result"
            out.append((f"#tool_result:{ident}", role, _result_text(block.get("content"))))
        elif kind == "thinking" and isinstance(block.get("thinking"), str):
            out.append((f"#thinking:{index}", "reasoning", block["thinking"]))
    return [(suffix, role, _journal_text(text)) for suffix, role, text in out if text.strip()]


def _extract_claude(obj, ctx):
    # Sidechain lines are subagent conversations the person never saw in the chat.
    if (obj.get("type") not in ("user", "assistant") or obj.get("isMeta")
            or obj.get("isCompactSummary") or obj.get("isSidechain")):
        return None
    msg = obj.get("message")
    if not isinstance(msg, dict) or msg.get("role") not in ("user", "assistant"):
        return None
    text = _mask_secrets(_text_blocks(msg.get("content"), ("text",)))
    uuid = obj.get("uuid") if isinstance(obj.get("uuid"), str) and obj["uuid"] else None
    cwd = obj.get("cwd") if isinstance(obj.get("cwd"), str) else None
    session, ts = obj.get("sessionId"), obj.get("timestamp")
    records = [("", msg["role"], session, ts, text, uuid, cwd)]
    records += [(suffix, role, session, ts, body, uuid, cwd)
                for suffix, role, body in _claude_journal(msg)]
    return records


def _extract_codex(obj, ctx):
    payload = obj.get("payload")
    if not isinstance(payload, dict):
        return None
    if obj.get("type") in ("session_meta", "turn_context"):
        if obj["type"] == "session_meta":
            ctx["session"] = _opt_str(payload.get("id"))
        if isinstance(payload.get("cwd"), str):
            ctx["cwd"] = payload["cwd"]
        return None
    # event_msg lines duplicate response_item messages for display; ignore them.
    if obj.get("type") != "response_item":
        return None
    session, ts, cwd = ctx.get("session"), obj.get("timestamp"), ctx.get("cwd")
    kind = payload.get("type")
    if kind == "message":
        text, reason = _codex_visible_text(payload)
        if reason:
            return None
        return [("", payload["role"], session, ts, _mask_secrets(text), None, cwd)]
    journal = _codex_journal(payload)
    if journal is None:
        return None
    suffix, role, text = journal
    return [(suffix, role, session, ts, _journal_text(text), None, cwd)]


def _codex_journal(payload):
    """(suffix, role, text) for a Codex tool call, its output, or a visible reasoning summary."""
    kind = payload.get("type")
    call = payload.get("call_id") if isinstance(payload.get("call_id"), str) else ""
    if kind in ("function_call", "custom_tool_call", "local_shell_call"):
        name = payload.get("name") if isinstance(payload.get("name"), str) else kind
        args = payload.get("arguments", payload.get("input", payload.get("action")))
        return f"#call:{call}", "tool_call", f"{name} {_json_text(args)}"
    if kind in ("function_call_output", "custom_tool_call_output", "local_shell_call_output"):
        return f"#output:{call}", "tool_result", _result_text(payload.get("output"))
    if kind == "reasoning":
        # Only the summary the client shows; encrypted or raw reasoning content is never read.
        summary = payload.get("summary")
        text = "\n".join(part["text"] for part in summary if isinstance(part, dict)
                         and isinstance(part.get("text"), str)) if isinstance(summary, list) else ""
        return "#reasoning", "reasoning", text
    return None


def _codex_visible_text(payload):
    """(text, None) for a visible Codex message, or ("", reason) when it is not conversation."""
    role = payload.get("role")
    if role not in ("user", "assistant"):
        return "", "not_conversation_role"
    if (payload.get("channel") not in CODEX_VISIBLE_CHANNELS
            or payload.get("phase") not in CODEX_VISIBLE_PHASES):
        return "", "hidden_channel"
    meta = payload.get("internal_chat_message_metadata_passthrough")
    kinds = meta.get("content_item_kinds") if isinstance(meta, dict) else None
    if isinstance(kinds, list) and kinds and all(
            isinstance(kind, str) and (kind in CODEX_INJECTED_KINDS or kind.endswith(".instructions"))
            for kind in kinds):
        return "", "injected_context"
    kinds = ("input_text", "output_text", "text")
    skip = CODEX_INSTRUCTION_PREFIXES
    if role == "assistant":
        skip += CODEX_EXTERNAL_TOOL_MARKERS
    text = _text_blocks(payload.get("content"), kinds, skip)
    if text.strip():
        return text, None
    raw = _text_blocks(payload.get("content"), kinds).lstrip().lower()
    if role == "assistant" and raw.startswith(CODEX_EXTERNAL_TOOL_MARKERS):
        return "", "external_agent_tool_traffic"
    return "", "injected_instructions" if raw else "empty"


def _jsonl_records(lines, client, stats, ctx):
    """Turn (offset, raw line) pairs into archive records; ctx carries Codex session/cwd."""
    extract = _extract_claude if client == "claude" else _extract_codex
    for offset, raw in lines:
        try:
            obj = json.loads(raw.decode("utf-8"))
            if not isinstance(obj, dict):
                raise ValueError
        except (UnicodeDecodeError, ValueError, RecursionError):
            stats["malformed"] += 1
            continue
        got = [record for record in (extract(obj, ctx) or []) if record[4].strip()]
        if not got:
            stats["skipped"] += 1
            continue
        for suffix, role, session, ts, text, uuid, cwd in got:
            try:
                text.encode("utf-8")  # JSON "\ud800" escapes yield unstorable lone surrogates
                (cwd or "").encode("utf-8")
            except UnicodeEncodeError:
                stats["malformed"] += 1
                continue
            # Stable message identity when the client provides one, else byte offset; a
            # journal record adds its kind so it never collides with the message text.
            locator = (f"uuid:{uuid}" if uuid else f"offset:{offset}") + suffix
            yield locator, offset, _sha(raw), role, _opt_str(session), _opt_str(ts), text, cwd


def _iter_jsonl(path, client, stats):
    yield from _jsonl_records(_iter_lines(path, stats), client, stats, {})


def _strip_inline_reasoning(text):
    """Remove leading <think>...</think> blocks; None if a block is left unclosed (fail closed).

    Leading blocks only: Hermes keeps reasoning in separate columns, and a tag later in
    the text may be a legitimate quotation. Matching ignores case.
    """
    while THINK_OPEN.match(text):
        block = THINK_BLOCK.match(text)
        if not block:
            return None
        text = text[block.end():]
    return text


def _hermes_check_source(path):
    """Refuse a Hermes source that is not a regular file owned by this OS user, or
    that group or others can write. A read-only 0644 source is accepted: reading it
    discloses nothing new, while a writable one could be swapped under us.

    Applies to the database and any -wal/-shm side file. Nothing is chmod-ed or
    created: an unsafe source is reported and left exactly as found.
    """
    for candidate in (path, path + "-wal", path + "-shm"):
        try:
            info = os.lstat(candidate)
        except FileNotFoundError:
            if candidate == path:
                raise ArchiveError("hermes source not found") from None
            continue
        if stat.S_ISLNK(info.st_mode):
            raise ArchiveError("refusing symlinked hermes source or side file")
        if not stat.S_ISREG(info.st_mode):
            raise ArchiveError("hermes source or side file is not a regular file")
        if info.st_uid != os.geteuid() or info.st_mode & 0o022:
            raise ArchiveError("hermes source must be owned by you and not group/other"
                               " writable (chmod go-w it yourself); nothing was read")
    return os.lstat(path)


def _hermes_open(path):
    """Read-only connection plus the column expressions this Hermes schema supports.

    mode=ro keeps SQLite's normal shared read locks (never immutable=1), so a live
    Hermes writer is respected and the source is never modified.
    """
    before = _hermes_check_source(path)
    conn = sqlite3.connect(Path(path).as_uri() + "?mode=ro", uri=True)
    try:
        after = os.lstat(path)
        if (after.st_dev, after.st_ino) != (before.st_dev, before.st_ino):
            raise ArchiveError("hermes source changed while being opened; refusing")
        try:
            kind = conn.execute("SELECT type FROM sqlite_master WHERE name = 'messages' COLLATE NOCASE"
                                " AND type IN ('table', 'view')").fetchone()
        except sqlite3.DatabaseError:
            raise ArchiveError("source is not a readable SQLite database") from None
        if kind is None:
            raise ArchiveError("hermes source lacks messages(id, session_id, role, content)")
        if kind[0] != "table":
            raise ArchiveError("hermes source messages must be a table, not a view")
        try:
            cols = {row[1] for row in conn.execute("PRAGMA table_info(messages)")}
        except sqlite3.DatabaseError:
            raise ArchiveError("source is not a readable SQLite database") from None
        if not {"id", "session_id", "role", "content"} <= cols:
            raise ArchiveError("hermes source lacks messages(id, session_id, role, content)")
    except BaseException:
        conn.close()
        raise
    ts_col = next((c for c in ("timestamp", "created_at", "ts") if c in cols), "NULL")
    # Rows Hermes itself hides from display (any non-NULL display_kind) are not archived.
    kind_col = "display_kind" if "display_kind" in cols else "NULL"
    # Journal columns: absent in older schemas, read as NULL there.
    call_col = "tool_calls" if "tool_calls" in cols else "NULL"
    tool_name_col = "tool_name" if "tool_name" in cols else "NULL"
    return conn, ts_col, kind_col, call_col, tool_name_col


def _hermes_session_prefix(path):
    """Namespace added to a profile source's session ids; "" for the top level.

    Hermes reuses session ids across its top-level database and the per-profile
    ones, so a stored id alone no longer names one conversation. Profile ids
    are stored as `profiles/<name>/<id>`; the top-level database keeps ids as
    Hermes wrote them. The path shape (.../profiles/<name>/state.db) decides,
    so the collector and an explicit `import` of the same file agree.
    """
    real = os.path.abspath(path)
    if os.path.basename(real) != "state.db":
        return ""
    name = os.path.basename(os.path.dirname(real))
    if os.path.basename(os.path.dirname(os.path.dirname(real))) != "profiles":
        return ""
    return f"profiles/{name}/" if PROFILE_NAME.match(name) else ""


def _hermes_tool_calls(raw):
    """The parsed `tool_calls` value as a list of dicts, or None when there are none.

    Hermes stores the OpenAI-style array as JSON text; a single object, an
    already-parsed value and an unparseable string are all tolerated because
    the column's shape is the client's, not ours.
    """
    if isinstance(raw, str):
        if not raw.strip():
            return None
        try:
            raw = json.loads(raw)
        except (ValueError, RecursionError):
            return None
    if isinstance(raw, dict):
        raw = [raw]
    if not isinstance(raw, list):
        return None
    calls = [call for call in raw if isinstance(call, dict)]
    return calls or None


def _hermes_journal(role, call_raw, tool_name, content):
    """(suffix, role, text) records a Hermes row contributes beside its message.

    - an assistant's `tool_calls` JSON becomes one `tool_call` record per call,
      "<function name> <arguments>";
    - a role='tool' row becomes one `tool_result` record, "<tool_name>: <content>"
      (the name falls back to "tool" when the column is empty).

    Everything goes through _journal_text, so records are secret-masked and
    bounded to JOURNAL_MAX characters. The `reasoning` column and inline
    <think> blocks are deliberately never read here: Hermes gives no way to
    tell a visible summary from hidden reasoning, so both stay out.

    Locators: the message keeps `row:<id>`; journal records append their suffix
    (`row:<id>#tool_call:<call id or index>`, `row:<id>#tool_result`), so the
    several records one row can yield never collide with the message locator
    or with each other.
    """
    out = []
    if role == "assistant":
        for index, call in enumerate(_hermes_tool_calls(call_raw) or ()):
            function = call.get("function") if isinstance(call.get("function"), dict) else call
            name = function.get("name") if isinstance(function.get("name"), str) else "tool"
            arguments = function.get("arguments", call.get("arguments"))
            ident = call.get("id") if isinstance(call.get("id"), str) and call["id"] else str(index)
            out.append((f"#tool_call:{ident}", "tool_call", f"{name} {_json_text(arguments)}"))
    if role == "tool" and isinstance(content, str) and content.strip():
        name = tool_name if isinstance(tool_name, str) and tool_name.strip() else "tool"
        out.append(("#tool_result", "tool_result", f"{name}: {content}"))
    return [(suffix, journal_role, _journal_text(text))
            for suffix, journal_role, text in out if text.strip()]


def _iter_hermes(path, stats, after_id=0, max_rows=None, max_bytes=None, cursor=None):
    """Rows with id > after_id in id order, bounded by rows and bytes.

    With a cursor dict, cursor["position"] follows the last row consumed (kept or not).
    """
    max_rows = HERMES_MAX_ROWS if max_rows is None else max_rows
    max_bytes = MAX_SCAN if max_bytes is None else max_bytes
    session_prefix = _hermes_session_prefix(path)
    conn, ts_col, kind_col, call_col, tool_name_col = _hermes_open(path)
    try:
        size_expr = "length(CAST(content AS BLOB))"
        # LIMIT bounds rows of any type/size; the one extra row only signals truncation.
        rows = conn.execute(
            f"SELECT id, session_id, role, typeof(content), {size_expr},"
            f" CASE WHEN {size_expr} <= {MAX_LINE} THEN content END, {ts_col}, {kind_col},"
            f" {call_col}, {tool_name_col}, typeof(id) FROM messages WHERE id > ?"
            " ORDER BY id LIMIT ?",
            (after_id, max_rows + 1))
        budget = 0
        scanned = 0
        for (mid, session, role, kind, size, content, ts, display_kind, call_raw,
             tool_name, id_kind) in rows:
            if scanned >= max_rows:
                stats["scan_truncated"] = True
                return
            if id_kind != "integer":
                raise ArchiveError("hermes messages.id is not an integer; refusing")
            budget += size or 0  # every row the scan touched, including skipped ones
            if "bytes_scanned" in stats:
                stats["bytes_scanned"] += size or 0
            # A collector cursor always consumes at least one row so a huge row cannot wedge it.
            if budget > max_bytes and (cursor is None or scanned):
                stats["scan_truncated"] = True
                return
            scanned += 1
            stats["scanned"] += 1
            if cursor is not None:
                cursor["position"] = mid
            session_value = _opt_str(session)
            if session_value is not None and session_prefix:
                session_value = session_prefix + session_value
            # Journal first: a text-less assistant tool-call row and a role='tool'
            # row are archived through it even when there is no message text.
            # A row Hermes hides from display keeps its journal out too.
            journal = () if display_kind is not None else _hermes_journal(
                role, call_raw, tool_name, content if kind == "text" else None)
            if kind == "null":
                if not journal:
                    stats["skipped"] += 1  # a row without text and without tool calls
                    continue
            elif kind != "text":
                stats["malformed"] += 1  # BLOB/numeric content is unexpected
                continue
            elif content is None:
                stats["oversize"] += 1
                continue
            else:
                text = _strip_inline_reasoning(content) if role == "assistant" else content
                hidden = (role not in ("user", "assistant") or display_kind is not None
                          or text is None or not text.strip())
                if hidden:
                    if not journal:
                        stats["skipped"] += 1
                else:
                    try:
                        content.encode("utf-8")  # JSON "\ud800" escapes yield unstorable surrogates
                    except UnicodeEncodeError:
                        stats["malformed"] += 1
                    else:
                        # record hash covers the source row as stored, before reasoning removal
                        record = json.dumps([mid, session, role, content, ts],
                                            ensure_ascii=False, default=str)
                        yield (f"row:{mid}", None, _sha(record.encode("utf-8")), role,
                               session_value, _opt_str(ts), text, None)
            for suffix, journal_role, body in journal:
                # Same row, different facet: the locator suffix keeps the journal
                # records distinct from the message and from each other.
                record = json.dumps([mid, suffix, session, role, ts],
                                    ensure_ascii=False, default=str)
                yield (f"row:{mid}{suffix}", None, _sha(record.encode("utf-8")),
                       journal_role, session_value, _opt_str(ts), body, None)
    finally:
        conn.close()


# ---------------------------------------------------------------- commands

def _now():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def _ensure_source(conn, client, path):
    conn.execute("INSERT OR IGNORE INTO sources(client, path, path_sha256) VALUES (?, ?, ?)",
                 (client, path, _sha(path.encode("utf-8", "surrogateescape"))))
    return conn.execute("SELECT id FROM sources WHERE client = ? AND path = ?",
                        (client, path)).fetchone()[0]


def _store(conn, source_id, record, now, stats, evidence_only=False, roles=None):
    """Insert one record unless it is already stored; True if inserted.

    Identity is (source, locator, evidence). After a collector reset the byte
    offsets of a rewritten or rotated file have moved, so `evidence_only` makes
    the evidence alone decide for offset locators; two identical messages (same
    session, role, timestamp and text) in such a file then collapse into one.
    """
    locator, offset, record_sha, role, session, ts, text, cwd = record
    if roles is not None and role not in roles:
        stats["skipped"] += 1
        return False
    # The single choke point: whatever an extractor forgot, no credential is stored.
    text = _mask_secrets(text)
    evidence = _evidence_sha(session, role, ts, text)
    if evidence_only and locator.startswith("offset:") and conn.execute(
            "SELECT 1 FROM entries WHERE source_id = ? AND evidence_sha256 = ? LIMIT 1",
            (source_id, evidence)).fetchone():
        stats["duplicates"] += 1
        return False
    if _mask_stored_twin(conn, source_id, locator, session, role, ts, text, evidence):
        stats["duplicates"] += 1
        return False
    cur = conn.execute(
        "INSERT OR IGNORE INTO entries(source_id, session_id, role, locator, source_offset,"
        " ts, content, content_sha256, evidence_sha256, record_sha256, imported_at, cwd)"
        " VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (source_id, session, role, locator, offset, ts, text,
         _sha(text.encode("utf-8")), evidence, record_sha, now, cwd))
    if cur.rowcount != 1:
        stats["duplicates"] += 1
        return False
    conn.execute("INSERT INTO entries_fts(rowid, content) VALUES (?, ?)", (cur.lastrowid, text))
    stats["inserted"] += 1
    return True


# Role selection is applied at this choke point so every client extractor follows
# the same consent rule and already stored rows remain untouched.


def _mask_stored_twin(conn, source_id, locator, session, role, ts, text, evidence):
    """Mask in place a row stored unmasked for this very record; True if there was one.

    Without it a raw row kept from before a masking change and the freshly masked record
    would differ in evidence and both stay, the secret included.
    """
    for entry_id, content in conn.execute(
            "SELECT id, content FROM entries WHERE source_id = ? AND locator = ?"
            " AND evidence_sha256 != ? AND session_id IS ? AND role = ? AND ts IS ?",
            (source_id, locator, evidence, session, role, ts)).fetchall():
        if content != text and _mask_secrets(content) == text:
            conn.execute("INSERT INTO entries_fts(entries_fts, rowid, content) VALUES ('delete', ?, ?)",
                         (entry_id, content))
            conn.execute("UPDATE entries SET content = ?, content_sha256 = ?, evidence_sha256 = ?"
                         " WHERE id = ?", (text, _sha(text.encode("utf-8")), evidence, entry_id))
            conn.execute("INSERT INTO entries_fts(rowid, content) VALUES (?, ?)", (entry_id, text))
            return True
    return False


def cmd_import(args):
    source = os.path.abspath(args.source)
    db = os.path.abspath(args.database)
    if not os.path.isfile(source):
        raise ArchiveError("source must be an existing regular file")
    if os.path.realpath(source) == os.path.realpath(db) or (
            os.path.exists(db) and os.path.samefile(source, db)):
        raise ArchiveError("source and destination are the same file")
    if not 1 <= args.max_records <= DEFAULT_MAX_RECORDS:
        raise ArchiveError(f"--max-records must be between 1 and {DEFAULT_MAX_RECORDS}")
    if args.client == "hermes":
        _hermes_check_source(source)  # before realpath hides a symlinked source
    real_source = os.path.realpath(source)
    stats = dict(scanned=0, inserted=0, duplicates=0, malformed=0, skipped=0, oversize=0,
                 partial_tail=0, scan_truncated=False, record_limit_reached=False)
    conn = _open_rw(db)
    records = None
    try:
        conn.execute("BEGIN IMMEDIATE")
        _prepare_write(conn)
        # Stored rows are masked as the collector would before any of them is compared
        # with a freshly masked record, or the raw copy would stay beside the masked one.
        _mask_upgrade(conn)
        source_id = _ensure_source(conn, args.client, real_source)
        records = (_iter_hermes(real_source, stats) if args.client == "hermes"
                   else _iter_jsonl(real_source, args.client, stats))
        now = _now()
        for record in records:
            if _store(conn, source_id, record, now, stats) and stats["inserted"] >= args.max_records:
                stats["record_limit_reached"] = True
                break
        conn.execute("COMMIT")
    except BaseException:
        if conn.in_transaction:
            conn.execute("ROLLBACK")
        raise
    finally:
        if records is not None:
            records.close()
        conn.close()
    # Deliberately filtered records ("skipped") do not count; unreadable ones ("malformed") do.
    bounded = bool(stats["scan_truncated"] or stats["record_limit_reached"]
                   or stats["oversize"] or stats["partial_tail"] or stats["malformed"])
    return {"client": args.client, "capture": "normalized_text", "stored_text": STORED_TEXT,
            "mode": "bounded_full_rescan_with_dedupe", "incremental": False,
            "complete_capture_claimed": False, "source_bounded_or_partial": bounded, **stats}


def _fts_query(query):
    terms = query.split()
    if not terms:
        raise ArchiveError("empty query")
    return " ".join('"' + term.replace('"', '""') + '"' for term in terms)


def cmd_search(args):
    if args.limit < 1:
        raise ArchiveError("--limit must be positive")
    limit = min(args.limit, SEARCH_MAX)
    conn = _open_ro(args.database)
    try:
        rows = conn.execute(
            "SELECT e.id, s.client, e.role, e.session_id, e.ts,"
            " snippet(entries_fts, 0, '', '', '…', 32)"
            " FROM entries_fts JOIN entries e ON e.id = entries_fts.rowid"
            " JOIN sources s ON s.id = e.source_id"
            " WHERE entries_fts MATCH ? AND " + _visible(conn) +
            " ORDER BY bm25(entries_fts), e.id LIMIT ?",
            (_fts_query(args.query), limit)).fetchall()
    except sqlite3.OperationalError:
        raise ArchiveError("query could not be evaluated") from None
    finally:
        conn.close()
    return {"trust": "untrusted_conversation", "limit": limit, "results": [
        {"id": r[0], "client": r[1], "role": r[2], "session_id": r[3], "timestamp": r[4],
         "excerpt": (r[5] or "")[:EXCERPT_MAX]} for r in rows]}


def cmd_read(args):
    conn = _open_ro(args.database)
    try:
        row = conn.execute(
            "SELECT e.id, s.client, e.role, e.session_id, e.ts, e.locator, e.content,"
            " e.content_sha256, e.record_sha256, s.path_sha256, e.imported_at, e.source_offset,"
            " e.evidence_sha256, " + ("e.cwd" if _schema_version(conn) == 3 else "NULL") +
            " FROM entries e JOIN sources s ON s.id = e.source_id WHERE e.id = ? AND "
            + _visible(conn), (args.id,)).fetchone()
    finally:
        conn.close()
    if row is None:
        raise ArchiveError("entry not found")
    raw = row[6].encode("utf-8")
    truncated = len(raw) > READ_MAX
    content = raw[:READ_MAX].decode("utf-8", "ignore") if truncated else row[6]
    return {"trust": "untrusted_conversation", "id": row[0], "client": row[1], "role": row[2],
            "session_id": row[3], "timestamp": row[4], "locator": row[5],
            "source_offset": row[11],
            "content": content, "truncated": truncated, "content_bytes": len(raw),
            "content_sha256": row[7], "evidence_sha256": row[12], "record_sha256": row[8],
            "source_path_sha256": row[9], "imported_at": row[10], "cwd": row[13]}


def cmd_status(args):
    conn = _open_ro(args.database)
    try:
        visible = _visible(conn)

        def grouped(column):
            return dict(conn.execute(
                f"SELECT {column}, count(*) FROM entries e JOIN sources s ON s.id = e.source_id"
                f" WHERE {visible} GROUP BY {column} ORDER BY {column}").fetchall())
        return {"schema": SCHEMA_MARKER if _schema_version(conn) == 3 else LEGACY_MARKER,
                "entries": conn.execute(
                    f"SELECT count(*) FROM entries e WHERE {visible}").fetchone()[0],
                "sources": conn.execute("SELECT count(*) FROM sources").fetchone()[0],
                "sessions": conn.execute(
                    f"SELECT count(DISTINCT session_id) FROM entries e WHERE {visible}").fetchone()[0],
                "by_client": grouped("s.client"), "by_role": grouped("e.role"),
                "quarantined": _quarantine_counts(conn)}
    finally:
        conn.close()


def _quarantine_counts(conn):
    if _visible(conn) == "1":
        return {}
    return dict(conn.execute("SELECT reason, count(*) FROM quarantine WHERE released_at IS NULL"
                             " GROUP BY reason ORDER BY reason").fetchall())


def cmd_quarantine(args):
    """Counts by reason, ids with --list (never text), or --release ID to show a row again."""
    if args.release is None:
        conn = _open_ro(args.database)
        try:
            listed = conn.execute(
                "SELECT entry_id, reason, rule, quarantined_at FROM quarantine"
                " WHERE released_at IS NULL ORDER BY entry_id").fetchall() if (
                    args.list and _visible(conn) != "1") else []
            return {"active_by_reason": _quarantine_counts(conn), "entries": [
                dict(zip(("id", "reason", "rule", "quarantined_at"), row)) for row in listed]}
        finally:
            conn.close()
    conn = _open_rw(args.database)
    try:
        conn.execute("BEGIN IMMEDIATE")
        _prepare_write(conn)
        released = conn.execute("UPDATE quarantine SET released_at = ? WHERE entry_id = ?"
                                " AND released_at IS NULL", (_now(), args.release)).rowcount
        conn.execute("COMMIT")
    finally:
        if conn.in_transaction:
            conn.execute("ROLLBACK")
        conn.close()
    if not released:
        raise ArchiveError("entry is not in active quarantine")
    return {"released": args.release}


# ---------------------------------------------------------------- collector

# key: (minimum, maximum, default). Every run is bounded by all of them at once.
CONFIG_LIMITS = {
    "since_days": (1, 3650, 14),
    "max_sources_per_run": (1, 200, 40),
    "max_bytes_per_run": (1 << 20, 64 << 20, 16 << 20),
    "max_bytes_per_source": (64 << 10, 32 << 20, 8 << 20),
    "max_records_per_run": (1, DEFAULT_MAX_RECORDS, 3000),
    "max_seconds": (1, 60, 20),
    "hermes_rows_per_run": (1, HERMES_MAX_ROWS, 5000),
}
CONFIG_ROLES = ("user", "assistant", "tool_call", "tool_result", "tool_error", "reasoning")
CONFIG_VERSION = 1
DISCOVERY_MAX = 20000   # files listed per client per run
HEAD_BYTES = 4096       # prefix fingerprint that detects a rewritten JSONL file
RUNS_KEPT = 200
# Separate, optional config that admits a few old (out-of-window) files into tracking
# each run. It lives in its own file because `load_config` rejects unknown keys, so an
# older binary that predates backfill still reads `collector.json` unchanged.
BACKFILL_NAME = "backfill.json"
BACKFILL_VERSION = 1
BACKFILL_MAX_FILES = 20  # files_per_run is 0..20; 0 admits nothing (a paused equivalent)
BACKFILL_STATES = ("running", "stopped")
CURSOR_FIELDS = ("client", "dev", "ino", "size", "position", "discarding", "head_len",
                 "head_sha256", "ctx", "state", "last_error", "resets", "last_reset_reason",
                 "malformed", "oversize", "first_seen", "last_visit", "last_caught_up")


def load_config(path):
    """Validated collector config, or None when the file does not exist."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    except FileNotFoundError:
        return None
    except OSError:
        raise ArchiveError("collector config is unreadable or a symlink") from None
    try:
        info = os.fstat(fd)
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid()
                or info.st_mode & 0o022):
            raise ArchiveError("collector config must be a regular file you own that"
                               " group/other cannot write")
        raw = os.read(fd, 64 << 10)
    finally:
        os.close(fd)
    try:
        data = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError):
        raise ArchiveError("collector config is not valid JSON") from None
    if not isinstance(data, dict):
        raise ArchiveError("collector config must be a JSON object")
    unknown = set(data) - {"version", "enabled", "clients", "roles", *CONFIG_LIMITS}
    if unknown:
        raise ArchiveError("unknown collector config key: " + sorted(unknown)[0][:40])
    if data.get("version", CONFIG_VERSION) != CONFIG_VERSION:
        raise ArchiveError("unsupported collector config version")
    if not isinstance(data.get("enabled", False), bool):
        raise ArchiveError("collector config enabled must be true or false")
    config = {"enabled": data.get("enabled", False)}
    # Per-client consent. A config without the key predates the selection and keeps
    # its original meaning: all three clients. An explicit list is exact; anything
    # unknown fails closed before discovery so a typo never widens what is read.
    if "clients" in data:
        clients = data["clients"]
        if (not isinstance(clients, list) or not clients
                or any(not isinstance(c, str) or c not in CLIENTS for c in clients)
                or len(set(clients)) != len(clients)):
            raise ArchiveError("collector config clients must be a non-empty list of"
                               " distinct names from: " + ", ".join(CLIENTS))
        config["clients"] = tuple(c for c in CLIENTS if c in clients)
        config["client_selection"] = "explicit"
    else:
        config["clients"] = CLIENTS
        config["client_selection"] = "legacy_all"
    if "roles" in data:
        roles = data["roles"]
        if (not isinstance(roles, list) or not roles
                or any(not isinstance(role, str) or role not in CONFIG_ROLES for role in roles)
                or len(set(roles)) != len(roles)):
            raise ArchiveError("collector config roles must be a non-empty list of distinct names from: "
                               + ", ".join(CONFIG_ROLES))
        config["roles"] = tuple(role for role in CONFIG_ROLES if role in roles)
        config["role_selection"] = "explicit"
    else:
        config["roles"] = CONFIG_ROLES
        config["role_selection"] = "legacy_all"
    for key, (low, high, default) in CONFIG_LIMITS.items():
        value = data.get(key, default)
        if isinstance(value, bool) or not isinstance(value, int) or not low <= value <= high:
            raise ArchiveError(f"collector config {key} must be an integer in [{low}, {high}]")
        config[key] = value
    return config


def load_backfill(path):
    """Validated optional backfill config, or None when the file does not exist.

    Only this file controls admission of old files; a missing file keeps the
    collector's previous behavior exactly. A bad/malformed/unreadable/insecure
    file denies backfill only -- the regular collector run is unaffected.
    The file must be a regular file, not a symlink, owned by you with owner-only
    permissions (no group/other read, write or execute), and at most 64 KiB. It
    is opened non-blocking so a FIFO is rejected without waiting for a writer,
    and an oversized file is refused rather than accepting a valid prefix.
    """
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        return None
    except OSError:
        raise ArchiveError("backfill config is unreadable or a symlink") from None
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode):
            raise ArchiveError("backfill config must be a regular file")
        if info.st_uid != os.geteuid() or info.st_mode & 0o077:
            raise ArchiveError("backfill config must be owner-only (chmod 600); refusing")
        if info.st_size > 64 << 10:
            raise ArchiveError("backfill config is larger than 64 KiB; refusing")
        raw = os.read(fd, (64 << 10) + 1)  # one byte past the cap: reject real overflow
    finally:
        os.close(fd)
    if len(raw) > 64 << 10:
        raise ArchiveError("backfill config grew past 64 KiB while being read; refusing")
    try:
        data = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError):
        raise ArchiveError("backfill config is not valid JSON") from None
    if not isinstance(data, dict):
        raise ArchiveError("backfill config must be a JSON object")
    unknown = set(data) - {"v", "files_per_run", "state"}
    if unknown:
        raise ArchiveError("unknown backfill config key: " + sorted(unknown)[0][:40])
    # Every field is required, matching the Rust reader: no implicit defaults.
    for required in ("v", "files_per_run", "state"):
        if required not in data:
            raise ArchiveError(f"backfill config is missing required key: {required}")
    version = data["v"]
    if type(version) is not int or version != BACKFILL_VERSION:
        raise ArchiveError("unsupported backfill config version")
    files = data["files_per_run"]
    if isinstance(files, bool) or not isinstance(files, int) or not 0 <= files <= BACKFILL_MAX_FILES:
        raise ArchiveError(f"backfill config files_per_run must be an integer in"
                           f" [0, {BACKFILL_MAX_FILES}]")
    state = data["state"]
    if state not in BACKFILL_STATES:
        raise ArchiveError("backfill config state must be running or stopped")
    return {"enabled": True, "files_per_run": files, "state": state}


def _backfill_coverage(bf, candidates, states, remaining, pending_new, gaps):
    """Honest backfill coverage: admitted/remaining counts and a contiguous watermark.

    `states` maps (client, path) -> cursor state (or "not_started"). The watermark is
    the mtime of the oldest file in the newest contiguous run of `caught_up` files:
    every file at or newer than it, including the file at that mtime and every
    same-mtime tie, must be `caught_up`. It is None when that run is empty -- an
    uncaught file anywhere at or above the boundary, an old file not yet admitted,
    an admitted-but-not-yet-caught file, or a bounded/truncated discovery gap.
    `contiguous` says whether such a run exists. It never claims the whole history
    is captured (`complete_capture_claimed` stays false): truncated discovery,
    unreadable roots and missing sources force it to None because the set of files
    on disk is not known to be complete.
    """
    if bf is None:
        return None
    in_scope = {(c[0], c[1]) for c in candidates}
    caught = {(client, path) for (client, path), state in states.items()
              if state == "caught_up" and (client, path) in in_scope}
    watermark = None
    contiguous = False
    if bf["state"] == "running" and not remaining and not pending_new and not gaps:
        levels = {}
        for client, path, info, _ in candidates:
            levels.setdefault(info.st_mtime, []).append((client, path))
        covered = []
        for mtime in sorted(levels, reverse=True):
            if all(key in caught for key in levels[mtime]):
                covered.append(mtime)
            else:
                break  # a gap: this mtime level (or a tie) is not fully caught
        if covered:
            watermark = min(covered)
            contiguous = True
    return {"config": {"v": BACKFILL_VERSION, "files_per_run": bf["files_per_run"],
                       "state": bf["state"]},
            "admitted_files": bf["admitted"],
            "remaining_out_of_window": remaining,
            "watermark_mtime": watermark, "contiguous": contiguous}


def _client_summary():
    return dict(root="absent", discovered=0, in_scope=0, out_of_window=0, missing=0,
                admitted=0, discovery_truncated=False, visited=0, inserted=0, duplicates=0,
                malformed=0, oversize=0, resets=0, bytes_read=0, rows_read=0)


def _list(path, summary):
    """Directory entries, or [] when absent; unreadable roots are reported, not raised."""
    try:
        with os.scandir(path) as entries:
            return sorted(entries, key=lambda e: e.name)
    except FileNotFoundError:
        return []
    except OSError:
        summary["root"] = "unreadable"
        return []


def _hermes_profile_databases(hermes_dir, summary, add):
    """Every profiles/<name>/state.db as its own source; snapshots never are.

    Hermes keeps a separate live state.db per profile beside the top-level one.
    Backups under state-snapshots/ duplicate that history and are deliberately
    never scanned. Symlinks are not followed at any step (directory entries are
    checked without following, the database itself is lstat-ed), so nothing
    outside the profile tree is ever read. An unreadable profiles directory or
    database is reported through the summary and never fails the run.
    """
    profiles = os.path.join(hermes_dir, "profiles")
    try:
        with os.scandir(profiles) as scan:
            names = sorted(e.name for e in scan if e.is_dir(follow_symlinks=False))
        summary["profiles_root"] = "present"
    except FileNotFoundError:
        summary["profiles_root"] = "absent"
        return
    except OSError:
        summary["profiles_root"] = "unreadable"
        return
    for name in names:
        if not PROFILE_NAME.match(name):
            continue
        path = os.path.join(profiles, name, "state.db")
        try:
            info = os.stat(path, follow_symlinks=False)
        except OSError:
            continue  # absent or unreadable: not a source, never a failure
        add("hermes", path, info)


def _discover(home, summary, clients=CLIENTS):
    """Only the canonical source locations of selected clients; symlinks are ignored."""
    found = []
    counts = dict.fromkeys(CLIENTS, 0)

    def add(client, path, info):
        if counts[client] >= DISCOVERY_MAX:
            summary[client]["discovery_truncated"] = True
            return
        if stat.S_ISREG(info.st_mode):
            counts[client] += 1
            found.append((client, path, info))

    def add_entry(client, entry):
        try:
            info = entry.stat(follow_symlinks=False)
        except OSError:
            return
        add(client, entry.path, info)

    def subdirs(path, client, pattern):
        return [e for e in _list(path, summary[client])
                if pattern(e.name) and e.is_dir(follow_symlinks=False)]

    claude = os.path.join(home, ".claude", "projects")
    if "claude" in clients and os.path.isdir(claude) and not os.path.islink(claude):
        summary["claude"]["root"] = "present"
        for project in subdirs(claude, "claude", lambda name: True):
            for entry in _list(project.path, summary["claude"]):
                if entry.name.endswith(".jsonl") and entry.is_file(follow_symlinks=False):
                    add_entry("claude", entry)

    codex = os.path.join(home, ".codex", "sessions")
    if "codex" in clients and os.path.isdir(codex) and not os.path.islink(codex):
        summary["codex"]["root"] = "present"

        def digits(n):
            return lambda name: len(name) == n and name.isdigit()
        for year in subdirs(codex, "codex", digits(4)):
            for month in subdirs(year.path, "codex", digits(2)):
                for day in subdirs(month.path, "codex", digits(2)):
                    for entry in _list(day.path, summary["codex"]):
                        if (entry.name.startswith("rollout-") and entry.name.endswith(".jsonl")
                                and entry.is_file(follow_symlinks=False)):
                            add_entry("codex", entry)

    hermes_dir = os.path.join(home, ".hermes")
    if "hermes" not in clients:
        return found
    try:
        entry = next((e for e in _list(hermes_dir, summary["hermes"]) if e.name == "state.db"),
                     None)
    except OSError:
        entry = None
    if entry is not None:
        summary["hermes"]["root"] = "present"
        add_entry("hermes", entry)
    _hermes_profile_databases(hermes_dir, summary["hermes"], add)
    return found


def _load_cursor(conn, source_id):
    row = conn.execute(f"SELECT {', '.join(CURSOR_FIELDS)} FROM cursors WHERE source_id = ?",
                       (source_id,)).fetchone()
    return dict(zip(CURSOR_FIELDS, row)) if row else None


def _save_cursor(conn, source_id, cursor):
    conn.execute(
        f"INSERT OR REPLACE INTO cursors(source_id, {', '.join(CURSOR_FIELDS)})"
        f" VALUES (?{', ?' * len(CURSOR_FIELDS)})",
        (source_id, *(cursor[field] for field in CURSOR_FIELDS)))


def _new_cursor(client, now):
    cursor = dict.fromkeys(CURSOR_FIELDS)
    cursor.update(client=client, position=0, discarding=0, head_len=0, state="pending",
                  resets=0, malformed=0, oversize=0, first_seen=now)
    return cursor


def _reset(cursor, reason, summary):
    cursor.update(position=0, discarding=0, head_len=0, head_sha256=None, ctx=None,
                  last_reset_reason=reason, resets=cursor["resets"] + 1, malformed=0, oversize=0)
    summary["resets"] += 1


def _read_lines_from(handle, cursor, budget, stats, deadline):
    """Complete lines from cursor["position"], advancing it past each consumed line.

    An unfinished last line is left for the next run. An oversized line is skipped
    across runs: `discarding` survives in the cursor until its newline arrives.
    """
    handle.seek(cursor["position"])
    start = cursor["position"]
    while cursor["position"] - start < budget and time.monotonic() < deadline:
        chunk = handle.readline(MAX_LINE + 1)
        if not chunk:
            return
        complete = chunk.endswith(b"\n")
        if cursor["discarding"]:
            cursor["position"] += len(chunk)
            cursor["discarding"] = 0 if complete else 1
        elif complete:
            offset = cursor["position"]
            cursor["position"] += len(chunk)
            stats["scanned"] += 1
            yield offset, chunk
        elif len(chunk) > MAX_LINE:
            stats["oversize"] += 1
            cursor["position"] += len(chunk)
            cursor["discarding"] = 1
        else:
            stats["partial_tail"] = 1
            return


def _visit_jsonl(conn, source_id, client, path, cursor, limits, stats, summary, now, roles):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as handle:
        info = os.fstat(handle.fileno())
        if not stat.S_ISREG(info.st_mode):
            raise ArchiveError("source is not a regular file")
        reason = None
        if cursor["dev"] is not None:
            if (info.st_dev, info.st_ino) != (cursor["dev"], cursor["ino"]):
                reason = "replaced"
            elif info.st_size < cursor["position"]:
                reason = "truncated"
            elif cursor["head_len"]:
                handle.seek(0)
                head = handle.read(cursor["head_len"])
                if _sha(head) != cursor["head_sha256"]:
                    reason = "rewritten"
        if reason:
            _reset(cursor, reason, summary)
        cursor.update(dev=info.st_dev, ino=info.st_ino)
        ctx = json.loads(cursor["ctx"]) if cursor["ctx"] else {}
        start = cursor["position"]
        budget = min(limits["max_bytes_per_source"], limits["bytes"])
        lines = _read_lines_from(handle, cursor, budget, stats, limits["deadline"])
        records = _jsonl_records(lines, client, stats, ctx)
        rescanning = cursor["resets"] > 0
        try:
            for record in records:
                if _store(conn, source_id, record, now, stats, evidence_only=rescanning,
                          roles=roles):
                    limits["records"] -= 1
                    if limits["records"] <= 0:
                        break
        finally:
            records.close()
            lines.close()
        consumed = cursor["position"] - start
        limits["bytes"] -= consumed
        summary["bytes_read"] += consumed
        cursor["head_len"] = min(HEAD_BYTES, cursor["position"])
        handle.seek(0)
        cursor["head_sha256"] = _sha(handle.read(cursor["head_len"]))
        cursor["ctx"] = json.dumps(ctx, ensure_ascii=False) if ctx else None
        size = os.fstat(handle.fileno()).st_size
    cursor["size"] = size
    if stats["partial_tail"] and not cursor["discarding"]:
        cursor["state"] = "partial_tail"
    elif cursor["position"] >= size and not cursor["discarding"]:
        cursor["state"] = "caught_up"
    else:
        cursor["state"] = "pending"


def _visit_hermes(conn, source_id, path, cursor, limits, stats, summary, now, roles):
    info = os.stat(path, follow_symlinks=False)
    probe, _, _, _, _ = _hermes_open(path)
    try:
        max_id = probe.execute("SELECT max(id) FROM messages").fetchone()[0]
    finally:
        probe.close()
    # Narrow the stat/open window: the file read must still be the regular file stat'ed.
    again = os.stat(path, follow_symlinks=False)
    if not stat.S_ISREG(again.st_mode) or (again.st_dev, again.st_ino) != (info.st_dev, info.st_ino):
        raise ArchiveError("hermes source changed while being opened; retrying next run")
    if max_id is not None and not isinstance(max_id, int):
        raise ArchiveError("hermes messages.id is not an integer; refusing")
    reason = None
    if cursor["dev"] is not None and (info.st_dev, info.st_ino) != (cursor["dev"], cursor["ino"]):
        reason = "replaced"
    elif cursor["position"] > (max_id or 0):
        reason = "truncated"
    if reason:
        _reset(cursor, reason, summary)
    cursor.update(dev=info.st_dev, ino=info.st_ino, size=max_id or 0)
    before = stats["scanned"]
    records = _iter_hermes(path, stats, after_id=cursor["position"],
                           max_rows=limits["hermes_rows_per_run"],
                           max_bytes=min(limits["max_bytes_per_source"], limits["bytes"]),
                           cursor=cursor)
    try:
        for record in records:
            if _store(conn, source_id, record, now, stats, roles=roles):
                limits["records"] -= 1
                if limits["records"] <= 0:
                    break
    finally:
        records.close()
    summary["rows_read"] += stats["scanned"] - before
    limits["bytes"] -= stats.get("bytes_scanned", 0)
    summary["bytes_read"] += stats.get("bytes_scanned", 0)
    cursor["state"] = "caught_up" if cursor["position"] >= (max_id or 0) else "pending"


def _needs_visit(client, info, cursor):
    if cursor is None or client == "hermes" or cursor["state"] == "error":
        return True
    if (info.st_dev, info.st_ino) != (cursor["dev"], cursor["ino"]) or cursor["discarding"]:
        return True
    if cursor["state"] == "partial_tail" and info.st_size == cursor["size"]:
        return False  # the unfinished line has not grown; nothing new to read
    return info.st_size != cursor["position"]


def _visit(conn, client, path, limits, summary, now, roles):
    stats = dict(scanned=0, inserted=0, duplicates=0, malformed=0, skipped=0, oversize=0,
                 partial_tail=0, scan_truncated=False, bytes_scanned=0)
    summary["visited"] += 1
    try:
        conn.execute("BEGIN IMMEDIATE")
        source_id = _ensure_source(conn, client, path)
        cursor = _load_cursor(conn, source_id) or _new_cursor(client, now)
        if client == "hermes":
            _visit_hermes(conn, source_id, path, cursor, limits, stats, summary, now, roles)
        else:
            _visit_jsonl(conn, source_id, client, path, cursor, limits, stats, summary, now,
                         roles)
        cursor.update(last_error=None, last_visit=now,
                      malformed=cursor["malformed"] + stats["malformed"],
                      oversize=cursor["oversize"] + stats["oversize"])
        if cursor["state"] == "caught_up":
            cursor["last_caught_up"] = now
        _save_cursor(conn, source_id, cursor)
        conn.execute("COMMIT")  # entries and the cursor that covers them, together
    except (ArchiveError, OSError, sqlite3.DatabaseError, ValueError) as exc:
        if conn.in_transaction:
            conn.execute("ROLLBACK")
        # Recorded as a class or our own message; never source text.
        error = str(exc) if isinstance(exc, ArchiveError) else type(exc).__name__
        conn.execute("BEGIN IMMEDIATE")
        source_id = _ensure_source(conn, client, path)
        cursor = _load_cursor(conn, source_id) or _new_cursor(client, now)
        cursor.update(state="error", last_error=error[:200], last_visit=now)
        _save_cursor(conn, source_id, cursor)
        conn.execute("COMMIT")
        return
    for key in ("inserted", "duplicates", "malformed", "oversize"):
        summary[key] += stats[key]


def cmd_collect(args):
    db = os.path.abspath(args.database)
    config = load_config(args.config)
    base = {"capture": "normalized_text", "stored_text": STORED_TEXT,
            "complete_capture_claimed": False}
    if config is None:
        return {**base, "status": "not_configured"}
    if not config["enabled"]:
        return {**base, "status": "disabled"}  # paused: no source or archive is touched
    _ensure_private_parent(os.path.dirname(db))
    lock = os.open(os.path.join(os.path.dirname(db), "collector.lock"),
                   os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return {**base, "status": "busy"}
        backfill_path = os.path.join(os.path.dirname(db), BACKFILL_NAME)
        return {**base, **_collect_locked(db, os.path.abspath(args.home), config, backfill_path)}
    finally:
        os.close(lock)


def _capture_upgrade(conn, summary):
    """Read every followed source again once when the extractors capture more.

    JSONL cursors restart at byte 0, Hermes cursors at row id 0 — the top-level
    database and every profiles/<name> one alike, because the journal records
    the new revision captures live in all of them. Rows the previous revision
    already stored keep their locator and evidence, so the re-read recognises
    them and only the newly captured kinds are added.
    """
    row = conn.execute("SELECT value FROM archive_meta WHERE key = 'capture_rev'").fetchone()
    if row and row[0] == CAPTURE_REV:
        return
    for source_id, client in conn.execute(
            "SELECT c.source_id, s.client FROM cursors c JOIN sources s ON s.id = c.source_id"
            " WHERE s.client IN ('claude', 'codex', 'hermes')").fetchall():
        cursor = _load_cursor(conn, source_id)
        _reset(cursor, "capture_upgrade", summary[client])
        cursor["state"] = "pending"
        _save_cursor(conn, source_id, cursor)
    _mask_stored_secrets(conn)
    conn.execute("INSERT OR REPLACE INTO archive_meta VALUES ('capture_rev', ?)", (CAPTURE_REV,))


def _mask_upgrade(conn):
    """Mask stored rows again once when SECRET_PATTERNS learn new shapes; no source is re-read."""
    row = conn.execute("SELECT value FROM archive_meta WHERE key = 'mask_rev'").fetchone()
    if row and row[0] == MASK_REV:
        return
    _mask_stored_secrets(conn)
    conn.execute("INSERT OR REPLACE INTO archive_meta VALUES ('mask_rev', ?)", (MASK_REV,))


def _mask_stored_secrets(conn):
    """Mask credentials in rows stored before masking existed; the row and its hashes follow.

    The evidence hash is recomputed from the masked text, so the re-read that follows a
    capture upgrade recognises the row instead of storing the masked copy beside it.
    """
    rows = conn.execute(
        "SELECT id, source_id, locator, session_id, role, ts, content FROM entries").fetchall()
    for entry_id, source_id, locator, session, role, ts, content in rows:
        masked = _mask_secrets(content)
        if masked == content:
            continue
        evidence = _evidence_sha(session, role, ts, masked)
        conn.execute("INSERT INTO entries_fts(entries_fts, rowid, content) VALUES ('delete', ?, ?)",
                     (entry_id, content))
        # Two stored versions of one record that differed only in a secret become the same
        # row once masked; the identity is unique, so the later copy is dropped, not kept.
        twin = conn.execute("SELECT id FROM entries WHERE source_id = ? AND locator = ?"
                            " AND evidence_sha256 = ? AND id != ?",
                            (source_id, locator, evidence, entry_id)).fetchone()
        if twin:
            _merge_quarantine(conn, keep=twin[0], drop=entry_id)
            conn.execute("DELETE FROM entries WHERE id = ?", (entry_id,))
            continue
        conn.execute("UPDATE entries SET content = ?, content_sha256 = ?, evidence_sha256 = ?"
                     " WHERE id = ?", (masked, _sha(masked.encode("utf-8")), evidence, entry_id))
        conn.execute("INSERT INTO entries_fts(rowid, content) VALUES (?, ?)", (entry_id, masked))


def _merge_quarantine(conn, keep, drop):
    """Carry a dropped twin's visibility over to the row that stays.

    The two rows hold the same masked text, so if either was visible (never quarantined,
    or released by the owner) the kept one becomes visible too: a release is never lost.
    """
    def hidden(entry_id):
        return conn.execute("SELECT 1 FROM quarantine WHERE entry_id = ? AND released_at IS NULL",
                            (entry_id,)).fetchone() is not None
    if hidden(keep) and not hidden(drop):
        released = conn.execute("SELECT released_at FROM quarantine WHERE entry_id = ?",
                                (drop,)).fetchone()
        conn.execute("UPDATE quarantine SET released_at = ? WHERE entry_id = ?",
                     ((released and released[0]) or _now(), keep))
    conn.execute("DELETE FROM quarantine WHERE entry_id = ?", (drop,))


def _collect_locked(db, home, config, backfill_path):
    started, clock = _now(), time.monotonic()
    limits = dict(config, bytes=config["max_bytes_per_run"],
                  records=config["max_records_per_run"], deadline=clock + config["max_seconds"])
    summary = {client: _client_summary() for client in CLIENTS}
    for client in CLIENTS:
        if client not in config["clients"]:
            summary[client]["root"] = "not_selected"  # not even stat'ed
    found = _discover(home, summary, config["clients"])
    conn = _open_rw(db)
    try:
        conn.execute("PRAGMA busy_timeout = 5000")
        conn.execute("BEGIN IMMEDIATE")
        _prepare_write(conn)
        _capture_upgrade(conn, summary)
        _mask_upgrade(conn)
        conn.execute("COMMIT")
        tracked = {(row[0], row[1]): dict(zip(CURSOR_FIELDS, row[2:])) for row in conn.execute(
            f"SELECT s.client, s.path, {', '.join('c.' + f for f in CURSOR_FIELDS)}"
            " FROM cursors c JOIN sources s ON s.id = c.source_id")}
        cutoff = time.time() - config["since_days"] * 86400
        in_window, old = [], []
        for client, path, info in found:
            path = os.path.realpath(path)
            summary[client]["discovered"] += 1
            cursor = tracked.get((client, path))
            if cursor is None and info.st_mtime < cutoff:
                old.append((client, path, info))  # older history: a backfill candidate
            else:
                in_window.append((client, path, info, cursor))
        # Backfill is optional and independent of the collector switch: a bad or absent
        # backfill.json never blocks the regular run. `running` admits up to N of the
        # oldest-newest eligible old files; `stopped` admits none. Newest-to-oldest with a
        # stable path tie-break, and only sources not already followed.
        backfill_error = None
        bf = None
        selected = []
        try:
            bf = load_backfill(backfill_path)
        except ArchiveError as exc:
            backfill_error = str(exc)
        if bf is not None:
            bf["admitted"] = 0
            eligible = [c for c in (old if bf["state"] == "running" else [])
                        if (c[0], c[1]) not in tracked]
            eligible.sort(key=lambda c: (-c[2].st_mtime, c[1]))
            selected = eligible[:bf["files_per_run"]]  # newest first, up to the quota
            for client, path, info in selected:
                summary[client]["in_scope"] += 1
                in_window.append((client, path, info, None))
        candidates = in_window
        present = {(c[0], c[1]) for c in candidates}
        for client, path in tracked:
            if client in config["clients"] and (client, path) not in present:
                summary[client]["missing"] += 1  # followed before, gone or unreadable now
        # Least recently visited first: every pending source gets a turn.
        pending = sorted((c for c in candidates if _needs_visit(c[0], c[2], c[3])),
                         key=lambda c: ((c[3] or {}).get("last_visit") or "", c[1]))
        stopped_by = None
        visits = 0
        for client, path, _, _ in pending:
            if visits >= config["max_sources_per_run"]:
                stopped_by = "source_budget"
            elif limits["bytes"] <= 0:
                stopped_by = "byte_budget"
            elif limits["records"] <= 0:
                stopped_by = "record_budget"
            elif time.monotonic() >= limits["deadline"]:
                stopped_by = "time_budget"
            if stopped_by:
                break
            visits += 1
            _visit(conn, client, path, limits, summary[client], _now(),
                     config["roles"])
        if stopped_by is None:  # a budget may run out during the last visit
            if limits["records"] <= 0:
                stopped_by = "record_budget"
            elif limits["bytes"] <= 0:
                stopped_by = "byte_budget"
            elif time.monotonic() >= limits["deadline"]:
                stopped_by = "time_budget"
        states = {}
        for client, path, info, _ in candidates:
            row = conn.execute(
                "SELECT c.state, c.position FROM cursors c JOIN sources s ON s.id = c.source_id"
                " WHERE s.client = ? AND s.path = ?", (client, path)).fetchone()
            state = row[0] if row else "not_started"
            bucket = states.setdefault(client, {})
            bucket[state] = bucket.get(state, 0) + 1
            if client != "hermes":
                summary[client].setdefault("bytes_in_scope", 0)
                summary[client]["bytes_in_scope"] += info.st_size
                summary[client].setdefault("bytes_covered", 0)
                summary[client]["bytes_covered"] += min(row[1], info.st_size) if row else 0
        for client in CLIENTS:
            summary[client]["sources_by_state"] = states.get(client, {})
        # Out-of-window accounting: every old file not already followed, minus the ones
        # whose admission this run genuinely registered a cursor for. A file selected for
        # admission that a global budget never reached has no cursor yet and stays
        # counted as out of window, so it is retried on the next run.
        admitted = 0
        for client, path, _ in selected:
            if conn.execute("SELECT 1 FROM cursors c JOIN sources s ON s.id = c.source_id"
                            " WHERE s.client = ? AND s.path = ?", (client, path)).fetchone():
                admitted += 1
                summary[client]["admitted"] += 1
        for client in CLIENTS:
            summary[client]["out_of_window"] = max(
                0, sum(1 for c in old if c[0] == client) - summary[client]["admitted"])
            summary[client]["selected"] = sum(1 for c in selected if c[0] == client)
        if bf is not None:
            bf["admitted"] = admitted
        # Coverage needs per-source cursor states across every in-scope source.
        source_states = {}
        for client, path, info, _ in candidates:
            row = conn.execute(
                "SELECT c.state FROM cursors c JOIN sources s ON s.id = c.source_id"
                " WHERE s.client = ? AND s.path = ?", (client, path)).fetchone()
            source_states[(client, path)] = row[0] if row else "not_started"
        remaining_total = sum(summary[client]["out_of_window"] for client in CLIENTS)
        pending_new = 0
        if bf is not None and bf["state"] == "running":
            pending_new = max(0, len(old) - admitted)  # old files waiting for admission
        # Truncated discovery, unreadable roots and missing sources mean the set of files
        # on disk is not known to be complete: the watermark must then be null.
        gaps = any(summary[client][key] for client in CLIENTS
                   for key in ("missing", "discovery_truncated")) or any(
            summary[client]["root"] == "unreadable" for client in CLIENTS)
        backfill = _backfill_coverage(bf, candidates, source_states, remaining_total,
                                      pending_new, gaps)
        if backfill is not None:
            backfill["error"] = backfill_error
        elif backfill_error is not None:
            backfill = {"config": None, "admitted_files": 0,
                        "remaining_out_of_window": remaining_total,
                        "watermark_mtime": None, "contiguous": False, "error": backfill_error}
        stored = dict(conn.execute(
            "SELECT s.client, count(*) FROM entries e JOIN sources s ON s.id = e.source_id"
            " WHERE " + _visible(conn) + " GROUP BY s.client").fetchall())
        result = {
            "status": "ok", "started_at": started,
            "elapsed_seconds": round(time.monotonic() - clock, 3),
            "stopped_by": stopped_by, "pending_before_run": len(pending), "visited": visits,
            "scope": {"since_days": config["since_days"],
                      "rule": "sources modified within since_days, plus every source"
                              " already followed; older history is counted, not collected"
                              " unless backfill admits it"},
            "backfill": backfill,
            "clients_selected": list(config["clients"]),
            "client_selection": config["client_selection"],
            "budgets": {k: config[k] for k in CONFIG_LIMITS if k != "since_days"},
            "clients": summary, "stored_entries": stored,
        }
        conn.execute("BEGIN IMMEDIATE")
        conn.execute("INSERT INTO collector_runs(started_at, finished_at, outcome, summary)"
                     " VALUES (?, ?, ?, ?)", (started, _now(), "ok", json.dumps(result)))
        conn.execute("DELETE FROM collector_runs WHERE id <= (SELECT max(id) FROM collector_runs)"
                     " - ?", (RUNS_KEPT,))
        conn.execute("COMMIT")
        return result
    finally:
        if conn.in_transaction:
            conn.execute("ROLLBACK")
        conn.close()


COMMANDS = {"import": cmd_import, "search": cmd_search, "read": cmd_read, "status": cmd_status,
            "collect": cmd_collect, "quarantine": cmd_quarantine}


def main(argv=None):
    parser = argparse.ArgumentParser(description="Private local transcript archive prototype")
    parser.add_argument("--database", required=True)
    sub = parser.add_subparsers(dest="command", required=True)
    imp = sub.add_parser("import")
    imp.add_argument("--client", choices=CLIENTS, required=True)
    imp.add_argument("--source", required=True)
    imp.add_argument("--max-records", type=int, default=DEFAULT_MAX_RECORDS)
    search = sub.add_parser("search")
    search.add_argument("query")
    search.add_argument("--limit", type=int, default=10)
    read = sub.add_parser("read")
    read.add_argument("id", type=int)
    sub.add_parser("status")
    collect = sub.add_parser("collect")
    collect.add_argument("--home", required=True)
    collect.add_argument("--config", required=True)
    quarantine = sub.add_parser("quarantine")
    quarantine.add_argument("--list", action="store_true")
    quarantine.add_argument("--release", type=int)
    args = parser.parse_args(argv)
    try:
        result = COMMANDS[args.command](args)
    except ArchiveError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    except (sqlite3.Error, OSError, UnicodeError) as exc:
        print(f"error: {type(exc).__name__}", file=sys.stderr)
        return 2
    try:
        json.dump(result, sys.stdout, ensure_ascii=False, indent=2)
        print()
        sys.stdout.flush()
    except BrokenPipeError:
        # Reader went away; keep the interpreter's exit flush from raising again.
        os.dup2(os.open(os.devnull, os.O_WRONLY), sys.stdout.fileno())
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
