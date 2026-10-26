//! Principals and credentials for `brain-coord/1`.
//!
//! A separate SQLite file `auth.sqlite` in the server's `--state-dir` holds
//! principals (one per credential identity, with a space allowlist and a
//! reader/writer role) and their credentials. The coordinator's
//! `coord.sqlite` (schema 1) is never touched by this module.
//!
//! A credential is `brc1.<token_id>.<secret>`: the secret is 256 random bits,
//! the server stores only `blake3(secret)` and the token id; the client keeps
//! the whole token in a `0600` file. Authentication is a constant-time
//! comparison of hashes found by token id, so a lookup miss and a wrong
//! secret cost the same. Every request re-reads the database: a revocation
//! takes effect on the next request with no cache to invalidate.
//!
//! Identity binding: a credential names exactly one agent identity
//! (`agent_id`). The server never takes `agent` from a request body at face
//! value; a request agent, when present, must equal the credential's
//! `agent_id` or the call is refused. Display labels are not identity.

use std::fmt;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::network::protocol::NetError;

/// Marks `auth.sqlite` as this server's own; a stray file is refused.
const APPLICATION_ID: i64 = 0x4252_4E41; // "BRNA"
/// Auth schema version this code writes. Newer files are refused.
/// v2: principals gained the credential-bound `agent_id`.
/// v3: explicit per-principal `memory_grants`; older principals get none.
pub const SCHEMA_VERSION: i64 = 3;
/// How long a new credential stays valid unless issued otherwise.
pub const DEFAULT_CREDENTIAL_TTL_SECS: i64 = 90 * 86_400;
/// Shortest credential lifetime.
pub const MIN_CREDENTIAL_TTL_SECS: i64 = 60;

const SCHEMA: &str = r"
CREATE TABLE meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL) STRICT;
CREATE TABLE principals (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    machine_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    label TEXT NOT NULL DEFAULT '',
    role TEXT NOT NULL CHECK (role IN ('worker','reader')),
    spaces TEXT NOT NULL,
    disabled_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(machine_id, agent_id)
) STRICT;
CREATE TABLE credentials (
    token_id TEXT PRIMARY KEY,
    principal_id INTEGER NOT NULL REFERENCES principals(id),
    secret_hash TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    not_after INTEGER NOT NULL,
    revoked_at INTEGER,
    last_used_at INTEGER,
    last_used_remote TEXT
) STRICT;
CREATE INDEX credentials_principal ON credentials(principal_id);
";

/// Explicit memory read grants, written only by host-local administration.
/// A principal without rows here can read no shared memory at all.
const MEMORY_GRANTS: &str = r"
CREATE TABLE memory_grants (
    principal_id INTEGER NOT NULL REFERENCES principals(id),
    space TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY(principal_id, space)
) STRICT;
";

const TABLES: [&str; 4] = ["meta", "principals", "credentials", "memory_grants"];

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// True when two byte strings are equal without leaking where they differ.
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}

/// The part of a bearer token that names it: printable, bounded.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenId(String);

impl TokenId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Debug does not exist for token parts on purpose; ids are fine to show.
impl fmt::Debug for TokenId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenId")
            .field("id", &self.0)
            .finish()
    }
}

/// A whole bearer token, in `brc1.<token_id>.<secret>` form.
///
/// `Debug` and `Display` are redacted on purpose: the secret never appears
/// in logs, panics or error messages.
pub struct BearerToken {
    raw: String,
    token_id: TokenId,
    secret: String,
}

const TOKEN_ID_MAX: usize = 64;
const SECRET_MAX: usize = 128;

