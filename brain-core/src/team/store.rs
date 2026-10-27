//! Host-local policy, complete generations, and bounded structured audit.
use super::*;
use crate::sharing::{
    EventKind, FactKind, LOCAL_PEER, OwnPage, Principal, ShareEvent, ShareStore, SharedFact,
    TrustedPeer,
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

pub(crate) const SQL: &str = "
CREATE TABLE IF NOT EXISTS team_policies(space TEXT PRIMARY KEY, document TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS team_generations(space TEXT PRIMARY KEY, policy_revision INTEGER NOT NULL, fingerprint TEXT NOT NULL, first_seq INTEGER NOT NULL, last_seq INTEGER NOT NULL, document TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS team_history(space TEXT NOT NULL, snapshot_id TEXT NOT NULL, generated_at INTEGER NOT NULL, document TEXT NOT NULL, PRIMARY KEY(space,snapshot_id));
CREATE TABLE IF NOT EXISTS team_observations(space TEXT NOT NULL, entity_id TEXT NOT NULL, document TEXT NOT NULL, PRIMARY KEY(space,entity_id));
CREATE TABLE IF NOT EXISTS team_audit(id INTEGER PRIMARY KEY, at INTEGER NOT NULL, document TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS team_peers(machine TEXT PRIMARY KEY, document TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS team_replica_history(machine TEXT NOT NULL, snapshot_id TEXT NOT NULL, generated_at INTEGER NOT NULL, revision INTEGER NOT NULL, document TEXT NOT NULL, PRIMARY KEY(machine,snapshot_id));
CREATE TABLE IF NOT EXISTS team_replicas(machine TEXT PRIMARY KEY, received_at INTEGER NOT NULL, document TEXT NOT NULL);
";
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Recipient {
    pub machine: String,
    pub agent: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: String,
    pub revision: u64,
    pub approved_at: i64,
    pub enabled: bool,
    pub origin: String,
    pub machine_id: String,
    pub project_id: String,
    pub project_root: String,
    pub source_space: String,
    pub space: String,
    pub recipients: Vec<Recipient>,
    pub classes: BTreeSet<String>,
    #[serde(default)]
    pub disabled_fields: BTreeSet<String>,
    pub annotation_policy: String,
    pub integrity: String,
    pub snapshot_retention_days: u32,
    pub audit_retention_days: u32,
    pub stale_after_seconds: i64,
}
impl Manifest {
    pub fn validate(&self) -> ShareResult<()> {
        if self.schema != "brain-team-manifest/1"
            || self.revision == 0
            || self.approved_at < 0
            || ![
                &self.origin,
                &self.machine_id,
                &self.project_id,
                &self.source_space,
                &self.space,
            ]
            .iter()
            .all(|s| id(s))
            || !is_team(&self.space)
            || is_team(&self.source_space)
            || !std::path::Path::new(&self.project_root).is_absolute()
            || self.project_root.chars().any(char::is_control)
            || self.recipients.is_empty()
            || self.recipients.len() > 24
            || self
                .recipients
                .iter()
                .any(|r| !id(&r.machine) || !id(&r.agent))
            || self.classes.iter().any(|s| !CLASSES.contains(&s.as_str()))
            || self.disabled_fields.iter().any(|s| {
                s.split_once('.')
                    .is_none_or(|(c, f)| !fields(c).contains(&f))
            })
            || self.annotation_policy != "automatic-masked"
            || self.integrity != "tls+blake3-unsigned"
            || self.snapshot_retention_days != 90
            || self.audit_retention_days != 365
            || self.stale_after_seconds != DAY
        {
            return Err(bad());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replica {
    pub slice: Slice,
    pub facts: Vec<Fact>,
    pub received_from_peer: String,
    pub received_at: i64,
}
impl Replica {
    pub fn validate(&self) -> ShareResult<()> {
        self.slice.validate(&self.facts)?;
        if !id(&self.received_from_peer) || self.received_at < 0 {
            return Err(bad());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Audit {
    pub request_id: String,
    pub received_at: i64,
    pub completed_at: i64,
    pub machine: Option<String>,
    pub agent: Option<String>,
    pub token_id: Option<String>,
    pub project_id: Option<String>,
    pub space: Option<String>,
    pub operation: String,
    pub valid_at: Option<i64>,
    pub recorded_at: Option<i64>,
    pub policy_revision: Option<u64>,
    pub snapshot_id: Option<String>,
    pub record_refs: Vec<String>,
    pub count: usize,
    pub outcome: String,
    pub latency_ms: u64,
    pub reason: String,
}
impl ShareStore {
    pub fn team_manifest(&self, space: &str) -> ShareResult<Option<Manifest>> {
        let text: Option<String> = self
            .conn
            .query_row(
                "SELECT document FROM team_policies WHERE space=?1",
                [space],
                |r| r.get(0),
            )
            .optional()?;
        text.map(|s| {
            let m: Manifest = serde_json::from_str(&s).map_err(|_| bad())?;
            m.validate()?;
            if m.origin != self.install_id() {
                return Err(bad());
            }
            Ok(m)
        })
        .transpose()
    }
    pub fn team_manifests(&self) -> ShareResult<Vec<Manifest>> {
        let mut st = self
            .conn
            .prepare("SELECT space FROM team_policies ORDER BY space")?;
        st.query_map([], |r| r.get::<_, String>(0))?
            .map(|s| self.team_manifest(&s?)?.ok_or_else(bad))
            .collect()
    }
    /// Trusted local configuration only. Reconfiguration always disables publication.
    pub fn team_configure(&mut self, mut manifest: Manifest) -> ShareResult<Manifest> {
        manifest.enabled = false;
        manifest.origin = self.install_id().into();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old: Option<String> = tx
            .query_row(
                "SELECT document FROM team_policies WHERE space=?1",
                [&manifest.space],
                |r| r.get(0),
            )
            .optional()?;
        manifest.revision = old
            .map(|s| serde_json::from_str::<Manifest>(&s).map(|m| m.revision + 1))
            .transpose()
            .map_err(|_| bad())?
            .unwrap_or(1);
        manifest.validate()?;
        tx.execute("INSERT INTO team_policies VALUES(?1,?2) ON CONFLICT(space) DO UPDATE SET document=excluded.document",params![manifest.space,serde_json::to_string(&manifest).map_err(|_|bad())?])?;
        tx.commit()?;
        Ok(manifest)
    }
    /// A class change invalidates the entire old generation until reprojected.
    pub fn team_toggle(
        &mut self,
        space: &str,
        class: Option<&str>,
        enabled: bool,
        now: i64,
    ) -> ShareResult<Manifest> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let text: String = tx.query_row(
            "SELECT document FROM team_policies WHERE space=?1",
            [space],
            |r| r.get(0),
        )?;
        let mut m: Manifest = serde_json::from_str(&text).map_err(|_| bad())?;
        if let Some(class) = class {
            if !CLASSES.contains(&class) {
                return Err(bad());
            }
            if enabled {
                m.classes.insert(class.into());
            } else {
                m.classes.remove(class);
            }
        } else {
            m.enabled = enabled;
        }
        m.revision += 1;
        m.approved_at = now;
        m.validate()?;
        tx.execute(
            "UPDATE team_policies SET document=?2 WHERE space=?1",
            params![space, serde_json::to_string(&m).map_err(|_| bad())?],
        )?;
        tx.commit()?;
        Ok(m)
    }
    pub fn team_toggle_field(
        &mut self,
        space: &str,
        class: &str,
        field: &str,
        enabled: bool,
        now: i64,
    ) -> ShareResult<Manifest> {
        if !fields(class).contains(&field) {
            return Err(bad());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let document: String = tx.query_row(
            "SELECT document FROM team_policies WHERE space=?1",
            [space],
            |r| r.get(0),
        )?;
        let mut m: Manifest = serde_json::from_str(&document).map_err(|_| bad())?;
        let key = format!("{class}.{field}");
        if enabled {
            m.disabled_fields.remove(&key);
        } else {
            m.disabled_fields.insert(key);
        }
        m.revision += 1;
        m.approved_at = now;
        m.validate()?;
        tx.execute(
            "UPDATE team_policies SET document=?2 WHERE space=?1",
            params![space, serde_json::to_string(&m).map_err(|_| bad())?],
        )?;
        tx.commit()?;
        Ok(m)
    }
    pub fn team_active_slice(&self, space: &str) -> ShareResult<Option<Slice>> {
        let s: Option<String> = self
            .conn
            .query_row(
                "SELECT document FROM team_generations WHERE space=?1",
                [space],
                |r| r.get(0),
            )
            .optional()?;
        s.map(|s| serde_json::from_str(&s).map_err(|_| bad()))
            .transpose()
    }
    pub(crate) fn team_authorize(&self, principal: &Principal, space: &str) -> ShareResult<()> {
        let m = self.team_manifest(space)?.ok_or_else(bad)?;
        let s = self.team_active_slice(space)?.ok_or_else(bad)?;
        if !m.enabled
            || s.manifest_revision != m.revision
            || !m
                .recipients
                .iter()
                .any(|r| r.machine == principal.machine_id() && r.agent == principal.agent_id())
            || chrono::Utc::now().timestamp() - s.generated_at > 90 * DAY
        {
            return Err(bad());
        }
        Ok(())
    }
    pub(crate) fn team_page(
        &self,
        principal: &Principal,
        space: &str,
        after: u64,
        limit: u32,
    ) -> ShareResult<OwnPage> {
        self.authorize_read(principal, space)?;
        if limit == 0 || limit > crate::sharing::MAX_WIRE_PAGE {
            return Err(bad());
        }
        let (first, last): (u64, u64) = self.conn.query_row(
            "SELECT first_seq,last_seq FROM team_generations WHERE space=?1",
            [space],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
        )?;
        // Generic history pages are forbidden; only this complete generation is served.
        if after != 0 && after < first.saturating_sub(1) {
            return Err(bad());
        }
        let start = after.max(first.saturating_sub(1));
        // Validate the whole prepared generation before releasing even its first
        // page. A valid event digest alone cannot validate a slice digest.
        let (all, _) = self.page(
            principal,
            space,
            self.install_id(),
            first.saturating_sub(1),
            (MAX_ENTITIES + 1) as u32,
        )?;
        let all = all
            .into_iter()
            .take_while(|e| e.seq <= last)
            .collect::<Vec<_>>();
        if all.len() as u64 != last - first + 1 || all.len() > MAX_ENTITIES + 1 {
            return Err(bad());
        }
        let m = self.team_manifest(space)?.ok_or_else(bad)?;
        let mut prepared = vec![];
        let mut manifest = None;
        for (index, e) in all.iter().enumerate() {
            if e.seq != first + index as u64
                || e.claimed_agent.is_some()
                || e.kind != EventKind::Put
            {
                return Err(bad());
            }
            let shared = e.fact.as_ref().ok_or_else(bad)?;
            if shared.claim.is_some() {
                return Err(bad());
            }
            if shared.payload["schema"] == SLICE_SCHEMA {
                if manifest.is_some() || index + 1 != all.len() {
                    return Err(bad());
                }
                manifest = Some(
                    serde_json::from_value::<Slice>(shared.payload.clone()).map_err(|_| bad())?,
                );
            } else {
                let f: Fact = serde_json::from_value(shared.payload.clone()).map_err(|_| bad())?;
                if !m.classes.contains(f.content.class())
                    || project_fields(&f, &m.disabled_fields)? != f
                {
                    return Err(bad());
                }
                prepared.push(f);
            }
        }
        let slice = manifest.ok_or_else(bad)?;
        slice.validate(&prepared)?;
        if slice.seq_first != first
            || slice.seq_last != last
            || slice != self.team_active_slice(space)?.ok_or_else(bad)?
        {
            return Err(bad());
        }
        let events = all
            .into_iter()
            .filter(|e| e.seq > start)
            .take(limit as usize)
            .collect::<Vec<_>>();
        let next_seq = events.last().map_or(start, |e| e.seq);
        Ok(OwnPage {
            origin: self.install_id().into(),
            events,
            next_seq,
            has_more: next_seq < last,
        })
    }
    /// Only this typed path may publish to reserved team spaces. The policy and
    /// full generation are committed in one SQLite transaction with share events.
    pub fn team_publish(
        &mut self,
        space: &str,
        mut facts: Vec<Fact>,
        source_watermarks: Watermarks,
        rejected: u32,
        collected_from: i64,
        now: i64,
    ) -> ShareResult<Value> {
        let m = self.team_manifest(space)?.ok_or_else(bad)?;
        if !m.enabled || facts.len() > MAX_ENTITIES {
            return Err(bad());
        }
        for f in &facts {
            f.validate()?;
            if project_fields(f, &m.disabled_fields)? != *f {
                return Err(bad());
            }
            if !m.classes.contains(f.content.class())
                || f.origin != m.origin
                || f.machine_id != m.machine_id
                || f.project_id != m.project_id
                || f.space != m.space
            {
                return Err(bad());
            }
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Recheck policy under the write lock; local disable always wins.
        let live: String = tx.query_row(
            "SELECT document FROM team_policies WHERE space=?1",
            [space],
            |r| r.get(0),
        )?;
        if serde_json::from_str::<Manifest>(&live).map_err(|_| bad())? != m {
            return Err(bad());
        }
        for f in &mut facts {
            let old: Option<String> = tx
                .query_row(
                    "SELECT document FROM team_observations WHERE space=?1 AND entity_id=?2",
                    params![space, f.entity_id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(old) = old {
                let previous: Fact = serde_json::from_str(&old).map_err(|_| bad())?;
                if f.source_kind == "work" || f.source_kind == "git" || f.source_kind == "derived" {
                    let mut compare = f.clone();
                    compare.revision = previous.revision;
                    compare.recorded_at = previous.recorded_at;
                    if f.source_kind == "work" {
                        compare.valid_from = previous.valid_from;
                    }
                    if compare == previous {
                        *f = previous;
                    } else {
                        f.revision = previous.revision + 1;
                        f.recorded_at = now;
                    }
                }
            }
            tx.execute("INSERT INTO team_observations VALUES(?1,?2,?3) ON CONFLICT(space,entity_id) DO UPDATE SET document=excluded.document",params![space,f.entity_id,serde_json::to_string(f).map_err(|_|bad())?])?;
        }
        facts.sort_by(|a, b| a.entity_id.cmp(&b.entity_id));
        let fingerprint = digest(&(m.revision, &facts, rejected))?;
        let old: Option<(String, String)> = tx
            .query_row(
                "SELECT fingerprint,document FROM team_generations WHERE space=?1",
                [space],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((key, doc)) = old
            && key == fingerprint
        {
            let slice: Slice = serde_json::from_str(&doc).map_err(|_| bad())?;
            slice.validate(&facts)?;
            tx.commit()?;
            return Ok(json!({"status":"ok","unchanged":true,"slice":slice}));
        }
        let previous_seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM events WHERE origin=?1 AND space=?2",
            params![m.origin, space],
            |r| r.get(0),
        )?;
        let live_count: i64 = tx.query_row(
            "SELECT count(*) FROM facts WHERE origin=?1 AND space=?2 AND status='live'",
            params![m.origin, space],
            |r| r.get(0),
        )?;
        let seq_first = (previous_seq + live_count + 1) as u64;
        let mut slice = Slice {
            schema: SLICE_SCHEMA.into(),
            manifest_revision: m.revision,
            origin: m.origin.clone(),
            machine_id: m.machine_id.clone(),
            space: m.space.clone(),
            project_id: m.project_id.clone(),
            snapshot_id: String::new(),
            generated_at: now,
            seq_first,
            seq_last: seq_first + facts.len() as u64,
            collected_from,
            source_watermarks,
            coverage: if rejected > 0 {
                "incomplete"
            } else {
                "bounded"
            }
            .into(),
            rejected,
            entity_digests: facts
                .iter()
                .map(|f| {
                    Ok(EntityDigest {
                        entity_id: f.entity_id.clone(),
                        revision: f.revision,
                        digest: digest(f)?,
                    })
                })
                .collect::<ShareResult<_>>()?,
            integrity: m.integrity.clone(),
            signature: None,
        };
        slice.seal()?;
        slice.validate(&facts)?;
        let mut seq: u64 = tx.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM events WHERE origin=?1 AND space=?2",
            params![m.origin, space],
            |r| r.get::<_, i64>(0),
        )? as u64;
        let peer = TrustedPeer::new(LOCAL_PEER, format!("unix:{now}"));
        // Retractions remain local history. The new served window starts AFTER them.
        let old_facts: Vec<i64> = {
            let mut st = tx.prepare(
                "SELECT origin_seq FROM facts WHERE origin=?1 AND space=?2 AND status='live'",
            )?;
            st.query_map(params![m.origin, space], |r| r.get(0))?
                .collect::<Result<_, _>>()?
        };
        for target in old_facts {
            seq += 1;
            let e = ShareEvent {
                origin: m.origin.clone(),
                space: space.into(),
                seq,
                kind: EventKind::Retract,
                target: Some(target as u64),
                fact: None,
                claimed_agent: None,
            };
            e.validate()?;
            crate::sharing::apply_event_tx(&tx, &e, &e.digest(), &peer)?;
        }
        let first = seq + 1;
        let mut payloads: Vec<(String, FactKind, Value)> = facts
            .iter()
            .map(|f| {
                Ok((
                    opaque(
                        "team",
                        &format!("{}:{}:{}", space, slice.snapshot_id, f.entity_id),
                    ),
                    if matches!(f.content, Content::Link { .. }) {
                        FactKind::Link
                    } else {
                        FactKind::Fact
                    },
                    serde_json::to_value(f).map_err(|_| bad())?,
                ))
            })
            .collect::<ShareResult<_>>()?;
        payloads.push((
            opaque("slice", &slice.snapshot_id),
            FactKind::Fact,
            serde_json::to_value(&slice).map_err(|_| bad())?,
        ));
        for (fact_id, kind, payload) in payloads {
            seq += 1;
            let e = ShareEvent {
                origin: m.origin.clone(),
                space: space.into(),
                seq,
                kind: EventKind::Put,
                target: None,
                fact: Some(SharedFact {
                    fact_id,
                    kind,
                    space: space.into(),
                    payload,
                    claim: None,
                }),
                claimed_agent: None,
            };
            e.validate()?;
            crate::sharing::apply_event_tx(&tx, &e, &e.digest(), &peer)?;
        }
        let doc = serde_json::to_string(&slice).map_err(|_| bad())?;
        tx.execute("INSERT INTO team_generations VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(space) DO UPDATE SET policy_revision=excluded.policy_revision,fingerprint=excluded.fingerprint,first_seq=excluded.first_seq,last_seq=excluded.last_seq,document=excluded.document",params![space,m.revision as i64,fingerprint,first as i64,seq as i64,doc])?;
        let active_ids = facts
            .iter()
            .map(|f| f.entity_id.clone())
            .collect::<Vec<_>>();
        tx.execute("DELETE FROM team_observations WHERE space=?1 AND json_extract(document,'$.recorded_at')<?2 AND entity_id NOT IN (SELECT value FROM json_each(?3))",params![space,now-90*DAY,serde_json::to_string(&active_ids).map_err(|_|bad())?])?;
        tx.execute(
            "INSERT INTO team_history VALUES(?1,?2,?3,?4)",
            params![space, slice.snapshot_id, now, doc],
        )?;
        tx.execute(
            "DELETE FROM team_history WHERE generated_at < ?1",
            [now - 90 * DAY],
        )?;
        // Raw share event history older than retention is not re-served; prune
        // old generations only. The active generation retains its complete window.
        tx.execute("DELETE FROM events WHERE space=?1 AND origin=?2 AND seq<?3 AND CAST(substr(applied_at,6) AS INTEGER)<?4 AND applied_at LIKE 'unix:%'",params![space,m.origin,first as i64,now-90*DAY])?;
        tx.execute("DELETE FROM facts WHERE space=?1 AND origin=?2 AND status='retracted' AND CAST(substr(applied_at,6) AS INTEGER)<?3 AND applied_at LIKE 'unix:%'",params![space,m.origin,now-90*DAY])?;
        tx.commit()?;
        Ok(json!({"status":"ok","unchanged":false,"slice":slice,"first_seq":first,"last_seq":seq}))
    }
    pub fn team_audit(&self, audit: &Audit) -> ShareResult<()> {
        // Only schema fields; no question, answer, tool payload or credential value.
        if !id(&audit.request_id)
            || audit.completed_at < audit.received_at
            || audit.record_refs.len() > MAX_ENTITIES * 16
            || audit.record_refs.iter().any(|s| !id(s))
            || audit
                .machine
                .iter()
                .chain(&audit.agent)
                .chain(&audit.token_id)
                .chain(&audit.project_id)
                .chain(&audit.space)
                .any(|s| !id(s))
            || !["share_pull", "share_query", "team_ask", "team_pull"]
                .contains(&audit.operation.as_str())
            || !["ok", "denied", "error", "cancelled"].contains(&audit.outcome.as_str())
            || !["ok", "unavailable", "refused", "invalid", "pending"]
                .contains(&audit.reason.as_str())
        {
            return Err(bad());
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO team_audit(at,document) VALUES(?1,?2)",
            params![
                audit.received_at,
                serde_json::to_string(audit).map_err(|_| bad())?
            ],
        )?;
        tx.execute(
            "DELETE FROM team_replica_history WHERE generated_at<?1",
            [audit.completed_at - 90 * DAY],
        )?;
        tx.execute(
            "DELETE FROM team_replicas WHERE json_extract(document,'$.slice.generated_at')<?1",
            [audit.completed_at - 90 * DAY],
        )?;
        tx.execute(
            "DELETE FROM team_audit WHERE at<?1",
            [audit.completed_at - 365 * DAY],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn team_save_peer(&self, machine: &str, document: &Value) -> ShareResult<()> {
        if !id(machine) {
            return Err(bad());
        }
        let tx = self.conn.unchecked_transaction()?;
        let old: Option<String> = tx
            .query_row(
                "SELECT document FROM team_peers WHERE machine=?1",
                [machine],
                |r| r.get(0),
            )
            .optional()?;
        let document = document.to_string();
        if old.as_deref().is_some_and(|s| s != document) {
            tx.execute("DELETE FROM team_replicas WHERE machine=?1", [machine])?;
            tx.execute(
                "DELETE FROM team_replica_history WHERE machine=?1",
                [machine],
            )?;
        }
        tx.execute("INSERT INTO team_peers VALUES(?1,?2) ON CONFLICT(machine) DO UPDATE SET document=excluded.document",params![machine,document])?;
        tx.commit()?;
        Ok(())
    }
    /// Locally configured names only; endpoint documents stay behind team_peer.
    pub fn team_peer_names(&self) -> ShareResult<Vec<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT machine FROM team_peers ORDER BY machine")?;
        Ok(statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }
    pub fn team_peer(&self, machine: &str) -> ShareResult<Option<Value>> {
        let s: Option<String> = self
            .conn
            .query_row(
                "SELECT document FROM team_peers WHERE machine=?1",
                [machine],
                |r| r.get(0),
            )
            .optional()?;
        s.map(|s| serde_json::from_str(&s).map_err(|_| bad()))
            .transpose()
    }
    pub fn team_save_replica(&self, machine: &str, replica: &Replica) -> ShareResult<()> {
        replica.validate()?;
        if let Some(old) = self.team_replica(machine)?
            && (old.slice.origin != replica.slice.origin
                || old.slice.space != replica.slice.space
                || old.slice.project_id != replica.slice.project_id
                || old.slice.manifest_revision > replica.slice.manifest_revision
                || (old.slice.manifest_revision == replica.slice.manifest_revision
                    && old.slice.seq_last > replica.slice.seq_last))
        {
            return Err(bad());
        }
        if machine != replica.slice.machine_id {
            return Err(bad());
        }
        let tx = self.conn.unchecked_transaction()?;
        let document = serde_json::to_string(replica).map_err(|_| bad())?;
        tx.execute("INSERT INTO team_replicas VALUES(?1,?2,?3) ON CONFLICT(machine) DO UPDATE SET received_at=excluded.received_at,document=excluded.document",params![machine,replica.received_at,document])?;
        tx.execute(
            "INSERT OR IGNORE INTO team_replica_history VALUES(?1,?2,?3,?4,?5)",
            params![
                machine,
                replica.slice.snapshot_id,
                replica.slice.generated_at,
                replica.slice.manifest_revision as i64,
                document
            ],
        )?;
        tx.execute(
            "DELETE FROM team_replicas WHERE received_at<?1",
            [replica.received_at - 90 * DAY],
        )?;
        tx.execute(
            "DELETE FROM team_replica_history WHERE generated_at<?1",
            [replica.received_at - 90 * DAY],
        )?;
        // A changed policy closes all old generations on the coordinator too.
        tx.execute(
            "DELETE FROM team_replica_history WHERE machine=?1 AND revision<>?2",
            params![machine, replica.slice.manifest_revision as i64],
        )?;
        tx.commit()?;
        Ok(())
    }
    /// Only snapshots actually observed here, under the freshly verified policy.
    /// History before the first observation is unknown, never reconstructed.
    pub fn team_replica_as_of(
        &self,
        machine: &str,
        current: &Slice,
        recorded_at: i64,
    ) -> ShareResult<Option<Replica>> {
        let text:Option<String>=self.conn.query_row("SELECT document FROM team_replica_history WHERE machine=?1 AND revision=?2 AND generated_at<=?3 ORDER BY generated_at DESC,snapshot_id DESC LIMIT 1",params![machine,current.manifest_revision as i64,recorded_at],|r|r.get(0)).optional()?;
        text.map(|s| {
            let r: Replica = serde_json::from_str(&s).map_err(|_| bad())?;
            r.validate()?;
            if r.slice.origin != current.origin
                || r.slice.space != current.space
                || r.slice.project_id != current.project_id
            {
                return Err(bad());
            }
            Ok(r)
        })
        .transpose()
    }
    pub fn team_replica(&self, machine: &str) -> ShareResult<Option<Replica>> {
        let s: Option<String> = self
            .conn
            .query_row(
                "SELECT document FROM team_replicas WHERE machine=?1",
                [machine],
                |r| r.get(0),
            )
            .optional()?;
        s.map(|s| {
            let r: Replica = serde_json::from_str(&s).map_err(|_| bad())?;
            r.validate()?;
            Ok(r)
        })
        .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (ShareStore, std::path::PathBuf, Manifest) {
        let path = crate::test_support::temp_path("team-policy");
        let mut s = ShareStore::open(&path).unwrap();
        let m = Manifest {
            schema: "brain-team-manifest/1".into(),
            revision: 1,
            approved_at: 10,
            enabled: false,
            origin: s.install_id().into(),
            machine_id: "7".into(),
            project_id: "proj".into(),
            project_root: "/synthetic/project".into(),
            source_space: "default".into(),
            space: "team.proj".into(),
            recipients: vec![Recipient {
                machine: "boss".into(),
                agent: "lead".into(),
            }],
            classes: CLASSES.iter().map(|s| (*s).into()).collect(),
            disabled_fields: BTreeSet::new(),
            annotation_policy: "automatic-masked".into(),
            integrity: "tls+blake3-unsigned".into(),
            snapshot_retention_days: 90,
            audit_retention_days: 365,
            stale_after_seconds: DAY,
        };
        let m = s.team_configure(m).unwrap();
        s.enable_space(&m.space).unwrap();
        (s, path, m)
    }
    fn publish(s: &mut ShareStore, m: &Manifest, now: i64) -> Value {
        let mut f = super::super::tests::fact();
        f.origin = s.install_id().into();
        s.team_publish(
            &m.space,
            vec![f],
            Watermarks {
                distill: Some(20),
                work: None,
                git: None,
            },
            0,
            now,
            now,
        )
        .unwrap()
    }
    #[test]
    fn local_optout_and_class_revocation_close_old_history() {
        let (mut s, path, m) = setup();
        let now = chrono::Utc::now().timestamp();
        let reader = Principal::new("boss", "lead").with_read_grant(&m.space);
        assert!(s.authorize_read(&reader, &m.space).is_err());
        s.team_toggle(&m.space, None, true, now).unwrap();
        let first = publish(&mut s, &m, now);
        assert_eq!(first["unchanged"], false);
        assert!(s.page_own(&reader, &m.space, 0, 10).is_ok());
        let duplicate = publish(&mut s, &m, now + 1);
        assert_eq!(duplicate["unchanged"], true);
        assert_eq!(
            first["slice"]["snapshot_id"],
            duplicate["slice"]["snapshot_id"]
        );
        let raw = SharedFact {
            fact_id: "raw".into(),
            kind: FactKind::Fact,
            space: m.space.clone(),
            payload: json!({"raw":"trap"}),
            claim: None,
        };
        assert!(s.publish(&m.space, raw, None).is_err());
        s.team_toggle(&m.space, Some("error"), false, now + 2)
            .unwrap();
        assert!(s.page_own(&reader, &m.space, 0, 10).is_err());
        s.team_publish(
            &m.space,
            vec![],
            Watermarks {
                distill: None,
                work: None,
                git: None,
            },
            0,
            now + 3,
            now + 3,
        )
        .unwrap();
        let page = s.page_own(&reader, &m.space, 0, 10).unwrap();
        assert_eq!(page.events.len(), 1);
        assert!(page.events[0].seq > 1);
        assert!(!page.events[0].encode().contains("Build issue"));
        let outsider = Principal::new("other", "lead").with_read_grant(&m.space);
        assert!(s.page_own(&outsider, &m.space, 0, 10).is_err());
        s.team_toggle(&m.space, None, false, now + 4).unwrap();
        s.enable_space(&m.space).unwrap();
        assert!(s.page_own(&reader, &m.space, 0, 10).is_err());
        drop(s);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn work_first_observation_is_stable_without_inventing_earlier_state() {
        let (mut s, path, m) = setup();
        let now = chrono::Utc::now().timestamp();
        s.team_toggle(&m.space, None, true, now).unwrap();
        let mut f = super::super::tests::fact();
        f.origin = s.install_id().into();
        f.source_kind = "work".into();
        f.trust = "coordinator_state".into();
        f.valid_from = now;
        f.recorded_at = now;
        f.content = Content::Task {
            title_summary: "Observed task".into(),
            state: "queued".into(),
            created_at: 10,
            updated_at: 20,
            owner_agent: None,
            owner_machine: None,
            lease_expires_at: None,
            dependency_refs: vec![],
        };
        let watermarks = Watermarks {
            distill: None,
            work: Some(20),
            git: None,
        };
        let first = s
            .team_publish(&m.space, vec![f.clone()], watermarks.clone(), 0, now, now)
            .unwrap();
        f.valid_from = now + 1;
        f.recorded_at = now + 1;
        let next = s
            .team_publish(&m.space, vec![f], watermarks, 0, now + 1, now + 1)
            .unwrap();
        assert_eq!(first["slice"]["snapshot_id"], next["slice"]["snapshot_id"]);
        assert_eq!(next["unchanged"], true);
        let text: String = s
            .conn
            .query_row(
                "SELECT document FROM team_observations WHERE space=?1",
                [&m.space],
                |r| r.get(0),
            )
            .unwrap();
        let observed: Fact = serde_json::from_str(&text).unwrap();
        assert_eq!(observed.valid_from, now);
        assert_eq!(observed.recorded_at, now);
        drop(s);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn publication_failure_is_atomic_and_audit_is_structured() {
        let (mut s, path, m) = setup();
        let now = chrono::Utc::now().timestamp();
        s.team_toggle(&m.space, None, true, now).unwrap();
        publish(&mut s, &m, now);
        let counts = s.counts(&m.space).unwrap().own_events;
        let mut f = super::super::tests::fact();
        f.origin = s.install_id().into();
        f.space = m.space.clone();
        f.content = Content::Link {
            from_ref: "missing".into(),
            to_ref: "also-missing".into(),
            relation: "fixed_by".into(),
            evidence_refs: vec![],
        };
        f.source_kind = "distill".into();
        assert!(
            s.team_publish(
                &m.space,
                vec![f],
                Watermarks {
                    distill: None,
                    work: None,
                    git: None
                },
                0,
                now,
                now
            )
            .is_err()
        );
        assert_eq!(s.counts(&m.space).unwrap().own_events, counts);
        let a = Audit {
            request_id: "req-one".into(),
            received_at: now,
            completed_at: now,
            machine: None,
            agent: None,
            token_id: None,
            project_id: None,
            space: None,
            operation: "team_ask".into(),
            valid_at: None,
            recorded_at: None,
            policy_revision: None,
            snapshot_id: None,
            record_refs: vec![],
            count: 0,
            outcome: "denied".into(),
            latency_ms: 0,
            reason: "refused".into(),
        };
        s.team_audit(&a).unwrap();
        let count: i64 = s
            .conn
            .query_row("SELECT count(*) FROM team_audit", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        s.conn.execute("CREATE TRIGGER no_audit BEFORE INSERT ON team_audit BEGIN SELECT RAISE(ABORT,'blocked'); END",[]).unwrap();
        assert!(s.team_audit(&a).is_err());
        drop(s);
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;
    #[test]
    fn verified_history_is_bounded_to_current_policy_and_replay_is_denied() {
        let path = crate::test_support::temp_path("team-replica-history");
        let store = ShareStore::open(&path).unwrap();
        let now = chrono::Utc::now().timestamp();
        let mut f = super::super::tests::fact();
        let mut slice = Slice {
            schema: SLICE_SCHEMA.into(),
            manifest_revision: 1,
            origin: f.origin.clone(),
            machine_id: f.machine_id.clone(),
            space: f.space.clone(),
            project_id: f.project_id.clone(),
            snapshot_id: String::new(),
            generated_at: now - 2 * DAY,
            collected_from: now - 2 * DAY,
            seq_first: 1,
            seq_last: 2,
            source_watermarks: Watermarks {
                distill: Some(20),
                work: None,
                git: None,
            },
            coverage: "bounded".into(),
            rejected: 0,
            entity_digests: vec![EntityDigest {
                entity_id: f.entity_id.clone(),
                revision: 1,
                digest: digest(&f).unwrap(),
            }],
            integrity: "tls+blake3-unsigned".into(),
            signature: None,
        };
        slice.seal().unwrap();
        let old = Replica {
            slice,
            facts: vec![f.clone()],
            received_from_peer: "7".into(),
            received_at: now - 2 * DAY,
        };
        store.team_save_replica("7", &old).unwrap();
        assert!(now - old.slice.generated_at > DAY); // Valid but stale, not an empty result.
        f.revision = 2;
        f.recorded_at = now - 10;
        let mut current = old.clone();
        current.facts = vec![f.clone()];
        current.slice.generated_at = now;
        current.slice.collected_from = now;
        current.slice.seq_first = 5;
        current.slice.seq_last = 6;
        current.slice.entity_digests = vec![EntityDigest {
            entity_id: f.entity_id.clone(),
            revision: 2,
            digest: digest(&f).unwrap(),
        }];
        current.slice.seal().unwrap();
        current.received_at = now;
        store.team_save_replica("7", &current).unwrap();
        assert!(store.team_save_replica("7", &old).is_err());
        assert_eq!(
            store
                .team_replica_as_of("7", &current.slice, now - DAY)
                .unwrap()
                .unwrap()
                .slice
                .snapshot_id,
            old.slice.snapshot_id
        );
        assert!(
            store
                .team_replica_as_of("7", &current.slice, now - 3 * DAY)
                .unwrap()
                .is_none()
        );
        current.slice.manifest_revision = 2;
        current.slice.seal().unwrap();
        store.team_save_replica("7", &current).unwrap();
        assert!(
            store
                .team_replica_as_of("7", &current.slice, now - DAY)
                .unwrap()
                .is_none()
        );
        store
            .team_save_peer("7", &json!({"binding":"old"}))
            .unwrap();
        store
            .team_save_peer("7", &json!({"binding":"new"}))
            .unwrap();
        assert!(store.team_replica("7").unwrap().is_none());
        drop(store);
        let _ = std::fs::remove_file(path);
    }
}
