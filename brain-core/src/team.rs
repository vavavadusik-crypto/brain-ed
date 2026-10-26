//! Closed, versioned team projection. All source text remains untrusted data.
use crate::sharing::{ShareError, ShareResult};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const FACT_SCHEMA: &str = "brain-team-fact/1";
pub const SLICE_SCHEMA: &str = "brain-team-slice/1";
pub const MAX_ENTITIES: usize = 24;
pub const CLASSES: &[&str] = &[
    "task",
    "plan",
    "error",
    "decision",
    "dead_end",
    "finding",
    "link",
    "commit",
    "participation",
    "session",
];
/// Content fields are fixed by schema; disabling a mandatory value rejects
/// that record. Empty optional annotations/references stay typed and bounded.
pub fn fields(class: &str) -> &'static [&'static str] {
    match class {
        "task" => &[
            "title_summary",
            "state",
            "created_at",
            "updated_at",
            "owner_agent",
            "owner_machine",
            "lease_expires_at",
            "dependency_refs",
        ],
        "plan" => &["what_summary", "why_summary", "status", "task_refs"],
        "error" => &[
            "what_summary",
            "why_summary",
            "status",
            "reason_summary",
            "evidence_refs",
            "transition_refs",
        ],
        "decision" => &[
            "what_summary",
            "why_summary",
            "status",
            "evidence_refs",
            "error_refs",
            "error_links",
        ],
        "dead_end" | "finding" => &["what_summary", "why_summary", "status"],
        "link" => &["from_ref", "to_ref", "relation", "evidence_refs"],
        "commit" => &[
            "oid",
            "parent_oids",
            "committed_at",
            "is_merge",
            "subject_summary",
            "task_refs",
        ],
        "participation" => &["client_kind", "started_at", "ended_at", "task_refs"],
        "session" => &[
            "client_kind",
            "first_observed_at",
            "last_observed_at",
            "summary",
            "fact_refs",
            "task_refs",
        ],
        _ => &[],
    }
}
pub fn project_fields(fact: &Fact, disabled: &BTreeSet<String>) -> ShareResult<Fact> {
    let class = fact.content.class();
    let mut value = serde_json::to_value(fact).map_err(|_| bad())?;
    for field in fields(class) {
        if !disabled.contains(&format!("{class}.{field}")) {
            continue;
        }
        let target = &mut value["content"][field];
        *target = match target {
            serde_json::Value::String(_) => serde_json::json!(""),
            serde_json::Value::Array(_) => serde_json::json!([]),
            _ => serde_json::Value::Null,
        };
    }
    let projected: Fact = serde_json::from_value(value).map_err(|_| bad())?;
    projected.validate()?;
    Ok(projected)
}
pub const DAY: i64 = 86400;

pub fn bad() -> ShareError {
    ShareError::InvalidData("team_validation_failed".into())
}
pub fn id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 96
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        && crate::redact::mask_secrets(s).0 == s
        && !s.contains("..")
        && !s.contains("brc1.")
}
pub fn is_team(space: &str) -> bool {
    space.starts_with("team.")
}
pub fn opaque(kind: &str, source: &str) -> String {
    format!("{kind}-{}", &blake3::hash(source.as_bytes()).to_hex()[..24])
}
pub fn digest<T: Serialize>(value: &T) -> ShareResult<String> {
    let value = serde_json::to_value(value).map_err(|_| bad())?;
    Ok(blake3::hash(value.to_string().as_bytes())
        .to_hex()
        .to_string())
}
fn hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// No paths are exported in phase 1. An annotation containing any slash, URI,
/// tilde or path traversal is refused, including relative and Windows paths.
/// This conservative refusal also covers non-existing paths and symlink names.
pub fn annotation(text: &str) -> ShareResult<String> {
    if text.len() > 1600 {
        return Err(bad());
    }
    let masked = crate::redact::mask_secrets(text).0;
    if masked.chars().any(|c| c.is_control() && !c.is_whitespace())
        || masked.contains(['/', '\\', '~'])
        || masked.contains("..")
        || masked.contains("://")
        || masked.contains("brc1.")
    {
        return Err(bad());
    }
    Ok(masked.split_whitespace().collect::<Vec<_>>().join(" "))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    pub schema: String,
    pub entity_id: String,
    pub revision: u64,
    pub origin: String,
    pub machine_id: String,
    pub space: String,
    pub project_id: String,
    pub session_ref: Option<String>,
    pub session_absence: Option<String>,
    pub agent_ref: Option<String>,
    pub model_ref: Option<String>,
    pub model_evidence: String,
    pub source_kind: String,
    pub source_ref: String,
    pub trust: String,
    pub valid_from: i64,
    pub valid_to: Option<i64>,
    pub recorded_at: i64,
    pub superseded_at: Option<i64>,
    pub coverage: String,
    pub content: Content,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "class", rename_all = "snake_case", deny_unknown_fields)]
