//! M3a: a deterministic context packet under an exact byte budget.
//!
//! No LLM and no archive text: the packet carries map records, host-supplied
//! open tasks and memory snippets (each labelled with provenance and trust;
//! data, not instructions) and refs an agent can read through the trusted host.
//! Size is the byte length of the whole serialized UTF-8 JSON, envelope,
//! escaping and counters included. Bytes are a proxy for tokens: the tokenizer
//! is unknown and no exact or upper token bound is promised for any model.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::params;
use serde_json::{Value, json};

use super::events::{MAX_EVIDENCE, validate_ref, validate_space};
use super::ingest::validate_install;
use super::store::{COLUMNS, MapStore, read_event};
use super::{MapError, MapErrorKind};

pub const PACKET_FORMAT: &str = "brain-packet/1";
/// Version of [`PacketInput`]; another one is refused.
pub const PACKET_INPUT_VERSION: u32 = 1;
/// Fixed section order. `MEMORIES` is an additive `brain-packet/1`
/// extension: it is appended after `REFS` so the positions of the original
/// seven sections never move and a consumer that reads sections by name or by
/// index keeps working.
pub const SECTIONS: [&str; 8] = [
    "SCOPE",
    "OPEN",
    "OUTCOMES",
    "DECISIONS",
    "GOTCHAS",
    "CONFLICTS",
    "REFS",
    "MEMORIES",
];
const SECTION_COUNT: usize = SECTIONS.len();
const MEMORIES: usize = 7;
/// Greedy fill priority: memories compete for bytes before conflicts and refs
/// (refs already name every included memory), the display order stays fixed.
const FILL_ORDER: [usize; SECTION_COUNT - 1] = [1, 2, 3, 4, MEMORIES, 5, 6];
/// Items per section considered before budgeting; the rest are counted as omitted.
const MAX_CANDIDATES: usize = 200;
const MAX_INPUT_ITEMS: usize = 1_000;
const TITLE_BYTES: usize = 200;
const MEMORY_BYTES: usize = 600;
const MAX_TOKEN: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tier {
    /// Default 2 048 bytes, at most 4 096.
    Weak,
    /// Default 12 288 bytes, at most 32 768.
    Strong,
}

impl Tier {
    #[must_use]
    pub fn default_budget(self) -> usize {
        match self {
            Self::Weak => 2_048,
            Self::Strong => 12_288,
        }
    }
    #[must_use]
    pub fn max_budget(self) -> usize {
        match self {
            Self::Weak => 4_096,
            Self::Strong => 32_768,
        }
    }
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Weak => "weak",
            Self::Strong => "strong",
        }
    }
}

/// The memory kind as its own metadata asserts it (agent classification,
/// never authority). Decisions and gotchas keep their own sections; every
/// other kind goes to `MEMORIES` under its own name, never relabelled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryKind {
    Decision,
    Gotcha,
    Fact,
    Task,
    Error,
    Note,
    /// A shared link between refs, as curated by the publishing origin.
    Link,
}

impl MemoryKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Gotcha => "gotcha",
            Self::Fact => "fact",
            Self::Task => "task",
            Self::Error => "error",
            Self::Note => "note",
            Self::Link => "link",
        }
    }

    fn section(self) -> usize {
        match self {
            Self::Decision => 3,
            Self::Gotcha => 4,
            Self::Fact | Self::Task | Self::Error | Self::Note | Self::Link => MEMORIES,
        }
    }
}

/// A ready task as the trusted host read it from the coordinator board.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenTask {
    pub space: String,
    pub id: i64,
    pub project: String,
    pub state: String,
    /// Agent-supplied text; clipped, labelled untrusted.
    pub title: String,
}

/// Which project a memory record is about, as its own metadata says.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemoryScope {
    Project(String),
    /// Declared as shared by the whole space.
    Space,
    /// No project recorded: never shown in a project-scoped packet.
    Unknown,
}

/// A task's project as the coordinator recorded it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskProject {
    pub space: String,
    pub task: i64,
    pub project: String,
}

/// A bounded graph memory record as the trusted host read it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemorySnippet {
    pub space: String,
    /// The memory id; the ref is `memory:<space>/<id>`.
    pub id: String,
    pub kind: MemoryKind,
    pub scope: MemoryScope,
    pub text: String,
    /// Who wrote it (agent/client), shown as is.
    pub provenance: String,
    /// `None`: the record has no evidence field. `Some(vec![])`: it has one, empty.
    pub evidence: Option<Vec<String>>,
    /// `Some` only for a fact received through opt-in sharing; `None` for
    /// this installation's own graph memory.
    pub shared: Option<SharedProvenance>,
}

/// Where a shared fact came from, kept as two distinct kinds of record: what
/// the publishing side *claims* (`origin`, `claimed_agent` — inert data) and
/// what this host itself recorded when it applied the pull
/// (`received_from_peer`). Nothing here grants or certifies anything.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SharedProvenance {
    pub origin: String,
    pub fact_id: String,
    /// The origin's `put` event sequence this fact references.
    pub origin_seq: u64,
    pub claimed_agent: Option<String>,
    pub received_from_peer: String,
}

/// Everything a packet is built from besides the map itself, all for one space.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PacketInput {
    pub version: u32,
    pub install: String,
    pub space: String,
    pub project: Option<String>,
    pub task: Option<i64>,
    pub tier: Tier,
    pub budget_bytes: Option<usize>,
    pub open_tasks: Vec<OpenTask>,
    /// Coordinator task→project records used to scope OUTCOMES and CONFLICTS.
    pub task_projects: Vec<TaskProject>,
    pub memories: Vec<MemorySnippet>,
}

/// A built packet. `json.len()` is the exact size the budget was checked against.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Packet {
    pub json: String,
    pub truncated: bool,
    /// Per section: items left out for budget or candidate caps.
    pub omitted: BTreeMap<&'static str, usize>,
    /// Per section: input records of another space, dropped, never merged.
    pub foreign_space: BTreeMap<&'static str, usize>,
    /// Per section: same-space records outside the requested project (another,
    /// unknown or ambiguous project), dropped.
    pub out_of_scope: BTreeMap<&'static str, usize>,
}

fn token(name: &str, value: &str) -> Result<(), MapError> {
    if value.is_empty() || value.len() > MAX_TOKEN || value.chars().any(char::is_control) {
        return Err(MapError::invalid(format!(
            "{name} is empty, over {MAX_TOKEN} bytes or has control characters"
        )));
    }
    Ok(())
}

/// Bytes a shared fact id may have; the share store enforces the same bound.
const MAX_SHARED_FACT_ID: usize = 128;

