//! The `brain-event/1` record: validation, limits and the canonical hash.
//!
//! Every limit is in bytes and refuses rather than truncates, so a value is
//! never cut inside a UTF-8 sequence and never silently changed.

use serde_json::Value;

use super::{MapError, MapErrorKind};

/// Event format version this binary writes and accepts.
pub const EVENT_VERSION: i64 = 1;
/// Kinds of event. Only `link_*` and `correction` feed the links projection;
/// `coord_recorded`, `outcome_claimed` and `outcome_observed` make the timeline.
pub const KINDS: [&str; 9] = [
    "coord_recorded",
    "link_proposed",
    "link_confirmed",
    "link_rejected",
    "outcome_observed",
    "outcome_claimed",
    "correction",
    "share_published",
    "import_progress",
];
/// Basis with its confidence ceiling in thousandths. Above the ceiling is refused.
pub const BASES: [(&str, u16); 7] = [
    ("user_confirmed", 1000),
    ("coordinator_record", 900),
    ("user_statement", 800),
    ("agent_claim", 700),
    ("cwd_evidence", 600),
    ("recorded_visible_message", 500),
    ("time_overlap_heuristic", 300),
];
/// Who the assertion says made it. `user` needs local authority.
pub const ASSERTED_KINDS: [&str; 5] = ["user", "agent", "coordinator", "collector", "brain"];
pub const OCCURRED_SOURCES: [&str; 3] = ["client_ts", "coordinator_clock", "unknown"];
/// Reference prefixes of the fixed grammar `<prefix>:<space-or-install>/<id>`.
pub const REF_PREFIXES: [&str; 8] = [
    "archive",
    "archive-session",
    "task",
    "coord-event",
    "memory",
    "project",
    "status",
    "outcome",
];

/// Version of the relation table below. Stored in `meta.relations`; a file
/// written under another policy is refused, so cardinality never changes
/// silently between machines, actors or replays.
pub const RELATION_POLICY: &str = "brain-relations/1";

/// How many objects one `(space, subject, relation)` slot may hold at once.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Cardinality {
    /// One true value: different live objects are a conflict.
    Single,
    /// An association: every object stands on its own; two objects never conflict.
    Multi,
}

/// Built-in, trusted relations with their cardinality and allowed object prefixes.
/// Fixed in code for `brain-relations/1`: event text, an agent or a replayed
/// record cannot add a relation or change its cardinality. Anything else is refused.
pub const RELATIONS: [(&str, Cardinality, &[&str]); 7] = [
    ("has_session", Cardinality::Multi, &["archive-session"]),
    (
        "has_decision",
        Cardinality::Multi,
        &["memory", "archive", "coord-event"],
    ),
    (
        "has_error",
        Cardinality::Multi,
        &["archive", "coord-event", "memory"],
    ),
    ("declared_root", Cardinality::Multi, &["project"]),
    ("depends_on", Cardinality::Multi, &["task"]),
    ("current_status", Cardinality::Single, &["status"]),
    ("outcome", Cardinality::Single, &["outcome"]),
];

/// Cardinality and object prefixes of a known relation; `invalid_input` otherwise.
pub fn relation_policy(relation: &str) -> Result<(Cardinality, &'static [&'static str]), MapError> {
    RELATIONS
        .iter()
        .find(|(name, _, _)| *name == relation)
        .map(|(_, c, p)| (*c, *p))
        .ok_or_else(|| {
            MapError::invalid(format!(
                "relation {} is not in {RELATION_POLICY}",
                printable(relation)
            ))
        })
}
/// Payload keys allowed: scalars only. `note`, `reason` and `command` are free
/// text a caller writes; the map never fills them from the archive, but it
/// cannot tell what a caller typed (see `docs/memory-map.md`, source policy).
const PAYLOAD_KEYS: [(&str, usize); 14] = [
    // Structured coordinator fields copied by ingestion; never note or data text.
    ("coord_kind", 32),
    ("actor_machine", 64),
    ("coord_id", 0),
    ("claim_id", 0),
    ("fence", 0),
    ("source_at", 0),
    // How many declared roots one cwd matched: >1 is an ambiguous, labelled proposal.
    ("candidates", 0),
    ("note", 500),
    ("reason", 200),
    ("command", 500),
    ("output_sha256", 64),
    ("exit_code", 0),
    ("duration_ms", 0),
    ("count", 0),
];