pub enum Content {
    Task {
        title_summary: String,
        state: String,
        created_at: i64,
        updated_at: i64,
        owner_agent: Option<String>,
        owner_machine: Option<String>,
        lease_expires_at: Option<i64>,
        dependency_refs: Vec<String>,
    },
    Plan {
        what_summary: String,
        why_summary: String,
        status: String,
        task_refs: Vec<String>,
    },
    Error {
        what_summary: String,
        why_summary: String,
        status: String,
        reason_summary: String,
        evidence_refs: Vec<String>,
        transition_refs: Vec<String>,
    },
    Decision {
        what_summary: String,
        why_summary: String,
        status: String,
        evidence_refs: Vec<String>,
        error_refs: Vec<String>,
        error_links: String,
    },
    DeadEnd {
        what_summary: String,
        why_summary: String,
        status: String,
    },
    Finding {
        what_summary: String,
        why_summary: String,
        status: String,
    },
    Link {
        from_ref: String,
        to_ref: String,
        relation: String,
        evidence_refs: Vec<String>,
    },
    Commit {
        oid: String,
        parent_oids: Vec<String>,
        committed_at: i64,
        is_merge: bool,
        subject_summary: String,
        task_refs: Vec<String>,
    },
    Participation {
        client_kind: String,
        started_at: Option<i64>,
        ended_at: Option<i64>,
        task_refs: Vec<String>,
    },
    Session {
        client_kind: String,
        first_observed_at: i64,
        last_observed_at: i64,
        summary: String,
        fact_refs: Vec<String>,
        task_refs: Vec<String>,
    },
}
impl Content {
    pub fn class(&self) -> &'static str {
        match self {
            Self::Task { .. } => "task",
            Self::Plan { .. } => "plan",
            Self::Error { .. } => "error",
            Self::Decision { .. } => "decision",
            Self::DeadEnd { .. } => "dead_end",
            Self::Finding { .. } => "finding",
            Self::Link { .. } => "link",
            Self::Commit { .. } => "commit",
            Self::Participation { .. } => "participation",
            Self::Session { .. } => "session",
        }
    }
    pub fn text(&self) -> String {
        match self {
            Self::Task {
                title_summary,
                state,
                ..
            } => format!("{title_summary} [{state}]"),
            Self::Plan {
                what_summary,
                why_summary,
                status,
                ..
            }
            | Self::Error {
                what_summary,
                why_summary,
                status,
                ..
            }
            | Self::Decision {
                what_summary,
                why_summary,
                status,
                ..
            }
            | Self::DeadEnd {
                what_summary,
                why_summary,
                status,
            }
            | Self::Finding {
                what_summary,
                why_summary,
                status,
            } => format!("{what_summary} [{status}] {why_summary}"),
            Self::Commit {
                subject_summary, ..
            } => subject_summary.clone(),
            Self::Session { summary, .. } => summary.clone(),
            Self::Link { relation, .. } => relation.clone(),
            Self::Participation { client_kind, .. } => client_kind.clone(),
        }
    }
    pub fn refs(&self) -> Vec<&str> {
        match self {
            Self::Task {
                dependency_refs, ..
            } => dependency_refs.iter().map(String::as_str).collect(),
            Self::Plan { task_refs, .. }
            | Self::Commit { task_refs, .. }
            | Self::Participation { task_refs, .. } => {
                task_refs.iter().map(String::as_str).collect()
            }
            Self::Decision { error_refs, .. } => error_refs.iter().map(String::as_str).collect(),
            Self::Link {
                from_ref, to_ref, ..
            } => vec![from_ref, to_ref],
            Self::Session {
                fact_refs,
                task_refs,
                ..
            } => fact_refs
                .iter()
                .chain(task_refs)
                .map(String::as_str)
                .collect(),
            _ => vec![],
        }
    }
}
fn clean(text: &str, required: bool) -> bool {
    (!required || !text.is_empty()) && annotation(text).is_ok_and(|s| s == text)
}
fn refs(items: &[String]) -> bool {
    items.len() <= MAX_ENTITIES && items.iter().all(|s| id(s))
}
fn oid(s: &str) -> bool {
    [40, 64].contains(&s.len())
        && s.bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}