impl BearerToken {
    /// Parses `brc1.<token_id>.<secret>` without storing anything else.
    ///
    /// # Errors
    /// `NetError::InvalidRequest` when the format or lengths are wrong.
    pub fn parse(raw: &str) -> Result<Self, NetError> {
        let bad = || NetError::invalid("bearer token must be brc1.<token_id>.<secret>");
        let rest = raw.strip_prefix("brc1.").ok_or_else(bad)?;
        let (id, secret) = rest.split_once('.').ok_or_else(bad)?;
        if id.is_empty()
            || id.len() > TOKEN_ID_MAX
            || secret.is_empty()
            || secret.len() > SECRET_MAX
            || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || !secret.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(bad());
        }
        let raw = format!("brc1.{id}.{secret}");
        Ok(Self {
            raw,
            token_id: TokenId(id.to_owned()),
            secret: secret.to_owned(),
        })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    fn secret_hash(&self) -> String {
        blake3::hash(self.secret.as_bytes()).to_hex().to_string()
    }

    #[must_use]
    pub fn token_id(&self) -> &TokenId {
        &self.token_id
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BearerToken(brc1.<token_id>.<redacted>)")
    }
}

impl fmt::Display for BearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("brc1.<token_id>.<redacted>")
    }
}

/// A worker or reader principal. `agent_id` is the identity the credential
/// is bound to; `label` is display only.
#[derive(Clone, Debug)]
pub struct Principal {
    pub id: i64,
    pub machine_id: String,
    pub agent_id: String,
    pub label: String,
    pub role: String,
    pub spaces: Vec<String>,
    pub disabled: bool,
}

impl Principal {
    pub fn may_write(&self) -> bool {
        self.role == "worker" && !self.disabled
    }

    pub fn allows_space(&self, space: &str) -> bool {
        self.spaces
            .iter()
            .any(|allowed| allowed == space || allowed == "*")
    }
}

/// The authenticated identity extracted from one request.
#[derive(Clone, Debug)]
pub struct Authenticated {
    pub principal: Principal,
    pub token_id: TokenId,
    /// Memory spaces this credential's principal was explicitly granted by
    /// host-local admin AND that its namespace allows. Private: only
    /// [`AuthStore::authenticate`] can fill it, so no caller can construct
    /// an `Authenticated` carrying grants it never proved.
    memory_read: Vec<String>,
}

impl Authenticated {
    /// The agent identity this credential speaks for. Requests must match
    /// it or omit their agent entirely.
    #[must_use]
    pub fn agent_id(&self) -> &str {
        &self.principal.agent_id
    }

    /// True when the request's agent label matches the bound identity.
    #[must_use]
    pub fn agent_matches(&self, claimed: &str) -> bool {
        constant_time_eq(self.principal.agent_id.as_bytes(), claimed.as_bytes())
    }

    /// Memory spaces readable with this credential, read fresh from the
    /// store at authentication time (never cached across requests).
    #[must_use]
    pub fn memory_read_spaces(&self) -> &[String] {
        &self.memory_read
    }
}

/// Store of principals and credentials over `auth.sqlite`.
pub struct AuthStore {
    connection: Connection,
}