pub const MAX_SPACE: usize = 64;
pub const MAX_REF: usize = 256;
pub const MAX_ACTOR: usize = 64;
pub const MAX_RELATION: usize = 32;
pub const MAX_EVIDENCE: usize = 16;
pub const MAX_PAYLOAD: usize = 4096;
pub const MAX_SEQ: i64 = 999_999_999_999;

/// Authority the *local caller* vouches for, kept apart from what the event asserts.
///
/// Nothing in an event body can create this. `LocalUser` must be built only by
/// code paths where a human at this machine acted directly (CLI prompt, UI
/// POST with a preview). An MCP tool or any agent-supplied input must use
/// [`Authority::Unverified`]. This is a caller contract, not identity proof:
/// the library cannot tell who typed a command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Authority {
    Unverified,
    LocalUser(LocalUserGrant),
}

/// Marker for [`Authority::LocalUser`]; see its contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalUserGrant(());

impl LocalUserGrant {
    /// Caller asserts a human at this machine performed this action directly.
    #[must_use]
    pub fn from_direct_local_action() -> Self {
        Self(())
    }
}

impl Authority {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unverified => "unverified",
            Self::LocalUser(_) => "local_user",
        }
    }
}

/// What a caller asks to append. Origin, seq, times and hash are the store's.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EventDraft {
    pub kind: String,
    pub space: String,
    pub subject: String,
    /// Explicit conflict key with `subject`; required for `link_*` and `correction`.
    pub relation: String,
    /// Empty only for a `correction` that retracts.
    pub object: String,
    pub basis: String,
    pub asserted_kind: String,
    pub client: String,
    pub agent: String,
    pub evidence: Vec<String>,
    pub occurred_at: Option<i64>,
    pub occurred_at_source: String,
    pub confidence_milli: u16,
    pub corrects: Option<String>,
    /// JSON object text; stored re-serialised with sorted keys.
    pub payload: String,
}

/// A stored event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Event {
    pub v: i64,
    pub origin_machine: String,
    pub origin_seq: i64,
    /// `local_user` or `unverified`, as vouched for on the origin machine.
    pub authority: String,
    pub recorded_at: i64,
    pub draft: EventDraft,
    pub content_hash: String,
}

impl Event {
    /// `machine:seq`, stable across replication and independent of time.
    #[must_use]
    pub fn event_id(&self) -> String {
        event_id(&self.origin_machine, self.origin_seq)
    }

    /// Claimed occurrence is more than 300 s after recording.
    #[must_use]
    pub fn clock_skew(&self) -> bool {
        self.draft
            .occurred_at
            .is_some_and(|at| at > self.recorded_at.saturating_add(300))
    }

    /// The canonical bytes: explicit field order, each value length-prefixed.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let d = &self.draft;
        let mut out = b"brain-event/1\n".to_vec();
        let mut field = |name: &str, value: &str| {
            out.extend_from_slice(format!("{name}={}:", value.len()).as_bytes());
            out.extend_from_slice(value.as_bytes());
            out.push(b'\n');
        };
        field("v", &self.v.to_string());
        field("origin_machine", &self.origin_machine);
        field("origin_seq", &self.origin_seq.to_string());
        field("authority", &self.authority);
        field("recorded_at", &self.recorded_at.to_string());
        field("kind", &d.kind);
        field("space", &d.space);
        field("subject", &d.subject);
        field("relation", &d.relation);
        field("object", &d.object);
        field("basis", &d.basis);
        field("asserted_kind", &d.asserted_kind);
        field("client", &d.client);
        field("agent", &d.agent);
        field("evidence_count", &d.evidence.len().to_string());
        for item in &d.evidence {
            field("evidence", item);
        }
        field(
            "occurred_at",
            &d.occurred_at
                .map_or_else(|| "none".to_owned(), |at| at.to_string()),
        );
        field("occurred_at_source", &d.occurred_at_source);
        field("confidence_milli", &d.confidence_milli.to_string());
        field("corrects", d.corrects.as_deref().unwrap_or("none"));
        field("payload", &d.payload);
        out
    }

    #[must_use]
    pub fn compute_hash(&self) -> String {
        blake3::hash(&self.canonical_bytes()).to_hex().to_string()
    }
}

