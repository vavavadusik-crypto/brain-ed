# Portability map: macOS and Windows

Status: analysis only (G6). No code was changed for this document. Every claim
carries a `file:line` citation against branch `g-portability`; lines were read,
not inferred. Items that could not be verified from the code alone are listed
in [§7 Open questions](#7-open-questions), not guessed.

- Date: 2026-10-26
- Scope: brain-core, brain-cli, tools/, packaging/, install.sh, tests/, vendor/sqlite-vec,
  .cargo/, .github/workflows/, rust-toolchain.toml
- Effort scale: **S** < 1 day, **M** 1–5 days, **L** > 1 week (one engineer).
- Risk column: the security risk *introduced by the replacement* (not the risk of staying on Linux).

## 1. Method

The whole tree was walked with targeted searches for the platform markers
named in the brief plus every hit they imply:
`renameat2|RENAME_EXCHANGE|O_NOFOLLOW|O_CREAT|O_EXCL`, `flock|fcntl|F_SETLEASE|try_lock`,
`0o600|0o700|PermissionsExt|set_permissions|from_mode`, `systemd|systemctl|\.timer`,
`/proc|/etc|/dev/|/run/`, `libc|std::os::unix|nix::`, `cfg(unix)|cfg(target_os)|cfg(not(unix))`,
`termios|signal|SIG`, `musl|zig|sqlite-vec`, `HOME|XDG|user-dirs`,
`symlink|read_link|symlink_metadata|canonicalize`, `umask|geteuid|chown`,
`UnixListener|UnixStream|MSG_DONTWAIT|F_DUPFD|localtime_r|ETXTBSY`.
Rough counts: 107 Rust files / ~82k lines; `std::os::unix`/`libc` references in
40 source files; `O_NOFOLLOW` in 18 files (brain-cli 14, brain-core 3, tools 2).

## 2. Executive summary

Brain ED is Linux-first by design: its security model (owner-only files,
no-symlink opens, uid checks, file leases) and its scheduling model (systemd
user timers) lean on Linux specifics deliberately and fail closed elsewhere.
The porting cost is concentrated in six places:

1. **Whole-home restore** (`brain-cli/src/backup.rs:444-575`): Linux file
   leases (`F_SETLEASE` + `SIGIO`) plus `renameat2(RENAME_EXCHANGE)` — the only
   place with no real equivalent on macOS or Windows. **L**.
2. **The secure-open pattern** (`O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC` + uid + mode +
   dev/ino recheck), repeated in 18 files — needs one cross-platform helper;
   the pattern is mechanical but wide (counted conservatively as **L**).
3. **`current_uid()`** via `/proc` in 6 modules — macOS trivial, Windows needs
   a SID/token decision. **M**.
4. **systemd user timers** (distill + backup) — launchd on macOS, Task
   Scheduler on Windows. **M** each.
5. **Hook stdin non-blocking read** (`hook.rs:213-301`) — `/proc/self/fd/0`
   reopen and `MSG_DONTWAIT` are Linux-only; macOS/Windows need poll/PeekNamedPipe
   designs. **L**.
6. **Distribution**: musl+Zig static ELF, tar bundles, bash installers,
   ubuntu-only CI — macOS needs codesigning/notarization, Windows needs MSVC +
   Authenticode + a real installer. **L** (mostly new packaging, not code).

Totals: 90 catalogued findings (86 effort-bearing; 4 rows are n/a by design) —
**S 36 / M 40 / L 10** (per-category counts in [§5](#5-effort-totals)).

## 3. What already anticipates non-Linux platforms

These branches exist today and reduce the work (or deliberately degrade):

- `brain-cli/src/background.rs:336-338` — `own_image()` falls back to
  `std::env::current_exe()` off Linux (weaker: may start a replaced binary;
  documented in the comment).
- `brain-cli/src/hook.rs:232-234` — off Linux the hook reads no stdin at all
  (fail-safe: no input journaled).
- `brain-cli/src/ui.rs:613-620` — stale-window detection off Linux reports
  "close the old window yourself" instead of scanning `/proc`.
- `brain-cli/src/ui.rs:1975-1995` — `open_browser` already has macOS (`open`)
  and Windows (`cmd /C start`) branches.
- `brain-cli/src/ui.rs:1878-1882` — `fresh_token` errors out off Unix.
- `brain-core/src/sharing.rs:2022-2029, 2057-2060` — off Unix the private
  dir/file creation is a plain `create_dir_all` / no-op (**security gap on
  Windows**, see §4.2).
- `brain-core/src/storage/sqlite.rs:123-125` — off Unix no private file is
  created (same gap class).
- `brain-core/src/network/tls.rs:304-306` — `current_uid()` off Linux returns
  `None`, and `open_source` then fails closed ("cannot establish the current
  uid", tls.rs:386-390).
- `brain-cli/src/client_registry.rs:363-365, 526-530` — off Unix
  `owner_only()` is `false` and identity checks are `Uninspectable` (fail
  closed).
- `brain-cli/src/private_fs.rs:68-75` — off Unix falls back to plain
  `create_dir_all`.
- `brain-cli/src/desktop.rs:495-497` — `make_executable` is a no-op off Unix.
- `brain-cli/src/main.rs:2744-2753` — `stable_brain_binary` already accepts
  `brain.exe`.
- `brain-cli/src/installer.rs:24` — hand-rolled `O_NOFOLLOW` is guarded to
  linux x86/x86_64 only, i.e. **silently absent** on macOS (a trap, see §4.3).

## 4. Catalog of platform-specific places

Format: **ID** — `file:line` — what it does — macOS replacement — Windows
replacement — effort — risk of the replacement.

### 4.1 Home, paths, environment

| ID | Place | What it does | macOS | Windows | Effort | Risk |
|----|-------|--------------|-------|---------|--------|------|
| P1 | `brain-cli/src/main.rs:2738-2743` (`user_home`), `:2734-2736` (`brain_home`) | Reads `$HOME` only; default brain home is `$HOME/.brain`; unset HOME becomes `.` | Works as-is (HOME is set); recommend `dirs::home_dir()` for robustness | HOME is usually unset — must use `%USERPROFILE%` / `SHGetKnownFolderPath(FOLDERID_Profile)` (crate `dirs`) | S | Low — but every `is_safe_absolute` check must accept `C:\…` paths (they are `Path::is_absolute()`-true) |
| P2 | `brain-cli/src/private_fs.rs:23-48` (`UNSAFE_HOME`, `is_safe_absolute`, `require_safe_paths`) | Refuses non-absolute, non-UTF-8 or control-character homes | Works as-is | Windows paths can be non-UTF-16-representable; OsStr→String lossiness policy must be defined (see §7 q3); drive-relative `C:foo` is not absolute | S | Low — fail-closed preserved |
| P3 | `brain-cli/src/desktop.rs:89-113` (`Layout::resolve`) | XDG base dirs: `XDG_DATA_HOME`→`~/.local/share/applications`, `…/icons`, `XDG_CONFIG_HOME`→`~/.config` | Replace with `~/Library/Application Support` + `dirs::data_dir()`; icons/launchers move into `.app` bundles | `%APPDATA%`/`%LOCALAPPDATA%` via `dirs`; Start Menu shortcuts instead of `.desktop` | M | Low — worst case shortcuts land nowhere |
| P4 | `brain-cli/src/desktop.rs:398-423` (`desktop_directory`, `parse_user_dir`) | Reads `~/.config/user-dirs.dirs` for the localized Desktop dir; falls back to `~/Desktop`, `~/Рабочий стол` | `~/Desktop` is fixed; `user-dirs.dirs` never exists — fallback suffices | `SHGetKnownFolderPath(FOLDERID_Desktop)` (crate `windows`) | M | Low |
| P5 | `brain-cli/src/installer.rs:659-660, 902-904, 1236-1247` | Writes client configs under `~/.config/opencode/…`, agent homes from `CLAUDE_CONFIG_DIR`/`CODEX_HOME`/`HERMES_HOME` (`client_home.rs:62-63`) | `.config` is a Linux convention; on macOS those CLIs also use `~/.claude` etc. — needs a per-client table (see §7 q7) | Same; clients use `%USERPROFILE%\.claude`, `%APPDATA%` | M | Medium — writing to the wrong client path could touch another tool's config |
| P6 | `brain-cli/src/explore.rs:213-250` (`source_group`) | Display heuristic grouping sources by path shape (`$HOME/…`, `/run/media/…`) | Extend with `/Users/<u>/…` | Extend with `C:\Users\…`; path splitting currently assumes `/` separators | S | None (display only) |
| P7 | `brain-cli/src/net.rs:832-840` (`window_observation_path`) | Runtime observation file under `temp_dir()/brain-ed-net-{uid}` | Works; uid comes from `current_uid()` (§4.4 L1) — off-Linux uid is `None→u32::MAX` | Use SID-derived key instead of uid | S | Low — collision if uid unknown: all users share the `u32::MAX` bucket (must not happen: gate on uid presence) |

### 4.2 Owner-only modes and ownership (0600/0700)

macOS keeps the POSIX model unchanged — the citations below port verbatim.
Windows has no mode bits; every row needs an ACL strategy: create with a
**PROTECTED_DACL** granting only the current user (crates: `windows`
`SetNamedSecurityInfoW`, or `windows-acl`), and replace "mode & 0o077" checks
with an `AccessCheck` for the current user + detect inherited
group/world-readable ACEs. Risk on every row: **an ACL mistake widens access
instead of narrowing it; tests must read back the DACL.**

| ID | Place | What it does | Effort |
|----|-------|--------------|--------|
| M1 | `brain-cli/src/private_fs.rs:17,20` (`PRIVATE_DIR=0o700`, `PRIVATE_FILE=0o600`), `:58-75` (`create_private_dir_all`), `:77-95` (`ensure_private_home`) | The central owner-only directory/file creation used by every integration point | M |
| M2 | `brain-cli/src/private_fs.rs:107-133` (`write_private`, `write_private_via`) | Private temp file + `fs::rename` publish, 0600 from first byte, `sync_all` | M (rename+sync are portable; mode part is the `[cfg(unix)]` block at 121-124) |
| M3 | `brain-cli/src/brief.rs:236-253` (`ensure_private_directory`, `write_private_file`, `repair_private_file`) | 0700 dir / 0600 file for the brief cache | S |
| M4 | `brain-cli/src/maintenance.rs:104-118` | `<db>.pre-redact-<stamp>` snapshot created 0600 | S |
| M5 | `brain-core/src/storage/sqlite.rs:110-125` (`create_private_database_file`) | Pre-creates the DB 0600 before SQLite opens it; **no-op off Unix** — on Windows the DB inherits profile ACLs (see §7 q11) | M |
| M6 | `brain-core/src/sharing.rs:2012-2029` (`create_private_dirs`, 0700; plain fallback off Unix), `:2031-2060` (`set_file_private`, 0600 + O_NOFOLLOW, clamps `-wal`/`-shm`; **no-op off Unix**) | Share databases owner-only | M |
| M7 | `brain-core/src/network/auth.rs:260-285` | Auth store: 0700 state dir, 0600 `auth.sqlite`, clamps `-wal`/`-shm` to 0600 | M |
| M8 | `brain-core/src/network/client.rs:210-243` (`read_credential_from`), `:257-277` (`write_credential_file`) | Credential file must be regular, ≤4 KiB, 0600, own uid; created 0600 in 0700 parent | M |
| M9 | `brain-core/src/network/tls.rs:388-395` (private key must have no group/other bits), `:1148-1156` (generated key set to 0600) | TLS material privacy | M |
| M10 | `brain-core/src/map/store.rs:210-213` (`current_uid`), `:215+` (`check_private` uid/0o022), `:279` (WAL clamp 0600) | Map store privacy | M |
| M11 | `brain-core/src/map/ingest.rs:157-159` (`current_uid`), `:161+` (`check_owned`) | Same for ingest | M |
| M12 | `brain-core/src/coordination.rs:22, 524-560` (`check_private_leaf`) | Refuses symlink / other-uid / group-or-other-readable coordination DB | M |
| M13 | `brain-cli/src/client_registry.rs:357-365` (`owner_only`), `:369-407` (`read_private`), `:510-530` (`identity`: uid + 0o022 + dev/ino) | Registry files private by construction | M |
| M14 | `brain-cli/src/installer.rs:1040-1077` (`path_probe` owner_only + dev/ino recheck) | Client config files must be private before they are read/written | M |
| M15 | `brain-cli/src/hook.rs:304-355` (`record_event`, `trusted_brain_home`) | Brain home must be 0700-class dir owned by this user, parent not world-writable | M |
| M16 | `brain-cli/src/hook.rs:435-461` (`open_checked`: uid, 0o077, nlink, dev/ino), `:499-519` (`append_private_json_line`: 0600 create) | Journal/dedupe state privacy | M |
| M17 | `brain-cli/src/background.rs:713-728` (`open_lock_file`: 0600 + O_NOFOLLOW) | Worker lock file privacy | S |
| M18 | `brain-cli/src/settings_history.rs:40-63` (`private_open` 0600 + clamp, `lock`) | Settings history privacy | S |
| M19 | `brain-cli/src/llm_command.rs:195-201` (`private_file` 0600 + O_NOFOLLOW) | LLM command private file | S |
| M20 | `brain-cli/src/remote_mcp.rs:87-110` (`load`: own uid, 0o077, O_NOFOLLOW, regular) | Remote config privacy | S |
| M21 | `brain-cli/src/archive.rs:155-190` (`current_uid`, `open` checks) | Archive DB must be owner-only before opening | S |
| M22 | `brain-cli/src/desktop.rs:487-494` (`make_executable` 0o755; no-op off Unix) | Launcher copy executable bit | S (Windows: exec is a property of the extension, no work) |
| M23 | `brain-cli/src/ui.rs:217-258` (`open_owner`) | Refuses to open the UI unless `~/.brain/ui` is a private dir of this user | S |
| M24 | `brain-cli/src/net.rs:852-862` | Window observation file must be own uid + 0o077 | S |
| M25 | `brain-cli/src/distill.rs:692-700` | keep-raw dir must be 0700 and owned by `geteuid()` | S (macOS) / M (Windows ACL) |
| M26 | `brain-core/src/network/server.rs:957` (test: 0700 state dir) | Test-only; ports with the auth store (M7) | — |

macOS effort total for §4.2 is essentially **S** (semantics identical); the
Windows ACL layer is one shared helper plus per-site integration (**M** each,
heavier for M5/M6/M8 where a wrong default silently persists).

### 4.3 Secure open: `O_NOFOLLOW`/`O_NONBLOCK`/`O_CLOEXEC` and dev/ino recheck

The recurring pattern is: `symlink_metadata` → mode/uid/size checks → open with
`custom_flags(libc::O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC)` → re-stat the descriptor
and compare `dev()`/`ino()` with the pre-open metadata. Found in: hook.rs
(`:435-461`, `:442`, `:506-510`), client_registry.rs (`:388`, `:605`, `:628`),
client_integration.rs (`:269`, `:339`), backup.rs (`:259`, `:489`),
settings_history.rs (`:49`, `:71`), remote_mcp.rs (`:93`), net.rs (`:855`),
import.rs (`:1419`), llm_command.rs (`:201`, `:407`), bundle.rs (`:256`),
installer.rs (`:1057`), background.rs (`:719`), distill.rs (`:684`, `:718`),
sharing.rs (`:2040`), network/client.rs (`:218`), network/tls.rs (`:348`).

| ID | Place | What it does | macOS | Windows | Effort | Risk |
|----|-------|--------------|-------|---------|--------|------|
| S1 | all of the above (helper to be extracted) | Refuse to follow a final-component symlink; refuse to block on a FIFO; keep fd out of children; verify the inode did not change between check and use | `O_NOFOLLOW` exists on Darwin; `O_DIRECTORY` does not (plain `open` of a dir works — verify, §7 q1); `O_CLOEXEC` exists | `OpenOptionsExt::custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)` opens the reparse point itself (analog of O_NOFOLLOW for the final component); `FILE_FLAG_FIRST_PIPE_INSTANCE`/overlapped handles replace O_NONBLOCK semantics; no CLOEXEC concept (handle inheritance is explicit); dev/ino analog: `std::os::windows::fs::MetadataExt::{volume_serial_number, file_index}` on the opened handle | L (one helper + mechanical call-site conversion) | Medium — O_NOFOLLOW/O_NONBLOCK nuances differ per filesystem; a wrong flag silently disables symlink refusal. Add per-platform tests |
| S2 | `brain-cli/src/client_registry.rs:34-36`, `brain-cli/src/installer.rs:24` | Hand-rolled flag constants `O_NOFOLLOW=0o400000`, `O_NONBLOCK=0o4000`, guarded `#[cfg(all(target_os="linux", any(x86, x86_64)))]` | **Trap**: on Darwin `O_NOFOLLOW` is 0o400 (0x100), not 0o400000. The guard makes the code compile but the protection vanish. Use `libc::O_NOFOLLOW` (libc is already a dependency) | Same constants are meaningless on Windows; use the FILE_FLAG_* form | S | High if missed (silent loss of symlink refusal) — trivial to avoid |
| S3 | `brain-cli/src/bundle.rs:240-268` (`join`) | Reads the bundle with `O_NOFOLLOW`, maps `ELOOP` to a clear refusal | Same flag exists | REPARSE_POINT flag + `GetFileInformationByHandleEx` check | S (with S1 helper) | Low |

### 4.4 `/proc`, `/dev`, `/etc` — Linux OS-surface reads

| ID | Place | What it does | macOS | Windows | Effort | Risk |
|----|-------|--------------|-------|---------|--------|------|
| L1 | `brain-cli/src/private_fs.rs:97-105` (`current_uid`), reused by `client_registry.rs:336-343`, `net.rs:835`, `hook.rs:329-331`, `remote_mcp.rs` | Owner of `/proc/self`, else owner of `$HOME` | `libc::geteuid()` directly (no /proc on Darwin) — S | No uid: use the process token SID (`windows` crate `GetCurrentProcessToken`, crate `whoami`); decide the "owner" type once (§7 q4) | M | Medium — a wrong owner comparison on Windows could accept foreign files |
| L2 | `brain-core/src/network/tls.rs:297-306` | `current_uid` parsed from `/proc/self/status` `Uid:` line | `libc::geteuid()` | SID as above | S | Low (fail-closed today: `None` → refuse, tls.rs:386-390) |
| L3 | `brain-core/src/map/ingest.rs:157-159`, `brain-core/src/map/store.rs:210-213`, `brain-cli/src/archive.rs:155-160` | `current_uid` via `fs::metadata("/proc/self")` — **hard error** off Linux (map/ingest refuses to run) | `libc::geteuid()` | SID | M | Low |
| L4 | `brain-cli/src/background.rs:326-338` (`own_image`) | Spawns the vector worker from `/proc/self/exe` so an install-time rename can't swap generations | `libproc::proc_pidpath` or `_NSGetExecutablePath` (no "(deleted)" detection — degrade to current_exe, already the fallback) | `GetModuleFileNameW`; "(deleted)" analog: compare file id of path vs running image | M | Low — worst case is the documented degraded fallback |
| L5 | `brain-cli/src/doctor.rs:25-35` | Reports `executable_replaced_since_start` via `read_link("/proc/self/exe")` ends with " (deleted)" | fstat the running image vs path (no direct link text) | file-id comparison as above; or report `None` | S | Low (diagnostic only) |
| L6 | `brain-cli/src/client_integration.rs:85-101` (`Source::RunningImage`) | Reads the running image bytes from `/proc/self/exe` for self-copy installs; **unsupported error off Linux** (`:96-101`) | `_NSGetExecutablePath` + `open`; bytes of a running binary are readable | `GetModuleFileNameW` + `CreateFile` (read sharing allowed) | M | Medium — self-copy installs must verify bytes match the running image |
| L7 | `brain-cli/src/hook.rs:213-234` (`open_stdin_nonblocking`) | Reopens `/proc/self/fd/0` with its own `O_NONBLOCK` so the client's descriptor flags are never touched; stream sockets via `MSG_DONTWAIT` dup (§4.5 K6); **off Linux reads nothing** | No `/proc`; `/dev/fd/N` on macOS re-opens through fdescfs and may share the open file description (§7 q5) — the safe replacement is `poll(2)`/`kqueue` with a deadline, which needs no flag changes | Pipes: `PeekNamedPipe` with deadline (no flag mutation); console: `WaitForSingleObject`+`ReadConsoleInput`/`ReadFile` in overlapped mode; sockets: `WSAEventSelect` or toggle `FIONBIO` around `recv` | L | Medium — deadline-bounded reads must not block the client's pipe; wrong handling hangs the agent client |
| L8 | `brain-cli/src/hook.rs:236-301` (`StreamSocket`) | Duplicates fd 0 (`F_DUPFD_CLOEXEC`), checks `SOCK_STREAM`, reads with per-call `MSG_DONTWAIT` | `fcntl(F_DUPFD_CLOEXEC)` + `SO_TYPE`/`SOCK_STREAM` check; per-call non-blocking → use `MSG_DONTWAIT` (Darwin has it) or poll | `WSADuplicateSocket`/`DuplicateHandle`; per-call non-blocking does not exist — set `FIONBIO`, `recv`, restore (§7 q6) | L | Medium — toggling flags on a shared socket can race other readers |
| L9 | `brain-cli/src/import.rs:1416-1431` | After opening a source no-follow, resolves `/proc/self/fd/{fd}` to verify the opened inode is still the scanned path | `/dev/fd/{fd}` (verify it resolves to the path, §7 q5) | `GetFinalPathNameByHandleW` | S (macOS) / M (Windows) | Low — check is belt-and-braces over dev/ino |
| L10 | `brain-core/src/network/tls.rs:309-334` (`open_inherited_fd`) | Reopens an inherited fd via `/proc/self/fd/N` with `O_NONBLOCK|O_CLOEXEC` so TLS key material can arrive over a descriptor | Same caveat as L7; `dup` + fstat validation | `DuplicateHandle` + handle validation | M | Medium — inherited-handle trust model differs per platform |
| L11 | `brain-cli/src/hook.rs:369-382` (`group_is_private`, reads `/etc/group`+`/etc/passwd`), duplicated in `brain-cli/src/llm_command.rs:165-193` | "User-private group" check (Ubuntu scheme) allows group-writable parents only if the group is private | Same files exist but may be NSS/opendirectoryd-backed — use `getgrgid_r`/`getpwuid_r` (libc) instead of parsing files | No concept: drop the group-writable allowance entirely (parents must be owner- or SYSTEM-only) | M | Medium — a wrong "private" verdict lets a peer rename `.brain` (the check exists to prevent exactly that, hook.rs:337-355) |
| L12 | `brain-cli/src/ui.rs:542-611` (`stop_listener`), `:533-540` | Finds the stale `brain ui` pid via `/proc/net/tcp` inode → `/proc/*/fd` scan; kills with `kill -TERM` | `lsof -ti tcp:PORT -sTCP:LISTEN` or libproc enumeration; SIGTERM exists | `GetExtendedTcpTable(TCP_TABLE_OWNER_PID_LISTENER)` (iphlpapi) gives the owning pid directly; `TerminateProcess` (no graceful TERM for arbitrary processes — see §7 q8) | M | Medium — killing the wrong pid; verify ownership before `TerminateProcess` |
| L13 | `brain-cli/src/net.rs:842-848` (`process_start`) | Reads `/proc/{pid}/stat` field 22 (start time) to detect pid reuse | `sysctl(KERN_PROC)` or libproc `proc_pidinfo` | Compare process creation time via `GetProcessTimes` | S | Low |
| L14 | `brain-cli/src/ui.rs:1868-1884` (`fresh_token`), `brain-cli/src/net.rs:1139-1147` (`fresh_request_id`) | 32/12 random bytes from `/dev/urandom`; **hard error off Unix** | `getentropy(2)` / `SecRandomCopyBytes`; simplest: crate `getrandom` | `ProcessPrng`/`BCryptGenRandom`; same crate | S | Low — OS CSPRNG is strictly better than reading the device |

### 4.5 libc calls and kernel primitives

| ID | Place | What it does | macOS | Windows | Effort | Risk |
|----|-------|--------------|-------|---------|--------|------|
| K1 | `brain-cli/src/backup.rs:444-575` (`atomic_home`: `Leases::acquire` `:463-501`, `SIGIO` handler `:449-457, 566`, `F_SETLEASE/F_WRLCK` `:493`, unlock `:561`) | Write-leases on DB files: any other open descriptor (incl. idle WAL holders) fails the lease, and an attempted open during restore raises `SIGIO` → abort. The restore fails closed without it | **No leases on Darwin.** Options: (a) degrade to exclusive `flock` + documented "close all clients" precondition (keeps fail-closed, loses break-on-open); (b) advisory check then proceed. No API can detect opens-in-flight | Windows oplocks are the real analog: `FSCTL_REQUEST_OPLOCK_LEVEL_2` breaks on any new open, completion via OVERLAPPED/IOCP (crate `windows-sys`). Restore could hold oplocks and abort on break | L | High — this is the atomic-restore correctness core. A weaker primitive silently admits concurrent writers during restore; must fail closed instead (the code already does: backup.rs:494-496 error message) |
| K2 | `brain-cli/src/backup.rs:524-553` (`rename_exchange`: glibc `renameat2` `:526-535`, musl raw syscall `:539-552`) | Atomic whole-directory swap of live home ↔ staged snapshot (`RENAME_EXCHANGE`) | `renamex_np(2)` with `RENAME_SWAP` (macOS 10.12+) — verify flag semantics (§7 q2); if unavailable: staged rename dance with a recovery journal (rename old→prev, new→home, delete prev) | `SetFileInformationByHandle(FileRenameInfoEx)` with `FILE_RENAME_FLAG_POSIX_SEMANTICS|FILE_RENAME_FLAG_REPLACE_IF_EXISTS` (Win10 1607+) replaces a file atomically, but there is **no directory exchange**; same staged dance + NTFS transaction is dead — use recovery journal | L | High — the current design guarantees "no rename gap and no recovery journal" (backup.rs:571-575); replacements reintroduce a window that must be made crash-safe and documented |
| K3 | `brain-cli/src/distill.rs:644-730` (`raw_directory` `openat`/`mkdirat` with `O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC` `:668-687, 683-686`, `geteuid` `:695`; `save_raw` `openat` `O_EXCL` `:705-730`) | Descriptor-walked keep-raw directory: no symlink in any component | `openat`/`mkdirat` exist on Darwin; drop `O_DIRECTORY` (absent), keep `O_NOFOLLOW` | No `openat` in Win32; use `NtCreateFile` with `OBJECT_ATTRIBUTES.RootDirectory` for relative opens, or the `cap-std` crate for portable capability-style traversal | M | Medium — descriptor-walk guarantees are the point; a path-based reimplementation re-opens TOCTOU gaps |
| K4 | `brain-cli/src/distill_schedule.rs:86-96` (`localtime_r`) | Quiet-hours uses the local hour via `localtime_r` | Same libc call exists | CRT `localtime_s`, or `chrono::Local` | S | Low |
| K5 | `brain-cli/src/desktop.rs:459-477` (`run_version_probe`, `TEXT_FILE_BUSY=26` retry) | Retries `--version` while a forked child holds the fresh binary open (`ETXTBSY`) | `ETXTBSY` exists identically | Not applicable (Windows blocks writing a running exe outright); retry loop is harmless | S | None |
| K6 | `brain-cli/src/hook.rs:236-301` | `F_DUPFD_CLOEXEC` + `MSG_DONTWAIT` (see L8) | Both exist on Darwin | See L8 | — (same work as L8) | see L8 |
| K7 | `brain-cli/src/main.rs:2386-2392` | Forwards graphify's child status using `ExitStatusExt::signal()` (128+signal convention) | `ExitStatusExt` exists (unix) | No signals; `ExitStatus` carries NTSTATUS — map to a documented exit-code convention | S | Low |
| K8 | `std::os::unix::ffi::OsStrExt` uses: `brain-cli/src/backup.rs:503`, `brain-cli/src/distill.rs:655`, `brain-cli/src/client_home.rs:132` (test) | Treats OsStr as raw bytes for `CString`/path matching | Same (OsStr is UTF-8 on Darwin) | Windows OsStr is WTF-16; byte-level path logic needs a UTF-8 policy (§7 q3) | M | Medium — non-UTF-8 Windows paths silently mishandled if ignored |

### 4.6 File locking (flock)

| ID | Place | What it does | macOS | Windows | Effort | Risk |
|----|-------|--------------|-------|---------|--------|------|
| F1 | `brain-cli/src/background.rs:666-703, 731-740` (`WorkerLock`) | Exclusive, non-blocking `File::try_lock` on a persistent lock file; kernel drops it on death; explicit unlock so forked children cannot pin it | `try_lock` is flock(2) on Darwin — works as-is (std is cross-platform; toolchain 1.97.1) | std `File::try_lock` maps to `LockFileEx` — works; **note**: fork-inheritance semantics (background.rs:660-664) don't exist (no fork); the comment and the test at `:909-941` need a Windows variant | S | Low |
| F2 | `brain-cli/src/client_registry.rs:566-597, 610-680` (`RegistryLock`) | Same pattern guarding registry read-modify-write | works | works | S | Low |
| F3 | `brain-cli/src/settings_history.rs:58-63` (`lock`) | Blocking `File::lock` on the history lock | works | works | S | Low |
| F4 | `tools/session_archive.py:1647-1656` | `fcntl.flock(LOCK_EX|LOCK_NB)` collector lock (created 0600 + O_NOFOLLOW `:1646-1648`) | fcntl.flock exists on macOS | Python has no portable flock: `msvcrt.locking` on a re-opened fd, or a lockdir with O_EXCL semantics | M | Medium — lock semantics across processes on Windows network drives are unreliable; fail closed |

### 4.7 Scheduling: systemd user timers

| ID | Place | What it does | macOS | Windows | Effort | Risk |
|----|-------|--------------|-------|---------|--------|------|
| D1 | `brain-cli/src/distill_schedule.rs:10-11` (unit names), `:30-37` (`systemctl --user`), `:146, 213-224` (status/enable/disable/daemon-reload) | Distill timer management via `systemctl --user`; every call is a spawned process | launchd user agents: write `~/Library/LaunchAgents/brain-ed.distill.plist`, `launchctl bootstrap gui/$(id -u)` / `bootout`; status via `launchctl print` | Task Scheduler: `schtasks /Create /TN brain-ed-distill /SC HOURLY …` (simplest) or the `ITaskService` COM API via the `windows` crate for exact parity (interval, quiet hours) | M | Medium — timer runs under the user session; on Windows a missed logon means a missed run (document it) |
| D2 | `brain-cli/src/distill_schedule.rs:196-215` (unit text: `Nice=10`, `IOSchedulingClass=idle`, `OnBootSec/OnUnitInactiveSec`, `QuietHours` handled in-process at `:125-131`) | Generates the `.service`/`.timer` unit text; argument quoting via `archive::systemd_argument` (`archive.rs:2014-2027`) | plist with `StartInterval`/`StartCalendarInterval`; quiet hours must move into the command (already partly in-process) | schtasks XML or `/SC` flags; `Nice`/`IOSchedulingClass` have no direct analog (priority via `/RL` at best) | M | Low |
| D3 | `brain-cli/src/backup.rs:714-760` (`timer_units`, `write_timer_units`, unit constants `:737-738`, dir `:746`) | Daily backup timer unit files under `~/.config/systemd/user` | `~/Library/LaunchAgents/brain-ed.backup.plist` with `StartCalendarInterval` daily | schtasks daily task | M | Low |
| D4 | `brain-cli/src/archive.rs:1993-2060` (`timer_units`, `systemd_argument`, `write_timer_units`, `write_named_timer_units`) | Shared unit-file writer with strict argument validation (rejects controls, quotes, `%`, `$`, `;`) | The validation logic ports; the writer becomes per-OS | Same | M | Low — keep the strict argument validation, it is the injection defense |
| D5 | `brain-cli/src/main.rs:566` (`collect` timer CLI), `:784-831` (schedule CLI), `:982-1013` and `:1361-1375` (`systemctl` closures for backup/distill timers) | CLI surface wiring the above | Route to the launchd backend | Route to the schtasks backend | M | Low |
| D6 | `docs/INSTALL.md:108` | Documents the systemd user timer step | Update docs per OS | Same | S | None |
| D7 | `brain-cli/assets/settings.html:1026, 1041, 1437` | Settings window shows systemd timer state from the server JSON | Show launchd state | Show scheduled-task state | S | None |

### 4.8 Desktop integration

| ID | Place | What it does | macOS | Windows | Effort | Risk |
|----|-------|--------------|-------|---------|--------|------|
| G1 | `brain-cli/src/desktop.rs:88-113, 115-160` (`menu_entries`, `desktop_entries`, `install`) | Writes two `.desktop` launcher entries + SVG icons into `~/.local/share/applications` and the XDG desktop dir | Build a minimal `Brain.app` bundle (`Info.plist`, icon in `Contents/Resources`), register by `lsregister -f` or plain Finder discovery | Create Start Menu `.lnk` shortcuts (`IShellLink`+`IPersistFile` COM, `windows` crate; or generate `.lnk` bytes with a crate) | M | Medium — shortcuts pointing at the wrong binary silently break the trust model built around `~/.brain/bin/brain` |
| G2 | `brain-cli/src/desktop.rs:503-511` (`mark_trusted` via `gio metadata::trusted`) | GNOME refuses untrusted launchers; this marks them trusted | Not needed (Gatekeeper model is per-app, handled by codesigning) | Not needed | S | None |
| G3 | `brain-cli/src/ui.rs:1975-1995` (`open_browser`) | Launches the user's browser on the window URL; macOS/Windows branches already exist | Verify `open` arg handling (URL passed as argv — safe) | `cmd /C start ""` quoting is notoriously unsafe for attacker-controlled URLs; the URL here is self-generated (`http://127.0.0.1:…#k=…`), but keep the fragment form | S | Medium on Windows if the URL ever becomes user-influenced (cmd metacharacters) — prefer `explorer.exe <url>` or ShellExecuteW |
| G4 | `brain-cli/src/desktop.rs:459-477` (`run_version_probe`) | Text-busy retry after copying the binary | Works (same errno) | Not applicable; keep the loop | S | None |

### 4.9 Entropy

Folded into L14 (`ui.rs:1868-1884`, `net.rs:1139-1147`): replace
`/dev/urandom` reads with the `getrandom` crate (macOS `getentropy(2)`,
Windows `ProcessPrng`). Effort **S** both platforms; risk **low** (the OS
CSPRNG is strictly stronger than a device file, and both call sites already
fail closed off-Unix).

### 4.10 Build, packaging, distribution, CI

| ID | Place | What it does | macOS | Windows | Effort | Risk |
|----|-------|--------------|-------|---------|--------|------|
| B1 | `Cargo.toml:23-26` + `vendor/sqlite-vec/PATCH-NOTES.md` + `vendor/sqlite-vec/build.rs:2-4` | Vendored sqlite-vec 0.1.9 because upstream fails on musl (`u_int8_t` BSD types, guarded by `#ifdef __GLIBC__`); compiled with the `cc` crate | Upstream compiles on Darwin (BSD types exist); the vendored patch also compiles (typedefs skipped) — needs a build test (§7 q9) | MSVC: types absent, typedefs skipped → expected to compile; needs a build test (§7 q9). Push the upstream fix to drop the vendor dir | M | Medium — a silent vector-search regression if the C extension fails differently per toolchain |
| B2 | `.cargo/config.toml:1-13` | musl linkers (`x86_64-linux-musl-gcc`), `crt-static`, `-static` | New `[target.aarch64-apple-darwin]`/`x86_64-apple-darwin` sections (likely empty) | New `[target.x86_64-pc-windows-msvc]` section (likely empty); the Linux musl sections stay | S | Low |
| B3 | `packaging/build-release.sh:1-56` | Static ELF release pipeline: pinned Rust 1.97.1, Zig 0.15.1 cc/ar wrappers (`zig-tool.sh:1-17`), `readelf` INTERP/NEEDED check, SHA256SUMS | Universal2 (aarch64+x86_64) build with Apple clang; `codesign --deep` + `notarytool` submission; `otool -L` instead of readelf | MSVC target, no zig needed for the target toolchain; `signtool` Authenticode signing; `dumpbin`/PE checks instead of readelf | L | Medium — unsigned macOS/Windows binaries trigger SmartScreen/Gatekeeper; skipping signing is a supply-chain decision, not a shortcut |
| B4 | `packaging/install-linux.sh:1-60+` | Bundle verification (no symlinks/special files, strict SHA256SUMS, no group/world-writable modes `tests/test_linux_bundle.py:464-473`) then user-level install | `.app`/`.dmg` or pkg with notarization replaces most of this | MSI or a zip + signing; the no-symlink inventory check translates to NTFS (junctions instead) | L | Medium — the bundle verifier is a security boundary; a per-OS rewrite must re-prove the same properties |
| B5 | `packaging/install-source.sh:1-115` | Builds from source, runs `brain ui --install-shortcut` | Works once desktop integration (G1) is ported; xdg assumptions in docs | Same; needs MSVC toolchain prerequisites documented | M | Low |
| B6 | `install.sh:1-80` | Downloads a musl tarball, verifies SHA-256, installs `~/.local/bin/brain`; refuses non-Linux (`:32`), Termux notes (`:52-58`) | Nearly drop-in: change the OS guard, keep `shasum` fallback (`:53-56`); artifacts differ | PowerShell equivalent (`Invoke-WebRequest`, `Get-FileHash`) or winget/MSI; `$HOME/.local/bin` is not on PATH by default | S (macOS) / L (Windows) | Medium — Windows PowerShell execution policy and download trust dialogs are the new attack surface; keep the verify-then-install order |
| B7 | `.github/workflows/ci.yml:21,35,51,89,101,111,171,200`, `release.yml:9,14`, `linux-bundle.yml:19` | All jobs on `ubuntu-24.04`; releases build musl x86_64+aarch64 only | Add `macos-latest` jobs + release targets | Add `windows-latest` jobs + release targets | S | Low |
| B8 | `docs/build-release.md:1-30`, `docs/linux-download.md:122` | Documents musl/Zig pipeline and its unproven-distribution status | Extend per OS | Extend per OS | S | None |
| B9 | `tools/package-linux.py:136,283,400,410,491,475-480,617` | Produces the Linux tar.gz with the same no-follow/O_EXCL/0o755 discipline | Linux artifact — stays as-is | Linux artifact — stays as-is | — | None (out of scope by design; the per-OS artifacts are new work, see B3/B4) |

### 4.11 Python tools (repository tooling)

| ID | Place | What it does | macOS | Windows | Effort | Risk |
|----|-------|--------------|-------|---------|--------|------|
| T1 | `tools/session_archive.py:258, 350, 367, 1170, 1245, 1498, 1647` (`O_NOFOLLOW`; `O_NONBLOCK` at `:1245`) | Never follows symlinks when reading sources or creating the archive DB | `os.O_NOFOLLOW` exists on macOS — works | `os.O_NOFOLLOW` does not exist on Windows: `os.lstat` + recheck, or `CreateFile` with `FILE_FLAG_OPEN_REPARSE_POINT` via pywin32 | S (macOS) / L (Windows) | Medium — the collector is a security boundary against hostile session sources; partial re-implementation is risky |
| T2 | `tools/session_archive.py:336-337` (0700 dirs), `:343-345` (`os.geteuid`, 0o022 check), `:367-369` (0600 + O_EXCL + O_NOFOLLOW create), `:1646-1648` (0600 lock) | Private archive parent, owner-only DB and lock | Works as-is | ACL helper (shared with §4.2) or document Linux-only | M | Medium — same as T1 |
| T3 | `tools/agent_bridge.py:449-450` (`os.set_blocking`), `:466` (`os.read` on fds) | Non-blocking drain of the agent bridge child's stdout/stderr | Works (POSIX) | `os.set_blocking` raises on Windows; needs overlapped pipes or a reader-thread redesign | M | Medium — a blocked bridge freezes the agent loop |
| T4 | `tools/check-release-secrets.py:28` (also `tests/test_release_secrets.py:68`) | `git -c core.hooksPath=/dev/null …` to bypass hooks | Works | `/dev/null` does not exist — use an empty non-hook directory or `-c core.hooksPath=` | S | None |
| T5 | `tools/distill_planner_benchmark.py:20,47`, `tools/distill_synthetic_benchmark.py:39,70` | Benchmark fixtures mkdir 0700 / chmod 0600 | Works | Skip the chmod part | S | None |

### 4.12 Tests

| ID | Place | What it does | macOS | Windows | Effort | Risk |
|----|-------|--------------|-------|---------|--------|------|
| W1 | `tests/end_to_end.sh:1-80` | Bash end-to-end suite; `/var/tmp` scratch (`:10,25`), `timeout` (`:76`) | Runs nearly as-is (bash, GNU timeout from coreutils) | Rewrite in Python/pytest or accept WSL-only | M | Low |
| W2 | Python tests: `fcntl` (`tests/test_session_collector.py:3,573`), `O_NOFOLLOW/O_CREAT/0o600` (`test_session_hermes_journal.py:70`, `test_backfill.py:88-96`), `mkfifo` (`test_backfill.py:401`), `os.chown` (`test_linux_bundle.py:897`), `0o700/0o755` chmods throughout | Security-property tests written against POSIX semantics | Most work unchanged | mkfifo/chown/fcntl do not exist; the ACL assertions have no direct translation — a Windows suite needs ACL-reading equivalents | L | Medium — without equivalent tests the Windows port cannot claim the same guarantees |
| W3 | Rust process tests: `/proc/self/fdinfo` (`brain-cli/tests/codex_hook_contract.rs:338`), `/proc/{pid}/stat` (`brain-cli/tests/distill_process.rs:1200`), `UnixStream::pair` (`:16,354`), `/proc/self/exe` (`client_integration.rs:1948`, `self_reexec_process.rs`) | Contract tests over Linux behavior | Mostly cfg-gateable; `/dev/fd` substitutes | Need Windows analogs or explicit `#[cfg]` skips with documented loss | M | Low — tests, not product |
| W4 | `tests/test_linux_bundle.py` (whole file, e.g. `:177,262,464-473,897`) | Verifies the Linux bundle verifier (symlink attacks, suid bits, foreign owners) | Stays Linux-only by design | Stays Linux-only | — | None (the Linux artifact keeps its verifier) |

## 5. Effort totals

| Category | Findings | S | M | L |
|----------|----------|---|---|---|
| 4.1 Paths/environment | 7 | 4 | 3 | 0 |
| 4.2 Owner-only modes | 26 | 11 | 14 | 0 (+1 n/a) |
| 4.3 Secure opens | 3 | 2 | 0 | 1 |
| 4.4 procfs//etc//dev | 14 | 4 | 8 | 2 |
| 4.5 libc/kernel primitives | 7 | 2 | 2 | 2 (+1 n/a) |
| 4.6 flock | 4 | 3 | 1 | 0 |
| 4.7 systemd timers | 7 | 2 | 5 | 0 |
| 4.8 Desktop integration | 4 | 3 | 1 | 0 |
| 4.9 Entropy | — | counted in 4.4 (L14) | | |
| 4.10 Build/packaging/CI | 9 | 3 | 2 | 3 (+1 n/a) |
| 4.11 Python tools | 5 | 2 | 2 | 1 |
| 4.12 Tests | 4 | 0 | 2 | 1 (+1 n/a) |
| **Total** | **90** | **36** | **40** | **10** |

macOS alone: the only L items are K1/K2 (whole-home restore),
S1 (secure-open helper, counted conservatively) and B3/B4 (distribution and
signing). Windows carries most L items and the only new security model (ACLs).

## 6. Staged porting plan

Each stage ends with the existing Linux suite green plus a per-OS smoke test;
nothing on Linux may change behavior (all new code sits behind
`#[cfg(target_os = …)]` beside the existing branches listed in §3).

**Stage 0 — decisions and harness (both platforms, ~1 week).**
Fix the security policy questions q3, q4 and q11 in §7; add macOS/Windows CI
jobs (B7) that build and run the unit (not process) test suites; extract the
secure-open helper (S1) with a Linux implementation first so the diff for
other platforms is additive.

**Stage 1 — macOS core (1–2 weeks).**
`libc::geteuid()` replacing all `/proc`-based uid lookups (L1-L3, keeping the
fail-closed shape); secure-open helper on Darwin (`O_NOFOLLOW` via libc — fixes
the S2 trap in installer.rs/client_registry.rs); `getrandom` for entropy (L14);
`dirs` for home (P1); `localtime_s`→`chrono::Local` (K4); graphify exit codes
(K7); OsStr policy (K8). After this stage: init/remember/recall/ask/serve/hook
work on macOS.

**Stage 2 — macOS product features (2–3 weeks).**
Hook stdin deadline reads via poll/kqueue (L7/L8) — this unblocks hook
journaling; TLS credential/key handling (L9, L10, M8-M10 with geteuid);
self-copy installs (L6); stale-window detection via lsof (L12, L13);
restore via `renamex_np(RENAME_SWAP)` with the lease layer degraded to
documented fail-closed (K1/K2); launchd timer backend (D1-D5); `.app` bundle
launchers (G1, G3, G4).

**Stage 3 — macOS distribution (1–2 weeks, parallelizable).**
sqlite-vec build verification (B1, §7 q9), universal2 release pipeline with
Apple clang (B3), codesign + notarize, install.sh variant (B6), docs (B8).

**Stage 4 — Windows core (2–3 weeks, after macOS lessons).**
SID/token owner model and the ACL helper (all of §4.2 + L1-L3); secure-open
with `FILE_FLAG_OPEN_REPARSE_POINT` + `file_index` recheck (S1-S3); entropy
(L14); paths `%USERPROFILE%` (P1, P2); `LockFileEx` locks (F1-F3 already work
via std); decide UTF-8 path policy (§7 q3).

**Stage 5 — Windows features (3–4 weeks).**
Hook stdin via PeekNamedPipe/overlapped (L7/L8); restore via oplocks +
`FileRenameInfoEx` + crash-safe recovery journal, failing closed on
filesystems without oplock support (K1/K2); descriptor-walk via cap-std or
NtCreateFile relative opens (K3); stale-window via `GetExtendedTcpTable` with
ownership verification (L12); Task Scheduler backend (D1-D5); Start Menu
shortcuts (G1); Python tools: port T1-T3 or explicitly freeze the collector
as Linux-only and say so in docs.

**Stage 6 — Windows distribution (1–2 weeks).**
MSVC target + CI (B2, B7), MSI/winget installer (B4, B6), Authenticode
signing (B3), docs.

Ordering rationale: macOS first because it keeps the POSIX security model
(modes, symlinks, flock, signals all exist), so §4.2/§4.3/§4.6 — the bulk of
the surface — port nearly verbatim and the remaining work is concentrated in
/proc substitutions. Windows requires a new owner/permission model (§4.2 on
ACLs) and has no leases and no directory exchange, so it inherits every design
decision macOS de-risks first.

## 7. Open questions

1. **Darwin flags (blocks S1/S2/S3):** confirm `O_NOFOLLOW` (0o400) and the
   absence of `O_DIRECTORY` on current Darwin; `openat`/`mkdirat` availability
   is assumed. Verify by a compile test, not documentation.
2. **`RENAME_SWAP` semantics (blocks K2):** does `renamex_np(RENAME_SWAP)` on
   APFS guarantee the same single-syscall atomic exchange as
   `renameat2(RENAME_EXCHANGE)` on ext4/xfs, including for directories? If
   not, the restore needs the staged-dance + recovery journal design.
3. **Non-UTF-8 paths on Windows (blocks K8, P2):** Brain validates homes as
   "absolute UTF-8 without control characters" (private_fs.rs:23-34). Windows
   paths are WTF-16. Do we (a) require UTF-8-representable paths (reject the
   rest), or (b) define a WTF-8 policy? (a) is recommended for v1.
4. **Windows owner model (blocks §4.2):** is the unit of ownership the user
   SID? Do we enforce owner-only via `OWNER_RIGHTS`-only DACLs, and how do we
   *verify* existing files (AccessCheck-based, since mode bits don't exist)?
5. **macOS `/dev/fd` semantics (blocks L7/L9):** does reopening
   `/dev/fd/0` create an independent open file description (Linux behavior)
   or share it (fdescfs/BSD behavior)? Determines whether poll(2) is the only
   safe design.
6. **Per-call non-blocking recv on Windows (blocks L8):** toggle `FIONBIO`
   around `recv` on a duplicated handle, or move to overlapped I/O end to end?
7. **Client config locations on macOS/Windows (blocks P5):** where do Claude,
   Codex, Cursor, opencode and Hermes keep their config on each OS? Needs a
   survey of the client applications, not of this codebase.
8. **Graceful termination on Windows (blocks L12):** `TerminateProcess` is the
   only reliable kill for an arbitrary process; is killing a stale `brain ui`
   acceptable without a graceful TERM handshake?
9. **sqlite-vec on macOS/MSVC (blocks B1):** does the vendored
   `#ifdef __GLIBC__` patch compile and pass vector tests on
   `aarch64-apple-darwin` and `x86_64-pc-windows-msvc`? (Expected yes, must be
   proven.)
10. **Windows filesystem floor (blocks K1/K2, S1):** NTFS-only? Oplocks and
    file ids behave differently on FAT32/exFAT/ReFS/network shares — decide
    the supported matrix and fail closed outside it, mirroring the lease
    behavior (backup.rs:494-496).
11. **Fail-open gaps in the current not(unix) branches (M5/M6):**
    `storage/sqlite.rs:123-125` and `sharing.rs:2022-2029, 2057-2060` are
    silent no-ops off Unix. Until the ACL layer exists, should Windows builds
    refuse to create share/auth databases instead of creating them without
    owner-only protection? (Recommended: refuse, keeping the fail-closed
    posture of the rest of the codebase.)
12. **Timer parity:** `Nice=10`/`IOSchedulingClass=idle` (distill_schedule.rs:207-209,
    backup.rs:724-725) have no launchd/Task Scheduler equivalent. Is losing
    them acceptable, or must the timer command self-nice?
13. **Signing and notarization (blocks B3/B4):** Apple Developer account and
    Windows code-signing certificate are external dependencies with budget and
    lead time. Who owns them?
14. **Termux/Android:** install.sh:8,32 and the aarch64-musl target serve
    Termux explicitly. Confirm Android remains Linux-in-scope and unaffected
    by this plan.
15. **`source_group` locale (P6):** the heuristic hardcodes `/home` and
    `/run/media` (explore.rs:213-250) and the fallback label is Russian
    ("(без источника)"). Should macOS/Windows path shapes be added, and should
    the label be localized?
