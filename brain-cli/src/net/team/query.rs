//! Fan-out over pinned TLS streams, intent filters and optional local semantic
//! + lexical RRF on verified complete replicas. A failed refresh never falls back to old answer content.
use super::super::{ConnectArgs, client_explicit as client};
use super::{Local, dir, intent::Question};
use brain_core::embeddings::Embedder;
use brain_core::network::{SHARE_STORE_FILE, TlsCallResult, TlsError};
use brain_core::sharing::{EventKind, FactKind, ShareResult, ShareStore};
use brain_core::team::{self, Audit, DAY, Fact, MAX_ENTITIES, Replica, SLICE_SCHEMA, Slice};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

#[derive(Debug, Args)]
pub(crate) struct PeerArgs {
    #[command(flatten)]
    local: Local,
    /// Enrolled name of the source machine.
    #[arg(long)]
    machine: String,
    /// Origin the source machine publishes under (pinned for verification).
    #[arg(long)]
    origin: String,
    /// Project id expected in the machine's slice.
    #[arg(long)]
    project: String,
    #[command(flatten)]
    connect: ConnectArgs,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Peer {
    schema: String,
    machine: String,
    origin: String,
    project: String,
    space: String,
    connect: ConnectArgs,
    #[serde(default)]
    via_coordinator: bool,
}
impl Peer {
    fn validate(&self) -> ShareResult<()> {
        if self.schema != "brain-team-peer/1"
            || ![&self.machine, &self.origin, &self.project, &self.space]
                .iter()
                .all(|s| team::id(s))
            || !team::is_team(&self.space)
            || self.connect.credential_fd.is_some()
            || (self.connect.dial.is_none() && self.connect.relay.via_relay.is_none())
            || self.connect.ca.as_ref().is_none_or(|s| !s.is_absolute())
            || self
                .connect
                .credential
                .as_ref()
                .is_none_or(|s| !s.is_absolute())
            || self.connect.server_name.is_none()
        {
            return Err(team::bad());
        }
        Ok(())
    }
}
#[derive(Debug, Args)]
pub(crate) struct AskArgs {
    pub(super) query: String,
    #[command(flatten)]
    pub(super) local: Local,
    /// Machines to ask, comma-separated; only explicitly listed ones are queried.
    #[arg(long, value_delimiter = ',', required = true)]
    pub(super) machines: Vec<String>,
    /// Answer as the facts stood at this Unix time (valid time).
    #[arg(long)]
    pub(super) valid_at: Option<i64>,
    /// Answer as the machines had recorded them by this Unix time.
    #[arg(long)]
    pub(super) recorded_at: Option<i64>,
    /// Maximum rows in the merged answer.
    #[arg(long,default_value_t=20,value_parser=clap::value_parser!(u32).range(1..=100))]
    pub(super) limit: u32,
    /// Overall deadline for all machines, in milliseconds.
    #[arg(long,default_value_t=15000,value_parser=clap::value_parser!(u64).range(50..=30000))]
    pub(super) deadline_ms: u64,
}
pub(super) fn configure(args: PeerArgs) -> ShareResult<Value> {
    let space = args.local.team_space.clone().ok_or_else(team::bad)?;
    let peer = Peer {
        schema: "brain-team-peer/1".into(),
        machine: args.machine,
        origin: args.origin,
        project: args.project,
        space,
        connect: args.connect,
        via_coordinator: false,
    };
    peer.validate()?;
    // Validate before persisting endpoint references. No coordinator request;
    // an explicit relay route may resolve its configured DNS host.
    let _ = client(&peer.connect).map_err(|_| team::bad())?;
    let store = ShareStore::open(dir(&args.local).join(SHARE_STORE_FILE))?;
    store.team_save_peer(
        &peer.machine,
        &serde_json::to_value(&peer).map_err(|_| team::bad())?,
    )?;
    Ok(
        json!({"status":"ok","machine":peer.machine,"origin":peer.origin,"space":peer.space,"project":peer.project}),
    )
}
/// Safe local projection: never serialize Peer or its ConnectArgs.
pub(super) fn peers_document(state: &std::path::Path) -> ShareResult<Value> {
    let path = state.join(SHARE_STORE_FILE);
    if !path.exists() {
        return Ok(json!({"status":"ok","peers":[]}));
    }
    let store = ShareStore::open_readonly(path)?;
    let now = super::super::unix_now();
    let mut peers = vec![];
    for machine in store.team_peer_names()? {
        let peer: Peer = serde_json::from_value(store.team_peer(&machine)?.ok_or_else(team::bad)?)
            .map_err(|_| team::bad())?;
        peer.validate()?;
        let replica = store.team_replica(&machine)?;
        let generated_at = replica.as_ref().map(|r| r.slice.generated_at);
        let age = generated_at.map(|t| (now - t).max(0));
        peers.push(json!({"machine":machine,"project":peer.project,"generated_at":generated_at,
            "age_seconds":age,"freshness":match age { None => "unknown", Some(a) if a > DAY => "stale", _ => "fresh" }}));
    }
    Ok(json!({"status":"ok","peers":peers}))
}
fn refresh(
    peer: &Peer,
    request_id: &str,
    started: Instant,
    budget: u64,
) -> Result<Replica, &'static str> {
    peer.validate().map_err(|_| "denied")?;
    let mut after = 0_u64;
    let mut facts = vec![];
    let mut slice: Option<Slice> = None;
    let mut previous_seq = None;
    let mut first_seq = None;
    for _ in 0..4 {
        let remaining = budget.saturating_sub(started.elapsed().as_millis() as u64);
        if remaining < 50 {
            return Err("timeout");
        }
        let mut connect = peer.connect.clone();
        connect.deadline_ms = remaining.min(connect.deadline_ms).clamp(50, 5000);
        let (tls, _) = client(&connect).map_err(|_| "denied")?;
        let mut args = Map::new();
        args.insert("space".into(), json!(peer.space));
        args.insert("after_seq".into(), json!(after));
        args.insert("limit".into(), json!(10));
        let tool = if peer.via_coordinator {
            args.insert("source_name".into(), json!(peer.machine));
            "team_pull"
        } else {
            "share_pull"
        };
        let body = match tls.call(tool, "", args, Some(request_id.into())) {
            TlsCallResult::Ok(body) => body,
            TlsCallResult::Refused(brain_core::network::NetError::Unavailable(_))
                if peer.via_coordinator =>
            {
                return Err("offline");
            }
            TlsCallResult::Refused(_) => return Err("denied"),
            TlsCallResult::Unreachable(_) => return Err("offline"),
            TlsCallResult::Tls(TlsError::Unreachable(_)) => return Err("offline"),
            TlsCallResult::Tls(_) => return Err("denied"),
        };
        let (events, more) = super::super::share::parse_page(&body, &peer.origin, &peer.space)
            .map_err(|_| "incomplete")?;
        if events.is_empty() && (slice.is_none() || more) {
            return Err("incomplete");
        }
        for event in events {
            // The first sequence may be >1: source serves only the current
            // complete generation. Every following sequence MUST be contiguous.
            if previous_seq.is_some_and(|s| event.seq != s + 1)
                || event.seq <= after
                || event.kind != EventKind::Put
                || event.claimed_agent.is_some()
            {
                return Err("incomplete");
            }
            first_seq.get_or_insert(event.seq);
            after = event.seq;
            previous_seq = Some(after);
            let shared = event.fact.ok_or("incomplete")?;
            if shared.claim.is_some() {
                return Err("incomplete");
            }
            if shared.payload["schema"] == SLICE_SCHEMA {
                if slice.is_some() || shared.kind != FactKind::Fact {
                    return Err("incomplete");
                }
                slice = Some(serde_json::from_value(shared.payload).map_err(|_| "incomplete")?);
            } else {
                if slice.is_some() || facts.len() >= MAX_ENTITIES {
                    return Err("incomplete");
                }
                let f: Fact = serde_json::from_value(shared.payload).map_err(|_| "incomplete")?;
                if (matches!(f.content, team::Content::Link { .. }))
                    != (shared.kind == FactKind::Link)
                {
                    return Err("incomplete");
                }
                facts.push(f);
            }
        }
        if !more {
            let slice = slice.ok_or("no_slice")?;
            if Some(slice.seq_first) != first_seq
                || Some(slice.seq_last) != previous_seq
                || slice.origin != peer.origin
                || slice.machine_id != peer.machine
                || slice.project_id != peer.project
                || slice.space != peer.space
                || slice.generated_at > super::super::unix_now() + 300
            {
                return Err("denied");
            }
            let replica = Replica {
                slice,
                facts,
                received_from_peer: peer.machine.clone(),
                received_at: super::super::unix_now(),
            };
            replica.validate().map_err(|_| "incomplete")?;
            if replica.received_at - replica.slice.generated_at > 90 * DAY {
                return Err("no_slice");
            }
            return Ok(replica);
        }
    }
    Err("incomplete")
}
fn audit(
    context: (&str, i64, Instant, &AskArgs),
    peer: Option<&Peer>,
    replica: Option<&Replica>,
    refs: Vec<String>,
    outcome: &str,
    reason: &str,
) -> Audit {
    let (request_id, received, started, args) = context;
    Audit {
        request_id: request_id.into(),
        received_at: received,
        completed_at: super::super::unix_now(),
        machine: None,
        agent: None,
        token_id: None,
        project_id: peer.map(|p| p.project.clone()),
        space: peer.map(|p| p.space.clone()),
        operation: "team_ask".into(),
        valid_at: args.valid_at,
        recorded_at: args.recorded_at,
        policy_revision: replica.map(|r| r.slice.manifest_revision),
        snapshot_id: replica.map(|r| r.slice.snapshot_id.clone()),
        count: refs.len(),
        record_refs: refs,
        outcome: outcome.into(),
        latency_ms: started.elapsed().as_millis() as u64,
        reason: reason.into(),
    }
}
fn terms(s: &str) -> BTreeSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .take(64)
        .map(str::to_lowercase)
        .collect()
}
#[derive(Clone)]
struct Hit {
    score: f64,
    fact: Fact,
    slice: Slice,
    received_at: i64,
    received_from_peer: String,
}
fn rank(
    replica: &Replica,
    question: &Question,
    query: &str,
    valid: i64,
    recorded: i64,
    embedder: Option<&dyn Embedder>,
) -> Vec<Fact> {
    let query_terms = &question.residual;
    let mut found = replica
        .facts
        .iter()
        .filter(|f| {
            f.valid_from <= valid
                && f.valid_to.is_none_or(|t| valid < t)
                && f.recorded_at <= recorded
                && f.superseded_at.is_none_or(|t| recorded < t)
        })
        .filter(|f| question.accepts(f, valid))
        .filter_map(|f| {
            let text = terms(&f.content.text());
            let score = query_terms.intersection(&text).count();
            (score > 0
                || (query_terms.is_empty()
                    && (question.intent.is_some()
                        || question.project.is_some()
                        || question.machine.is_some()
                        || question.since.is_some())))
            .then_some((score, f.clone()))
        })
        .collect::<Vec<_>>();
    found.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.entity_id.cmp(&b.1.entity_id))
            .then_with(|| a.1.revision.cmp(&b.1.revision))
    });
    let lexical = found.into_iter().map(|(_, f)| f).collect::<Vec<_>>();
    let Some(embedder) = embedder else {
        return lexical;
    };
    let Ok(vector) = embedder.embed(query) else {
        return lexical;
    };
    if vector.len() != embedder.dimensions() || !usable_vector(&vector) {
        return lexical;
    }
    let mut semantic = Vec::new();
    for f in &replica.facts {
        if f.valid_from > valid
            || f.valid_to.is_some_and(|t| valid >= t)
            || f.recorded_at > recorded
            || f.superseded_at.is_some_and(|t| recorded >= t)
            || !question.accepts(f, valid)
        {
            continue;
        }
        // A broken backend degrades the whole signal rather than favouring a partial batch.
        let Ok(other) = embedder.embed(&f.content.text()) else {
            return lexical;
        };
        if other.len() != vector.len() || !usable_vector(&other) {
            return lexical;
        }
        let dot: f64 = vector
            .iter()
            .zip(&other)
            .map(|(a, b)| f64::from(*a) * f64::from(*b))
            .sum();
        let norm = |v: &[f32]| v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
        let similarity = dot / (norm(&vector) * norm(&other));
        if similarity > 0.0 {
            semantic.push((similarity, f.clone()));
        }
    }
    semantic.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.entity_id.cmp(&b.1.entity_id))
            .then_with(|| a.1.revision.cmp(&b.1.revision))
    });
    fuse(
        &lexical,
        &semantic.into_iter().map(|(_, f)| f).collect::<Vec<_>>(),
    )
}
fn usable_vector(v: &[f32]) -> bool {
    !v.is_empty() && v.iter().all(|x| x.is_finite()) && v.iter().any(|x| *x != 0.0)
}
fn fuse(lexical: &[Fact], semantic: &[Fact]) -> Vec<Fact> {
    let mut scores = BTreeMap::new();
    for ranking in [lexical, semantic] {
        for (i, f) in ranking.iter().enumerate() {
            let entry = scores
                .entry((f.entity_id.clone(), f.revision))
                .or_insert((0.0_f64, f.clone()));
            entry.0 += 1.0 / (60.0 + i as f64 + 1.0);
        }
    }
    let mut ranked = scores.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|a, b| b.1.0.total_cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().map(|(_, (_, f))| f).collect()
}
pub(super) fn ask(config: &crate::RuntimeConfig, args: AskArgs) -> ShareResult<Value> {
    let started = Instant::now();
    let now = super::super::unix_now();
    if args.query.is_empty()
        || args.query.len() > 2000
        || args.machines.is_empty()
        || !(1..=100).contains(&args.limit)
        || args.machines.len() > 16
        || args.machines.iter().any(|s| !team::id(s))
        || args.valid_at.is_some_and(|t| t < 0)
        || args.recorded_at.is_some_and(|t| t < 0)
    {
        return Err(team::bad());
    }
    let question = Question::parse(&args.query, args.valid_at.unwrap_or(now));
    // Same mode, language selection and cached-only loader as brain ask; no worker or indexing.
    #[cfg(feature = "model2vec")]
    let model = {
        let plan = crate::embedding_runtime::SemanticPlan::current(
            &crate::brain_home(),
            &args.query,
            &config.models_dir,
        );
        if plan.may_load_model() {
            brain_core::embeddings::Model2VecEmbedder::from_pretrained_cached(
                plan.model_id(),
                &config.models_dir,
                true,
            )
            .ok()
        } else {
            None
        }
    };
    #[cfg(feature = "model2vec")]
    let embedder = model.as_ref().map(|m| m as &dyn Embedder);
    #[cfg(not(feature = "model2vec"))]
    let embedder: Option<&dyn Embedder> = {
        let _ = config;
        None
    };
    super::onboarding::refresh_if_joined(&dir(&args.local))?;
    let store = ShareStore::open(dir(&args.local).join(SHARE_STORE_FILE))?;
    let request_id = super::super::fresh_request_id().map_err(|_| team::bad())?;
    // A crash/cancel after this durable start leaves a pending record, never
    // an unjournaled success. Every completed result is logged before stdout.
    store.team_audit(&audit(
        (&request_id, now, started, &args),
        None,
        None,
        vec![],
        "cancelled",
        "pending",
    ))?;
    let mut statuses = vec![];
    let mut merged: BTreeMap<(String, String, String, String, u64), Hit> = BTreeMap::new();
    let machines = args.machines.iter().collect::<BTreeSet<_>>();
    let mut origins = BTreeSet::new();
    for machine in machines {
        let cached = store.team_replica(machine)?;
        let peer = store
            .team_peer(machine)?
            .and_then(|v| serde_json::from_value::<Peer>(v).ok());
        let outcome = if let Some(peer) = &peer {
            if args
                .local
                .team_space
                .as_ref()
                .is_some_and(|s| s != &peer.space)
                || !origins.insert((
                    peer.origin.clone(),
                    peer.space.clone(),
                    peer.project.clone(),
                ))
            {
                Err("denied")
            } else {
                refresh(peer, &request_id, started, args.deadline_ms)
            }
        } else {
            Err("no_slice")
        };
        match outcome {
            Ok(replica) => {
                store.team_save_replica(machine, &replica)?;
                let replica = if let Some(recorded) = args.recorded_at {
                    if let Some(historical) =
                        store.team_replica_as_of(machine, &replica.slice, recorded)?
                    {
                        historical
                    } else {
                        store.team_audit(&audit(
                            (&request_id, now, started, &args),
                            peer.as_ref(),
                            Some(&replica),
                            vec![],
                            "denied",
                            "unavailable",
                        ))?;
                        statuses.push(json!({"machine":machine,"status":"history_unknown","generated_at":replica.slice.generated_at,"age_seconds":(now-replica.slice.generated_at).max(0),"coverage":"unknown"}));
                        continue;
                    }
                } else {
                    replica
                };
                let ranked = rank(
                    &replica,
                    &question,
                    &args.query,
                    args.valid_at.unwrap_or(now),
                    args.recorded_at.unwrap_or(now),
                    embedder,
                );
                let refs = ranked
                    .iter()
                    .take(args.limit as usize)
                    .map(|f| f.entity_id.clone())
                    .collect();
                store.team_audit(&audit(
                    (&request_id, now, started, &args),
                    peer.as_ref(),
                    Some(&replica),
                    refs,
                    "ok",
                    "ok",
                ))?;
                let age = (now - replica.slice.generated_at).max(0);
                let status = if age > DAY {
                    "stale"
                } else if replica.slice.coverage == "incomplete" {
                    "incomplete"
                } else if ranked.is_empty() {
                    "empty"
                } else {
                    "ok"
                };
                statuses.push(json!({"machine":machine,"status":status,"snapshot_id":replica.slice.snapshot_id,"generated_at":replica.slice.generated_at,"age_seconds":age,"coverage":replica.slice.coverage}));
                for (i, f) in ranked.into_iter().enumerate() {
                    let key = (
                        f.origin.clone(),
                        f.space.clone(),
                        f.project_id.clone(),
                        f.entity_id.clone(),
                        f.revision,
                    );
                    let score = 1.0 / (60.0 + i as f64 + 1.0);
                    // One verified origin gets one vote; identical texts on different
                    // machines remain separate attributed facts.
                    merged.entry(key).or_insert(Hit {
                        score,
                        fact: f,
                        slice: replica.slice.clone(),
                        received_at: replica.received_at,
                        received_from_peer: replica.received_from_peer.clone(),
                    });
                }
            }
            Err(status) => {
                store.team_audit(&audit(
                    (&request_id, now, started, &args),
                    peer.as_ref(),
                    cached.as_ref(),
                    vec![],
                    if status == "denied" {
                        "denied"
                    } else {
                        "error"
                    },
                    if status == "denied" {
                        "refused"
                    } else {
                        "unavailable"
                    },
                ))?;
                statuses.push(json!({"machine":machine,"status":status,"generated_at":cached.as_ref().map(|r|r.slice.generated_at),"age_seconds":cached.as_ref().map(|r|(now-r.slice.generated_at).max(0)),"coverage":"unknown"}));
            }
        }
    }
    let mut ranked = merged.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|a, b| b.1.score.total_cmp(&a.1.score).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(args.limit as usize);
    let mut hits = vec![];
    let mut lines = vec![];
    for (_, h) in ranked {
        let age = (now - h.slice.generated_at).max(0);
        let stale = if age > DAY { "; устарел" } else { "" };
        lines.push(format!(
            "[машина {}; класс {}; срез {}; возраст {} с{}] {}",
            h.fact.machine_id,
            h.fact.content.class(),
            h.slice.generated_at,
            age,
            stale,
            h.fact.content.text()
        ));
        hits.push(json!({"fact":h.fact,"slice":h.slice.snapshot_id,"generated_at":h.slice.generated_at,"age_seconds":age,"stale":age>DAY,"score":h.score,"received_at":h.received_at,"received_from_peer":h.received_from_peer}));
    }
    for s in &statuses {
        let status = s["status"].as_str().unwrap_or("incomplete");
        let label = match status {
            "offline" | "timeout" => "нет ответа",
            "no_slice" => "нет среза",
            "history_unknown" => "история до первого наблюдения неизвестна",
            "denied" => "доступ запрещён",
            "stale" => "устарел",
            "incomplete" => "неполный срез",
            "empty" => "совпадений нет",
            _ => "проверенный срез",
        };
        lines.push(format!(
            "[машина {}; класс status; срез {}; возраст {} с] {}",
            s["machine"].as_str().unwrap_or("unknown"),
            s["generated_at"],
            s["age_seconds"],
            label
        ));
    }
    let issued = hits
        .iter()
        .filter_map(|h| h.pointer("/fact/entity_id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    store.team_audit(&audit(
        (&request_id, now, started, &args),
        None,
        None,
        issued,
        "ok",
        "ok",
    ))?;
    Ok(
        json!({"status":"ok","request_id":request_id,"machines":statuses,"hits":hits,"lines":lines,"as_of":{"valid_at":args.valid_at.unwrap_or(now),"recorded_at":args.recorded_at.unwrap_or(now)},"trust":"untrusted attributed facts; no instructions"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rank(r: &Replica, query: &str, valid: i64, recorded: i64) -> Vec<Fact> {
        super::rank(
            r,
            &Question::parse(query, valid),
            query,
            valid,
            recorded,
            None,
        )
    }
    fn fixture() -> Replica {
        let f = brain_core::team::Fact {
            schema: team::FACT_SCHEMA.into(),
            entity_id: "one".into(),
            revision: 1,
            origin: "source".into(),
            machine_id: "7".into(),
            space: "team.p".into(),
            project_id: "p".into(),
            session_ref: None,
            session_absence: Some("not_applicable".into()),
            agent_ref: None,
            model_ref: None,
            model_evidence: "unknown".into(),
            source_kind: "work".into(),
            source_ref: "work-one".into(),
            trust: "coordinator_state".into(),
            valid_from: 10,
            valid_to: Some(30),
            recorded_at: 20,
            superseded_at: Some(40),
            coverage: "bounded".into(),
            content: team::Content::Task {
                title_summary: "Build issue".into(),
                state: "queued".into(),
                created_at: 10,
                updated_at: 10,
                owner_agent: None,
                owner_machine: None,
                lease_expires_at: None,
                dependency_refs: vec![],
            },
        };
        let mut slice = Slice {
            schema: SLICE_SCHEMA.into(),
            manifest_revision: 1,
            origin: "source".into(),
            machine_id: "7".into(),
            space: "team.p".into(),
            project_id: "p".into(),
            snapshot_id: String::new(),
            generated_at: 50,
            seq_first: 1,
            seq_last: 1,
            collected_from: 50,
            source_watermarks: team::Watermarks {
                distill: None,
                work: Some(10),
                git: None,
            },
            coverage: "bounded".into(),
            rejected: 0,
            entity_digests: vec![],
            integrity: "tls+blake3-unsigned".into(),
            signature: None,
        };
        slice.seal().unwrap();
        Replica {
            slice,
            facts: vec![f.clone()],
            received_from_peer: "7".into(),
            received_at: 50,
        }
    }
    #[test]
    fn ranking_obeys_both_time_axes_and_keeps_distinct_sources() {
        let mut r = fixture();
        let mut f = r.facts[0].clone();
        assert_eq!(rank(&r, "build", 15, 25).len(), 1);
        assert!(rank(&r, "build", 30, 25).is_empty());
        assert!(rank(&r, "build", 15, 40).is_empty());
        assert!(rank(&r, "build", 15, 19).is_empty());
        f.entity_id = "two".into();
        r.facts.push(f);
        assert_eq!(rank(&r, "build", 15, 25).len(), 2);
    }
    #[test]
    fn intent_filters_status_scope_and_event_time() {
        let mut r = fixture();
        let f = &mut r.facts[0];
        f.valid_to = None;
        f.superseded_at = None;
        for status in [
            "suspected",
            "confirmed",
            "reopened",
            "fixed",
            "wont_fix",
            "not_a_bug",
            "became_feature",
        ] {
            f.content = team::Content::Error {
                what_summary: "Encoding trouble".into(),
                why_summary: "Invalid bytes".into(),
                status: status.into(),
                reason_summary: String::new(),
                evidence_refs: vec![],
                transition_refs: vec![],
            };
            let q = Question::parse("Какие ошибки сейчас открыты?", 10 * DAY);
            assert_eq!(
                q.accepts(f, 10 * DAY),
                ["suspected", "confirmed", "reopened"].contains(&status)
            );
        }
        assert!(
            rank(
                &r,
                "Какие ошибки открыты в проекте other?",
                10 * DAY,
                10 * DAY
            )
            .is_empty()
        );
        assert!(
            rank(
                &r,
                "Какие ошибки открыты на машине other?",
                10 * DAY,
                10 * DAY
            )
            .is_empty()
        );
        for status in ["proposed", "adopted", "worked", "failed", "reverted"] {
            r.facts[0].content = team::Content::Decision {
                what_summary: "Global cache".into(),
                why_summary: "Stale reads".into(),
                status: status.into(),
                evidence_refs: vec![],
                error_refs: vec![],
                error_links: "unknown".into(),
            };
            assert_eq!(
                rank(
                    &r,
                    "Какие решения не сработали и почему?",
                    10 * DAY,
                    10 * DAY
                )
                .len(),
                usize::from(status == "failed")
            );
        }
        r.facts[0].content = team::Content::DeadEnd {
            what_summary: "Polling".into(),
            why_summary: "Too costly".into(),
            status: "abandoned".into(),
        };
        assert_eq!(
            rank(&r, "Which decisions failed?", 10 * DAY, 10 * DAY).len(),
            1
        );
        for status in ["proposed", "active", "completed", "abandoned", "confirmed"] {
            r.facts[0].content = team::Content::Plan {
                what_summary: "Retry".into(),
                why_summary: "Reduce timeouts".into(),
                status: status.into(),
                task_refs: vec![],
            };
            assert_eq!(
                rank(&r, "Что в планах на следующий спринт?", 10 * DAY, 10 * DAY).len(),
                usize::from(["proposed", "active"].contains(&status))
            );
        }
        for (state, expected) in [
            ("active", 1),
            ("queued", 0),
            ("paused", 0),
            ("completed", 0),
            ("failed", 0),
        ] {
            r.facts[0].content = team::Content::Task {
                title_summary: "Retry".into(),
                state: state.into(),
                created_at: 10,
                updated_at: 10,
                owner_agent: None,
                owner_machine: None,
                lease_expires_at: None,
                dependency_refs: vec![],
            };
            assert_eq!(
                rank(&r, "Чем заняты машины?", 10 * DAY, 10 * DAY).len(),
                expected
            );
        }
        for (when, expected) in [
            (3 * DAY - 1, 0),
            (3 * DAY, 1),
            (10 * DAY, 1),
            (10 * DAY + 1, 0),
        ] {
            r.facts[0].content = team::Content::Commit {
                oid: "abcdef".into(),
                parent_oids: vec![],
                committed_at: when,
                is_merge: false,
                subject_summary: "Retry".into(),
                task_refs: vec![],
            };
            assert_eq!(
                rank(&r, "Чем заняты машины?", 10 * DAY, 10 * DAY).len(),
                expected
            );
            assert_eq!(
                rank(&r, "Retry за неделю", 10 * DAY, 10 * DAY).len(),
                expected
            );
        }
        assert!(rank(&r, "   ", 10 * DAY, 10 * DAY).is_empty());
    }

    struct SemanticStub;
    impl Embedder for SemanticStub {
        fn model_id(&self) -> &str {
            "synthetic"
        }
        fn dimensions(&self) -> usize {
            2
        }
        fn embed(&self, text: &str) -> Result<Vec<f32>, brain_core::embeddings::EmbeddingError> {
            Ok(if text.contains("Build") || text.contains("compile") {
                vec![1.0, 0.0]
            } else {
                vec![0.0, 1.0]
            })
        }
    }
    #[test]
    fn semantic_recovers_paraphrases_but_never_bypasses_filters() {
        let mut r = fixture();
        let query = "compile";
        let q = Question::parse(query, 15);
        assert!(super::rank(&r, &q, query, 15, 25, None).is_empty());
        assert_eq!(
            super::rank(&r, &q, query, 15, 25, Some(&SemanticStub)).len(),
            1
        );
        for (valid, recorded) in [(30, 25), (15, 40), (15, 19)] {
            assert!(super::rank(&r, &q, query, valid, recorded, Some(&SemanticStub)).is_empty());
        }
        let q = Question::parse("Open errors compile", 15);
        assert!(super::rank(&r, &q, query, 15, 25, Some(&SemanticStub)).is_empty());
        let q = Question::parse("compile in project forbidden", 15);
        assert!(super::rank(&r, &q, query, 15, 25, Some(&SemanticStub)).is_empty());
        let mut other = r.facts[0].clone();
        other.entity_id = "two".into();
        r.facts.push(other);
        let q = Question::parse("compile on machine forbidden", 15);
        assert!(super::rank(&r, &q, query, 15, 25, Some(&SemanticStub)).is_empty());
        // RRF adds the independent signals and retains revision identities.
        let mut revised = r.facts[0].clone();
        revised.revision = 2;
        let result = fuse(
            &[r.facts[0].clone(), r.facts[1].clone()],
            &[r.facts[1].clone(), revised],
        );
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].entity_id, "two");
    }
    struct BrokenEmbedder(u8);
    impl Embedder for BrokenEmbedder {
        fn model_id(&self) -> &str {
            "broken"
        }
        fn dimensions(&self) -> usize {
            2
        }
        fn embed(&self, text: &str) -> Result<Vec<f32>, brain_core::embeddings::EmbeddingError> {
            match self.0 {
                0 => Err(brain_core::embeddings::EmbeddingError::Backend(
                    "synthetic".into(),
                )),
                1 => Ok(vec![1.0]),
                2 => Ok(vec![f32::NAN, 1.0]),
                3 => Ok(vec![0.0, 0.0]),
                _ if text.starts_with("Build") => Err(
                    brain_core::embeddings::EmbeddingError::Backend("fact failure".into()),
                ),
                _ => Ok(vec![1.0, 0.0]),
            }
        }
    }
    #[test]
    fn backend_failures_preserve_lexical_results() {
        let r = fixture();
        for mode in 0..5 {
            let q = Question::parse("build", 15);
            let result = super::rank(&r, &q, "build", 15, 25, Some(&BrokenEmbedder(mode)));
            assert_eq!(result.len(), 1);
            assert_eq!(result[0].entity_id, "one");
            let q = Question::parse("compile", 15);
            assert!(super::rank(&r, &q, "compile", 15, 25, Some(&BrokenEmbedder(mode))).is_empty());
        }
    }
}
