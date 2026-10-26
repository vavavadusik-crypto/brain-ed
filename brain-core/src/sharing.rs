//! Opt-in shared memory core: facts, links and provenance exchanged between
//! installations. Design: outputs/phase15-shared-memory-design.md with the
//! phase 17 corrections applied. Not wired into any CLI, MCP or network yet.
//!
//! The model in one paragraph: every installation keeps a separate
//! `share.sqlite` (application id `BRNS`, schema version 1, file `0600`,
//! directory `0700`) that it alone writes. Sharing is default deny. A host
//! enables whole spaces; a caller may only read a space when the
//! *authenticated credential* carries an explicit `memory:read:<space>` grant
//! AND the host enabled the space — the intersection is checked before every
//! query and before replay, never after. Facts leave the installation only
//! through explicit `publish` of a curated payload; no code path here reads
//! the private memory graph or the conversation archive.
//!
//! ONE append-only event sequence per origin per space: `put`, `retract` and
//! `supersede` are all events on the same `(origin, space, seq)` stream, so a
//! peer pulling one space sees a gap-free sequence. Materialized facts
//! reference the original `put` event (`origin_seq`); retraction and
//! supersession append history and never mutate or delete event rows. Paging
//! is over that one stream, with a single cursor per `(peer, space, origin)`
//! — there are no separate facts/events cursors.
//!
//! Provenance is recorded in two deliberately separate places: the event
//! carries `claimed_agent` (what the *remote side claims* — data, never
//! trusted), while the applied row records `peer_id`/`applied_at` from the
//! *trusted host context* at apply time. A claimed agent or origin can never
//! raise authority: authorization only consults the host-built
//! [`Principal`], and no payload field, boolean or project text is consulted
//! by it. Payload keys that name host context (`trusted_peer`,
//! `host_context`, `authority`, `grant`, `credential`, `token`) are refused
//! outright.
//!
//! Known limitation, by design: revocation cannot erase data already
//! exported to peers. A retraction appends to the local stream and reaches a
//! peer only when that peer pulls it; a peer that never pulls keeps its
//! copy, and revoking a credential only stops future access. Provenance is
//! recorded, not proven.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::map::packet::{MemoryKind, MemoryScope, MemorySnippet, SharedProvenance};
use crate::network::auth::Authenticated;

/// `BRNS` — the share store's own application id, distinct from `BRNA`
/// (credentials) and the memory graph stores.
pub const SHARE_APPLICATION_ID: u32 = 0x4252_4E53;
pub const SHARE_SCHEMA_VERSION: u32 = 1;
/// Canonical event encoding version, covered by the digest.
pub const EVENT_FORMAT: &str = "brain-share-event/1";
pub const READ_GRANT_PREFIX: &str = "memory:read:";
/// Trusted peer label used when the host itself appends to its own origin.
pub const LOCAL_PEER: &str = "local-host";

pub const MAX_ID_BYTES: usize = 64;
pub const MAX_FACT_ID_BYTES: usize = 128;
pub const MAX_PAYLOAD_BYTES: usize = 4096;
pub const MAX_CLAIM_BYTES: usize = 600;
pub const MAX_PAGE: u32 = 100;
pub const MAX_BATCH_EVENTS: usize = 100;
pub const MAX_BATCH_BYTES: usize = 256 * 1024;
/// Sequence numbers stay JSON-safe.
pub const MAX_SEQ: u64 = 1 << 53;
/// Wire page bounds: a `share_pull` response must fit the 256 KiB client
/// response cap with room to spare, whatever the payload sizes.
pub const MAX_WIRE_PAGE: u32 = 10;
pub const MAX_WIRE_PAGE_BYTES: usize = 48 * 1024;

const WIRE_EVENT_KEYS: [&str; 8] = [
    "origin",
    "space",
    "seq",
    "kind",
    "target",
    "fact",
    "claimed_agent",
    "digest",
];
const WIRE_FACT_KEYS: [&str; 4] = ["fact_id", "kind", "payload", "claim"];

/// Keys that name host context; a payload carrying one is refused.
pub const FORBIDDEN_PAYLOAD_KEYS: [&str; 6] = [
    "trusted_peer",
    "host_context",
    "authority",
    "grant",
    "credential",
    "token",
];

/// Typed, stable errors of the shared-memory core.
#[derive(Debug, Error)]
pub enum ShareError {
    #[error("invalid shared data: {0}")]
    InvalidData(String),
    #[error("share store not found: {0}")]
    NotFound(String),
    #[error(
        "incompatible share store: application_id {found_app:#x} version {found_version}, expected {expected_app:#x} version {expected_version}"
    )]
    Incompatible {
        found_app: u32,
        found_version: u32,
        expected_app: u32,
        expected_version: u32,
    },
    #[error("space `{0}` is not enabled for sharing on this host")]
    SpaceNotEnabled(String),
    #[error("capability denied: no memory:read grant for space `{space}`")]
    CapabilityDenied { space: String },
    #[error("conflict: event {origin}/{space}/{seq} already exists with different bytes")]
    Conflict {
        origin: String,
        space: String,
        seq: u64,
    },
    #[error("cursor mismatch for {origin}/{space}: stored {stored}, offered {offered}")]
    CursorMismatch {
        origin: String,
        space: String,
        stored: u64,
        offered: u64,
    },
    #[error("event gap for {origin}/{space}: expected seq {expected}, got {got}")]
    Gap {
        origin: String,
        space: String,
        expected: u64,
        got: u64,
    },
    #[error("page limit {requested} out of bounds 1..={MAX_PAGE}")]
    PageLimit { requested: u32 },
    #[error("batch too large: {events} events, {bytes} encoded bytes")]
    BatchTooLarge { events: usize, bytes: usize },
    #[error("payload is {bytes} bytes, over the {MAX_PAYLOAD_BYTES}-byte bound")]
    PayloadTooLarge { bytes: usize },
    #[error("claim is {bytes} bytes, over the {MAX_CLAIM_BYTES}-byte bound")]
    ClaimTooLarge { bytes: usize },
    #[error("payload key `{0}` is reserved for host context")]
    ForbiddenPayloadKey(String),
    #[error("retract/supersede target {origin}/{space}/{target} is not a live fact")]
    UnknownTarget {
        origin: String,
        space: String,
        target: u64,
    },
    #[error("fact `{fact_id}` in space `{space}` is not a live local fact")]
    NoLiveFact { space: String, fact_id: String },
    #[error("storage error: {0}")]
    Storage(#[from] rusqlite::Error),
}

impl ShareError {
    /// Stable machine-readable kind, mirroring the `error_kind` convention
    /// of the coordinator protocol.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidData(_) => "invalid_data",
            Self::NotFound(_) => "not_found",
            Self::Incompatible { .. } => "incompatible",
            Self::SpaceNotEnabled(_) => "space_not_enabled",
            Self::CapabilityDenied { .. } => "capability_denied",
            Self::Conflict { .. } => "share_conflict",
            Self::CursorMismatch { .. } => "cursor_mismatch",
            Self::Gap { .. } => "gap",
            Self::PageLimit { .. } => "page_limit",
            Self::BatchTooLarge { .. } => "batch_too_large",
            Self::PayloadTooLarge { .. } => "payload_too_large",
            Self::ClaimTooLarge { .. } => "claim_too_large",
            Self::ForbiddenPayloadKey(_) => "forbidden_payload_key",
            Self::UnknownTarget { .. } => "unknown_target",
            Self::NoLiveFact { .. } => "no_live_fact",
            Self::Storage(_) => "storage",
        }
    }
}

pub type ShareResult<T> = Result<T, ShareError>;

/// The authenticated reader principal. Its fields are private and the only
/// public constructor is [`Principal::from_authenticated`], which takes an
/// [`Authenticated`] that only `AuthStore::authenticate` can produce after
/// verifying a live credential. Request data, a remote event's claimed
/// origin/agent or an MCP payload therefore cannot construct or raise one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Principal {
    machine_id: String,
    agent_id: String,
    /// Explicit memory grants, e.g. `memory:read:team`. Credentials issued
    /// before sharing existed carry none — that is the default-deny path.
    grants: Vec<String>,
}