#[must_use]
pub fn event_id(machine: &str, seq: i64) -> String {
    format!("{machine}:{seq:012}")
}

/// Splits and checks `machine:seq`.
pub fn parse_event_id(id: &str) -> Result<(String, i64), MapError> {
    let bad = || MapError::invalid(format!("not an event id: {}", printable(id)));
    let (machine, seq) = id.split_once(':').ok_or_else(bad)?;
    validate_machine(machine)?;
    if seq.len() != 12 || !seq.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let seq: i64 = seq.parse().map_err(|_| bad())?;
    if seq < 1 {
        return Err(bad());
    }
    Ok((machine.to_owned(), seq))
}

pub fn validate_machine(machine: &str) -> Result<(), MapError> {
    if machine.len() == 34
        && machine.starts_with("m-")
        && machine[2..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(MapError::invalid(format!(
            "not a Brain machine id: {}",
            printable(machine)
        )))
    }
}

/// Short, control-free echo of untrusted input for error messages.
fn printable(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(40).collect()
}

fn bounded(name: &str, value: &str, max: usize) -> Result<(), MapError> {
    if value.is_empty() {
        return Err(MapError::invalid(format!("{name} is empty")));
    }
    if value.len() > max {
        return Err(MapError::invalid(format!(
            "{name} is {} bytes, limit {max}",
            value.len()
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(MapError::invalid(format!(
            "{name} contains control characters"
        )));
    }
    Ok(())
}

fn one_of(name: &str, value: &str, allowed: &[&str]) -> Result<(), MapError> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(MapError::invalid(format!(
            "{name} {} is not one of {allowed:?}",
            printable(value)
        )))
    }
}

pub fn validate_space(space: &str) -> Result<(), MapError> {
    bounded("space", space, MAX_SPACE)?;
    if space
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        Ok(())
    } else {
        Err(MapError::invalid(
            "space allows only ASCII letters, digits, '.', '_' and '-'",
        ))
    }
}

/// `<prefix>:<scope>/<id>`, both parts non-empty.
pub fn validate_ref(name: &str, value: &str) -> Result<(), MapError> {
    bounded(name, value, MAX_REF)?;
    let ok = value.split_once(':').is_some_and(|(prefix, rest)| {
        REF_PREFIXES.contains(&prefix)
            && rest
                .split_once('/')
                .is_some_and(|(scope, id)| !scope.is_empty() && !id.is_empty())
    });
    if ok {
        Ok(())
    } else {
        Err(MapError::invalid(format!(
            "{name} does not follow <prefix>:<scope>/<id>"
        )))
    }
}

/// Validates and returns the payload re-serialised with sorted keys.
pub fn canonical_payload(payload: &str) -> Result<String, MapError> {
    let text = if payload.is_empty() { "{}" } else { payload };
    if text.len() > MAX_PAYLOAD {
        return Err(MapError::invalid(format!(
            "payload is {} bytes, limit {MAX_PAYLOAD}",
            text.len()
        )));
    }
    let Value::Object(map) = serde_json::from_str::<Value>(text)
        .map_err(|e| MapError::invalid(format!("payload: {e}")))?
    else {
        return Err(MapError::invalid("payload must be a JSON object"));
    };
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    let mut parts = Vec::with_capacity(keys.len());
    for key in keys {
        let Some(&(_, max)) = PAYLOAD_KEYS.iter().find(|(k, _)| k == key) else {
            return Err(MapError::invalid(format!(
                "payload key {} is not allowed",
                printable(key)
            )));
        };
        let value = &map[key];
        match value {
            Value::String(s) if max > 0 => bounded(key, s, max)?,
            Value::Number(n) if max == 0 && n.is_i64() => {}
            _ => {
                return Err(MapError::invalid(format!(
                    "payload key {key} has the wrong type"
                )));
            }
        }
        parts.push(format!("{}:{}", Value::String(key.clone()), value));
    }
    Ok(format!("{{{}}}", parts.join(",")))
}

