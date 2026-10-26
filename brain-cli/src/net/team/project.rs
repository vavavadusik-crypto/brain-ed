//! Read-only, bounded source adapters. Never select node body, archive rows,
//! work detail/note/data, Git author, remote, diff, or unstructured tool output.
use crate::RuntimeConfig;
use brain_core::coordination::Coordinator;
use brain_core::sharing::ShareResult;
use brain_core::team::{self, Content, FACT_SCHEMA, Fact, MAX_ENTITIES, Manifest, Watermarks};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

fn reference(m: &Manifest, kind: &str, raw: &str) -> String {
    team::opaque(kind, &format!("{}:{}:{}", m.origin, m.source_space, raw))
}
fn base(m: &Manifest, kind: &str, raw: &str, content: Content, valid: i64, recorded: i64) -> Fact {
    Fact {
        schema: FACT_SCHEMA.into(),
        entity_id: reference(m, kind, raw),
        revision: 1,
        origin: m.origin.clone(),
        machine_id: m.machine_id.clone(),
        space: m.space.clone(),
        project_id: m.project_id.clone(),
        session_ref: None,
        session_absence: Some("not_applicable".into()),
        agent_ref: None,
        model_ref: None,
        model_evidence: "unknown".into(),
        source_kind: kind.into(),
        source_ref: reference(m, "source", &format!("{kind}:{raw}")),
        trust: match kind {
            "work" => "coordinator_state",
            "git" => "git_object",
            "derived" => "derived_from_allowed_facts",
            _ => "untrusted_model_extraction",
        }
        .into(),
        valid_from: valid,
        valid_to: None,
        recorded_at: recorded,
        superseded_at: None,
        coverage: "bounded".into(),
        content,
    }
}
struct Collector<'a> {
    m: &'a Manifest,
    now: i64,
    facts: Vec<Fact>,
    rejected: u32,
    watermarks: Watermarks,
}
impl Collector<'_> {
    fn add(&mut self, f: Fact) -> bool {
        if !self.m.classes.contains(f.content.class()) {
            return false;
        }
        let Ok(f) = team::project_fields(&f, &self.m.disabled_fields) else {
            self.rejected += 1;
            return false;
        };
        if self.facts.len() >= MAX_ENTITIES || f.validate().is_err() {
            self.rejected += 1;
            return false;
        }
        self.facts.push(f);
        true
    }
    fn distill(&mut self, path: &Path) -> ShareResult<()> {
        if !path.exists() {
            return Ok(());
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let mut st=conn.prepare("SELECT node_id, revision, metadata, valid_from, valid_to, recorded_at, superseded_at FROM node_versions WHERE space_id=?1 AND json_extract(metadata,'$.distilled_fact')=1 AND superseded_at IS NULL AND valid_from<=?2 AND (valid_to IS NULL OR valid_to>?2) AND length(metadata)<=32768 ORDER BY node_id LIMIT 101")?;
        let rows = st.query_map(params![self.m.source_space, self.now], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Option<i64>>(6)?,
            ))
        })?;
        let mut nodes = BTreeMap::new();
        for (n, row) in rows.enumerate() {
            if n == 100 {
                self.rejected += 1;
                break;
            }
            let (raw, rev, meta, valid, to, recorded, superseded) = row?;
            let v: Value = serde_json::from_str(&meta).map_err(|_| team::bad())?;
            let project = v["project"].as_str().unwrap_or("");
            // Distill's project is evidence cwd. Resolve locally; never export it.
            let p = Path::new(project);
            if !p.is_absolute() {
                continue;
            }
            let Ok(p) = p.canonicalize() else {
                self.rejected += 1;
                continue;
            };
            if !p.starts_with(&self.m.project_root) {
                continue;
            }
            let kind = v["kind"].as_str().unwrap_or("");
            if !self.m.classes.contains(kind) {
                continue;
            }
            let (Ok(what), Ok(why)) = (
                team::annotation(v["what"].as_str().unwrap_or("")),
                team::annotation(v["why"].as_str().unwrap_or("")),
            ) else {
                self.rejected += 1;
                continue;
            };
            let status = v["status"].as_str().unwrap_or("").to_string();
            let evidence = v["evidence"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_i64)
                        .take(24)
                        .map(|i| reference(self.m, "evidence", &i.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if evidence.is_empty() {
                self.rejected += 1;
                continue;
            }
            let content = match kind {
                "plan" => Content::Plan {
                    what_summary: what,
                    why_summary: why,
                    status,
                    task_refs: vec![],
                },
                "error" => Content::Error {
                    what_summary: what,
                    why_summary: why.clone(),
                    status,
                    reason_summary: why,
                    evidence_refs: evidence,
                    transition_refs: vec![],
                },
                "decision" => Content::Decision {
                    what_summary: what,
                    why_summary: why,
                    status,
                    evidence_refs: evidence,
                    error_refs: vec![],
                    error_links: "unknown".into(),
                },
                "dead_end" => Content::DeadEnd {
                    what_summary: what,
                    why_summary: why,
                    status,
                },
                "finding" => Content::Finding {
                    what_summary: what,
                    why_summary: why,
                    status,
                },
                _ => {
                    self.rejected += 1;
                    continue;
                }
            };
            let mut f = base(self.m, "distill", &raw, content, valid, recorded);
            f.revision = rev as u64;
            f.valid_to = to;
            f.superseded_at = superseded;
            let Some(session) = v["archive_session"].as_str().filter(|s| !s.is_empty()) else {
                self.rejected += 1;
                continue;
            };
            f.session_ref = Some(reference(self.m, "session", session));
            f.session_absence = None;
            if self.add(f.clone()) {
                nodes.insert(raw, f.entity_id);
                self.watermarks.distill = Some(self.watermarks.distill.unwrap_or(0).max(recorded));
            }
        }
        if self.m.classes.contains("link") {
            let mut st=conn.prepare("SELECT e.id,e.from_node_id,e.to_node_id,v.relation,v.valid_from,v.recorded_at FROM edges e JOIN edge_versions v ON e.space_id=v.space_id AND e.id=v.edge_id WHERE e.space_id=?1 AND v.superseded_at IS NULL AND v.valid_from<=?2 AND (v.valid_to IS NULL OR v.valid_to>?2) AND json_extract(v.metadata,'$.distilled')=1 ORDER BY e.id LIMIT 101")?;
            let rows = st.query_map(params![self.m.source_space, self.now], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            })?;
            for row in rows {
                let (raw, from, to, relation, valid, recorded) = row?;
                if let (Some(from), Some(to)) = (nodes.get(&from), nodes.get(&to)) {
                    self.add(base(
                        self.m,
                        "distill",
                        &raw,
                        Content::Link {
                            from_ref: from.clone(),
                            to_ref: to.clone(),
                            relation,
                            evidence_refs: vec![],
                        },
                        valid,
                        recorded,
                    ));
                }
            }
        }
        Ok(())
    }
    fn work(&mut self, path: &Path) -> ShareResult<()> {
        if !path.exists() {
            return Ok(());
        }
        let coord = Coordinator::open_read_only(path).map_err(|_| team::bad())?;
        let board = coord
            .board(
                &self.m.source_space,
                Some(&self.m.project_id),
                None,
                false,
                0,
                100,
            )
            .map_err(|_| team::bad())?;
        if board["has_more"] == true {
            self.rejected += 1;
        }
        let tasks = board["tasks"].as_array().ok_or_else(team::bad)?;
        let mut admitted = BTreeMap::new();
        let mut agents = BTreeMap::new();
        for t in tasks {
            let raw = t["id"].as_i64().ok_or_else(team::bad)?;
            let Ok(title) = team::annotation(t["title"].as_str().unwrap_or("")) else {
                self.rejected += 1;
                continue;
            };
            let created = t["created_at"].as_i64().ok_or_else(team::bad)?;
            let updated = t["updated_at"].as_i64().ok_or_else(team::bad)?;
            let owner = &t["owner"];
            let mut f = base(
                self.m,
                "work",
                &raw.to_string(),
                Content::Task {
                    title_summary: title,
                    state: t["state"].as_str().unwrap_or("").into(),
                    created_at: created,
                    updated_at: updated,
                    owner_agent: owner["agent"]
                        .as_str()
                        .map(|s| reference(self.m, "agent", s)),
                    owner_machine: owner["machine"]
                        .as_str()
                        .map(|s| reference(self.m, "machine", s)),
                    lease_expires_at: owner["expires_at"].as_i64(),
                    dependency_refs: vec![],
                },
                self.now,
                self.now,
            );
            f.agent_ref = t["created_by"]["agent"]
                .as_str()
                .map(|s| reference(self.m, "agent", s));
            if self.add(f.clone()) {
                admitted.insert(raw, f.entity_id.clone());
                if let Some(a) = f.agent_ref {
                    agents.entry(a).or_insert_with(Vec::new).push(f.entity_id);
                }
            }
            self.watermarks.work = Some(self.watermarks.work.unwrap_or(0).max(updated));
        }
        // Only dependency IDs are read. Work show (which includes raw notes) is never used.
        if self.m.classes.contains("link") {
            let conn = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?;
            let mut st=conn.prepare("SELECT d.task_id,d.depends_on FROM task_deps d JOIN tasks t ON t.id=d.task_id WHERE t.space=?1 AND t.project=?2 ORDER BY d.task_id,d.depends_on LIMIT 101")?;
            for row in st.query_map(params![self.m.source_space, self.m.project_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })? {
                let (a, b) = row?;
                if let (Some(from), Some(to)) = (admitted.get(&a), admitted.get(&b)) {
                    let from = from.clone();
                    let to = to.clone();
                    if let Some(f) = self.facts.iter_mut().find(|f| f.entity_id == from)
                        && let Content::Task {
                            dependency_refs, ..
                        } = &mut f.content
                    {
                        dependency_refs.push(to.clone());
                    }
                    self.add(base(
                        self.m,
                        "work",
                        &format!("dep-{a}-{b}"),
                        Content::Link {
                            from_ref: from,
                            to_ref: to,
                            relation: "depends_on".into(),
                            evidence_refs: vec![],
                        },
                        self.now,
                        self.now,
                    ));
                }
            }
        }
        for (a, task_refs) in agents {
            let mut f = base(
                self.m,
                "work",
                &format!("agent-{a}"),
                Content::Participation {
                    client_kind: "unknown".into(),
                    started_at: None,
                    ended_at: None,
                    task_refs,
                },
                self.now,
                self.now,
            );
            f.agent_ref = Some(a);
            self.add(f);
        }
        Ok(())
    }
    fn git(&mut self) -> ShareResult<()> {
        if !self.m.classes.contains("commit") {
            return Ok(());
        }
        let root = Path::new(&self.m.project_root);
        let top = git(root, &["rev-parse", "--show-toplevel"])?;
        if Path::new(top.trim())
            .canonicalize()
            .map_err(|_| team::bad())?
            != root
        {
            return Err(team::bad());
        }
        let log = git(
            root,
            &[
                "log",
                "--no-show-signature",
                "-8",
                "--format=%H%x00%P%x00%ct%x00%s",
            ],
        )?;
        for line in log.lines() {
            let parts = line.splitn(4, '\0').collect::<Vec<_>>();
            if parts.len() != 4 {
                self.rejected += 1;
                continue;
            }
            let Ok(time) = parts[2].parse::<i64>() else {
                self.rejected += 1;
                continue;
            };
            let Ok(subject) = team::annotation(parts[3]) else {
                self.rejected += 1;
                continue;
            };
            let parents = parts[1]
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>();
            self.add(base(
                self.m,
                "git",
                parts[0],
                Content::Commit {
                    oid: parts[0].into(),
                    is_merge: parents.len() > 1,
                    parent_oids: parents,
                    committed_at: time,
                    subject_summary: subject,
                    task_refs: vec![],
                },
                time,
                self.now,
            ));
            self.watermarks.git = Some(self.watermarks.git.unwrap_or(0).max(time));
        }
        Ok(())
    }
    fn sessions(&mut self) {
        let mut groups: BTreeMap<String, Vec<Fact>> = BTreeMap::new();
        for f in &self.facts {
            if let Some(s) = &f.session_ref {
                groups.entry(s.clone()).or_default().push(f.clone());
            }
        }
        for (session, facts) in groups {
            let first = facts.iter().map(|f| f.valid_from).min().unwrap_or(0);
            let last = facts.iter().map(|f| f.valid_from).max().unwrap_or(first);
            // Summaries contain only already-admitted annotations. No raw archive read.
            let text = facts
                .iter()
                .map(|f| f.content.text())
                .collect::<Vec<_>>()
                .join("; ");
            let summary = text.chars().take(350).collect::<String>();
            let mut f = base(
                self.m,
                "derived",
                &session,
                Content::Session {
                    client_kind: "unknown".into(),
                    first_observed_at: first,
                    last_observed_at: last,
                    summary,
                    fact_refs: facts.iter().map(|f| f.entity_id.clone()).collect(),
                    task_refs: vec![],
                },
                first,
                self.now,
            );
            f.session_ref = Some(session);
            f.session_absence = None;
            self.add(f);
        }
    }
}
fn git(root: &Path, args: &[&str]) -> ShareResult<String> {
    let mut child = Command::new("git")
        .arg("--no-pager")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| team::bad())?;
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .ok_or_else(team::bad)?
        .take(32769)
        .read_to_end(&mut bytes)
        .map_err(|_| team::bad())?;
    if bytes.len() > 32768 {
        let _ = child.kill();
        let _ = child.wait();
        return Err(team::bad());
    }
    if !child.wait().map_err(|_| team::bad())?.success() {
        return Err(team::bad());
    }
    String::from_utf8(bytes).map_err(|_| team::bad())
}
pub(super) fn collect(
    config: &RuntimeConfig,
    m: &Manifest,
    now: i64,
) -> ShareResult<(Vec<Fact>, Watermarks, u32)> {
    let root = Path::new(&m.project_root)
        .canonicalize()
        .map_err(|_| team::bad())?;
    if root != Path::new(&m.project_root) || !root.is_dir() {
        return Err(team::bad());
    }
    let mut c = Collector {
        m,
        now,
        facts: vec![],
        rejected: 0,
        watermarks: Watermarks {
            distill: None,
            work: None,
            git: None,
        },
    };
    c.distill(&config.database)?;
    c.work(&brain_core::coordination::database_path(
        &crate::brain_home(),
    ))?;
    // A non-Git project simply has an unknown Git watermark, marked incomplete.
    if c.git().is_err() {
        c.rejected += 1;
    }
    c.sessions();
    // Reapply field policy after dependency assembly, then reach a fixed point
    // of closed references. A dropped target cannot leave a dangling summary.
    c.facts = c
        .facts
        .into_iter()
        .filter_map(|f| match team::project_fields(&f, &m.disabled_fields) {
            Ok(f) => Some(f),
            Err(_) => {
                c.rejected += 1;
                None
            }
        })
        .collect();
    loop {
        let ids = c
            .facts
            .iter()
            .map(|f| f.entity_id.clone())
            .collect::<BTreeSet<_>>();
        let before = c.facts.len();
        c.facts
            .retain(|f| f.content.refs().iter().all(|r| ids.contains(*r)));
        c.rejected += (before - c.facts.len()) as u32;
        if before == c.facts.len() {
            break;
        }
    }
    Ok((c.facts, c.watermarks, c.rejected))
}