impl Principal {
    /// The reader identity of one authenticated request: machine and agent
    /// from the credential binding, grants from host-local admin rows that
    /// also fall inside the credential's namespace.
    #[must_use]
    pub fn from_authenticated(authenticated: &Authenticated) -> Self {
        Self {
            machine_id: authenticated.principal.machine_id.clone(),
            agent_id: authenticated.agent_id().to_string(),
            grants: authenticated
                .memory_read_spaces()
                .iter()
                .map(|space| format!("{READ_GRANT_PREFIX}{space}"))
                .collect(),
        }
    }

    #[must_use]
    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    #[must_use]
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    #[cfg(test)]
    pub(crate) fn new(machine_id: impl Into<String>, agent_id: impl Into<String>) -> Self {
        Self {
            machine_id: machine_id.into(),
            agent_id: agent_id.into(),
            grants: Vec::new(),
        }
    }

    /// Old work credentials start with no memory grants; deny is the default.
    #[cfg(test)]
    pub(crate) fn without_grants(mut self) -> Self {
        self.grants.clear();
        self
    }

    #[cfg(test)]
    pub(crate) fn with_read_grant(mut self, space: &str) -> Self {
        self.grants.push(format!("{READ_GRANT_PREFIX}{space}"));
        self
    }

    #[must_use]
    pub fn has_read_grant(&self, space: &str) -> bool {
        self.grants
            .iter()
            .any(|g| g == &format!("{READ_GRANT_PREFIX}{space}"))
    }
}

/// Trusted host context for one apply: who we received a batch from and
/// when, as recorded locally. Distinct from claimed provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustedPeer {
    pub peer_id: String,
    pub applied_at: String,
}

impl TrustedPeer {
    #[must_use]
    pub fn new(peer_id: impl Into<String>, applied_at: impl Into<String>) -> Self {
        Self {
            peer_id: peer_id.into(),
            applied_at: applied_at.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum FactKind {
    Fact,
    Link,
}

impl FactKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Link => "link",
        }
    }

    pub fn parse(s: &str) -> ShareResult<Self> {
        match s {
            "fact" => Ok(Self::Fact),
            "link" => Ok(Self::Link),
            other => Err(ShareError::InvalidData(format!(
                "unknown fact kind `{other}`"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum EventKind {
    Put,
    Retract,
    Supersede,
}

impl EventKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Put => "put",
            Self::Retract => "retract",
            Self::Supersede => "supersede",
        }
    }

    pub fn parse(s: &str) -> ShareResult<Self> {
        match s {
            "put" => Ok(Self::Put),
            "retract" => Ok(Self::Retract),
            "supersede" => Ok(Self::Supersede),
            other => Err(ShareError::InvalidData(format!(
                "unknown event kind `{other}`"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FactStatus {
    Live,
    Retracted,
}

impl FactStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Retracted => "retracted",
        }
    }

    pub fn parse(s: &str) -> ShareResult<Self> {
        match s {
            "live" => Ok(Self::Live),
            "retracted" => Ok(Self::Retracted),
            other => Err(ShareError::InvalidData(format!(
                "unknown fact status `{other}`"
            ))),
        }
    }
}

/// The explicitly curated payload a host publishes. There is deliberately no
/// constructor that reads the private store or archive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SharedFact {
    pub fact_id: String,
    pub kind: FactKind,
    pub space: String,
    /// A JSON object, canonicalized before hashing and size checks.
    pub payload: Value,
    /// Optional human claim text, ≤600 bytes.
    pub claim: Option<String>,
}

/// Deepest payload nesting accepted; reserved-key checks visit every level.
const MAX_PAYLOAD_DEPTH: usize = 64;

/// Refuses a reserved host-context key at any depth, in any letter case.
///
/// Only object keys are checked: narrative string values that merely mention
/// a reserved word stay ordinary data.
fn reject_reserved_payload_keys(payload: &Value) -> ShareResult<()> {
    let mut pending = vec![(payload, 0_usize)];
    while let Some((value, depth)) = pending.pop() {
        if depth > MAX_PAYLOAD_DEPTH {
            return Err(ShareError::InvalidData(format!(
                "payload nesting exceeds {MAX_PAYLOAD_DEPTH} levels"
            )));
        }
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let folded = key.to_lowercase();
                    if FORBIDDEN_PAYLOAD_KEYS.contains(&folded.as_str()) {
                        return Err(ShareError::ForbiddenPayloadKey(key.clone()));
                    }
                    pending.push((child, depth + 1));
                }
            }
            Value::Array(items) => pending.extend(items.iter().map(|item| (item, depth + 1))),
            _ => {}
        }
    }
    Ok(())
}

impl SharedFact {
    /// Canonical payload bytes: object keys sorted recursively, so the digest
    /// is independent of map iteration order.
    #[must_use]
    pub fn canonical_payload(&self) -> String {
        sorted_json(&self.payload)
    }

    pub(crate) fn validate(&self) -> ShareResult<()> {
        validate_id(&self.fact_id, MAX_FACT_ID_BYTES, "fact_id")?;
        validate_space(&self.space)?;
        if !self.payload.is_object() {
            return Err(ShareError::InvalidData(
                "payload must be a JSON object".into(),
            ));
        }
        reject_reserved_payload_keys(&self.payload)?;
        let canonical = self.canonical_payload();
        if canonical.len() > MAX_PAYLOAD_BYTES {
            return Err(ShareError::PayloadTooLarge {
                bytes: canonical.len(),
            });
        }
        if canonical.as_bytes().contains(&0) {
            return Err(ShareError::InvalidData(
                "payload contains a NUL byte".into(),
            ));
        }
        if let Some(claim) = &self.claim {
            if claim.len() > MAX_CLAIM_BYTES {
                return Err(ShareError::ClaimTooLarge { bytes: claim.len() });
            }
            if claim.as_bytes().contains(&0) {
                return Err(ShareError::InvalidData("claim contains a NUL byte".into()));
            }
        }
        Ok(())
    }
}

/// One event on the single append-only `(origin, space)` stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShareEvent {
    pub origin: String,
    pub space: String,
    pub seq: u64,
    pub kind: EventKind,
    /// `retract`/`supersede`: the original `put` seq of the target fact.
    pub target: Option<u64>,
    /// `put`/`supersede`: the fact payload.
    pub fact: Option<SharedFact>,
    /// Claimed provenance: remote data, recorded, never trusted.
    pub claimed_agent: Option<String>,
}

impl ShareEvent {
    /// Canonical encoded form; the blake3 digest is taken over exactly these
    /// bytes. Trusted context (peer, applied_at) is excluded: it is local
    /// metadata, not part of the event's identity. `seq` may be `0` while an
    /// event waits for its in-transaction allocation.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut buf = String::new();
        buf.push_str(EVENT_FORMAT);
        buf.push('\n');
        buf.push_str(&self.origin);
        buf.push('\n');
        buf.push_str(&self.space);
        buf.push('\n');
        buf.push_str(&self.seq.to_string());
        buf.push('\n');
        buf.push_str(self.kind.as_str());
        buf.push('\n');
        buf.push_str(
            &self
                .target
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".into()),
        );
        buf.push('\n');
        if let Some(fact) = &self.fact {
            buf.push_str("1\n");
            buf.push_str(&fact.fact_id);
            buf.push('\n');
            buf.push_str(fact.kind.as_str());
            buf.push('\n');
            buf.push_str(&fact.canonical_payload());
            buf.push('\n');
            buf.push_str(&serde_json::to_string(&fact.claim).unwrap_or_else(|_| "null".into()));
        } else {
            buf.push('-');
        }
        // Claimed provenance is part of the identity: the same slot replayed
        // with a different claimed agent is a conflict, not a duplicate.
        buf.push('\n');
        buf.push_str(&serde_json::to_string(&self.claimed_agent).unwrap_or_else(|_| "null".into()));
        buf
    }

    #[must_use]
    pub fn digest(&self) -> String {
        blake3::hash(self.encode().as_bytes()).to_hex().to_string()
    }