/// The shared provenance object of one memory item. Origin and peer are ids;
/// the claimed agent is clipped data, labelled as a claim by its key name.
fn shared_json(shared: &SharedProvenance) -> Result<Value, MapError> {
    token("shared origin", &shared.origin)?;
    token("shared peer", &shared.received_from_peer)?;
    if shared.fact_id.is_empty()
        || shared.fact_id.len() > MAX_SHARED_FACT_ID
        || shared.fact_id.chars().any(char::is_control)
    {
        return Err(MapError::invalid(format!(
            "shared fact id is empty, over {MAX_SHARED_FACT_ID} bytes or has control characters"
        )));
    }
    let claimed_agent = shared
        .claimed_agent
        .as_deref()
        .map(|agent| clip(&agent.replace(char::is_control, " "), MAX_TOKEN).0);
    Ok(json!({
        "origin": shared.origin,
        "fact_id": shared.fact_id,
        "origin_seq": shared.origin_seq,
        "claimed_agent": claimed_agent,
        "received_from_peer": shared.received_from_peer,
    }))
}

/// Cuts at a char boundary so the result plus "…" fits `max` bytes.
fn clip(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_owned(), false);
    }
    let mut end = max - '…'.len_utf8();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}…", &text[..end]), true)
}

/// Total order for duplicates of one id: the shorter, then lexically smaller rendering.
fn smaller(a: &Value, b: &Value) -> bool {
    let (a, b) = (a.to_string(), b.to_string());
    (a.len(), a) < (b.len(), b)
}

fn counts(values: &[usize; SECTION_COUNT]) -> String {
    let map: BTreeMap<&str, usize> = SECTIONS
        .iter()
        .copied()
        .zip(values.iter().copied())
        .collect();
    serde_json::to_string(&map).unwrap_or_default()
}

struct Render<'a> {
    tier: Tier,
    budget: usize,
    map: &'static str,
    foreign: [usize; SECTION_COUNT],
    out_of_scope: [usize; SECTION_COUNT],
    omitted: [usize; SECTION_COUNT],
    included: [Vec<&'a str>; SECTION_COUNT],
}

impl Render<'_> {
    fn assemble(&self, bytes: usize) -> String {
        let sections: Vec<String> = SECTIONS
            .iter()
            .zip(&self.included)
            .map(|(name, items)| format!("{{\"name\":\"{name}\",\"items\":[{}]}}", items.join(",")))
            .collect();
        format!(
            "{{\"format\":\"{PACKET_FORMAT}\",\"tier\":\"{}\",\"budget_bytes\":{},\"bytes\":{bytes},\
             \"size\":\"utf8_json_bytes\",\"tokenizer\":\"unknown\",\"truncated\":{},\"trust\":\"mixed_labeled\",\
             \"map\":\"{}\",\"omitted\":{},\"foreign_space\":{},\"out_of_scope\":{},\"sections\":[{}]}}",
            self.tier.as_str(),
            self.budget,
            self.omitted.iter().any(|&n| n > 0),
            self.map,
            counts(&self.omitted),
            counts(&self.foreign),
            counts(&self.out_of_scope),
            sections.join(","),
        )
    }

    /// `bytes` names the packet's own length: iterate to the fixed point.
    fn render(&self) -> String {
        let mut bytes = 0;
        loop {
            let out = self.assemble(bytes);
            if out.len() == bytes {
                return out;
            }
            bytes = out.len();
        }
    }
}

impl MapStore {
    /// Builds the context packet for one space (and optionally one project
    /// and/or task) from this map plus the host's inputs. Sections in
    /// [`SECTIONS`] order; items sorted and deduplicated; greedy fill in that
    /// order, each item kept only if the whole packet still fits. SCOPE is
    /// never dropped. Works on a read-only store.
    ///
    /// # Errors
    ///
    /// `incompatible` for another input version; `invalid_input` for a budget
    /// above the tier's maximum or below the minimal packet with its SCOPE, or
    /// malformed input.
    pub fn context_packet(&self, input: &PacketInput) -> Result<Packet, MapError> {
        build(Some(self), input)
    }
}

/// The same packet when no map is configured: coordinator and memory inputs
/// only, labelled `map: not_configured` and `source: coordinator_only`.
///
/// # Errors
///
/// As [`MapStore::context_packet`].
pub fn context_packet_unmapped(input: &PacketInput) -> Result<Packet, MapError> {
    build(None, input)
}

fn task_number(subject: &str, space: &str) -> Option<i64> {
    subject
        .strip_prefix("task:")?
        .strip_prefix(space)?
        .strip_prefix('/')?
        .parse()
        .ok()
}