impl Fact {
    pub fn validate(&self) -> ShareResult<()> {
        let f = self;
        if f.schema != FACT_SCHEMA
            || f.revision == 0
            || ![
                &f.entity_id,
                &f.origin,
                &f.machine_id,
                &f.space,
                &f.project_id,
                &f.source_ref,
            ]
            .iter()
            .all(|s| id(s))
            || !is_team(&f.space)
            || f.valid_from < 0
            || f.recorded_at < 0
            || f.valid_to.is_some_and(|t| t <= f.valid_from)
            || f.superseded_at.is_some_and(|t| t <= f.recorded_at)
            || f.coverage != "bounded"
            || f.model_ref.is_some()
            || f.model_evidence != "unknown"
            || f.agent_ref.as_ref().is_some_and(|s| !id(s))
            || f.session_ref.as_ref().is_some_and(|s| !id(s))
            || (f.session_ref.is_some() != f.session_absence.is_none())
            || f.session_absence
                .as_ref()
                .is_some_and(|s| s != "not_applicable")
        {
            return Err(bad());
        }
        let source_ok = match f.content {
            Content::Task { .. } | Content::Participation { .. } => {
                f.source_kind == "work" && f.trust == "coordinator_state"
            }
            Content::Commit { .. } => f.source_kind == "git" && f.trust == "git_object",
            _ => {
                ["distill", "work", "derived"].contains(&f.source_kind.as_str())
                    && [
                        "untrusted_model_extraction",
                        "coordinator_state",
                        "derived_from_allowed_facts",
                    ]
                    .contains(&f.trust.as_str())
            }
        };
        let other = ["proposed", "active", "completed", "abandoned", "confirmed"];
        let valid = match &f.content {
            Content::Task {
                title_summary,
                state,
                created_at,
                updated_at,
                owner_agent,
                owner_machine,
                lease_expires_at,
                dependency_refs,
            } => {
                clean(title_summary, true)
                    && ["queued", "active", "paused", "completed", "failed"]
                        .contains(&state.as_str())
                    && *created_at >= 0
                    && updated_at >= created_at
                    && owner_agent.iter().chain(owner_machine).all(|s| id(s))
                    && lease_expires_at.is_none_or(|t| t >= 0)
                    && refs(dependency_refs)
            }
            Content::Plan {
                what_summary,
                why_summary,
                status,
                task_refs,
            } => {
                clean(what_summary, true)
                    && clean(why_summary, false)
                    && other.contains(&status.as_str())
                    && refs(task_refs)
            }
            Content::Error {
                what_summary,
                why_summary,
                status,
                reason_summary,
                evidence_refs,
                transition_refs,
            } => {
                clean(what_summary, true)
                    && clean(why_summary, false)
                    && clean(reason_summary, status == "wont_fix")
                    && [
                        "suspected",
                        "confirmed",
                        "fixed",
                        "reopened",
                        "wont_fix",
                        "not_a_bug",
                        "became_feature",
                    ]
                    .contains(&status.as_str())
                    && refs(evidence_refs)
                    && refs(transition_refs)
            }
            Content::Decision {
                what_summary,
                why_summary,
                status,
                evidence_refs,
                error_refs,
                error_links,
            } => {
                clean(what_summary, true)
                    && clean(why_summary, false)
                    && ["proposed", "adopted", "worked", "failed", "reverted"]
                        .contains(&status.as_str())
                    && refs(evidence_refs)
                    && refs(error_refs)
                    && ["known", "unknown"].contains(&error_links.as_str())
            }
            Content::DeadEnd {
                what_summary,
                why_summary,
                status,
            }
            | Content::Finding {
                what_summary,
                why_summary,
                status,
            } => {
                clean(what_summary, true)
                    && clean(why_summary, false)
                    && other.contains(&status.as_str())
            }
            Content::Link {
                from_ref,
                to_ref,
                relation,
                evidence_refs,
            } => {
                id(from_ref)
                    && id(to_ref)
                    && [
                        "caused_by",
                        "fixed_by",
                        "superseded_by",
                        "became",
                        "depends_on",
                    ]
                    .contains(&relation.as_str())
                    && refs(evidence_refs)
            }
            Content::Commit {
                oid: hash,
                parent_oids,
                committed_at,
                is_merge,
                subject_summary,
                task_refs,
            } => {
                oid(hash)
                    && parent_oids.len() <= 16
                    && parent_oids.iter().all(|s| oid(s))
                    && *committed_at >= 0
                    && *is_merge == (parent_oids.len() > 1)
                    && clean(subject_summary, true)
                    && refs(task_refs)
            }
            Content::Participation {
                client_kind,
                started_at,
                ended_at,
                task_refs,
            } => {
                client_kind == "unknown"
                    && started_at.is_none_or(|t| t >= 0)
                    && ended_at.is_none_or(|t| t >= started_at.unwrap_or(0))
                    && refs(task_refs)
            }
            Content::Session {
                client_kind,
                first_observed_at,
                last_observed_at,
                summary,
                fact_refs,
                task_refs,
            } => {
                client_kind == "unknown"
                    && *first_observed_at >= 0
                    && last_observed_at >= first_observed_at
                    && clean(summary, true)
                    && refs(fact_refs)
                    && refs(task_refs)
            }
        };
        if !source_ok || !valid || serde_json::to_vec(f).map_err(|_| bad())?.len() > 4096 {
            return Err(bad());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EntityDigest {
    pub entity_id: String,
    pub revision: u64,
    pub digest: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Watermarks {
    pub distill: Option<i64>,
    pub work: Option<i64>,
    pub git: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Slice {
    pub schema: String,
    pub manifest_revision: u64,
    pub origin: String,
    pub machine_id: String,
    pub space: String,
    pub project_id: String,
    pub snapshot_id: String,
    pub generated_at: i64,
    pub seq_first: u64,
    pub seq_last: u64,
    pub collected_from: i64,
    pub source_watermarks: Watermarks,
    pub coverage: String,
    pub rejected: u32,
    pub entity_digests: Vec<EntityDigest>,
    pub integrity: String,
    pub signature: Option<String>,
}
impl Slice {
    pub fn seal(&mut self) -> ShareResult<()> {
        self.snapshot_id.clear();
        self.snapshot_id = digest(self)?;
        Ok(())
    }
    pub fn validate(&self, facts: &[Fact]) -> ShareResult<()> {
        let mut sealed = self.clone();
        sealed.seal()?;
        if self.schema != SLICE_SCHEMA
            || self.manifest_revision == 0
            || !hex(&self.snapshot_id)
            || sealed.snapshot_id != self.snapshot_id
            || self.integrity != "tls+blake3-unsigned"
            || self.signature.is_some()
            || ![
                &self.origin,
                &self.machine_id,
                &self.space,
                &self.project_id,
            ]
            .iter()
            .all(|s| id(s))
            || !is_team(&self.space)
            || self.generated_at < self.collected_from
            || self.seq_first == 0
            || self.seq_last < self.seq_first
            || self.seq_last > crate::sharing::MAX_SEQ
            || self.seq_last - self.seq_first != facts.len() as u64
            || [
                self.source_watermarks.distill,
                self.source_watermarks.work,
                self.source_watermarks.git,
            ]
            .iter()
            .flatten()
            .any(|t| *t < 0)
            || self.collected_from < 0
            || !["bounded", "incomplete"].contains(&self.coverage.as_str())
            || (self.rejected > 0 && self.coverage != "incomplete")
            || facts.len() > MAX_ENTITIES
            || facts.len() != self.entity_digests.len()
            || serde_json::to_vec(self).map_err(|_| bad())?.len() > 4096
        {
            return Err(bad());
        }
        let mut seen = BTreeSet::new();
        for (f, d) in facts.iter().zip(&self.entity_digests) {
            f.validate()?;
            if !seen.insert(f.entity_id.as_str())
                || f.entity_id != d.entity_id
                || f.revision != d.revision
                || digest(f)? != d.digest
                || f.origin != self.origin
                || f.space != self.space
                || f.machine_id != self.machine_id
                || f.project_id != self.project_id
            {
                return Err(bad());
            }
        }
        if facts
            .iter()
            .any(|f| f.content.refs().iter().any(|r| !seen.contains(r)))
        {
            return Err(bad());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(crate) fn fact() -> Fact {
        Fact {
            schema: FACT_SCHEMA.into(),
            entity_id: "fact-one".into(),
            revision: 1,
            origin: "install-one".into(),
            machine_id: "7".into(),
            space: "team.proj".into(),
            project_id: "proj".into(),
            session_ref: None,
            session_absence: Some("not_applicable".into()),
            agent_ref: None,
            model_ref: None,
            model_evidence: "unknown".into(),
            source_kind: "distill".into(),
            source_ref: "evidence-one".into(),
            trust: "untrusted_model_extraction".into(),
            valid_from: 10,
            valid_to: None,
            recorded_at: 20,
            superseded_at: None,
            coverage: "bounded".into(),
            content: Content::Error {
                what_summary: "Build issue".into(),
                why_summary: "Observed failure".into(),
                status: "confirmed".into(),
                reason_summary: "Observed failure".into(),
                evidence_refs: vec![],
                transition_refs: vec![],
            },
        }
    }
    #[test]
    fn closed_schema_checks_all_text_and_envelope() {
        let f = fact();
        f.validate().unwrap();
        let mut v = serde_json::to_value(&f).unwrap();
        v["raw_session"] = "trap".into();
        assert!(serde_json::from_value::<Fact>(v).is_err());
        let mut v = serde_json::to_value(&f).unwrap();
        v["content"]["tool_output"] = "trap".into();
        assert!(serde_json::from_value::<Fact>(v).is_err());
        for value in [
            "/home/secret",
            "../outside",
            "C:\\profile",
            "https://user:pass@host",
            "brc1.fake.secret",
        ] {
            assert!(annotation(value).is_err());
            let mut f = f.clone();
            f.machine_id = value.into();
            assert!(f.validate().is_err());
        }
        let secret = ["sk-", "synthetic01234567890123456789"].concat();
        assert!(
            !annotation(&format!("Key {secret}"))
                .unwrap()
                .contains(&secret)
        );
        let mut f = f.clone();
        if let Content::Error {
            status,
            reason_summary,
            ..
        } = &mut f.content
        {
            *status = "wont_fix".into();
            reason_summary.clear();
        }
        assert!(f.validate().is_err());
    }
    #[test]
    fn field_allowlist_omits_optional_text_and_rejects_mandatory_fields() {
        let f = fact();
        let mut disabled = BTreeSet::new();
        disabled.insert("error.why_summary".into());
        let filtered = project_fields(&f, &disabled).unwrap();
        if let Content::Error { why_summary, .. } = filtered.content {
            assert!(why_summary.is_empty());
        } else {
            panic!("wrong class");
        }
        disabled.insert("error.what_summary".into());
        assert!(project_fields(&f, &disabled).is_err());
    }
    #[test]
    fn slice_digest_and_closure_are_required() {
        let facts = vec![fact()];
        let mut slice = Slice {
            schema: SLICE_SCHEMA.into(),
            manifest_revision: 1,
            origin: "install-one".into(),
            machine_id: "7".into(),
            space: "team.proj".into(),
            project_id: "proj".into(),
            snapshot_id: String::new(),
            generated_at: 30,
            seq_first: 1,
            seq_last: 2,
            collected_from: 29,
            source_watermarks: Watermarks {
                distill: Some(20),
                work: None,
                git: None,
            },
            coverage: "bounded".into(),
            rejected: 0,
            entity_digests: vec![EntityDigest {
                entity_id: facts[0].entity_id.clone(),
                revision: 1,
                digest: digest(&facts[0]).unwrap(),
            }],
            integrity: "tls+blake3-unsigned".into(),
            signature: None,
        };
        slice.seal().unwrap();
        slice.validate(&facts).unwrap();
        let mut tampered = facts.clone();
        tampered[0].valid_from += 1;
        assert!(slice.validate(&tampered).is_err());
        slice.signature = Some("fake".into());
        assert!(slice.validate(&facts).is_err());
    }
}

pub(crate) mod store;
pub use store::{Audit, Manifest, Recipient, Replica};