    /// The JSON form carried by `share_pull`, digest included.
    #[must_use]
    pub fn to_wire(&self) -> Value {
        serde_json::json!({
            "origin": self.origin,
            "space": self.space,
            "seq": self.seq,
            "kind": self.kind.as_str(),
            "target": self.target,
            "fact": self.fact.as_ref().map(|fact| serde_json::json!({
                "fact_id": fact.fact_id,
                "kind": fact.kind.as_str(),
                "payload": fact.payload,
                "claim": fact.claim,
            })),
            "claimed_agent": self.claimed_agent,
            "digest": self.digest(),
        })
    }

    /// Strict inverse of [`ShareEvent::to_wire`]: unknown keys, wrong types,
    /// non-integer numbers, invalid events and a digest that does not match
    /// the reconstructed bytes are all `invalid_data`.
    pub fn from_wire(value: &Value) -> ShareResult<Self> {
        let bad = |what: &str| ShareError::InvalidData(format!("malformed wire event: {what}"));
        let object = value.as_object().ok_or_else(|| bad("not an object"))?;
        if let Some(key) = object
            .keys()
            .find(|k| !WIRE_EVENT_KEYS.contains(&k.as_str()))
        {
            return Err(bad(&format!("unknown key `{key}`")));
        }
        let text = |key: &str| -> ShareResult<String> {
            object
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| bad(key))
        };
        let seq = object
            .get("seq")
            .and_then(Value::as_u64)
            .ok_or_else(|| bad("seq"))?;
        let target = match object.get("target") {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_u64().ok_or_else(|| bad("target"))?),
        };
        let claimed_agent = match object.get("claimed_agent") {
            None | Some(Value::Null) => None,
            Some(Value::String(agent)) => Some(agent.clone()),
            Some(_) => return Err(bad("claimed_agent")),
        };
        let space = text("space")?;
        let fact = match object.get("fact") {
            None | Some(Value::Null) => None,
            Some(Value::Object(fact)) => {
                if let Some(key) = fact.keys().find(|k| !WIRE_FACT_KEYS.contains(&k.as_str())) {
                    return Err(bad(&format!("unknown fact key `{key}`")));
                }
                let claim = match fact.get("claim") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(claim)) => Some(claim.clone()),
                    Some(_) => return Err(bad("fact.claim")),
                };
                Some(SharedFact {
                    fact_id: fact
                        .get("fact_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| bad("fact.fact_id"))?
                        .to_string(),
                    kind: FactKind::parse(
                        fact.get("kind")
                            .and_then(Value::as_str)
                            .ok_or_else(|| bad("fact.kind"))?,
                    )?,
                    space: space.clone(),
                    payload: fact
                        .get("payload")
                        .cloned()
                        .ok_or_else(|| bad("fact.payload"))?,
                    claim,
                })
            }
            Some(_) => return Err(bad("fact")),
        };
        let event = Self {
            origin: text("origin")?,
            space,
            seq,
            kind: EventKind::parse(&text("kind")?)?,
            target,
            fact,
            claimed_agent,
        };
        event.validate()?;
        if event.digest() != text("digest")? {
            return Err(ShareError::InvalidData(
                "wire event digest does not match its content".into(),
            ));
        }
        Ok(event)
    }

    pub(crate) fn validate(&self) -> ShareResult<()> {
        validate_id(&self.origin, MAX_ID_BYTES, "origin")?;
        validate_space(&self.space)?;
        if self.seq == 0 || self.seq > MAX_SEQ {
            return Err(ShareError::InvalidData(format!(
                "seq {0} outside 1..={MAX_SEQ}",
                self.seq
            )));
        }
        if let Some(fact) = &self.fact
            && fact.space != self.space
        {
            return Err(ShareError::InvalidData(format!(
                "fact space `{}` does not match event space `{}`",
                fact.space, self.space
            )));
        }
        if let Some(agent) = &self.claimed_agent
            && (agent.is_empty()
                || agent.len() > MAX_ID_BYTES
                || agent.chars().any(char::is_control))
        {
            return Err(ShareError::InvalidData(
                "claimed_agent is empty, over 64 bytes, or contains NUL".into(),
            ));
        }
        match self.kind {
            EventKind::Put => {
                if self.target.is_some() {
                    return Err(ShareError::InvalidData(
                        "put must not carry a target".into(),
                    ));
                }
                self.fact
                    .as_ref()
                    .ok_or_else(|| ShareError::InvalidData("put requires a fact payload".into()))?
                    .validate()?;
            }
            EventKind::Retract => {
                if self.target.is_none() {
                    return Err(ShareError::InvalidData("retract requires a target".into()));
                }
                if self.fact.is_some() {
                    return Err(ShareError::InvalidData(
                        "retract must not carry a fact".into(),
                    ));
                }
            }
            EventKind::Supersede => {
                if self.target.is_none() {
                    return Err(ShareError::InvalidData(
                        "supersede requires a target".into(),
                    ));
                }
                self.fact
                    .as_ref()
                    .ok_or_else(|| {
                        ShareError::InvalidData("supersede requires a fact payload".into())
                    })?
                    .validate()?;
            }
        }
        Ok(())
    }
}

/// One `share_pull` page: events of this host's own origin only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnPage {
    pub origin: String,
    pub events: Vec<ShareEvent>,
    pub next_seq: u64,
    pub has_more: bool,
}

/// Host-local counts for `status`; no payloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShareCounts {
    pub space: String,
    pub enabled: bool,
    pub own_events: u64,
    pub own_live_facts: u64,
    pub received_live_facts: u64,
}

/// A materialized fact, referencing the original `put` event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FactRow {
    pub origin: String,
    pub fact_id: String,
    pub space: String,
    pub kind: FactKind,
    pub payload: Value,
    pub payload_digest: String,
    pub claim: Option<String>,
    pub status: FactStatus,
    /// The original `put` event this fact references.
    pub origin_seq: u64,
    /// The latest event that touched it (put, supersede or retract).
    pub last_seq: u64,
    pub claimed_agent: Option<String>,
    /// Trusted host context at apply time; never a remote claim.
    pub peer_id: String,
    pub applied_at: String,
}

/// One cursor over the single event stream, per `(peer, space, origin)`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CursorState {
    pub entries: BTreeMap<String, u64>,
}

impl CursorState {
    #[must_use]
    pub fn get(&self, origin: &str) -> Option<u64> {
        self.entries.get(origin).copied()
    }

    #[must_use]
    pub fn with(origin: &str, last_seq: u64) -> Self {
        Self {
            entries: BTreeMap::from([(origin.to_string(), last_seq)]),
        }
    }
}

impl fmt::Display for CursorState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for (origin, seq) in &self.entries {
            if !first {
                f.write_str(", ")?;
            }
            first = false;
            write!(f, "{origin}:{seq}")?;
        }
        Ok(())
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn sorted_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted = Map::new();
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                sorted.insert(key.clone(), sorted_value(&map[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted_value).collect()),
        other => other.clone(),
    }
}

fn sorted_json(value: &Value) -> String {
    serde_json::to_string(&sorted_value(value)).unwrap_or_else(|_| "{}".to_string())
}

fn validate_id(id: &str, max: usize, what: &str) -> ShareResult<()> {
    let valid = !id.is_empty()
        && id.len() <= max
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.');
    if valid {
        Ok(())
    } else {
        Err(ShareError::InvalidData(format!(
            "invalid {what} `{id}`: 1..={max} bytes of [A-Za-z0-9._-] required"
        )))
    }
}

fn validate_space(space: &str) -> ShareResult<()> {
    validate_id(space, MAX_ID_BYTES, "space")
}

const SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS meta(
  key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS host_spaces(
  space TEXT PRIMARY KEY, enabled INTEGER NOT NULL DEFAULT 1);
CREATE TABLE IF NOT EXISTS events(
  origin TEXT NOT NULL,
  space TEXT NOT NULL,
  seq INTEGER NOT NULL,
  kind TEXT NOT NULL,
  target INTEGER,
  event_digest TEXT NOT NULL,
  fact_id TEXT,
  fact_kind TEXT,
  payload TEXT,
  payload_digest TEXT,
  claim TEXT,
  claimed_agent TEXT,
  peer_id TEXT NOT NULL,
  applied_at TEXT NOT NULL,
  PRIMARY KEY(origin, space, seq)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_events_space ON events(space, origin, seq);
CREATE TABLE IF NOT EXISTS facts(
  origin TEXT NOT NULL,
  fact_id TEXT NOT NULL,
  space TEXT NOT NULL,
  kind TEXT NOT NULL,
  payload TEXT NOT NULL,
  payload_digest TEXT NOT NULL,
  claim TEXT,
  status TEXT NOT NULL,
  origin_seq INTEGER NOT NULL,
  last_seq INTEGER NOT NULL,
  claimed_agent TEXT,
  peer_id TEXT NOT NULL,
  applied_at TEXT NOT NULL,
  PRIMARY KEY(origin, fact_id)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_facts_space ON facts(space, status, origin, fact_id);
CREATE TABLE IF NOT EXISTS pull_cursor(
  peer_id TEXT NOT NULL,
  space TEXT NOT NULL,
  origin TEXT NOT NULL,
  last_seq INTEGER NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY(peer_id, space, origin)) WITHOUT ROWID;
";

/// The shared-memory store: one `share.sqlite`, parameterized by path.
/// Read-only openings never create or migrate anything.
pub struct ShareStore {
    pub(crate) conn: Connection,
    path: PathBuf,
    install_id: String,
}

impl ShareStore {
    /// Open (creating or initializing if needed) a share store.
    pub fn open(path: impl AsRef<Path>) -> ShareResult<Self> {
        let path = path.as_ref().to_path_buf();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        if !parent.as_os_str().is_empty() && !parent.exists() {
            // Only directories created here are made private; an existing
            // parent belongs to someone else's policy and is never chmodded.
            create_private_dirs(parent)?;
        }
        refuse_unsafe_store_path(&path)?;
        // Permissions are fixed BEFORE SQLite opens the file: closing any
        // other descriptor of an open SQLite file would drop this process's
        // POSIX locks on it. SQLite gives -wal/-shm the database's mode.
        set_file_private(&path)?;
        let conn = Connection::open_with_flags(&path, rw_create_nofollow())?;
        conn.busy_timeout(Duration::from_secs(5))?;
        // Identity check first: nothing may write to a foreign file, not
        // even a journal-mode pragma.
        let (app, version) = read_app_and_version(&conn)?;
        if app == 0 && version == 0 {
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            conn.pragma_update(None, "foreign_keys", "ON")?;
            conn.pragma_update(None, "application_id", SHARE_APPLICATION_ID)?;
            conn.pragma_update(None, "user_version", SHARE_SCHEMA_VERSION)?;
        } else if app != SHARE_APPLICATION_ID || version != SHARE_SCHEMA_VERSION {
            conn.close().ok();
            return Err(ShareError::Incompatible {
                found_app: app,
                found_version: version,
                expected_app: SHARE_APPLICATION_ID,
                expected_version: SHARE_SCHEMA_VERSION,
            });
        } else {
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            conn.pragma_update(None, "foreign_keys", "ON")?;
        }
        conn.execute_batch(SCHEMA_SQL)?;
        conn.execute_batch(crate::team::store::SQL)?;
        let install_id = Self::ensure_install_id(&conn, &path)?;
        Ok(Self {
            conn,
            path,
            install_id,
        })
    }

    /// Open read-only. Never creates, never migrates; a missing file is
    /// `NotFound`, a foreign or wrong-version file is `Incompatible`.
    pub fn open_readonly(path: impl AsRef<Path>) -> ShareResult<Self> {
        let path = path.as_ref().to_path_buf();
        if path.symlink_metadata().is_err() {
            return Err(ShareError::NotFound(path.display().to_string()));
        }
        refuse_unsafe_store_path(&path)?;
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let (app, version) = read_app_and_version(&conn)?;
        if app != SHARE_APPLICATION_ID || version != SHARE_SCHEMA_VERSION {
            conn.close().ok();
            return Err(ShareError::Incompatible {
                found_app: app,
                found_version: version,
                expected_app: SHARE_APPLICATION_ID,
                expected_version: SHARE_SCHEMA_VERSION,
            });
        }
        let install_id: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key='install_id'", [], |r| {
                r.get(0)
            })
            .optional()?;
        let Some(install_id) = install_id else {
            return Err(ShareError::Incompatible {
                found_app: app,
                found_version: version,
                expected_app: SHARE_APPLICATION_ID,
                expected_version: SHARE_SCHEMA_VERSION,
            });
        };
        Ok(Self {
            conn,
            path,
            install_id,
        })
    }

    fn ensure_install_id(conn: &Connection, path: &Path) -> ShareResult<String> {
        let existing: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key='install_id'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(id) = existing {
            validate_id(&id, MAX_ID_BYTES, "install_id")?;
            return Ok(id);
        }
        let seed = format!(
            "{}:{}:{}",
            path.display(),
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let hex = blake3::hash(seed.as_bytes()).to_hex();
        let id = format!("install-{}", &hex[..16]);
        conn.execute(
            "INSERT INTO meta(key, value) VALUES('install_id', ?1)",
            params![id],
        )?;
        Ok(id)
    }

    #[must_use]
    pub fn install_id(&self) -> &str {
        &self.install_id
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Host-level opt-in: enable a space for sharing.
    pub fn enable_space(&self, space: &str) -> ShareResult<()> {
        validate_space(space)?;
        self.conn.execute(
            "INSERT INTO host_spaces(space, enabled) VALUES(?1, 1) ON CONFLICT(space) DO UPDATE SET enabled = 1",
            params![space],
        )?;
        Ok(())
    }

    /// Host-level opt-out. Does not touch already-exported events.
    pub fn disable_space(&self, space: &str) -> ShareResult<bool> {
        let changed = self.conn.execute(
            "UPDATE host_spaces SET enabled = 0 WHERE space = ?1 AND enabled = 1",
            params![space],
        )?;
        Ok(changed == 1)
    }

    pub fn enabled_spaces(&self) -> ShareResult<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT space FROM host_spaces WHERE enabled = 1 ORDER BY space")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn space_enabled(&self, space: &str) -> ShareResult<bool> {
        let enabled: Option<i64> = self
            .conn
            .query_row(
                "SELECT enabled FROM host_spaces WHERE space = ?1",
                params![space],
                |r| r.get(0),
            )
            .optional()?;
        Ok(enabled == Some(1))
    }

    /// Authorization gate, run BEFORE every query and before replay:
    /// host-enabled space ∩ authenticated credential grants.
    pub fn authorize_read(&self, principal: &Principal, space: &str) -> ShareResult<()> {
        validate_space(space)?;
        if crate::team::is_team(space) {
            self.team_authorize(principal, space)?;
        }
        if !self.space_enabled(space)? {
            return Err(ShareError::SpaceNotEnabled(space.to_string()));
        }
        if !principal.has_read_grant(space) {
            return Err(ShareError::CapabilityDenied {
                space: space.to_string(),
            });
        }
        Ok(())
    }

    /// Explicitly publish one curated fact under this installation's origin.
    /// The only outbound path; nothing here reads the private store.
    pub fn publish(
        &mut self,
        space: &str,
        fact: SharedFact,
        claimed_agent: Option<&str>,
    ) -> ShareResult<u64> {
        if crate::team::is_team(space) {
            return Err(crate::team::bad());
        }
        if fact.space != space {
            return Err(ShareError::InvalidData(format!(
                "fact space `{}` does not match publish space `{space}`",
                fact.space
            )));
        }
        if !self.space_enabled(space)? {
            return Err(ShareError::SpaceNotEnabled(space.to_string()));
        }
        let event = ShareEvent {
            origin: self.install_id.clone(),
            space: space.to_string(),
            seq: 0,
            kind: EventKind::Put,
            target: None,
            fact: Some(fact),
            claimed_agent: claimed_agent.map(str::to_string),
        };
        let peer = TrustedPeer::new(LOCAL_PEER, now_rfc3339());
        self.append_local(event, &peer)
    }

    /// Append a retraction for one of this installation's live facts.
    pub fn retract(&mut self, space: &str, fact_id: &str) -> ShareResult<u64> {
        validate_space(space)?;
        validate_id(fact_id, MAX_FACT_ID_BYTES, "fact_id")?;
        let target = self.local_fact_origin_seq(space, fact_id)?;
        let event = ShareEvent {
            origin: self.install_id.clone(),
            space: space.to_string(),
            seq: 0,
            kind: EventKind::Retract,
            target: Some(target),
            fact: None,
            claimed_agent: None,
        };
        let peer = TrustedPeer::new(LOCAL_PEER, now_rfc3339());
        self.append_local(event, &peer)
    }

    /// Append a supersession: the fact's payload is replaced, the previous
    /// payload stays in the event stream as history.
    pub fn supersede(
        &mut self,
        space: &str,
        fact_id: &str,
        replacement: SharedFact,
        claimed_agent: Option<&str>,
    ) -> ShareResult<u64> {
        if crate::team::is_team(space) {
            return Err(crate::team::bad());
        }
        if replacement.fact_id != fact_id {
            return Err(ShareError::InvalidData(format!(
                "supersede cannot rename: `{fact_id}` != `{}`",
                replacement.fact_id
            )));
        }
        if replacement.space != space {
            return Err(ShareError::InvalidData(format!(
                "fact space `{}` does not match supersede space `{space}`",
                replacement.space
            )));
        }
        validate_space(space)?;
        let target = self.local_fact_origin_seq(space, fact_id)?;
        let event = ShareEvent {
            origin: self.install_id.clone(),
            space: space.to_string(),
            seq: 0,
            kind: EventKind::Supersede,
            target: Some(target),
            fact: Some(replacement),
            claimed_agent: claimed_agent.map(str::to_string),
        };
        let peer = TrustedPeer::new(LOCAL_PEER, now_rfc3339());
        self.append_local(event, &peer)
    }

    /// The `origin_seq` of a live local fact, for retract/supersede targets.
    fn local_fact_origin_seq(&self, space: &str, fact_id: &str) -> ShareResult<u64> {
        self.conn
            .query_row(
                "SELECT origin_seq FROM facts WHERE origin = ?1 AND space = ?2 \
                 AND fact_id = ?3 AND status = 'live'",
                params![self.install_id, space, fact_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .map(|s| u64::try_from(s).unwrap_or(0))
            .ok_or_else(|| ShareError::NoLiveFact {
                space: space.to_string(),
                fact_id: fact_id.to_string(),
            })
    }

    /// Append one local event, allocating its sequence number inside the
    /// transaction so two processes can never take the same slot.
    fn append_local(&mut self, mut event: ShareEvent, peer: &TrustedPeer) -> ShareResult<u64> {
        validate_id(&event.origin, MAX_ID_BYTES, "origin")?;
        validate_space(&event.space)?;
        let tx = self.conn.transaction()?;
        let last: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM events WHERE origin = ?1 AND space = ?2",
                params![event.origin, event.space],
                |r| r.get(0),
            )
            .unwrap_or(0);
        event.seq = u64::try_from(last + 1).unwrap_or(1);
        if event.seq > MAX_SEQ {
            return Err(ShareError::InvalidData(format!(
                "seq {0} outside 1..={MAX_SEQ}",
                event.seq
            )));
        }
        event.validate()?;
        let digest = event.digest();
        apply_event_tx(&tx, &event, &digest, peer)?;
        tx.commit()?;
        Ok(event.seq)
    }

    /// Stable paging over the single event stream of one origin and space.
    /// Authorized before any row is read. `after_seq = 0` starts from the
    /// beginning. Every served event is re-digested and a mismatch is a
    /// hard error, so tampered rows never travel.
    pub fn page(
        &self,
        principal: &Principal,
        space: &str,
        origin: &str,
        after_seq: u64,
        limit: u32,
    ) -> ShareResult<(Vec<ShareEvent>, u64)> {
        self.authorize_read(principal, space)?;
        validate_id(origin, MAX_ID_BYTES, "origin")?;
        if limit == 0 || limit > MAX_PAGE {
            return Err(ShareError::PageLimit { requested: limit });
        }
        let mut stmt = self.conn.prepare(
            "SELECT seq, kind, target, fact_id, fact_kind, payload, claim, \
             claimed_agent, event_digest FROM events \
             WHERE origin = ?1 AND space = ?2 AND seq > ?3 \
             ORDER BY seq LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![
                origin,
                space,
                i64::try_from(after_seq).unwrap_or(i64::MAX),
                i64::from(limit)
            ],
            |row| {
                Ok(RawEvent {
                    seq: row.get(0)?,
                    kind: row.get(1)?,
                    target: row.get(2)?,
                    fact_id: row.get(3)?,
                    fact_kind: row.get(4)?,
                    payload: row.get(5)?,
                    claim: row.get(6)?,
                    claimed_agent: row.get(7)?,
                    stored_digest: row.get(8)?,
                })
            },
        )?;
        let mut events = Vec::new();
        let mut next = after_seq;
        for raw in rows {
            let raw = raw?;
            let fact = match raw.fact_id {
                Some(fact_id) => Some(SharedFact {
                    fact_id,
                    kind: FactKind::parse(raw.fact_kind.as_deref().unwrap_or("fact"))?,
                    space: space.to_string(),
                    payload: serde_json::from_str(&raw.payload.clone().unwrap_or_default())
                        .unwrap_or(Value::Null),
                    claim: raw.claim,
                }),
                None => None,
            };
            let event = ShareEvent {
                origin: origin.to_string(),
                space: space.to_string(),
                seq: u64::try_from(raw.seq).unwrap_or(0),
                kind: EventKind::parse(&raw.kind)?,
                target: raw.target.map(|t| u64::try_from(t).unwrap_or(0)),
                fact,
                claimed_agent: raw.claimed_agent,
            };
            if event.digest() != raw.stored_digest {
                return Err(ShareError::InvalidData(format!(
                    "stored event {origin}/{space}/{} does not match its recorded digest",
                    event.seq
                )));
            }
            next = event.seq;
            events.push(event);
        }
        Ok((events, next))
    }

    /// Apply one page batch pulled from a peer, atomically: the local host
    /// must have enabled the space (receiving is opt-in too), then validate,
    /// reject cursor mismatch/gaps/out-of-order, apply,
    /// and advance the cursor only inside the same transaction. A failure
    /// rolls back the whole batch — the cursor never claims a partial apply.
    /// Re-offering the same event bytes is idempotent (even via another
    /// peer); same slot with different bytes is a conflict.
    pub fn apply_batch(
        &mut self,
        space: &str,
        peer: &TrustedPeer,
        expected: &CursorState,
        events: &[ShareEvent],
    ) -> ShareResult<CursorState> {
        validate_space(space)?;
        validate_id(&peer.peer_id, MAX_ID_BYTES, "peer_id")?;
        if events.len() > MAX_BATCH_EVENTS {
            return Err(ShareError::BatchTooLarge {
                events: events.len(),
                bytes: 0,
            });
        }
        let bytes: usize = events.iter().map(|e| e.encode().len()).sum();
        if bytes > MAX_BATCH_BYTES {
            return Err(ShareError::BatchTooLarge {
                events: events.len(),
                bytes,
            });
        }
        // Local opt-in before replay, every call. The reader credential that
        // authorized the pull at the remote host is not local authority.
        if !self.space_enabled(space)? {
            return Err(ShareError::SpaceNotEnabled(space.to_string()));
        }
        if events.is_empty() {
            return self.cursor(&peer.peer_id, space);
        }
        let origin = events[0].origin.clone();
        for event in events {
            event.validate()?;
            if event.origin != origin {
                return Err(ShareError::InvalidData(
                    "batch mixes more than one origin".into(),
                ));
            }
            if event.origin == self.install_id {
                return Err(ShareError::InvalidData(
                    "a remote batch cannot claim this installation's own origin".into(),
                ));
            }
            if event.space != space {
                return Err(ShareError::InvalidData(format!(
                    "event space `{}` does not match batch space `{space}`",
                    event.space
                )));
            }
        }
        for pair in events.windows(2) {
            if pair[0].seq >= pair[1].seq {
                return Err(ShareError::Gap {
                    origin: origin.clone(),
                    space: space.to_string(),
                    expected: pair[0].seq + 1,
                    got: pair[1].seq,
                });
            }
        }
        let tx = self.conn.transaction()?;
        let stored: i64 = tx
            .query_row(
                "SELECT last_seq FROM pull_cursor WHERE peer_id = ?1 AND space = ?2 \
                 AND origin = ?3",
                params![peer.peer_id, space, origin],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let offered = expected.get(&origin).unwrap_or(0);
        if offered != u64::try_from(stored).unwrap_or(0) {
            return Err(ShareError::CursorMismatch {
                origin,
                space: space.to_string(),
                stored: u64::try_from(stored).unwrap_or(0),
                offered,
            });
        }
        // The stored stream must stay gap-free per (origin, space), across
        // every peer that ever delivered a part of it.
        let mut stream_max: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM events WHERE origin = ?1 AND space = ?2",
                params![origin, space],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let mut last = stored;
        for event in events {
            let digest = event.digest();
            let seq = i64::try_from(event.seq).unwrap_or(i64::MAX);
            let stored_digest: Option<String> = tx
                .query_row(
                    "SELECT event_digest FROM events WHERE origin = ?1 AND space = ?2 \
                     AND seq = ?3",
                    params![origin, space, seq],
                    |r| r.get(0),
                )
                .optional()?;
            match stored_digest {
                Some(d) if d == digest => {
                    // Idempotent replay of an already-applied event.
                }
                Some(_) => {
                    return Err(ShareError::Conflict {
                        origin,
                        space: space.to_string(),
                        seq: event.seq,
                    });
                }
                None => {
                    if seq != stream_max + 1 {
                        return Err(ShareError::Gap {
                            origin: origin.clone(),
                            space: space.to_string(),
                            expected: u64::try_from(stream_max + 1).unwrap_or(0),
                            got: event.seq,
                        });
                    }
                    apply_event_tx(&tx, event, &digest, peer)?;
                    stream_max = seq;
                }
            }
            last = seq;
        }
        // The cursor never regresses below what this peer already had.
        let final_last = last.max(stored);
        let updated_at = now_rfc3339();
        tx.execute(
            "INSERT INTO pull_cursor(peer_id, space, origin, last_seq, updated_at) \
             VALUES(?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(peer_id, space, origin) \
             DO UPDATE SET last_seq = excluded.last_seq, updated_at = excluded.updated_at",
            params![peer.peer_id, space, origin, final_last, updated_at],
        )?;
        tx.commit()?;
        Ok(CursorState::with(
            &origin,
            u64::try_from(final_last).unwrap_or(0),
        ))
    }

    /// The stored cursor for one peer and space, over the event stream.
    pub fn cursor(&self, peer_id: &str, space: &str) -> ShareResult<CursorState> {
        let mut stmt = self.conn.prepare(
            "SELECT origin, last_seq FROM pull_cursor WHERE peer_id = ?1 AND space = ?2",
        )?;
        let rows = stmt.query_map(params![peer_id, space], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut entries = BTreeMap::new();
        for row in rows {
            let (origin, last) = row?;
            entries.insert(origin, u64::try_from(last).unwrap_or(0));
        }
        Ok(CursorState { entries })
    }

    /// Live (non-retracted) facts of a space. Revocation is re-checked on
    /// every call: the status is read fresh, nothing is cached.
    pub fn live_facts(
        &self,
        principal: &Principal,
        space: &str,
        limit: u32,
    ) -> ShareResult<Vec<FactRow>> {
        self.authorize_read(principal, space)?;
        if limit == 0 || limit > MAX_PAGE {
            return Err(ShareError::PageLimit { requested: limit });
        }
        let mut stmt = self.conn.prepare(
            "SELECT origin, fact_id, space, kind, payload, payload_digest, claim, \
             status, origin_seq, last_seq, claimed_agent, peer_id, applied_at \
             FROM facts WHERE space = ?1 AND status = 'live' \
             ORDER BY origin, fact_id LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![space, i64::from(limit)], raw_fact_row_mapper)?;
        let mut facts = Vec::new();
        for raw in rows {
            facts.push(fact_from_raw(raw?)?);
        }
        Ok(facts)
    }

    /// One fact by id (live or retracted), for audit.
    pub fn fact(
        &self,
        principal: &Principal,
        space: &str,
        fact_id: &str,
    ) -> ShareResult<Option<FactRow>> {
        self.authorize_read(principal, space)?;
        let row = self
            .conn
            .query_row(
                "SELECT origin, fact_id, space, kind, payload, payload_digest, claim, \
                 status, origin_seq, last_seq, claimed_agent, peer_id, applied_at \
                 FROM facts WHERE space = ?1 AND fact_id = ?2",
                params![space, fact_id],
                raw_fact_row_mapper,
            )
            .optional()?;
        row.map(fact_from_raw).transpose()
    }

    /// `share_pull`: one bounded page of THIS installation's own event
    /// stream. Relayed events from other origins are never re-served, so a
    /// puller only ever receives provenance this host itself asserts.
    /// Authorized (host-enabled space ∩ credential grant) before any read.
    pub fn page_own(
        &self,
        principal: &Principal,
        space: &str,
        after_seq: u64,
        limit: u32,
    ) -> ShareResult<OwnPage> {
        if crate::team::is_team(space) {
            return self.team_page(principal, space, after_seq, limit);
        }
        if limit == 0 || limit > MAX_WIRE_PAGE {
            self.authorize_read(principal, space)?;
            return Err(ShareError::PageLimit { requested: limit });
        }
        if after_seq > MAX_SEQ {
            self.authorize_read(principal, space)?;
            return Err(ShareError::InvalidData(format!(
                "after_seq outside 0..={MAX_SEQ}"
            )));
        }
        let origin = self.install_id.clone();
        // One extra row tells whether more remains without a second query.
        let (mut events, _) = self.page(principal, space, &origin, after_seq, limit + 1)?;
        let mut has_more = events.len() > limit as usize;
        events.truncate(limit as usize);
        let mut bytes = 0_usize;
        let mut kept = 0_usize;
        for event in &events {
            bytes += event.to_wire().to_string().len();
            if kept > 0 && bytes > MAX_WIRE_PAGE_BYTES {
                has_more = true;
                break;
            }
            kept += 1;
        }
        events.truncate(kept);
        let next_seq = events.last().map_or(after_seq, |event| event.seq);
        Ok(OwnPage {
            origin,
            events,
            next_seq,
            has_more,
        })
    }

    /// `share_query`: live facts THIS installation published in a space,
    /// authorized before any read.
    pub fn own_live_facts(
        &self,
        principal: &Principal,
        space: &str,
        limit: u32,
    ) -> ShareResult<Vec<FactRow>> {
        self.authorize_read(principal, space)?;
        if crate::team::is_team(space) {
            return Err(crate::team::bad());
        }
        if limit == 0 || limit > MAX_WIRE_PAGE {
            return Err(ShareError::PageLimit { requested: limit });
        }
        self.facts_where(space, Some(&self.install_id), true, limit)
    }

    /// Host-local administration only (the CLI on this machine, never the
    /// wire): every fact of a space, own and received, live or retracted.
    pub fn local_facts(
        &self,
        space: &str,
        include_retracted: bool,
        limit: u32,
    ) -> ShareResult<Vec<FactRow>> {
        validate_space(space)?;
        if limit == 0 || limit > MAX_PAGE {
            return Err(ShareError::PageLimit { requested: limit });
        }
        let mut stmt = self.conn.prepare(
            "SELECT origin, fact_id, space, kind, payload, payload_digest, claim, \
             status, origin_seq, last_seq, claimed_agent, peer_id, applied_at \
             FROM facts WHERE space = ?1 AND (?2 OR status = 'live') \
             ORDER BY origin, fact_id LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![space, include_retracted, i64::from(limit)],
            raw_fact_row_mapper,
        )?;
        rows.map(|raw| fact_from_raw(raw?)).collect()
    }

    fn facts_where(
        &self,
        space: &str,
        origin: Option<&str>,
        live_only: bool,
        limit: u32,
    ) -> ShareResult<Vec<FactRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT origin, fact_id, space, kind, payload, payload_digest, claim, \
             status, origin_seq, last_seq, claimed_agent, peer_id, applied_at \
             FROM facts WHERE space = ?1 AND (?2 IS NULL OR origin = ?2) \
             AND (NOT ?3 OR status = 'live') ORDER BY origin, fact_id LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![space, origin, live_only, i64::from(limit)],
            raw_fact_row_mapper,
        )?;
        rows.map(|raw| fact_from_raw(raw?)).collect()
    }

    /// Host-local status counts for one space (no payloads).
    pub fn counts(&self, space: &str) -> ShareResult<ShareCounts> {
        validate_space(space)?;
        let count = |sql: &str| -> ShareResult<u64> {
            let n: i64 = self
                .conn
                .query_row(sql, params![space, self.install_id], |r| r.get(0))?;
            Ok(u64::try_from(n).unwrap_or(0))
        };
        Ok(ShareCounts {
            space: space.to_string(),
            enabled: self.space_enabled(space)?,
            own_events: count("SELECT COUNT(*) FROM events WHERE space = ?1 AND origin = ?2")?,
            own_live_facts: count(
                "SELECT COUNT(*) FROM facts WHERE space = ?1 AND origin = ?2 AND status = 'live'",
            )?,
            received_live_facts: count(
                "SELECT COUNT(*) FROM facts WHERE space = ?1 AND origin <> ?2 AND status = 'live'",
            )?,
        })
    }

    /// Adapter from facts stored here into `map_packet` memory snippets.
    ///
    /// The reader is either a remote [`Principal`] (host-enabled space ∩
    /// credential grant, exactly like every other read) or the host itself
    /// through [`SharedReader::HostLocal`], which only checks that the host
    /// enabled the space. Only live facts are returned, so a retraction
    /// pulled here removes the fact from retrieval; copies a peer already
    /// holds are never erased by it.
    ///
    /// Scope comes from the curated payload (`"project"` or
    /// `"scope":"space"`) and is applied BEFORE `limit`, so facts of other
    /// projects can never crowd out this one. Scope classifies; it grants
    /// nothing. Kinds stay what they are — a fact is a `fact`, a link a
    /// `link` — and provenance keeps the origin's claims apart from the peer
    /// this host recorded at apply time.
    pub fn shared_snippets(
        &self,
        reader: &SharedReader<'_>,
        space: &str,
        project: Option<&str>,
        query: Option<&str>,
        limit: u32,
    ) -> ShareResult<Vec<MemorySnippet>> {
        match reader {
            SharedReader::Remote(principal) => self.authorize_read(principal, space)?,
            SharedReader::HostLocal(_) => {
                validate_space(space)?;
                if !self.space_enabled(space)? {
                    return Err(ShareError::SpaceNotEnabled(space.to_string()));
                }
            }
        }
        if limit == 0 || limit > MAX_PAGE {
            return Err(ShareError::PageLimit { requested: limit });
        }
        let terms: Vec<String> = query
            .unwrap_or_default()
            .split_whitespace()
            .filter(|term| term.chars().count() >= 2)
            .map(str::to_lowercase)
            .collect();
        // Newest first; scope is filtered in SQL so the scan bound below can
        // never be exhausted by another project's facts.
        let mut stmt = self.conn.prepare(
            "SELECT origin, fact_id, space, kind, payload, payload_digest, claim, \
             status, origin_seq, last_seq, claimed_agent, peer_id, applied_at \
             FROM facts WHERE space = ?1 AND status = 'live' \
             AND (?2 IS NULL OR json_extract(payload, '$.project') = ?2 \
                  OR (json_extract(payload, '$.project') IS NULL \
                      AND json_extract(payload, '$.scope') = 'space')) \
             ORDER BY last_seq DESC, origin, fact_id LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![space, project, i64::from(SHARED_SCAN_LIMIT)],
            raw_fact_row_mapper,
        )?;
        let mut ranked: Vec<(usize, MemorySnippet)> = Vec::new();
        for raw in rows {
            let fact = fact_from_raw(raw?)?;
            // A link's refs live in its payload, so it is always shown.
            let text = match (&fact.claim, fact.kind) {
                (Some(claim), FactKind::Link) => format!("{claim} {}", sorted_json(&fact.payload)),
                (Some(claim), FactKind::Fact) => claim.clone(),
                (None, _) => sorted_json(&fact.payload),
            };
            let matched = if terms.is_empty() {
                1
            } else {
                let haystack = format!("{text} {}", sorted_json(&fact.payload)).to_lowercase();
                terms
                    .iter()
                    .filter(|term| haystack.contains(term.as_str()))
                    .count()
            };
            if matched == 0 {
                continue;
            }
            ranked.push((matched, shared_snippet(fact, text)));
        }
        // Stable: equal matches keep newest-first order from SQL.
        ranked.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        Ok(ranked
            .into_iter()
            .take(limit as usize)
            .map(|(_, snippet)| snippet)
            .collect())
    }
}

/// Bound on live facts one retrieval scans after its scope filter.
pub const SHARED_SCAN_LIMIT: u32 = 2_000;

/// The host reading its own share store, by its own configuration (a CLI
/// flag given when the process starts). It is not an authenticated network
/// principal and is never built from a request: no MCP argument, payload or
/// remote claim can produce one. Constructing it is the host's decision.
#[derive(Clone, Copy, Debug)]
pub struct HostLocalRead(());

impl HostLocalRead {
    /// Only for host-configured reads of this installation's own store.
    #[must_use]
    pub fn from_host_configuration() -> Self {
        Self(())
    }
}

/// Who reads shared facts for retrieval.
#[derive(Clone, Copy, Debug)]
pub enum SharedReader<'a> {
    /// A remote caller: its credential's grants are checked.
    Remote(&'a Principal),
    /// This host, through its own configuration.
    HostLocal(HostLocalRead),
}

/// One stored fact as a packet memory. The id is a digest of origin and fact
/// id, so it fits the packet's id grammar and never collides across origins.
fn shared_snippet(fact: FactRow, text: String) -> MemorySnippet {
    let scope = match (
        fact.payload.get("project").and_then(Value::as_str),
        fact.payload.get("scope").and_then(Value::as_str),
    ) {
        (Some(project), _) if crate::coordination::is_valid_project(project) => {
            MemoryScope::Project(project.to_owned())
        }
        (None, Some("space")) => MemoryScope::Space,
        _ => MemoryScope::Unknown,
    };
    let digest = blake3::hash(format!("{}\0{}", fact.origin, fact.fact_id).as_bytes()).to_hex();
    MemorySnippet {
        space: fact.space,
        id: format!("shared-{}", &digest[..32]),
        kind: match fact.kind {
            FactKind::Fact => MemoryKind::Fact,
            FactKind::Link => MemoryKind::Link,
        },
        scope,
        text,
        provenance: "shared_fact".to_owned(),
        evidence: None,
        shared: Some(SharedProvenance {
            origin: fact.origin,
            fact_id: fact.fact_id,
            origin_seq: fact.origin_seq,
            claimed_agent: fact.claimed_agent,
            received_from_peer: fact.peer_id,
        }),
    }
}

/// Raw columns of one event row, before reconstruction.
struct RawEvent {
    seq: i64,
    kind: String,
    target: Option<i64>,
    fact_id: Option<String>,
    fact_kind: Option<String>,
    payload: Option<String>,
    claim: Option<String>,
    claimed_agent: Option<String>,
    stored_digest: String,
}

/// Raw columns of one fact row, before reconstruction and validation.
struct RawFactRow {
    origin: String,
    fact_id: String,
    space: String,
    kind: String,
    payload_text: String,
    payload_digest: String,
    claim: Option<String>,
    status: String,
    origin_seq: i64,
    last_seq: i64,
    claimed_agent: Option<String>,
    peer_id: String,
    applied_at: String,
}

fn raw_fact_row_mapper(row: &Row<'_>) -> rusqlite::Result<RawFactRow> {
    Ok(RawFactRow {
        origin: row.get(0)?,
        fact_id: row.get(1)?,
        space: row.get(2)?,
        kind: row.get(3)?,
        payload_text: row.get(4)?,
        payload_digest: row.get(5)?,
        claim: row.get(6)?,
        status: row.get(7)?,
        origin_seq: row.get(8)?,
        last_seq: row.get(9)?,
        claimed_agent: row.get(10)?,
        peer_id: row.get(11)?,
        applied_at: row.get(12)?,
    })
}

/// Rebuild a materialized fact; corrupt stored data is a domain error, not
/// a panic and not a silent default.
fn fact_from_raw(raw: RawFactRow) -> ShareResult<FactRow> {
    Ok(FactRow {
        origin: raw.origin,
        fact_id: raw.fact_id,
        space: raw.space,
        kind: FactKind::parse(&raw.kind)?,
        payload: serde_json::from_str(&raw.payload_text)
            .map_err(|e| ShareError::InvalidData(format!("stored payload is not JSON: {e}")))?,
        payload_digest: raw.payload_digest,
        claim: raw.claim,
        status: FactStatus::parse(&raw.status)?,
        origin_seq: u64::try_from(raw.origin_seq).unwrap_or(0),
        last_seq: u64::try_from(raw.last_seq).unwrap_or(0),
        claimed_agent: raw.claimed_agent,
        peer_id: raw.peer_id,
        applied_at: raw.applied_at,
    })
}

pub(crate) fn apply_event_tx(
    tx: &rusqlite::Transaction<'_>,
    event: &ShareEvent,
    digest: &str,
    peer: &TrustedPeer,
) -> ShareResult<()> {
    let insert_event = |fact: Option<&SharedFact>| -> ShareResult<()> {
        tx.execute(
            "INSERT INTO events(origin, space, seq, kind, target, event_digest, \
             fact_id, fact_kind, payload, payload_digest, claim, claimed_agent, \
             peer_id, applied_at) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                event.origin,
                event.space,
                i64::try_from(event.seq).unwrap_or(0),
                event.kind.as_str(),
                event.target.map(|t| i64::try_from(t).unwrap_or(0)),
                digest,
                fact.map(|f| f.fact_id.clone()),
                fact.map(|f| f.kind.as_str()),
                fact.map(|f| f.canonical_payload()),
                fact.map(|f| {
                    blake3::hash(f.canonical_payload().as_bytes())
                        .to_hex()
                        .to_string()
                }),
                fact.and_then(|f| f.claim.clone()),
                event.claimed_agent,
                peer.peer_id,
                peer.applied_at,
            ],
        )?;
        Ok(())
    };
    let seq = i64::try_from(event.seq).unwrap_or(0);
    match event.kind {
        EventKind::Put => {
            let fact = event.fact.as_ref().expect("validated");
            let exists: Option<i64> = tx
                .query_row(
                    "SELECT 1 FROM facts WHERE origin = ?1 AND fact_id = ?2",
                    params![event.origin, fact.fact_id],
                    |r| r.get(0),
                )
                .optional()?;
            if exists.is_some() {
                return Err(ShareError::InvalidData(format!(
                    "put after put for {}/{}: supersede is required",
                    event.origin, fact.fact_id
                )));
            }
            insert_event(Some(fact))?;
            tx.execute(
                "INSERT INTO facts(origin, fact_id, space, kind, payload, payload_digest, \
                 claim, status, origin_seq, last_seq, claimed_agent, peer_id, applied_at) \
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, 'live', ?8, ?8, ?9, ?10, ?11)",
                params![
                    event.origin,
                    fact.fact_id,
                    event.space,
                    fact.kind.as_str(),
                    fact.canonical_payload(),
                    blake3::hash(fact.canonical_payload().as_bytes())
                        .to_hex()
                        .to_string(),
                    fact.claim,
                    seq,
                    event.claimed_agent,
                    peer.peer_id,
                    peer.applied_at,
                ],
            )?;
        }
        EventKind::Retract => {
            let target = event.target.expect("validated");
            insert_event(None)?;
            let changed = tx.execute(
                "UPDATE facts SET status = 'retracted', last_seq = ?4 \
                 WHERE origin = ?1 AND space = ?2 AND origin_seq = ?3 AND status = 'live'",
                params![
                    event.origin,
                    event.space,
                    i64::try_from(target).unwrap_or(0),
                    seq
                ],
            )?;
            if changed == 0 {
                return Err(ShareError::UnknownTarget {
                    origin: event.origin.clone(),
                    space: event.space.clone(),
                    target,
                });
            }
        }
        EventKind::Supersede => {
            let target = event.target.expect("validated");
            let fact = event.fact.as_ref().expect("validated");
            let stored_id: Option<String> = tx
                .query_row(
                    "SELECT fact_id FROM facts WHERE origin = ?1 AND space = ?2 \
                     AND origin_seq = ?3 AND status = 'live'",
                    params![
                        event.origin,
                        event.space,
                        i64::try_from(target).unwrap_or(0)
                    ],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(stored_id) = stored_id else {
                return Err(ShareError::UnknownTarget {
                    origin: event.origin.clone(),
                    space: event.space.clone(),
                    target,
                });
            };
            if stored_id != fact.fact_id {
                return Err(ShareError::InvalidData(format!(
                    "supersede cannot rename: `{stored_id}` != `{}`",
                    fact.fact_id
                )));
            }
            insert_event(Some(fact))?;
            let changed = tx.execute(
                "UPDATE facts SET payload = ?4, payload_digest = ?5, claim = ?6, \
                 kind = ?7, last_seq = ?8 \
                 WHERE origin = ?1 AND space = ?2 AND origin_seq = ?3 AND status = 'live'",
                params![
                    event.origin,
                    event.space,
                    i64::try_from(target).unwrap_or(0),
                    fact.canonical_payload(),
                    blake3::hash(fact.canonical_payload().as_bytes())
                        .to_hex()
                        .to_string(),
                    fact.claim,
                    fact.kind.as_str(),
                    seq
                ],
            )?;
            if changed == 0 {
                return Err(ShareError::UnknownTarget {
                    origin: event.origin.clone(),
                    space: event.space.clone(),
                    target,
                });
            }
        }
    }
    Ok(())
}

fn read_app_and_version(conn: &Connection) -> ShareResult<(u32, u32)> {
    let app: i64 = conn.pragma_query_value(None, "application_id", |r| r.get(0))?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    Ok((
        u32::try_from(app).unwrap_or(0),
        u32::try_from(version).unwrap_or(0),
    ))
}

fn rw_create_nofollow() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NOFOLLOW
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
}

/// SQLite sidecars sit next to the database as `<name>-wal` / `<name>-shm`.
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// The store file and its sidecars must not be symlinks, must be regular
/// files and (on unix) must belong to this uid. A missing file is fine:
/// it is created with `SQLITE_OPEN_NOFOLLOW`.
fn refuse_unsafe_store_path(path: &Path) -> ShareResult<()> {
    for candidate in [
        path.to_path_buf(),
        sidecar(path, "-wal"),
        sidecar(path, "-shm"),
    ] {
        let Ok(meta) = candidate.symlink_metadata() else {
            continue;
        };
        if meta.file_type().is_symlink() {
            return Err(ShareError::InvalidData(format!(
                "{} is a symlink; the share store is never opened through one",
                candidate.display()
            )));
        }
        if !meta.is_file() {
            return Err(ShareError::InvalidData(format!(
                "{} is not a regular file",
                candidate.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if crate::network::tls::current_uid() != Some(meta.uid()) {
                return Err(ShareError::InvalidData(format!(
                    "{} is owned by another user",
                    candidate.display()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn create_private_dirs(dir: &Path) -> ShareResult<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| ShareError::InvalidData(format!("cannot create {}: {e}", dir.display())))
}

#[cfg(not(unix))]
fn create_private_dirs(dir: &Path) -> ShareResult<()> {
    std::fs::create_dir_all(dir)
        .map_err(|e| ShareError::InvalidData(format!("cannot create {}: {e}", dir.display())))
}

/// Create the database file 0600 with `O_NOFOLLOW`, or clamp an existing
/// (already verified: regular, own, not a symlink) file and sidecars by
/// path. Runs before any SQLite connection exists, so no lock is disturbed.
#[cfg(unix)]
fn set_file_private(path: &Path) -> ShareResult<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let fail = |what: &Path, e: std::io::Error| {
        ShareError::InvalidData(format!("cannot secure {}: {e}", what.display()))
    };
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(fail(path, e)),
    }
    for candidate in [
        path.to_path_buf(),
        sidecar(path, "-wal"),
        sidecar(path, "-shm"),
    ] {
        if candidate.symlink_metadata().is_ok() {
            std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| fail(&candidate, e))?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_file_private(_path: &Path) -> ShareResult<()> {
    Ok(())
}

#[cfg(test)]
mod tests;