/// Checks a draft without regard to store state. Returns the canonical payload.
pub fn validate_draft(d: &EventDraft) -> Result<String, MapError> {
    one_of("kind", &d.kind, &KINDS)?;
    validate_space(&d.space)?;
    validate_ref("subject", &d.subject)?;
    let assertion = d.kind.starts_with("link_") || d.kind == "correction";
    if assertion || !d.relation.is_empty() {
        bounded("relation", &d.relation, MAX_RELATION)?;
        if !d
            .relation
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'_')
        {
            return Err(MapError::invalid("relation allows only a-z and '_'"));
        }
        let (_, prefixes) = relation_policy(&d.relation)?;
        if let Some((prefix, _)) = d.object.split_once(':')
            && !prefixes.contains(&prefix)
        {
            return Err(MapError::invalid(format!(
                "relation {} takes objects {prefixes:?}",
                d.relation
            )));
        }
    }
    // Empty object: a retracting correction, or a non-link event without one.
    if !d.object.is_empty() || (assertion && d.kind != "correction") {
        validate_ref("object", &d.object)?;
    }
    let ceiling = BASES
        .iter()
        .find(|(b, _)| *b == d.basis)
        .map(|(_, c)| *c)
        .ok_or_else(|| MapError::invalid(format!("basis {} is unknown", printable(&d.basis))))?;
    if d.confidence_milli > ceiling {
        return Err(MapError::invalid(format!(
            "confidence {} exceeds the {} ceiling {ceiling}",
            d.confidence_milli, d.basis
        )));
    }
    one_of("asserted_kind", &d.asserted_kind, &ASSERTED_KINDS)?;
    bounded("client", &d.client, MAX_ACTOR)?;
    bounded("agent", &d.agent, MAX_ACTOR)?;
    if d.evidence.len() > MAX_EVIDENCE {
        return Err(MapError::invalid(format!(
            "{} evidence refs, limit {MAX_EVIDENCE}",
            d.evidence.len()
        )));
    }
    for item in &d.evidence {
        validate_ref("evidence", item)?;
    }
    one_of(
        "occurred_at_source",
        &d.occurred_at_source,
        &OCCURRED_SOURCES,
    )?;
    if (d.occurred_at.is_none()) != (d.occurred_at_source == "unknown") {
        return Err(MapError::invalid(
            "occurred_at must be absent exactly when its source is unknown",
        ));
    }
    match (&d.corrects, d.kind == "correction") {
        (Some(id), true) => {
            parse_event_id(id)?;
        }
        (None, false) => {}
        _ => {
            return Err(MapError::invalid(
                "corrects is required for, and only for, a correction",
            ));
        }
    }
    canonical_payload(&d.payload)
}

/// Authority rules that hold for every event, local or replayed.
pub fn check_authority(d: &EventDraft, authority: &str) -> Result<(), MapError> {
    let local = authority == "local_user";
    if !local && authority != "unverified" {
        return Err(MapError::invalid(
            "authority must be local_user or unverified",
        ));
    }
    let needs = d.basis == "user_confirmed"
        || d.asserted_kind == "user"
        || d.kind == "link_confirmed"
        || d.kind == "link_rejected";
    if needs && !local {
        return Err(MapError::new(
            MapErrorKind::Forbidden,
            "user confirmation, rejection or a user assertion needs local user authority; event text cannot grant it",
        ));
    }
    if (d.kind == "link_confirmed" || d.kind == "link_rejected") && d.basis != "user_confirmed" {
        return Err(MapError::invalid(
            "link_confirmed and link_rejected carry basis user_confirmed",
        ));
    }
    Ok(())
}