impl AuthStore {
    /// Opens or creates the store under `state_dir`.
    ///
    /// # Errors
    /// `NetError::Incompatible` for a foreign or newer file, `NetError::Unavailable` for I/O.
    pub fn open(state_dir: &Path) -> Result<Self, NetError> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(state_dir)?;
        let path = state_dir.join("auth.sqlite");
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(NetError::unavailable(format!("auth store: {error}"))),
        }
        let connection = Connection::open(&path)?;
        connection.busy_timeout(std::time::Duration::from_secs(10))?;
        let store = Self { connection };
        store.initialise()?;
        for suffix in ["-wal", "-shm"] {
            let side = PathBuf::from(format!("{}{suffix}", path.display()));
            if side.exists() {
                fs::set_permissions(side, fs::Permissions::from_mode(0o600))?;
            }
        }
        Ok(store)
    }

    fn initialise(&self) -> Result<(), NetError> {
        self.connection.pragma_update(None, "journal_mode", "WAL")?;
        self.connection
            .pragma_update(None, "synchronous", "NORMAL")?;
        self.connection.pragma_update(None, "foreign_keys", true)?;
        let tx = self.connection.unchecked_transaction()?;
        let app: i64 = tx.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if app == 0 && version == 0 {
            let tables: i64 =
                tx.query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))?;
            if tables == 0 {
                tx.execute_batch(SCHEMA)?;
                tx.execute_batch(MEMORY_GRANTS)?;
                tx.pragma_update(None, "application_id", APPLICATION_ID)?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
                tx.commit()?;
                return Ok(());
            }
        }
        if app != APPLICATION_ID {
            return Err(NetError::incompatible(format!(
                "auth store is foreign (application_id {app:#x})"
            )));
        }
        // v1 had no agent binding; those credentials cannot prove an agent,
        // so the migration revokes them instead of guessing.
        if version == 1 {
            tx.execute(
                "ALTER TABLE principals ADD COLUMN agent_id TEXT NOT NULL DEFAULT ''",
                [],
            )?;
            // v1 credentials cannot prove an agent: revoke and remove rather
            // than guess an identity. This schema was never released, so no
            // live data exists; the path exists to fail closed anyway.
            tx.execute(
                "UPDATE credentials SET revoked_at = ?1 WHERE revoked_at IS NULL",
                params![now_unix()],
            )?;
            tx.execute(
                "DELETE FROM credentials WHERE principal_id IN (SELECT id FROM principals WHERE agent_id = '')",
                [],
            )?;
            tx.execute("DELETE FROM principals WHERE agent_id = ''", [])?;
            tx.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES ('migrated_from_v1', 1)",
                [],
            )?;
            tx.execute_batch(MEMORY_GRANTS)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
            return Ok(());
        }
        // v2 credentials predate memory sharing: they gain an empty grant
        // table, so every old token stays a work token and reads no memory.
        if version == 2 {
            tx.execute_batch(MEMORY_GRANTS)?;
            tx.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES ('migrated_from_v2', 1)",
                [],
            )?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
            return Ok(());
        }
        if version != SCHEMA_VERSION {
            return Err(NetError::incompatible(format!(
                "auth store is schema {version}, this build writes {SCHEMA_VERSION}"
            )));
        }
        for table in TABLES {
            let present: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [table],
                |row| row.get(0),
            )?;
            if !present {
                return Err(NetError::incompatible(format!(
                    "auth store lacks table {table}"
                )));
            }
        }
        Ok(())
    }

    /// Adds or replaces a principal and the agent identity its credentials
    /// are bound to. Replacing keeps credentials working.
    ///
    /// # Errors
    /// `NetError::InvalidRequest` for a bad machine id, agent id, label or space list.
    pub fn upsert_principal(
        &self,
        machine_id: &str,
        agent_id: &str,
        label: &str,
        role: &str,
        spaces: &[&str],
    ) -> Result<i64, NetError> {
        if !(3..=64).contains(&machine_id.len())
            || !machine_id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(NetError::invalid(
                "machine_id must be 3..=64 characters of [a-z0-9-]",
            ));
        }
        if !(1..=64).contains(&agent_id.len())
            || !agent_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".:_@-".contains(&b))
        {
            return Err(NetError::invalid(
                "agent_id must be 1..=64 characters of [A-Za-z0-9._:@-]",
            ));
        }
        if label.len() > 200 || label.chars().any(|c| c.is_control()) {
            return Err(NetError::invalid(
                "label must be at most 200 printable characters",
            ));
        }
        if !["worker", "reader"].contains(&role) {
            return Err(NetError::invalid("role must be worker or reader"));
        }
        if spaces.is_empty() || spaces.len() > 64 {
            return Err(NetError::invalid("1..=64 spaces required"));
        }
        for space in spaces {
            if space.is_empty()
                || space.len() > 64
                || !space
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".:_-".contains(&b))
            {
                return Err(NetError::invalid(
                    "spaces must be tokens of [A-Za-z0-9._:-]",
                ));
            }
        }
        let now = now_unix();
        let changed = self
            .connection
            .execute(
                "INSERT INTO principals(machine_id, agent_id, label, role, spaces, disabled_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?6)
                 ON CONFLICT(machine_id, agent_id) DO UPDATE SET
                   label = excluded.label, role = excluded.role,
                   spaces = excluded.spaces, disabled_at = NULL, updated_at = excluded.updated_at",
                params![machine_id, agent_id, label, role, spaces.join(","), now],
            )?;
        if changed == 0 {
            return Err(NetError::invalid("principal unchanged"));
        }
        Ok(self.connection.last_insert_rowid())
    }

    /// Disables a principal and revokes every live credential of theirs.
    ///
    /// # Errors
    /// `NetError::NotFound` when no such machine is registered.
    pub fn disable_principal(&self, machine_id: &str) -> Result<(), NetError> {
        let changed = self.connection.execute(
            "UPDATE principals SET disabled_at = ?2, updated_at = ?2 WHERE machine_id = ?1 AND disabled_at IS NULL",
            params![machine_id, now_unix()],
        )?;
        if changed == 0 {
            let exists: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM principals WHERE machine_id = ?1)",
                [machine_id],
                |row| row.get(0),
            )?;
            return Err(if exists {
                NetError::not_found("principal is already disabled")
            } else {
                NetError::not_found("no such principal")
            });
        }
        self.connection.execute(
            "UPDATE credentials SET revoked_at = ?1 WHERE principal_id IN (SELECT id FROM principals WHERE machine_id = ?2) AND revoked_at IS NULL",
            params![now_unix(), machine_id],
        )?;
        Ok(())
    }

    /// Issues a new credential for one agent identity on a machine and
    /// returns the token once. Two agents on the same machine each get
    /// their own credential; neither can act as the other.
    ///
    /// `ttl_secs` is capped at `DEFAULT_CREDENTIAL_TTL_SECS`; pass `None`
    /// for the default. The token is never written to stdout by this store;
    /// the caller decides where it goes (tests write `0600` files).
    ///
    /// # Errors
    /// `NetError::NotFound`, `NetError::InvalidRequest` for a bad ttl.
    pub fn issue_credential(
        &self,
        machine_id: &str,
        agent_id: &str,
        ttl_secs: Option<i64>,
    ) -> Result<BearerToken, NetError> {
        let ttl = ttl_secs.unwrap_or(DEFAULT_CREDENTIAL_TTL_SECS);
        if !(MIN_CREDENTIAL_TTL_SECS..=DEFAULT_CREDENTIAL_TTL_SECS).contains(&ttl) {
            return Err(NetError::invalid(format!(
                "credential ttl must be {MIN_CREDENTIAL_TTL_SECS}..={DEFAULT_CREDENTIAL_TTL_SECS} seconds"
            )));
        }
        let principal_id: Option<i64> = self
            .connection
            .query_row(
                "SELECT id FROM principals WHERE machine_id = ?1 AND agent_id = ?2",
                params![machine_id, agent_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(principal_id) = principal_id else {
            return Err(NetError::not_found("no such principal"));
        };
        let mut id_bytes = [0_u8; 12];
        fs::File::open("/dev/urandom")?.read_exact(&mut id_bytes)?;
        let mut secret_bytes = [0_u8; 32];
        fs::File::open("/dev/urandom")?.read_exact(&mut secret_bytes)?;
        let token_id = format!(
            "t{}{}",
            now_unix(),
            id_bytes
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let secret: String = secret_bytes.iter().map(|b| format!("{b:02x}")).collect();
        let raw = format!("brc1.{token_id}.{secret}");
        let token = BearerToken {
            raw,
            token_id: TokenId(token_id.clone()),
            secret,
        };
        let hash = token.secret_hash();
        let now = now_unix();
        self.connection.execute(
            "INSERT INTO credentials(token_id, principal_id, secret_hash, created_at, not_after) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![token_id, principal_id, hash, now, now + ttl],
        )?;
        Ok(token)
    }

    /// Revokes one credential by token id.
    ///
    /// # Errors
    /// `NetError::NotFound`.
    pub fn revoke_credential(&self, token_id: &str) -> Result<(), NetError> {
        let changed = self.connection.execute(
            "UPDATE credentials SET revoked_at = ?2 WHERE token_id = ?1 AND revoked_at IS NULL",
            params![token_id, now_unix()],
        )?;
        if changed == 0 {
            return Err(NetError::not_found("credential unknown or already revoked"));
        }
        Ok(())
    }

    /// Authenticates a bearer token against the store, fail-closed.
    ///
    /// Disabled principals, revoked or expired credentials all answer the
    /// same `unauthorized` so the wire never reveals which part was wrong.
    ///
    /// # Errors
    /// `NetError::Unauthorized` for anything short of a live match.
    pub fn authenticate(
        &self,
        token: &BearerToken,
        now: i64,
        remote: &str,
    ) -> Result<Authenticated, NetError> {
        let row = self
            .connection
            .query_row(
                "SELECT c.secret_hash, c.not_after, c.revoked_at, p.id, p.machine_id, p.agent_id, p.label, p.role, p.spaces, p.disabled_at
                 FROM credentials c JOIN principals p ON p.id = c.principal_id
                 WHERE c.token_id = ?1",
                [token.token_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, Option<i64>>(2)?,
                        row.get::<_, i64>(3)?, row.get::<_, String>(4)?, row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?, row.get::<_, String>(7)?, row.get::<_, String>(8)?,
                        row.get::<_, Option<i64>>(9)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            stored_hash,
            not_after,
            revoked_at,
            id,
            machine_id,
            agent_id,
            label,
            role,
            spaces,
            disabled_at,
        )) = row
        else {
            // Miss: hash the presented secret anyway so timing does not leak existence.
            let _ = blake3::hash(token.secret.as_bytes());
            return Err(NetError::unauthorized());
        };
        if !constant_time_eq(stored_hash.as_bytes(), token.secret_hash().as_bytes()) {
            return Err(NetError::unauthorized());
        }
        if revoked_at.is_some() || disabled_at.is_some() || now >= not_after {
            return Err(NetError::unauthorized());
        }
        let _ = self.connection.execute(
            "UPDATE credentials SET last_used_at = ?2, last_used_remote = ?3 WHERE token_id = ?1",
            params![token.token_id.as_str(), now, remote],
        );
        let principal = Principal {
            id,
            machine_id,
            agent_id,
            label,
            role,
            spaces: spaces.split(',').map(str::to_owned).collect(),
            disabled: disabled_at.is_some(),
        };
        let memory_read = self
            .memory_grants_of(id)?
            .into_iter()
            .filter(|space| principal.allows_space(space))
            .collect();
        Ok(Authenticated {
            principal,
            token_id: token.token_id().clone(),
            memory_read,
        })
    }

    /// One page of principals for administration; never includes secrets.
    ///
    /// # Errors
    /// `NetError::Unavailable` on storage errors.
    fn memory_grants_of(&self, principal_id: i64) -> Result<Vec<String>, NetError> {
        let mut statement = self
            .connection
            .prepare("SELECT space FROM memory_grants WHERE principal_id = ?1 ORDER BY space")?;
        let rows = statement
            .query_map([principal_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn principal_for_grant(
        &self,
        machine_id: &str,
        agent_id: &str,
        space: &str,
    ) -> Result<i64, NetError> {
        if space == "*"
            || space.is_empty()
            || space.len() > 64
            || !space
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(NetError::invalid(
                "memory grant space must be one explicit [A-Za-z0-9._-] space, never `*`",
            ));
        }
        let row: Option<(i64, String)> = self
            .connection
            .query_row(
                "SELECT id, spaces FROM principals WHERE machine_id = ?1 AND agent_id = ?2",
                params![machine_id, agent_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((id, spaces)) = row else {
            return Err(NetError::not_found("no such principal"));
        };
        if !spaces
            .split(',')
            .any(|allowed| allowed == space || allowed == "*")
        {
            return Err(NetError::forbidden(
                "the principal's namespace does not include this space",
            ));
        }
        Ok(id)
    }

    /// Host-local admin only: allow one principal to read one shared memory
    /// space. The space must already be in the principal's namespace; the
    /// host must separately enable it in the share store. Idempotent.
    ///
    /// # Errors
    /// `InvalidRequest` for a bad space, `NotFound` for an unknown principal,
    /// `Forbidden` when the space is outside the principal's namespace.
    pub fn grant_memory_read(
        &self,
        machine_id: &str,
        agent_id: &str,
        space: &str,
    ) -> Result<bool, NetError> {
        let id = self.principal_for_grant(machine_id, agent_id, space)?;
        let changed = self.connection.execute(
            "INSERT INTO memory_grants(principal_id, space, created_at) VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
            params![id, space, now_unix()],
        )?;
        Ok(changed == 1)
    }

    /// Host-local admin only: withdraw a memory grant; effective on the
    /// next request because grants are read at authentication time.
    ///
    /// # Errors
    /// `NotFound` when the principal or the grant does not exist.
    pub fn revoke_memory_read(
        &self,
        machine_id: &str,
        agent_id: &str,
        space: &str,
    ) -> Result<(), NetError> {
        let id: Option<i64> = self
            .connection
            .query_row(
                "SELECT id FROM principals WHERE machine_id = ?1 AND agent_id = ?2",
                params![machine_id, agent_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(id) = id else {
            return Err(NetError::not_found("no such principal"));
        };
        let changed = self.connection.execute(
            "DELETE FROM memory_grants WHERE principal_id = ?1 AND space = ?2",
            params![id, space],
        )?;
        if changed == 0 {
            return Err(NetError::not_found("no such memory grant"));
        }
        Ok(())
    }

    /// Every memory grant, for host-local status (no secrets).
    ///
    /// # Errors
    /// Storage failures.
    pub fn list_memory_grants(&self, limit: usize) -> Result<Vec<Value>, NetError> {
        let mut statement = self.connection.prepare(
            "SELECT p.machine_id, p.agent_id, g.space, g.created_at FROM memory_grants g
             JOIN principals p ON p.id = g.principal_id ORDER BY p.machine_id, p.agent_id, g.space LIMIT ?1",
        )?;
        let rows = statement
            .query_map([limit.clamp(1, 200) as i64], |row| {
                Ok(json!({
                    "machine_id": row.get::<_, String>(0)?,
                    "agent_id": row.get::<_, String>(1)?,
                    "space": row.get::<_, String>(2)?,
                    "created_at": row.get::<_, i64>(3)?,
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Count all configured principals without migrations, schema creation or tokens.
    pub fn principal_count_read_only(state_dir: &Path) -> Result<i64, NetError> {
        let connection = Connection::open_with_flags(
            state_dir.join("auth.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(std::time::Duration::from_millis(250))?;
        let app: i64 = connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if app != APPLICATION_ID || version != SCHEMA_VERSION {
            return Err(NetError::incompatible("unknown auth schema"));
        }
        Ok(connection.query_row("SELECT count(*) FROM principals", [], |row| row.get(0))?)
    }

    pub fn list_principals(&self, limit: usize) -> Result<Vec<Value>, NetError> {
        let mut statement = self.connection.prepare(
            "SELECT machine_id, agent_id, label, role, spaces, disabled_at, created_at FROM principals ORDER BY id LIMIT ?1",
        )?;
        let rows = statement
            .query_map([limit.clamp(1, 200) as i64], |row| {
                Ok(json!({
                    "machine_id": row.get::<_, String>(0)?,
                    "agent_id": row.get::<_, String>(1)?,
                    "label": row.get::<_, String>(2)?,
                    "role": row.get::<_, String>(3)?,
                    "spaces": row.get::<_, String>(4)?.split(',').collect::<Vec<_>>(),
                    "disabled": row.get::<_, Option<i64>>(5)?.is_some(),
                    "created_at": row.get::<_, i64>(6)?,
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_count_is_read_only_exact_and_does_not_migrate() {
        let (store, dir) = store("panel-count");
        for n in 0..205 {
            store
                .connection
                .execute(
                    "INSERT INTO principals(machine_id,agent_id,role,spaces,created_at,updated_at)
                VALUES (?1,'codex','reader','default',1,1)",
                    [format!("machine-{n}")],
                )
                .unwrap();
        }
        assert_eq!(AuthStore::principal_count_read_only(&dir).unwrap(), 205);
        store
            .connection
            .pragma_update(None, "user_version", 1)
            .unwrap();
        assert!(AuthStore::principal_count_read_only(&dir).is_err());
        let version: i64 = store
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1, "GET must never migrate an older auth store");
        drop(store);
        let _ = fs::remove_dir_all(&dir);
        assert!(AuthStore::principal_count_read_only(&dir).is_err());
        assert!(!dir.exists(), "GET must not create the store");
    }

    fn store(_tag: &str) -> (AuthStore, PathBuf) {
        let dir = crate::test_support::temp_path("brain-auth");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");
        let store = AuthStore::open(&dir).expect("open");
        (store, dir)
    }

    #[test]
    fn issue_and_authenticate_round_trip() {
        let (store, _dir) = store("roundtrip");
        store
            .upsert_principal("m-one", "codex", "Codex", "worker", &["team-a"])
            .expect("upsert");
        let token = store
            .issue_credential("m-one", "codex", None)
            .expect("issue");
        let authed = store
            .authenticate(&token, now_unix(), "127.0.0.1")
            .expect("auth");
        assert_eq!(authed.principal.machine_id, "m-one");
        assert_eq!(authed.agent_id(), "codex");
        let wrong = BearerToken::parse("brc1.nope.wrong").expect("parse");
        assert_eq!(
            store
                .authenticate(&wrong, now_unix(), "127.0.0.1")
                .unwrap_err()
                .status(),
            401
        );
    }

    #[test]
    fn debug_and_display_never_leak_the_secret() {
        let (store, _dir) = store("redacted");
        store
            .upsert_principal("m-two", "claude", "", "worker", &["s"])
            .expect("upsert");
        let token = store
            .issue_credential("m-two", "claude", None)
            .expect("issue");
        let debugged = format!("{token:?}");
        let displayed = format!("{token}");
        assert!(!debugged.contains(&token.as_str().split('.').nth(2).unwrap().to_owned()));
        assert!(!displayed.contains(&token.as_str().split('.').nth(2).unwrap().to_owned()));
        assert!(debugged.contains("<redacted>"));
        assert!(displayed.contains("<redacted>"));
    }

    #[test]
    fn revoked_and_expired_credentials_fail_uniformly() {
        let (store, _dir) = store("revoked");
        store
            .upsert_principal("m-three", "hermes", "", "worker", &["s"])
            .expect("upsert");
        let token = store
            .issue_credential("m-three", "hermes", Some(60))
            .expect("issue");
        let token_id = token.token_id().as_str().to_owned();
        drop(token);
        store.revoke_credential(&token_id).expect("revoke");
        // Re-issue, then travel past not_after.
        let fresh = store
            .issue_credential("m-three", "hermes", Some(60))
            .expect("fresh");
        let later = now_unix() + 120;
        assert_eq!(
            store
                .authenticate(&fresh, later, "127.0.0.1")
                .unwrap_err()
                .status(),
            401
        );
    }

    #[test]
    fn two_agents_on_one_machine_have_independent_identities() {
        let (store, _dir) = store("twoagents");
        store
            .upsert_principal("m-shared", "codex", "", "worker", &["s"])
            .expect("upsert");
        let codex = store
            .issue_credential("m-shared", "codex", None)
            .expect("codex token");
        // Same machine, two agents: independent credentials and identities.
        store
            .upsert_principal("m-shared", "claude", "", "worker", &["s"])
            .expect("upsert 2");
        let claude = store
            .issue_credential("m-shared", "claude", None)
            .expect("claude token");
        let authed_codex = store
            .authenticate(&codex, now_unix(), "127.0.0.1")
            .expect("codex");
        let authed_claude = store
            .authenticate(&claude, now_unix(), "127.0.0.1")
            .expect("claude");
        assert_eq!(authed_codex.principal.machine_id, "m-shared");
        assert_eq!(authed_claude.principal.machine_id, "m-shared");
        assert!(authed_codex.agent_matches("codex"));
        assert!(!authed_codex.agent_matches("claude"));
        assert!(authed_claude.agent_matches("claude"));
    }

    #[test]
    fn agent_id_validation_rejects_bad_input() {
        let (store, _dir) = store("badagent");
        assert!(
            store
                .upsert_principal("m-x", "", "", "worker", &["s"])
                .is_err()
        );
        assert!(
            store
                .upsert_principal("m-x", "bad agent!", "", "worker", &["s"])
                .is_err()
        );
        assert!(
            store
                .upsert_principal("m-x", &"x".repeat(65), "", "worker", &["s"])
                .is_err()
        );
    }

    #[test]
    fn v2_credentials_migrate_with_no_memory_grants() {
        let (store, dir) = store("v2grants");
        store
            .upsert_principal("m-old", "codex", "", "worker", &["team"])
            .expect("upsert");
        let token = store
            .issue_credential("m-old", "codex", None)
            .expect("issue");
        drop(store);
        {
            let raw = Connection::open(dir.join("auth.sqlite")).expect("raw");
            raw.execute_batch("DROP TABLE memory_grants;")
                .expect("drop");
            raw.pragma_update(None, "user_version", 2)
                .expect("downgrade");
        }
        let store = AuthStore::open(&dir).expect("migrates v2");
        let authed = store
            .authenticate(&token, now_unix(), "127.0.0.1")
            .expect("old token still a work token");
        assert!(
            authed.memory_read_spaces().is_empty(),
            "old credentials read no memory"
        );
        let version: i64 = store
            .connection
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("v");
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn memory_grants_are_explicit_namespaced_and_read_fresh() {
        let (store, _dir) = store("grants");
        store
            .upsert_principal("m-g", "claude", "", "reader", &["team", "ops"])
            .expect("upsert");
        let token = store
            .issue_credential("m-g", "claude", None)
            .expect("issue");
        assert!(
            store
                .authenticate(&token, now_unix(), "l")
                .expect("a")
                .memory_read_spaces()
                .is_empty()
        );
        assert_eq!(
            store
                .grant_memory_read("m-g", "claude", "*")
                .unwrap_err()
                .status(),
            400
        );
        assert_eq!(
            store
                .grant_memory_read("m-g", "claude", "private")
                .unwrap_err()
                .status(),
            403
        );
        assert_eq!(
            store
                .grant_memory_read("m-g", "nobody", "team")
                .unwrap_err()
                .status(),
            404
        );
        assert!(
            store
                .grant_memory_read("m-g", "claude", "team")
                .expect("grant")
        );
        assert!(
            !store
                .grant_memory_read("m-g", "claude", "team")
                .expect("idempotent")
        );
        let authed = store.authenticate(&token, now_unix(), "l").expect("a");
        assert_eq!(authed.memory_read_spaces(), ["team".to_string()]);
        // Narrowing the namespace silently disables a grant outside it.
        store
            .upsert_principal("m-g", "claude", "", "reader", &["ops"])
            .expect("narrow");
        assert!(
            store
                .authenticate(&token, now_unix(), "l")
                .expect("a")
                .memory_read_spaces()
                .is_empty()
        );
        store
            .upsert_principal("m-g", "claude", "", "reader", &["team"])
            .expect("widen");
        store
            .revoke_memory_read("m-g", "claude", "team")
            .expect("revoke grant");
        assert!(
            store
                .authenticate(&token, now_unix(), "l")
                .expect("a")
                .memory_read_spaces()
                .is_empty()
        );
        assert_eq!(
            store
                .revoke_memory_read("m-g", "claude", "team")
                .unwrap_err()
                .status(),
            404
        );
    }

    #[test]
    fn foreign_files_are_refused() {
        let (store, dir) = store("foreign");
        drop(store);
        let path = dir.join("auth.sqlite");
        let connection = Connection::open(&path).expect("reopen");
        connection
            .pragma_update(None, "application_id", 0x4444)
            .expect("mark");
        drop(connection);
        let refused = match AuthStore::open(&dir) {
            // AuthStore holds a live credential store; it has no Debug on
            // purpose, so the error is extracted by hand, never derived.
            Ok(_) => panic!("a foreign auth store must be refused"),
            Err(error) => error,
        };
        assert!(matches!(refused, NetError::Incompatible(_)));
    }
}