#[allow(clippy::too_many_lines)]
fn build(map: Option<&MapStore>, input: &PacketInput) -> Result<Packet, MapError> {
    if input.version != PACKET_INPUT_VERSION {
        return Err(MapError::new(
            MapErrorKind::Incompatible,
            format!(
                "packet input version {} is not {PACKET_INPUT_VERSION}",
                input.version
            ),
        ));
    }
    validate_install(&input.install)?;
    validate_space(&input.space)?;
    let project_ok = |p: &str| {
        if crate::coordination::is_valid_project(p) {
            Ok(())
        } else {
            Err(MapError::invalid(
                "project must follow the coordinator's project grammar",
            ))
        }
    };
    if let Some(project) = &input.project {
        project_ok(project)?;
    }
    if input.task.is_some_and(|t| t < 1) {
        return Err(MapError::invalid("task must be a positive id"));
    }
    let budget = input
        .budget_bytes
        .unwrap_or_else(|| input.tier.default_budget());
    if budget > input.tier.max_budget() {
        return Err(MapError::invalid(format!(
            "budget {budget} exceeds the {} maximum {}",
            input.tier.as_str(),
            input.tier.max_budget()
        )));
    }
    if input.open_tasks.len() > MAX_INPUT_ITEMS
        || input.memories.len() > MAX_INPUT_ITEMS
        || input.task_projects.len() > MAX_INPUT_ITEMS
    {
        return Err(MapError::invalid(format!(
            "at most {MAX_INPUT_ITEMS} open tasks, task projects and memories"
        )));
    }
    let space = input.space.as_str();
    let project = input.project.as_deref();
    let task_ref = input.task.map(|t| format!("task:{space}/{t}"));
    let project_ref = project.map(|p| format!("project:{space}/{p}"));
    let mut foreign = [0; SECTION_COUNT];
    let mut out_of_scope = [0; SECTION_COUNT];
    let mut extra = [0; SECTION_COUNT];
    let mut items: [Vec<Value>; SECTION_COUNT] = Default::default();
    let mut refs: BTreeSet<String> = BTreeSet::new();

    // Task → projects, from coordinator records only. Several projects = ambiguous.
    let mut task_projects: BTreeMap<i64, BTreeSet<&str>> = BTreeMap::new();
    for tp in &input.task_projects {
        if tp.space != space {
            continue;
        }
        project_ok(&tp.project)?;
        task_projects
            .entry(tp.task)
            .or_default()
            .insert(tp.project.as_str());
    }
    for t in input.open_tasks.iter().filter(|t| t.space == space) {
        if crate::coordination::is_valid_project(&t.project) {
            task_projects
                .entry(t.id)
                .or_default()
                .insert(t.project.as_str());
        }
    }
    let ambiguous: Vec<i64> = task_projects
        .iter()
        .filter(|(_, p)| p.len() > 1)
        .map(|(t, _)| *t)
        .collect();
    // Some(true) only when the coordinator recorded exactly this project for the task.
    let in_project = |task: i64| -> bool {
        project.is_some_and(|p| {
            task_projects
                .get(&task)
                .is_some_and(|set| set.len() == 1 && set.contains(p))
        })
    };

    let kind = match (project, input.task) {
        (_, Some(_)) => "task",
        (Some(_), None) => "project",
        (None, None) => "space",
    };
    let mut scope = json!({
        "kind": kind, "install": input.install, "space": space,
        "map_ref": format!("map:{}/{space}", input.install),
        "map": if map.is_some() { "ready" } else { "not_configured" },
        "source": if map.is_some() { "map+coordinator+memory" } else { "coordinator_only+memory" },
        "trust": "map_scope",
    });
    if let Some(p) = &project_ref {
        scope["project"] = json!(p);
    }
    if let Some(t) = &task_ref {
        scope["task"] = json!(t);
        if let Some(set) = input.task.and_then(|t| task_projects.get(&t)) {
            let state = match (set.len(), project) {
                (1, Some(p)) if set.contains(p) => "in_project",
                (1, Some(_)) => "other_project",
                (1, None) => "known",
                _ => "ambiguous",
            };
            scope["task_project"] =
                json!({"state": state, "basis": "coordinator_record", "projects": set});
        } else {
            scope["task_project"] = json!({"state": "unknown"});
        }
    }
    if !ambiguous.is_empty() {
        scope["ambiguous_tasks"] = json!(ambiguous.iter().take(20).collect::<Vec<_>>());
    }

    items[0].push(scope);

    // OPEN
    let mut open: BTreeMap<i64, Value> = BTreeMap::new();
    for t in &input.open_tasks {
        if t.space != space {
            foreign[1] += 1;
            continue;
        }
        if t.id < 1 {
            return Err(MapError::invalid("open task id must be positive"));
        }
        project_ok(&t.project)?;
        token("open task state", &t.state)?;
        if project.is_some() && !in_project(t.id) {
            out_of_scope[1] += 1;
            continue;
        }
        let (title, clipped) = clip(&t.title, TITLE_BYTES);
        let mut v = json!({"ref": format!("task:{space}/{}", t.id), "project": t.project, "state": t.state, "title": title, "trust": "agent_supplied", "project_basis": "coordinator_record"});
        if clipped {
            v["clipped"] = json!(true);
        }
        // Duplicates of one id: the smallest rendering wins, independent of input order.
        let keep = open.get(&t.id).is_none_or(|old| smaller(&v, old));
        if keep {
            open.insert(t.id, v);
        }
    }
    for v in open.into_values() {
        refs.insert(v["ref"].as_str().unwrap_or_default().to_owned());
        items[1].push(v);
    }

    let cap = i64::try_from(MAX_CANDIDATES).unwrap_or(i64::MAX);
    if let Some(m) = map {
        // OUTCOMES: newest first; a label says how much to trust it.
        let outcome_filter = "space = ?1 AND kind IN ('outcome_claimed', 'outcome_observed') AND (?2 IS NULL OR subject = ?2)";
        let total: i64 = m.reader().query_row(
            &format!("SELECT count(*) FROM events WHERE {outcome_filter}"),
            params![space, task_ref],
            |r| r.get(0),
        )?;
        let mut stmt = m.reader().prepare(&format!(
            "SELECT {COLUMNS} FROM events WHERE {outcome_filter} ORDER BY local_seq DESC LIMIT ?3"
        ))?;
        let mut fetched = 0;
        for e in stmt.query_map(params![space, task_ref, cap], read_event)? {
            let e = e?;
            fetched += 1;
            let task = task_number(&e.draft.subject, space);
            if project.is_some() && input.task.is_none() && !task.is_some_and(in_project) {
                out_of_scope[2] += 1;
                continue;
            }
            let word = e.draft.object.split_once('/').map_or("", |(_, w)| w);
            // Only a local user's observation on this machine is "observed"; an
            // agent or remote record saying so stays a claim.
            let label = if e.draft.kind == "outcome_observed"
                && e.authority == "local_user"
                && e.origin_machine == m.machine()
            {
                "observed"
            } else if word.starts_with("unknown") {
                "unknown"
            } else {
                "claimed"
            };
            let mut v = json!({
                "task": e.draft.subject, "label": label, "outcome": word, "basis": e.draft.basis,
                "by": e.draft.agent, "client": e.draft.client, "event": e.event_id(), "evidence": e.draft.evidence,
            });
            if let Some(at) = e.draft.occurred_at {
                v["source_at"] = json!(at);
            }
            if project.is_some() && input.task.is_none() {
                v["project_basis"] = json!("coordinator_record");
            }
            refs.insert(e.draft.subject.clone());
            refs.extend(e.draft.evidence.iter().cloned());
            items[2].push(v);
        }
        extra[2] = usize::try_from(total).unwrap_or(0).saturating_sub(fetched);

        // CONFLICTS: only rows the projection marks as conflict, within the scope.
        let total: i64 = m.reader().query_row(
            "SELECT count(*) FROM links WHERE space = ?1 AND status = 'conflict'",
            [space],
            |r| r.get(0),
        )?;
        let mut stmt = m.reader().prepare(
            "SELECT subject, relation, object, actors, current_events FROM links WHERE space = ?1 AND status = 'conflict' \
             ORDER BY subject, relation, object LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![space, cap], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut fetched = 0;
        for row in rows {
            let (subject, relation, object, actors, events) = row?;
            fetched += 1;
            let wanted = match (&task_ref, &project_ref) {
                (Some(t), _) => &subject == t,
                (None, Some(p)) => {
                    &subject == p || task_number(&subject, space).is_some_and(in_project)
                }
                (None, None) => true,
            };
            if !wanted {
                out_of_scope[5] += 1;
                continue;
            }
            let actors: Vec<String> = serde_json::from_str(&actors).unwrap_or_default();
            let events: Vec<String> = serde_json::from_str(&events).unwrap_or_default();
            refs.insert(subject.clone());
            refs.insert(object.clone());
            items[5].push(json!({"subject": subject, "relation": relation, "object": object, "actors": actors.len(), "events": events}));
        }
        extra[5] = usize::try_from(total).unwrap_or(0).saturating_sub(fetched);

        // Live links of the scoped task/project: their objects and evidence become refs.
        if let Some(subject) = task_ref.as_ref().or(project_ref.as_ref()) {
            let mut stmt = m.reader().prepare(
                "SELECT object, current_events FROM links WHERE space = ?1 AND subject = ?2 \
                 AND status IN ('proposed', 'confirmed', 'conflict') ORDER BY relation, object LIMIT ?3",
            )?;
            let rows: Vec<(String, String)> = stmt
                .query_map(params![space, subject, cap], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            for (object, events) in rows {
                refs.insert(object);
                for id in serde_json::from_str::<Vec<String>>(&events).unwrap_or_default() {
                    let evidence: Option<String> = m
                        .reader()
                        .query_row(
                            "SELECT evidence FROM events WHERE event_id = ?1",
                            [&id],
                            |r| r.get(0),
                        )
                        .ok();
                    refs.extend(
                        serde_json::from_str::<Vec<String>>(&evidence.unwrap_or_default())
                            .unwrap_or_default(),
                    );
                }
            }
            refs.insert(subject.clone());
        }
    }

    // DECISIONS, GOTCHAS, MEMORIES
    let mut memories: BTreeMap<usize, BTreeMap<String, Value>> = BTreeMap::new();
    for mem in &input.memories {
        let section = mem.kind.section();
        if mem.space != space {
            foreign[section] += 1;
            continue;
        }
        let id_ok = !mem.id.is_empty()
            && mem.id.len() <= MAX_TOKEN
            && mem.id != "."
            && mem.id != ".."
            && mem
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        if !id_ok {
            return Err(MapError::invalid(
                "memory id must be 1..=64 of [A-Za-z0-9._-]",
            ));
        }
        let scope_label = match (&mem.scope, project) {
            (MemoryScope::Project(p), _) if !crate::coordination::is_valid_project(p) => {
                return Err(MapError::invalid(
                    "memory project must follow the coordinator's project grammar",
                ));
            }
            (MemoryScope::Project(p), Some(wanted)) if p == wanted => {
                format!("project:{space}/{p}")
            }
            (MemoryScope::Space, _) => "space_shared".to_owned(),
            (MemoryScope::Project(p), None) => format!("project:{space}/{p}"),
            (MemoryScope::Unknown, None) => "unknown".to_owned(),
            (_, Some(_)) => {
                out_of_scope[section] += 1;
                continue;
            }
        };
        let reference = format!("memory:{space}/{}", mem.id);
        validate_ref("memory id", &reference)?;
        token("memory provenance", &mem.provenance)?;
        let (text, clipped) = clip(&mem.text, MEMORY_BYTES);
        let mut v = json!({"ref": reference, "text": text, "provenance": mem.provenance, "scope": scope_label, "trust": "memory_record_untrusted"});
        if let Some(shared) = &mem.shared {
            v["trust"] = json!("shared_claim_untrusted");
            v["shared"] = shared_json(shared)?;
        }
        if section == MEMORIES {
            v["kind"] = json!(mem.kind.as_str());
        }
        if clipped {
            v["clipped"] = json!(true);
        }
        if let Some(evidence) = &mem.evidence {
            if evidence.len() > MAX_EVIDENCE {
                return Err(MapError::invalid(format!(
                    "memory evidence over {MAX_EVIDENCE} refs"
                )));
            }
            for r in evidence {
                validate_ref("memory evidence", r)?;
            }
            let mut sorted = evidence.clone();
            sorted.sort();
            sorted.dedup();
            v["evidence"] = json!(sorted);
        }
        let slot = memories.entry(section).or_default();
        let keep = slot.get(&reference).is_none_or(|old| smaller(&v, old));
        if keep {
            slot.insert(reference, v);
        }
    }
    for (section, slot) in memories {
        for v in slot.into_values() {
            refs.insert(v["ref"].as_str().unwrap_or_default().to_owned());
            if let Some(Value::Array(ev)) = v.get("evidence") {
                refs.extend(ev.iter().filter_map(Value::as_str).map(str::to_owned));
            }
            items[section].push(v);
        }
    }

    // REFS: sorted, stable, ready for paging through the host.
    items[6] = refs.into_iter().map(Value::String).collect();

    let rendered: Vec<Vec<String>> = items
        .iter()
        .map(|section| {
            section
                .iter()
                .take(MAX_CANDIDATES)
                .map(Value::to_string)
                .collect()
        })
        .collect();
    let mut omitted = [0; SECTION_COUNT];
    for s in 1..SECTION_COUNT {
        omitted[s] = items[s].len() + extra[s];
    }
    let mut r = Render {
        tier: input.tier,
        budget,
        map: if map.is_some() {
            "ready"
        } else {
            "not_configured"
        },
        foreign,
        out_of_scope,
        omitted,
        included: Default::default(),
    };
    // SCOPE is part of the minimal packet: a packet never claims a scope it does not show.
    r.included[0].push(&rendered[0][0]);
    let minimal = r.render().len();
    if minimal > budget {
        return Err(MapError::invalid(format!(
            "budget {budget} is below the {minimal}-byte minimal packet with its SCOPE"
        )));
    }
    for s in FILL_ORDER {
        for item in &rendered[s] {
            r.included[s].push(item);
            r.omitted[s] -= 1;
            if r.render().len() > budget {
                r.included[s].pop();
                r.omitted[s] += 1;
            }
        }
    }
    let json = r.render();
    debug_assert!(json.len() <= budget);
    let to_map =
        |v: [usize; SECTION_COUNT]| SECTIONS.iter().copied().zip(v).collect::<BTreeMap<_, _>>();
    Ok(Packet {
        truncated: r.omitted.iter().any(|&n| n > 0),
        omitted: to_map(r.omitted),
        foreign_space: to_map(foreign),
        out_of_scope: to_map(out_of_scope),
        json,
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use rusqlite::Connection;

    use super::*;
    use crate::map::events::{Authority, EventDraft, LocalUserGrant};
    use crate::map::store::{MapStatus, map_status};

    const A: &str = "m-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "m-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn dir(_tag: &str) -> Dir {
        let d = crate::test_support::temp_path("brain-m3a");
        let _ = std::fs::remove_dir_all(&d);
        Dir(d)
    }

    fn ev(kind: &str, subject: &str, relation: &str, object: &str, agent: &str) -> EventDraft {
        EventDraft {
            kind: kind.into(),
            space: "sp".into(),
            subject: subject.into(),
            relation: relation.into(),
            object: object.into(),
            basis: "agent_claim".into(),
            asserted_kind: "agent".into(),
            client: agent.into(),
            agent: agent.into(),
            occurred_at_source: "unknown".into(),
            confidence_milli: 700,
            ..EventDraft::default()
        }
    }

    fn user() -> Authority {
        Authority::LocalUser(LocalUserGrant::from_direct_local_action())
    }

    /// Outcomes claimed, observed (local user), observed-by-agent, unknown, a
    /// remote observation, a single-valued conflict, multi-valued sessions,
    /// and one other-space outcome.
    fn populated(d: &Dir) -> MapStore {
        let mut m = MapStore::open(&d.0.join("a").join("map.sqlite"), A).unwrap();
        let coord = |object: &str| EventDraft {
            basis: "coordinator_record".into(),
            asserted_kind: "coordinator".into(),
            client: "coordinator".into(),
            confidence_milli: 900,
            occurred_at: Some(100),
            occurred_at_source: "coordinator_clock".into(),
            evidence: vec!["coord-event:sp/3".into()],
            ..ev("outcome_claimed", "task:sp/1", "outcome", object, "codex")
        };
        m.append(coord("outcome:sp/claimed_completed"), Authority::Unverified)
            .unwrap();
        m.append(
            EventDraft {
                subject: "task:sp/2".into(),
                ..coord("outcome:sp/unknown_lease_expired")
            },
            Authority::Unverified,
        )
        .unwrap();
        m.append(
            ev(
                "outcome_observed",
                "task:sp/1",
                "outcome",
                "outcome:sp/observed_ok",
                "codex",
            ),
            Authority::Unverified,
        )
        .unwrap();
        let observed = EventDraft {
            basis: "user_confirmed".into(),
            asserted_kind: "user".into(),
            confidence_milli: 1000,
            evidence: Vec::new(),
            ..ev(
                "outcome_observed",
                "task:sp/1",
                "outcome",
                "outcome:sp/observed_ok",
                "cli",
            )
        };
        m.append(observed.clone(), user()).unwrap();
        let mut b = MapStore::open(&d.0.join("b").join("map.sqlite"), B).unwrap();
        m.replay(&b.append(observed, user()).unwrap()).unwrap();
        m.append(
            ev(
                "link_proposed",
                "task:sp/1",
                "current_status",
                "status:sp/open",
                "codex",
            ),
            Authority::Unverified,
        )
        .unwrap();
        m.append(
            ev(
                "link_proposed",
                "task:sp/1",
                "current_status",
                "status:sp/done",
                "claude",
            ),
            Authority::Unverified,
        )
        .unwrap();
        m.append(
            ev(
                "link_proposed",
                "task:sp/1",
                "has_session",
                "archive-session:codex/s1",
                "codex",
            ),
            Authority::Unverified,
        )
        .unwrap();
        m.append(
            EventDraft {
                evidence: vec!["archive:inst-a/9".into()],
                ..ev(
                    "link_proposed",
                    "task:sp/1",
                    "has_session",
                    "archive-session:claude/s2",
                    "claude",
                )
            },
            Authority::Unverified,
        )
        .unwrap();
        m.append(
            EventDraft {
                space: "other".into(),
                subject: "task:other/1".into(),
                object: "outcome:other/claimed_failed".into(),
                ..coord("")
            },
            Authority::Unverified,
        )
        .unwrap();
        m
    }

    fn input() -> PacketInput {
        PacketInput {
            version: PACKET_INPUT_VERSION,
            install: "inst-a".into(),
            space: "sp".into(),
            project: None,
            task: None,
            tier: Tier::Strong,
            budget_bytes: None,
            task_projects: Vec::new(),
            open_tasks: vec![
                OpenTask {
                    space: "sp".into(),
                    id: 4,
                    project: "p".into(),
                    state: "queued".into(),
                    title: "four".into(),
                },
                OpenTask {
                    space: "other".into(),
                    id: 5,
                    project: "p".into(),
                    state: "queued".into(),
                    title: "elsewhere".into(),
                },
            ],
            memories: vec![
                MemorySnippet {
                    space: "sp".into(),
                    id: "d1".into(),
                    kind: MemoryKind::Decision,
                    scope: MemoryScope::Space,
                    text: "use WAL".into(),
                    provenance: "codex".into(),
                    evidence: None,
                    shared: None,
                },
                MemorySnippet {
                    space: "sp".into(),
                    id: "g1".into(),
                    kind: MemoryKind::Gotcha,
                    scope: MemoryScope::Space,
                    text: "fts5 needs bundled".into(),
                    provenance: "claude".into(),
                    evidence: Some(Vec::new()),
                    shared: None,
                },
                MemorySnippet {
                    space: "other".into(),
                    id: "d9".into(),
                    kind: MemoryKind::Decision,
                    scope: MemoryScope::Space,
                    text: "foreign-canary-x".into(),
                    provenance: "zed".into(),
                    evidence: None,
                    shared: None,
                },
            ],
        }
    }

    fn parse(p: &Packet) -> Value {
        let v: Value = serde_json::from_str(&p.json).expect("valid JSON");
        assert_eq!(
            v["bytes"].as_u64().unwrap() as usize,
            p.json.len(),
            "bytes names the whole packet"
        );
        assert!(p.json.len() <= v["budget_bytes"].as_u64().unwrap() as usize);
        v
    }

    fn section<'a>(v: &'a Value, name: &str) -> &'a Vec<Value> {
        v["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .unwrap()["items"]
            .as_array()
            .unwrap()
    }

    #[test]
    fn sections_label_outcomes_keep_spaces_apart_and_show_only_real_conflicts() {
        let d = dir("labels");
        let m = populated(&d);
        let p = m.context_packet(&input()).unwrap();
        let v = parse(&p);
        let names: Vec<&str> = v["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, SECTIONS.to_vec());
        let labels: Vec<(String, String, String)> = section(&v, "OUTCOMES")
            .iter()
            .map(|o| {
                (
                    o["task"].as_str().unwrap().into(),
                    o["label"].as_str().unwrap().into(),
                    o["by"].as_str().unwrap().into(),
                )
            })
            .collect();
        let t = |a: &str, b: &str, c: &str| (a.to_owned(), b.to_owned(), c.to_owned());
        // Newest first; the agent's and the remote machine's "observed" stay claims.
        assert_eq!(
            labels,
            vec![
                t("task:sp/1", "claimed", "cli"),
                t("task:sp/1", "observed", "cli"),
                t("task:sp/1", "claimed", "codex"),
                t("task:sp/2", "unknown", "codex"),
                t("task:sp/1", "claimed", "codex"),
            ]
        );
        let first = &section(&v, "OUTCOMES")[4];
        assert_eq!(
            (first["outcome"].as_str(), first["source_at"].as_i64()),
            (Some("claimed_completed"), Some(100))
        );
        assert_eq!(
            section(&v, "OUTCOMES")[1]["evidence"],
            json!([]),
            "empty evidence is present and empty"
        );
        let conflicts: Vec<&str> = section(&v, "CONFLICTS")
            .iter()
            .map(|c| c["relation"].as_str().unwrap())
            .collect();
        assert_eq!(
            conflicts,
            vec!["current_status", "current_status"],
            "two sessions are not a conflict"
        );
        assert_eq!(section(&v, "OPEN").len(), 1);
        assert_eq!(
            section(&v, "DECISIONS")[0]["trust"],
            "memory_record_untrusted"
        );
        assert!(
            section(&v, "DECISIONS")[0].get("evidence").is_none(),
            "absent evidence stays absent"
        );
        assert_eq!(section(&v, "GOTCHAS")[0]["evidence"], json!([]));
        assert_eq!(
            (
                p.foreign_space["OPEN"],
                p.foreign_space["DECISIONS"],
                p.foreign_space["OUTCOMES"]
            ),
            (1, 1, 0)
        );
        assert!(
            !p.json.contains("elsewhere")
                && !p.json.contains("foreign-canary")
                && !p.json.contains("other/")
        );
        let refs: Vec<&str> = section(&v, "REFS")
            .iter()
            .map(|r| r.as_str().unwrap())
            .collect();
        let mut sorted = refs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(refs, sorted);
        assert!(
            refs.contains(&"coord-event:sp/3")
                && refs.contains(&"memory:sp/d1")
                && refs.contains(&"task:sp/4")
        );
        assert!(!p.truncated);
        // Scoped to task 1: its live links' objects and evidence become refs.
        let scoped = m
            .context_packet(&PacketInput {
                task: Some(1),
                ..input()
            })
            .unwrap();
        let v = parse(&scoped);
        let refs: Vec<&str> = section(&v, "REFS")
            .iter()
            .map(|r| r.as_str().unwrap())
            .collect();
        assert!(refs.contains(&"archive:inst-a/9") && refs.contains(&"archive-session:codex/s1"));
        assert!(
            section(&v, "OUTCOMES")
                .iter()
                .all(|o| o["task"] == "task:sp/1")
        );
        assert_eq!(v["sections"][0]["items"][0]["task"], "task:sp/1");
    }

    #[test]
    fn budget_is_exact_for_the_whole_serialized_packet() {
        let d = dir("budget");
        let m = populated(&d);
        let complete = m
            .context_packet(&PacketInput {
                budget_bytes: Some(32_768),
                ..input()
            })
            .unwrap();
        assert!(!complete.truncated);
        // The envelope names its budget, so the floor depends on the budget's digits: find it.
        let mut floor = 10;
        let floor = loop {
            match m.context_packet(&PacketInput {
                budget_bytes: Some(floor),
                ..input()
            }) {
                Ok(_) => break floor,
                Err(err) => {
                    assert_eq!(err.kind, MapErrorKind::InvalidInput);
                    let named: usize = err
                        .message
                        .split("below the ")
                        .nth(1)
                        .unwrap()
                        .split('-')
                        .next()
                        .unwrap()
                        .parse()
                        .unwrap();
                    assert!(named > floor);
                    floor = named;
                }
            }
        };
        let at_floor = m
            .context_packet(&PacketInput {
                budget_bytes: Some(floor),
                ..input()
            })
            .unwrap();
        assert_eq!(at_floor.json.len(), floor);
        let v = parse(&at_floor);
        assert!(at_floor.truncated);
        let shown: Vec<usize> = v["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["items"].as_array().unwrap().len())
            .collect();
        assert_eq!(
            shown,
            vec![1, 0, 0, 0, 0, 0, 0, 0],
            "only SCOPE, never dropped"
        );
        assert_eq!(
            kind_of(m.context_packet(&PacketInput {
                budget_bytes: Some(floor - 1),
                ..input()
            })),
            MapErrorKind::InvalidInput
        );
        assert_eq!(
            kind_of(m.context_packet(&PacketInput {
                tier: Tier::Weak,
                budget_bytes: Some(4_097),
                ..input()
            })),
            MapErrorKind::InvalidInput
        );
        assert_eq!(
            kind_of(m.context_packet(&PacketInput {
                budget_bytes: Some(32_769),
                ..input()
            })),
            MapErrorKind::InvalidInput
        );
        assert!(
            m.context_packet(&PacketInput {
                tier: Tier::Weak,
                ..input()
            })
            .unwrap()
            .json
            .len()
                <= 2_048
        );
        let full = complete.json.len();
        let mut last = 0;
        for budget in (floor..=full + 10).step_by(7) {
            let p = m
                .context_packet(&PacketInput {
                    budget_bytes: Some(budget),
                    ..input()
                })
                .unwrap();
            let v = parse(&p);
            let included: usize = v["sections"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["items"].as_array().unwrap().len())
                .sum();
            let omitted: usize = p.omitted.values().sum();
            let all = m
                .context_packet(&PacketInput {
                    budget_bytes: Some(32_768),
                    ..input()
                })
                .unwrap();
            let total: usize = parse(&all)["sections"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["items"].as_array().unwrap().len())
                .sum();
            assert_eq!(
                included + omitted,
                total,
                "omitted counts are truthful at {budget}"
            );
            assert_eq!(p.truncated, omitted > 0);
            // Greedy fill is deterministic per budget; it is not promised monotone across budgets.
            let again = m
                .context_packet(&PacketInput {
                    budget_bytes: Some(budget),
                    ..input()
                })
                .unwrap();
            assert_eq!(again.json, p.json);
            last = last.max(included);
        }
        assert_eq!(last, full_items(&m));
    }

    fn full_items(m: &MapStore) -> usize {
        let all = m
            .context_packet(&PacketInput {
                budget_bytes: Some(32_768),
                ..input()
            })
            .unwrap();
        parse(&all)["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["items"].as_array().unwrap().len())
            .sum()
    }

    fn kind_of<T: std::fmt::Debug>(r: Result<T, MapError>) -> MapErrorKind {
        r.expect_err("expected error").kind
    }

    #[test]
    fn escaping_is_counted_utf8_stays_whole_and_order_never_depends_on_input() {
        let d = dir("escape");
        let m = populated(&d);
        let tricky = format!(
            "Решение: \"quote\" \\ back\u{1} ctl 😀 {}",
            "ж😀".repeat(200)
        );
        let mut memories: Vec<MemorySnippet> = (0..300)
            .map(|i| MemorySnippet {
                space: "sp".into(),
                id: format!("m{i:03}"),
                kind: MemoryKind::Decision,
                scope: MemoryScope::Space,
                text: if i == 0 {
                    tricky.clone()
                } else {
                    format!("пункт {i} «ок»")
                },
                provenance: "codex".into(),
                evidence: Some(vec![format!("memory:sp/src{}", i % 7)]),
                shared: None,
            })
            .collect();
        memories.push(memories[5].clone());
        let base = PacketInput {
            memories: memories.clone(),
            tier: Tier::Weak,
            budget_bytes: Some(4_096),
            ..input()
        };
        let p = m.context_packet(&base).unwrap();
        let v = parse(&p);
        let first = &section(&v, "DECISIONS")[0];
        let text = first["text"].as_str().unwrap();
        assert!(text.len() <= MEMORY_BYTES && text.ends_with('…') && first["clipped"] == true);
        assert!(
            tricky.starts_with(text.trim_end_matches('…')),
            "clipped on a char boundary"
        );
        assert!(
            p.json.contains("\\\"quote\\\"") && p.json.contains("\\u0001") && p.json.contains("😀")
        );
        let shown = section(&v, "DECISIONS").len();
        assert!(shown < 300 && p.truncated);
        assert_eq!(
            shown + p.omitted["DECISIONS"],
            300,
            "the duplicate counts once"
        );
        let ids: Vec<&str> = section(&v, "DECISIONS")
            .iter()
            .map(|x| x["ref"].as_str().unwrap())
            .collect();
        let expected: Vec<String> = (0..shown).map(|i| format!("memory:sp/m{i:03}")).collect();
        assert_eq!(ids, expected, "a stable prefix in id order");
        memories.reverse();
        let again = m
            .context_packet(&PacketInput {
                memories,
                ..base.clone()
            })
            .unwrap();
        assert_eq!(again.json, p.json, "input order does not matter");
        assert_eq!(m.context_packet(&base).unwrap().json, p.json);
        assert_eq!(
            kind_of(m.context_packet(&PacketInput {
                version: 2,
                ..input()
            })),
            MapErrorKind::Incompatible
        );
        let bad = MemorySnippet {
            id: "../x y".into(),
            ..base.memories[1].clone()
        };
        assert_eq!(
            kind_of(m.context_packet(&PacketInput {
                memories: vec![bad],
                ..input()
            })),
            MapErrorKind::InvalidInput
        );
    }

    #[test]
    fn read_only_open_creates_nothing_changes_nothing_and_refuses_unknown_files() {
        let d = dir("readonly");
        let missing = d.0.join("home").join("map").join("map.sqlite");
        assert_eq!(map_status(&missing).unwrap(), MapStatus::NotConfigured);
        assert_eq!(
            kind_of(MapStore::open_read_only(&missing)),
            MapErrorKind::NotFound
        );
        assert!(!d.0.exists(), "no directory was created");
        let m = populated(&d);
        let path = d.0.join("a").join("map.sqlite");
        let packet = m.context_packet(&input()).unwrap();
        drop(m);
        let before = std::fs::read(&path).unwrap();
        let mut ro = MapStore::open_read_only(&path).unwrap();
        assert_eq!(ro.machine(), A);
        assert_eq!(
            ro.context_packet(&input()).unwrap(),
            packet,
            "same packet from a read-only store"
        );
        assert_eq!(ro.timeline("sp", Some(1), 0, 100, None).unwrap().0.len(), 4);
        assert!(
            ro.append(
                ev(
                    "link_proposed",
                    "task:sp/9",
                    "has_session",
                    "archive-session:codex/z",
                    "codex"
                ),
                Authority::Unverified
            )
            .is_err()
        );
        assert!(ro.rebuild().is_err());
        drop(ro);
        assert!(
            before == std::fs::read(&path).unwrap(),
            "the main database file is unchanged"
        );
        match map_status(&path).unwrap() {
            MapStatus::Ready {
                machine, events, ..
            } => assert_eq!((machine.as_str(), events), (A, 10)),
            MapStatus::NotConfigured => panic!("configured"),
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(kind_of(map_status(&path)), MapErrorKind::Incompatible);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644,
            "no chmod"
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = d.0.join("link");
        std::fs::create_dir(&link).unwrap();
        std::fs::set_permissions(&link, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&path, link.join("map.sqlite")).unwrap();
        assert_eq!(
            kind_of(MapStore::open_read_only(&link.join("map.sqlite"))),
            MapErrorKind::Incompatible
        );
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE meta SET value = 'brain-ingest/9' WHERE key = 'ingest'",
                [],
            )
            .unwrap();
        assert_eq!(
            kind_of(MapStore::open_read_only(&path)),
            MapErrorKind::Incompatible
        );
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", 2)
            .unwrap();
        assert_eq!(kind_of(map_status(&path)), MapErrorKind::Incompatible);
    }

    #[test]
    fn a_project_packet_shows_only_that_project_and_labels_ambiguity() {
        let d = dir("project");
        let mut m = MapStore::open(&d.0.join("a").join("map.sqlite"), A).unwrap();
        let out = |task: i64, word: &str| EventDraft {
            basis: "coordinator_record".into(),
            asserted_kind: "coordinator".into(),
            client: "coordinator".into(),
            confidence_milli: 900,
            ..ev(
                "outcome_claimed",
                &format!("task:sp/{task}"),
                "outcome",
                &format!("outcome:sp/{word}"),
                "codex",
            )
        };
        for (t, w) in [
            (1, "claimed_completed"),
            (2, "claimed_failed"),
            (3, "claimed_completed"),
        ] {
            m.append(out(t, w), Authority::Unverified).unwrap();
        }
        for (subject, agent, object) in [
            ("task:sp/2", "codex", "status:sp/open"),
            ("task:sp/2", "claude", "status:sp/done"),
            ("project:sp/A", "codex", "status:sp/open"),
            ("project:sp/A", "claude", "status:sp/done"),
        ] {
            m.append(
                ev("link_proposed", subject, "current_status", object, agent),
                Authority::Unverified,
            )
            .unwrap();
        }
        let tp = |task: i64, project: &str| TaskProject {
            space: "sp".into(),
            task,
            project: project.into(),
        };
        let mem = |id: &str, scope: MemoryScope| MemorySnippet {
            space: "sp".into(),
            id: id.into(),
            kind: MemoryKind::Decision,
            scope,
            text: format!("text of {id}"),
            provenance: "codex".into(),
            evidence: None,
            shared: None,
        };
        let task = |id: i64, project: &str| OpenTask {
            space: "sp".into(),
            id,
            project: project.into(),
            state: "queued".into(),
            title: format!("title {id}"),
        };
        let base = PacketInput {
            project: Some("A".into()),
            open_tasks: vec![task(1, "A"), task(2, "B")],
            task_projects: vec![tp(1, "A"), tp(2, "B"), tp(3, "A"), tp(3, "B")],
            memories: vec![
                mem("ma", MemoryScope::Project("A".into())),
                mem("mb", MemoryScope::Project("B".into())),
                mem("ms", MemoryScope::Space),
                mem("mu", MemoryScope::Unknown),
            ],
            ..input()
        };
        let p = m.context_packet(&base).unwrap();
        let v = parse(&p);
        let scope = &section(&v, "SCOPE")[0];
        assert_eq!(
            (scope["kind"].as_str(), scope["project"].as_str()),
            (Some("project"), Some("project:sp/A"))
        );
        assert_eq!(
            scope["ambiguous_tasks"],
            json!([3]),
            "task 3 is in two projects: labelled, not assumed"
        );
        let tasks: Vec<&str> = section(&v, "OUTCOMES")
            .iter()
            .map(|o| o["task"].as_str().unwrap())
            .collect();
        assert_eq!(tasks, vec!["task:sp/1"]);
        assert_eq!(
            section(&v, "OUTCOMES")[0]["project_basis"],
            "coordinator_record"
        );
        let open: Vec<&str> = section(&v, "OPEN")
            .iter()
            .map(|o| o["ref"].as_str().unwrap())
            .collect();
        assert_eq!(open, vec!["task:sp/1"]);
        let conflicts: Vec<&str> = section(&v, "CONFLICTS")
            .iter()
            .map(|c| c["subject"].as_str().unwrap())
            .collect();
        assert_eq!(conflicts, vec!["project:sp/A", "project:sp/A"]);
        let decisions: Vec<(&str, &str)> = section(&v, "DECISIONS")
            .iter()
            .map(|x| (x["ref"].as_str().unwrap(), x["scope"].as_str().unwrap()))
            .collect();
        assert_eq!(
            decisions,
            vec![
                ("memory:sp/ma", "project:sp/A"),
                ("memory:sp/ms", "space_shared")
            ]
        );
        assert_eq!(
            (
                p.out_of_scope["OPEN"],
                p.out_of_scope["OUTCOMES"],
                p.out_of_scope["CONFLICTS"],
                p.out_of_scope["DECISIONS"]
            ),
            (1, 2, 2, 2)
        );
        for leak in [
            "task:sp/2",
            "task:sp/3",
            "memory:sp/mb",
            "memory:sp/mu",
            "title 2",
        ] {
            assert!(!p.json.contains(leak), "{leak} leaked into project A");
        }
        // Whole space: everything, each memory labelled with its scope.
        let all = parse(
            &m.context_packet(&PacketInput {
                project: None,
                ..base.clone()
            })
            .unwrap(),
        );
        assert_eq!(section(&all, "SCOPE")[0]["kind"], "space");
        assert_eq!(section(&all, "OUTCOMES").len(), 3);
        let labels: Vec<&str> = section(&all, "DECISIONS")
            .iter()
            .map(|x| x["scope"].as_str().unwrap())
            .collect();
        assert_eq!(
            labels,
            vec!["project:sp/A", "project:sp/B", "space_shared", "unknown"]
        );
        // A task of another project, asked for under A, says so.
        let other = parse(
            &m.context_packet(&PacketInput {
                task: Some(2),
                ..base.clone()
            })
            .unwrap(),
        );
        assert_eq!(
            section(&other, "SCOPE")[0]["task_project"]["state"],
            "other_project"
        );
        // Too small for SCOPE: a stable error, never a packet without its scope.
        let e = m
            .context_packet(&PacketInput {
                budget_bytes: Some(300),
                ..base.clone()
            })
            .unwrap_err();
        assert!(
            e.kind == MapErrorKind::InvalidInput && e.message.contains("SCOPE"),
            "{e}"
        );
        assert_eq!(
            kind_of(m.context_packet(&PacketInput {
                project: Some("../x".into()),
                ..base.clone()
            })),
            MapErrorKind::InvalidInput
        );
        // No map: coordinator-only, said so; nothing pretends to be a healthy empty map.
        let unmapped = context_packet_unmapped(&base).unwrap();
        let u = parse(&unmapped);
        assert_eq!(
            (
                u["map"].as_str(),
                section(&u, "SCOPE")[0]["source"].as_str()
            ),
            (Some("not_configured"), Some("coordinator_only+memory"))
        );
        assert!(section(&u, "OUTCOMES").is_empty() && section(&u, "CONFLICTS").is_empty());
        assert_eq!(section(&u, "OPEN").len(), 1);
    }

    fn typed(id: &str, kind: MemoryKind, scope: MemoryScope, text: &str) -> MemorySnippet {
        MemorySnippet {
            space: "sp".into(),
            id: id.into(),
            kind,
            scope,
            text: text.into(),
            provenance: "graph_memory".into(),
            evidence: None,
            shared: None,
        }
    }

    fn typed_input(project: Option<&str>) -> PacketInput {
        let a = || MemoryScope::Project("A".into());
        PacketInput {
            project: project.map(str::to_owned),
            open_tasks: Vec::new(),
            memories: vec![
                typed("f1", MemoryKind::Fact, a(), "fact A"),
                typed("t1", MemoryKind::Task, a(), "task A"),
                typed("e1", MemoryKind::Error, a(), "error A"),
                typed("n1", MemoryKind::Note, a(), "note A"),
                typed("d1", MemoryKind::Decision, a(), "decision A"),
                typed("g1", MemoryKind::Gotcha, a(), "gotcha A"),
                typed(
                    "fb",
                    MemoryKind::Fact,
                    MemoryScope::Project("B".into()),
                    "fact B",
                ),
                typed("fs", MemoryKind::Fact, MemoryScope::Space, "fact shared"),
                typed("fu", MemoryKind::Note, MemoryScope::Unknown, "note unknown"),
                MemorySnippet {
                    space: "other".into(),
                    ..typed("fo", MemoryKind::Fact, a(), "foreign fact")
                },
            ],
            ..input()
        }
    }

    #[test]
    fn every_memory_kind_keeps_its_own_kind_and_section() {
        let p = context_packet_unmapped(&typed_input(Some("A"))).unwrap();
        let v = parse(&p);
        let names: Vec<&str> = v["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, SECTIONS.to_vec(), "MEMORIES is appended after REFS");
        let texts = |name: &str| -> Vec<String> {
            section(&v, name)
                .iter()
                .map(|i| i["text"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(texts("DECISIONS"), vec!["decision A"]);
        assert_eq!(texts("GOTCHAS"), vec!["gotcha A"]);
        let kinds: BTreeMap<String, String> = section(&v, "MEMORIES")
            .iter()
            .map(|i| {
                (
                    i["text"].as_str().unwrap().to_owned(),
                    i["kind"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        let expected: BTreeMap<String, String> = [
            ("error A", "error"),
            ("fact A", "fact"),
            ("fact shared", "fact"),
            ("note A", "note"),
            ("task A", "task"),
        ]
        .into_iter()
        .map(|(t, k)| (t.to_owned(), k.to_owned()))
        .collect();
        assert_eq!(kinds, expected, "project B, unknown and foreign stay out");
        for item in section(&v, "MEMORIES") {
            assert_eq!(item["trust"], "memory_record_untrusted");
            assert!(v.to_string().contains(item["ref"].as_str().unwrap()));
        }
        assert_eq!(p.out_of_scope["MEMORIES"], 2, "fact B and unknown note");
        assert_eq!(p.foreign_space["MEMORIES"], 1);
        assert!(
            section(&v, "DECISIONS")
                .iter()
                .all(|i| i.get("kind").is_none())
        );
    }

    #[test]
    fn space_packet_labels_unknown_and_project_scopes_honestly() {
        let v = parse(&context_packet_unmapped(&typed_input(None)).unwrap());
        let scope = |text: &str| -> String {
            section(&v, "MEMORIES")
                .iter()
                .find(|i| i["text"] == text)
                .unwrap()["scope"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(scope("note unknown"), "unknown");
        assert_eq!(scope("fact shared"), "space_shared");
        assert_eq!(scope("fact B"), "project:sp/B");
        let b = parse(&context_packet_unmapped(&typed_input(Some("B"))).unwrap());
        let shown: Vec<&str> = section(&b, "MEMORIES")
            .iter()
            .map(|i| i["text"].as_str().unwrap())
            .collect();
        assert_eq!(shown, vec!["fact B", "fact shared"]);
    }

    #[test]
    fn typed_memories_respect_the_whole_packet_budget() {
        let mut input = typed_input(None);
        input.tier = Tier::Weak;
        input.memories = (0..300)
            .map(|i| {
                typed(
                    &format!("m{i:03}"),
                    MemoryKind::Fact,
                    MemoryScope::Space,
                    &"x".repeat(700),
                )
            })
            .collect();
        for budget in [None, Some(4_096)] {
            input.budget_bytes = budget;
            let p = context_packet_unmapped(&input).unwrap();
            assert!(p.json.len() <= budget.unwrap_or(2_048));
            let v = parse(&p);
            assert_eq!(v["bytes"].as_u64().unwrap() as usize, p.json.len());
            assert!(p.truncated && p.omitted["MEMORIES"] > 0);
            assert!(section(&v, "MEMORIES").iter().all(|i| i["clipped"] == true));
        }
        input.tier = Tier::Strong;
        input.budget_bytes = Some(32_768);
        assert!(context_packet_unmapped(&input).unwrap().json.len() <= 32_768);
        input.budget_bytes = Some(32_769);
        assert_eq!(
            kind_of(context_packet_unmapped(&input)),
            MapErrorKind::InvalidInput
        );
    }
}
