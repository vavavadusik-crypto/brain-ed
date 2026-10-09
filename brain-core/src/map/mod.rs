//! The work map (M1): a private append-only ledger of `brain-event/1`
//! assertions in its own `brain-map/1` SQLite file, plus a `links` projection
//! that is a pure function of the ledger and can be rebuilt byte for byte.
//!
//! It never touches the memory graph, the conversation archive or the
//! coordinator. Conflicting assertions are all kept; nothing is decided by
//! arrival order or clocks. Each relation has a fixed cardinality from the
//! built-in `brain-relations/1` table ([`events::RELATIONS`]): associations
//! such as `has_session` hold many objects, values such as `current_status`
//! hold one. Confirm, reject and resolve need [`Authority::LocalUser`], which
//! the trusted host vouches for; it is a contract, not proof of identity.
//! [`ingest`] reads explicit coordinator and (opt-in) archive-metadata sources.
//! See `docs/memory-map.md`. Not wired into any CLI, MCP or UI yet.

pub mod events;
pub mod ingest;
pub mod packet;
pub mod store;

use std::fmt;

pub use events::{
    Authority, Cardinality, Event, EventDraft, LocalUserGrant, RELATION_POLICY, RELATIONS,
};
pub use ingest::{ArchiveSource, IngestBudget, IngestReport, SourceKind, TimelineEntry};
pub use packet::{
    MemoryKind, MemoryScope, MemorySnippet, OpenTask, Packet, PacketInput, TaskProject, Tier,
    context_packet_unmapped,
};
pub use store::{LinkRow, MapStatus, MapStore, Replayed, map_status};

/// Stable word an agent can branch on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MapErrorKind {
    InvalidInput,
    Forbidden,
    NotFound,
    Conflict,
    Incompatible,
    Unavailable,
}

impl MapErrorKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::Incompatible => "incompatible",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug)]
pub struct MapError {
    pub kind: MapErrorKind,
    pub message: String,
}

impl MapError {
    pub fn new(kind: MapErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(MapErrorKind::InvalidInput, message)
    }
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.message)
    }
}

impl std::error::Error for MapError {}

impl From<rusqlite::Error> for MapError {
    fn from(error: rusqlite::Error) -> Self {
        Self::new(MapErrorKind::Unavailable, error.to_string())
    }
}

impl From<std::io::Error> for MapError {
    fn from(error: std::io::Error) -> Self {
        Self::new(MapErrorKind::Unavailable, error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    use rusqlite::{Connection, params};

    use super::*;

    const A: &str = "m-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "m-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "m-cccccccccccccccccccccccccccccccc";
    const SUBJ: &str = "task:sp/7";

    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct Fx {
        dir: Dir,
        clock: Arc<AtomicI64>,
    }

    fn fx(tag: &str) -> Fx {
        let dir = std::env::temp_dir().join(format!("brain-map-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Fx {
            dir: Dir(dir),
            clock: Arc::new(AtomicI64::new(1_000)),
        }
    }

    impl Fx {
        fn path(&self, name: &str) -> PathBuf {
            self.dir.0.join(name).join("map.sqlite")
        }
        fn open(&self, name: &str, machine: &str) -> MapStore {
            let clock = Arc::clone(&self.clock);
            MapStore::open(&self.path(name), machine)
                .expect("open")
                .with_clock(move || clock.load(Ordering::SeqCst))
        }
        fn set(&self, t: i64) {
            self.clock.store(t, Ordering::SeqCst);
        }
    }

    /// The relation a test object belongs to, by its prefix.
    fn relation_for(object: &str) -> &'static str {
        match object.split_once(':').map_or("", |(p, _)| p) {
            "status" => "current_status",
            "task" => "depends_on",
            "memory" => "has_decision",
            _ => "has_session",
        }
    }

    fn draft(kind: &str, object: &str, agent: &str) -> EventDraft {
        EventDraft {
            kind: kind.into(),
            space: "sp".into(),
            subject: SUBJ.into(),
            relation: relation_for(object).into(),
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

    fn as_user(d: EventDraft) -> EventDraft {
        EventDraft {
            basis: "user_confirmed".into(),
            asserted_kind: "user".into(),
            confidence_milli: 1000,
            ..d
        }
    }

    fn confirm(object: &str) -> EventDraft {
        as_user(draft("link_confirmed", object, "cli"))
    }

    fn reject(object: &str) -> EventDraft {
        as_user(draft("link_rejected", object, "cli"))
    }

    fn correct(target: &Event, object: &str) -> EventDraft {
        EventDraft {
            corrects: Some(target.event_id()),
            relation: target.draft.relation.clone(),
            ..draft("correction", object, &target.draft.agent)
        }
    }

    fn user() -> Authority {
        Authority::LocalUser(LocalUserGrant::from_direct_local_action())
    }

    fn rows(s: &MapStore) -> Vec<LinkRow> {
        s.links_page("sp", None, 100).unwrap().0
    }

    fn statuses(s: &MapStore) -> Vec<(String, String)> {
        rows(s).into_iter().map(|r| (r.object, r.status)).collect()
    }

    fn row(s: &MapStore, object: &str) -> LinkRow {
        rows(s)
            .into_iter()
            .find(|r| r.object == object)
            .expect("row")
    }

    fn kind<T: fmt::Debug>(r: Result<T, MapError>) -> MapErrorKind {
        r.expect_err("expected error").kind
    }

    #[test]
    fn ledger_rejects_update_and_delete_and_files_are_private() {
        let f = fx("append");
        let mut s = f.open("a", A);
        s.append(
            draft("link_proposed", "archive-session:codex/1", "codex"),
            Authority::Unverified,
        )
        .unwrap();
        let c = s.connection();
        assert!(c.execute("UPDATE events SET object = 'x'", []).is_err());
        assert!(c.execute("DELETE FROM events", []).is_err());
        assert_eq!(
            c.query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        let path = f.path("a");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let wal = PathBuf::from(format!("{}-wal", path.display()));
        assert_eq!(
            std::fs::metadata(&wal).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(s);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(kind(MapStore::open(&path, A)), MapErrorKind::Incompatible);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(
            path.parent().unwrap(),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert_eq!(kind(MapStore::open(&path, A)), MapErrorKind::Incompatible);
    }

    #[test]
    fn symlinked_file_or_directory_is_refused() {
        let f = fx("symlink");
        drop(f.open("real", A));
        let real = f.path("real");
        let link_dir = f.dir.0.join("linkfile");
        std::fs::DirBuilder::new().mode_private(&link_dir);
        std::os::unix::fs::symlink(&real, link_dir.join("map.sqlite")).unwrap();
        assert_eq!(
            kind(MapStore::open(&link_dir.join("map.sqlite"), A)),
            MapErrorKind::Incompatible
        );
        let dangling = f.dir.0.join("dangling");
        std::fs::DirBuilder::new().mode_private(&dangling);
        std::os::unix::fs::symlink(f.dir.0.join("nowhere"), dangling.join("map.sqlite")).unwrap();
        assert_eq!(
            kind(MapStore::open(&dangling.join("map.sqlite"), A)),
            MapErrorKind::Incompatible
        );
        assert!(
            !f.dir.0.join("nowhere").exists(),
            "a dangling link must not be followed into a new file"
        );
        std::os::unix::fs::symlink(real.parent().unwrap(), f.dir.0.join("linkdir")).unwrap();
        assert_eq!(
            kind(MapStore::open(
                &f.dir.0.join("linkdir").join("map.sqlite"),
                A
            )),
            MapErrorKind::Incompatible
        );
    }

    trait PrivateDir {
        fn mode_private(&mut self, path: &std::path::Path);
    }
    impl PrivateDir for std::fs::DirBuilder {
        fn mode_private(&mut self, path: &std::path::Path) {
            use std::os::unix::fs::DirBuilderExt;
            self.recursive(true).mode(0o700).create(path).unwrap();
        }
    }

    #[test]
    fn seq_ids_are_stable_and_newer_or_foreign_files_are_refused() {
        let f = fx("seq");
        let mut s = f.open("a", A);
        let e1 = s
            .append(
                draft("link_proposed", "archive-session:codex/1", "codex"),
                Authority::Unverified,
            )
            .unwrap();
        let e2 = s
            .append(draft("outcome_claimed", "", "codex"), Authority::Unverified)
            .unwrap();
        assert_eq!(e1.event_id(), format!("{A}:000000000001"));
        assert_eq!(e2.event_id(), format!("{A}:000000000002"));
        drop(s);
        assert_eq!(
            kind(MapStore::open(&f.path("a"), B)),
            MapErrorKind::Incompatible
        );
        Connection::open(f.path("a"))
            .unwrap()
            .pragma_update(None, "user_version", 2)
            .unwrap();
        assert_eq!(
            kind(MapStore::open(&f.path("a"), A)),
            MapErrorKind::Incompatible
        );
        // A file written under another relation policy is not reinterpreted.
        drop(f.open("p", A));
        Connection::open(f.path("p"))
            .unwrap()
            .execute(
                "UPDATE meta SET value = 'brain-relations/2' WHERE key = 'relations'",
                [],
            )
            .unwrap();
        assert_eq!(
            kind(MapStore::open(&f.path("p"), A)),
            MapErrorKind::Incompatible
        );
        let mut bad = e1.clone();
        bad.v = 2;
        let mut t = f.open("b", B);
        assert_eq!(kind(t.replay(&bad)), MapErrorKind::Incompatible);
    }

    #[test]
    fn replay_is_idempotent_ordered_and_refuses_tampering() {
        let f = fx("replay");
        let mut a = f.open("a", A);
        let e1 = a
            .append(
                draft("link_proposed", "archive-session:codex/1", "codex"),
                Authority::Unverified,
            )
            .unwrap();
        let e2 = a
            .append(
                correct(&e1, "archive-session:codex/2"),
                Authority::Unverified,
            )
            .unwrap();
        let mut b = f.open("b", B);
        assert_eq!(
            kind(b.replay(&e2)),
            MapErrorKind::Conflict,
            "gap in origin seq"
        );
        assert_eq!(b.replay(&e1).unwrap(), Replayed::Inserted);
        assert_eq!(b.replay(&e1).unwrap(), Replayed::Duplicate);
        let mut forged = e2.clone();
        forged.draft.object = "archive-session:codex/9".into();
        assert_eq!(
            kind(b.replay(&forged)),
            MapErrorKind::Conflict,
            "hash mismatch"
        );
        for hash in ["", "zz", &e2.content_hash.to_uppercase()] {
            let mut h = e2.clone();
            h.content_hash = hash.to_owned();
            assert_eq!(kind(b.replay(&h)), MapErrorKind::Conflict, "{hash}");
        }
        for seq in [0, events::MAX_SEQ + 1, -1] {
            let mut h = e2.clone();
            h.origin_seq = seq;
            h.content_hash = h.compute_hash();
            assert_eq!(kind(b.replay(&h)), MapErrorKind::InvalidInput, "{seq}");
        }
        forged.content_hash = forged.compute_hash();
        assert_eq!(b.replay(&forged).unwrap(), Replayed::Inserted);
        // Same id, other (validly hashed) content: refused, not overwritten.
        assert_eq!(kind(b.replay(&e2)), MapErrorKind::Conflict);
        // Own-origin event this store never wrote cannot be imported.
        let mut own = e1.clone();
        own.origin_machine = B.into();
        own.content_hash = own.compute_hash();
        assert_eq!(kind(b.replay(&own)), MapErrorKind::Conflict);
        // An unknown relation is refused on replay as on append.
        let mut odd = a
            .append(
                draft("link_proposed", "archive-session:codex/3", "codex"),
                Authority::Unverified,
            )
            .unwrap();
        odd.draft.relation = "session".into();
        odd.content_hash = odd.compute_hash();
        assert_eq!(kind(b.replay(&odd)), MapErrorKind::InvalidInput);
    }

    #[test]
    fn replay_checks_authority_before_calling_a_copy_duplicate() {
        let f = fx("dupauth");
        let mut a = f.open("a", A);
        let mut forged = a
            .append(
                draft("link_proposed", "archive-session:codex/1", "codex"),
                Authority::Unverified,
            )
            .unwrap();
        forged.draft = EventDraft {
            payload: "{}".into(),
            ..confirm("archive-session:codex/1")
        };
        forged.content_hash = forged.compute_hash();
        // Smuggle the forged record into B's ledger past the API, then replay the identical copy.
        let mut b = f.open("b", B);
        let d = &forged.draft;
        b.connection()
            .execute(
                "INSERT INTO events(event_id, v, origin_machine, origin_seq, authority, kind, space, subject, relation, \
                 object, basis, asserted_kind, client, agent, evidence, occurred_src, recorded_at, received_at, \
                 confidence_milli, payload, content_hash) VALUES (?1, 1, ?2, 1, 'unverified', ?3, ?4, ?5, ?6, ?7, ?8, \
                 ?9, ?10, ?11, '[]', 'unknown', ?12, ?12, ?13, ?14, ?15)",
                params![
                    forged.event_id(), A, d.kind, d.space, d.subject, d.relation, d.object, d.basis, d.asserted_kind,
                    d.client, d.agent, forged.recorded_at, d.confidence_milli, "{}", forged.content_hash
                ],
            )
            .unwrap();
        assert_eq!(kind(b.replay(&forged)), MapErrorKind::Forbidden);
    }

    #[test]
    fn rebuild_is_byte_identical_and_independent_of_arrival_order() {
        let f = fx("rebuild");
        let mut a = f.open("a", A);
        let mut b = f.open("b", B);
        let ea = a
            .append(
                draft("link_proposed", "archive-session:codex/1", "codex"),
                Authority::Unverified,
            )
            .unwrap();
        let sa = a
            .append(
                draft("link_proposed", "status:sp/open", "codex"),
                Authority::Unverified,
            )
            .unwrap();
        let eb = b
            .append(
                draft("link_proposed", "archive-session:claude/2", "claude"),
                Authority::Unverified,
            )
            .unwrap();
        let eb2 = b
            .append(
                correct(&eb, "archive-session:claude/3"),
                Authority::Unverified,
            )
            .unwrap();
        let sb = b
            .append(
                draft("link_proposed", "status:sp/done", "claude"),
                Authority::Unverified,
            )
            .unwrap();
        let mut c1 = f.open("c1", C);
        let mut c2 = f.open("c2", C);
        for e in [&ea, &sa, &eb, &eb2, &sb] {
            c1.replay(e).unwrap();
        }
        for e in [&eb, &eb2, &sb, &ea, &sa] {
            c2.replay(e).unwrap();
        }
        let before = c1.links_dump().unwrap();
        assert_eq!(
            statuses(&c1),
            vec![
                ("status:sp/done".into(), "conflict".into()),
                ("status:sp/open".into(), "conflict".into()),
                ("archive-session:claude/2".into(), "superseded".into()),
                ("archive-session:claude/3".into(), "proposed".into()),
                ("archive-session:codex/1".into(), "proposed".into()),
            ]
        );
        assert_eq!(before, c2.links_dump().unwrap());
        c1.rebuild().unwrap();
        assert_eq!(before, c1.links_dump().unwrap());
        c1.connection().execute("DELETE FROM links", []).unwrap();
        c1.rebuild().unwrap();
        assert_eq!(before, c1.links_dump().unwrap());
    }

    #[test]
    fn correction_and_as_of_answer_before_and_after() {
        let f = fx("asof");
        let mut s = f.open("a", A);
        f.set(2_000);
        let e1 = s
            .append(
                draft("link_proposed", "archive-session:codex/1", "codex"),
                Authority::Unverified,
            )
            .unwrap();
        f.set(3_000);
        let e2 = s
            .append(
                correct(&e1, "archive-session:codex/2"),
                Authority::Unverified,
            )
            .unwrap();
        let before: Vec<_> = s
            .links_as_of("sp", 2_500, 0, 50)
            .unwrap()
            .0
            .into_iter()
            .map(|r| (r.object, r.status))
            .collect();
        assert_eq!(
            before,
            vec![("archive-session:codex/1".into(), "proposed".into())]
        );
        assert_eq!(
            statuses(&s),
            vec![
                ("archive-session:codex/1".into(), "superseded".into()),
                ("archive-session:codex/2".into(), "proposed".into()),
            ]
        );
        // A fork of the chain is refused; correcting the head works.
        assert_eq!(
            kind(s.append(
                correct(&e1, "archive-session:codex/3"),
                Authority::Unverified
            )),
            MapErrorKind::Conflict
        );
        let e3 = s.append(correct(&e2, ""), Authority::Unverified).unwrap();
        assert!(e3.draft.object.is_empty());
        assert!(statuses(&s).iter().all(|(_, st)| st == "superseded"));
        // Correction must name a held event of the same slot and actor.
        let mut other = correct(&e2, "archive-session:codex/4");
        other.corrects = Some(format!("{A}:000000000099"));
        assert_eq!(
            kind(s.append(other, Authority::Unverified)),
            MapErrorKind::NotFound
        );
        let moved = EventDraft {
            relation: "has_error".into(),
            ..correct(&e3, "archive:codex/4")
        };
        assert_eq!(
            kind(s.append(moved, Authority::Unverified)),
            MapErrorKind::InvalidInput
        );
        let stranger = EventDraft {
            client: "claude".into(),
            agent: "claude".into(),
            ..correct(&e3, "archive-session:codex/5")
        };
        assert_eq!(
            kind(s.append(stranger, Authority::Unverified)),
            MapErrorKind::Forbidden
        );
    }

    #[test]
    fn as_of_follows_local_receipt_not_origin_clock() {
        let f = fx("receipt");
        let mut a = f.open("a", A);
        let mut b = f.open("b", B);
        f.set(9_000);
        let ea = a
            .append(
                draft("link_proposed", "archive-session:codex/1", "codex"),
                Authority::Unverified,
            )
            .unwrap();
        f.set(1_000);
        let eb = b
            .append(
                draft("link_proposed", "archive-session:claude/2", "claude"),
                Authority::Unverified,
            )
            .unwrap();
        let mut c = f.open("c", C);
        f.set(5_000);
        c.replay(&ea).unwrap();
        f.set(6_000);
        c.replay(&eb).unwrap();
        let at = |c: &MapStore, t| {
            c.links_as_of("sp", t, 0, 100)
                .unwrap()
                .0
                .into_iter()
                .map(|r| r.object)
                .collect::<Vec<_>>()
        };
        assert!(at(&c, 4_999).is_empty());
        // `ea` was recorded at 9 000 on A but reached C at 5 000.
        assert_eq!(at(&c, 5_500), vec!["archive-session:codex/1".to_owned()]);
        assert_eq!(at(&c, 6_000).len(), 2);
        // A clock that steps back does not reorder receipt.
        f.set(10);
        let e3 = a.append(correct(&ea, ""), Authority::Unverified).unwrap();
        c.replay(&e3).unwrap();
        assert_eq!(at(&c, 5_999), vec!["archive-session:codex/1".to_owned()]);
        assert_eq!(
            c.links_as_of("sp", 6_000, 0, 100)
                .unwrap()
                .0
                .iter()
                .filter(|r| r.status == "superseded")
                .count(),
            1
        );
        assert_eq!(
            kind(c.links_as_of("sp", 6_000, 0, 0)),
            MapErrorKind::InvalidInput
        );
    }

    #[test]
    fn single_value_conflict_survives_a_correction_and_agreement_is_not_conflict() {
        let f = fx("conflict");
        let mut a = f.open("a", A);
        let mut b = f.open("b", B);
        let (x, y) = ("status:sp/open", "status:sp/done");
        let ea = a
            .append(draft("link_proposed", x, "codex"), Authority::Unverified)
            .unwrap();
        let eb = b
            .append(draft("link_proposed", y, "claude"), Authority::Unverified)
            .unwrap();
        a.replay(&eb).unwrap();
        assert_eq!(
            statuses(&a),
            vec![(y.into(), "conflict".into()), (x.into(), "conflict".into())]
        );
        // A switching to B's value does not erase that they disagreed.
        a.append(correct(&ea, y), Authority::Unverified).unwrap();
        assert!(
            rows(&a)
                .iter()
                .all(|r| r.contested && r.resolution == "none")
        );
        assert_eq!(row(&a, y).status, "conflict");
        assert_eq!(row(&a, y).actors.len(), 2);
        assert!(row(&a, x).all_events.contains(&ea.event_id()));
        // Only a local user resolves it.
        a.append(confirm(y), user()).unwrap();
        assert_eq!(row(&a, y).status, "confirmed");
        assert_eq!(row(&a, x).status, "overridden");
        assert!(rows(&a).iter().all(|r| r.resolution == "user_confirmed"));
        // Same value from two origins: agreement.
        let mut c = f.open("c", C);
        let d = EventDraft {
            subject: "task:sp/8".into(),
            ..draft("link_proposed", y, "codex")
        };
        c.append(d.clone(), Authority::Unverified).unwrap();
        let ed = b
            .append(
                EventDraft {
                    client: "claude".into(),
                    agent: "claude".into(),
                    ..d
                },
                Authority::Unverified,
            )
            .unwrap();
        c.replay(&eb).unwrap();
        c.replay(&ed).unwrap();
        let r = rows(&c)
            .into_iter()
            .find(|r| r.subject == "task:sp/8")
            .unwrap();
        assert_eq!(
            (r.status.as_str(), r.contested, r.actors.len()),
            ("proposed", false, 2)
        );
        // One actor holding two live values of a single-valued relation is a conflict too.
        let mut s = f.open("s", A);
        s.append(draft("link_proposed", x, "codex"), Authority::Unverified)
            .unwrap();
        s.append(draft("link_proposed", y, "codex"), Authority::Unverified)
            .unwrap();
        assert!(
            rows(&s)
                .iter()
                .all(|r| r.status == "conflict" && r.contested)
        );
    }

    #[test]
    fn multi_valued_relations_keep_every_object_and_user_disputes_are_explicit() {
        let f = fx("multi");
        let mut a = f.open("a", A);
        let mut b = f.open("b", B);
        let (s1, s2, s3) = (
            "archive-session:codex/1",
            "archive-session:claude/2",
            "archive-session:codex/3",
        );
        a.append(draft("link_proposed", s1, "codex"), Authority::Unverified)
            .unwrap();
        let eb = b
            .append(draft("link_proposed", s2, "claude"), Authority::Unverified)
            .unwrap();
        a.replay(&eb).unwrap();
        a.append(draft("link_proposed", s3, "codex"), Authority::Unverified)
            .unwrap();
        a.append(
            draft("link_proposed", "task:sp/1", "codex"),
            Authority::Unverified,
        )
        .unwrap();
        a.append(
            draft("link_proposed", "task:sp/2", "codex"),
            Authority::Unverified,
        )
        .unwrap();
        // Two origins, different sessions: three true associations, no conflict.
        assert!(
            rows(&a)
                .iter()
                .all(|r| r.status == "proposed" && !r.contested && r.resolution == "none")
        );
        assert_eq!(
            rows(&a)
                .iter()
                .filter(|r| r.relation == "has_session")
                .count(),
            3
        );
        assert_eq!(row(&a, s2).actors, vec![format!("{B}/agent/claude/claude")]);
        // Confirming one session leaves the others as they were: nothing is overridden.
        a.append(confirm(s1), user()).unwrap();
        assert_eq!(row(&a, s1).status, "confirmed");
        assert_eq!(
            (row(&a, s2).status.as_str(), row(&a, s2).resolution.as_str()),
            ("proposed", "none")
        );
        // The user rejects a different object; only that object changes.
        let rej = a.append(reject(s3), user()).unwrap();
        assert_eq!(
            (row(&a, s3).status.as_str(), row(&a, s3).contested),
            ("rejected", false)
        );
        assert_eq!(row(&a, s1).status, "confirmed");
        // Confirming and rejecting the same object is an explicit dispute, not a silent winner.
        a.append(confirm(s3), user()).unwrap();
        let r3 = row(&a, s3);
        assert_eq!(
            (r3.status.as_str(), r3.contested, r3.resolution.as_str()),
            ("conflict", true, "none")
        );
        assert_eq!(
            r3.current_events.len(),
            3,
            "proposal, rejection and confirmation all kept"
        );
        // Only the user's own correction of its rejection settles it.
        let agent_try = EventDraft {
            client: "codex".into(),
            agent: "codex".into(),
            ..correct(&rej, "")
        };
        assert_eq!(
            kind(a.append(agent_try, Authority::Unverified)),
            MapErrorKind::Forbidden
        );
        assert_eq!(
            kind(a.append(as_user(correct(&rej, "")), Authority::Unverified)),
            MapErrorKind::Forbidden
        );
        a.append(as_user(correct(&rej, "")), user()).unwrap();
        assert_eq!(
            (row(&a, s3).status.as_str(), row(&a, s3).contested),
            ("confirmed", false)
        );
        assert!(row(&a, s3).all_events.contains(&rej.event_id()));
        // Relations and object kinds come from the fixed policy only.
        for (relation, object) in [
            ("session", s1),
            ("has_session", "status:sp/done"),
            ("current_status", s1),
            ("multi", s1),
        ] {
            let d = EventDraft {
                relation: relation.into(),
                ..draft("link_proposed", object, "codex")
            };
            assert_eq!(
                kind(a.append(d, Authority::Unverified)),
                MapErrorKind::InvalidInput,
                "{relation}"
            );
        }
        assert_eq!(
            events::relation_policy("has_session").unwrap().0,
            Cardinality::Multi
        );
        assert_eq!(
            events::relation_policy("outcome").unwrap().0,
            Cardinality::Single
        );
        let rebuilt = a.links_dump().unwrap();
        a.rebuild().unwrap();
        assert_eq!(rebuilt, a.links_dump().unwrap());
    }

    #[test]
    fn agent_input_cannot_confirm_and_remote_confirmation_is_only_a_claim() {
        let f = fx("authority");
        let mut s = f.open("a", A);
        let y = "archive-session:claude/2";
        assert_eq!(
            kind(s.append(confirm(y), Authority::Unverified)),
            MapErrorKind::Forbidden
        );
        assert_eq!(
            kind(s.append(reject(y), Authority::Unverified)),
            MapErrorKind::Forbidden
        );
        let bogus = EventDraft {
            basis: "user_confirmed".into(),
            confidence_milli: 1000,
            ..draft("link_proposed", y, "codex")
        };
        assert_eq!(
            kind(s.append(bogus, Authority::Unverified)),
            MapErrorKind::Forbidden
        );
        let posing = EventDraft {
            asserted_kind: "user".into(),
            ..draft("link_proposed", y, "codex")
        };
        assert_eq!(
            kind(s.append(posing, Authority::Unverified)),
            MapErrorKind::Forbidden
        );
        let weak = EventDraft {
            basis: "agent_claim".into(),
            ..confirm(y)
        };
        assert_eq!(kind(s.append(weak, user())), MapErrorKind::InvalidInput);
        let over = EventDraft {
            confidence_milli: 701,
            ..draft("link_proposed", y, "codex")
        };
        assert_eq!(
            kind(s.append(over, Authority::Unverified)),
            MapErrorKind::InvalidInput
        );
        assert!(
            s.events_page("sp", 0, 100).unwrap().0.is_empty(),
            "refused appends leave no trace"
        );
        // A genuine remote confirmation does not resolve here; a forged one is refused.
        let mut b = f.open("b", B);
        let remote = b.append(confirm(y), user()).unwrap();
        assert_eq!(s.replay(&remote).unwrap(), Replayed::Inserted);
        let r = rows(&s).pop().unwrap();
        assert_eq!(
            (r.status.as_str(), r.resolution.as_str()),
            ("proposed", "none")
        );
        let remote_reject = b.append(reject(y), user()).unwrap();
        s.replay(&remote_reject).unwrap();
        assert_eq!(
            rows(&s).pop().unwrap().status,
            "proposed",
            "a remote rejection has no authority here"
        );
        let mut forged = b
            .append(draft("link_proposed", y, "codex"), Authority::Unverified)
            .unwrap();
        forged.draft.kind = "link_confirmed".into();
        forged.content_hash = forged.compute_hash();
        assert_eq!(kind(s.replay(&forged)), MapErrorKind::Forbidden);
        let mut lifted = b
            .append(draft("link_proposed", y, "codex"), Authority::Unverified)
            .unwrap();
        lifted.authority = "root".into();
        lifted.content_hash = lifted.compute_hash();
        assert_eq!(kind(s.replay(&lifted)), MapErrorKind::InvalidInput);
    }

    #[test]
    fn spaces_are_isolated_and_paging_is_bounded() {
        let f = fx("scope");
        let mut s = f.open("a", A);
        for i in 0..5 {
            let d = EventDraft {
                subject: format!("task:sp/{i}"),
                ..draft("link_proposed", "archive-session:codex/1", "codex")
            };
            s.append(d, Authority::Unverified).unwrap();
        }
        let other = EventDraft {
            space: "other".into(),
            ..draft("link_proposed", "archive-session:codex/9", "zed")
        };
        let eo = s.append(other, Authority::Unverified).unwrap();
        s.append(
            draft("link_proposed", "archive-session:codex/2", "codex"),
            Authority::Unverified,
        )
        .unwrap();
        assert!(rows(&s).iter().all(|r| r.space == "sp"));
        assert_eq!(s.links_page("other", None, 100).unwrap().0.len(), 1);
        // A correction cannot move an event to another space.
        let moved = EventDraft {
            space: "sp".into(),
            ..correct(&eo, "archive-session:codex/1")
        };
        assert_eq!(
            kind(s.append(moved, Authority::Unverified)),
            MapErrorKind::InvalidInput
        );
        let (first, more) = s.links_page("sp", None, 2).unwrap();
        assert!(more);
        let (second, _) = s.links_page("sp", first.last(), 2).unwrap();
        assert_ne!(first[1].subject, second[0].subject);
        assert_eq!(
            kind(s.links_page("sp", None, 101)),
            MapErrorKind::InvalidInput
        );
        // Space-scoped event pages never show another space, across every page boundary.
        assert_eq!(kind(s.events_page("sp", 0, 0)), MapErrorKind::InvalidInput);
        let (page, next) = s.events_page("sp", 0, 4).unwrap();
        assert_eq!((page.len(), next), (4, Some(4)));
        let (rest, end) = s.events_page("sp", 4, 4).unwrap();
        assert_eq!((rest.len(), end), (2, None));
        assert!(page.iter().chain(&rest).all(|e| e.draft.space == "sp"));
        assert_eq!(
            rest[1].origin_seq, 7,
            "the cursor skips the foreign event without exposing it"
        );
        let foreign = s.events_page("other", 0, 100).unwrap().0;
        assert_eq!(foreign, vec![eo]);
        assert!(s.events_page("nobody", 0, 100).unwrap().0.is_empty());
        assert!(s.events_page("sp' OR '1'='1", 0, 100).unwrap().0.is_empty());
        // The trusted export is the only cross-space read.
        let (all, n) = s.trusted_export_page(0, 4).unwrap();
        assert_eq!((all.len(), n), (4, Some(4)));
        assert_eq!(s.trusted_export_page(4, 100).unwrap().0.len(), 3);
    }

    #[test]
    fn byte_limits_respect_utf8_and_payload_is_scalar_only() {
        let f = fx("utf8");
        let mut s = f.open("a", A);
        let base = draft("link_proposed", "archive-session:codex/1", "codex");
        // "task:sp/" is 8 bytes; Cyrillic is 2 bytes, emoji 4.
        let fits = format!("task:sp/{}", "ж".repeat(124));
        assert_eq!(fits.len(), 256);
        s.append(
            EventDraft {
                subject: fits,
                ..base.clone()
            },
            Authority::Unverified,
        )
        .unwrap();
        let over = format!("task:sp/{}ж", "😀".repeat(62));
        assert_eq!(over.len(), 258);
        assert_eq!(
            kind(s.append(
                EventDraft {
                    subject: over,
                    ..base.clone()
                },
                Authority::Unverified
            )),
            MapErrorKind::InvalidInput
        );
        let note = format!("{{\"note\":\"{}\"}}", "я".repeat(251));
        assert_eq!(
            kind(s.append(
                EventDraft {
                    payload: note,
                    ..base.clone()
                },
                Authority::Unverified
            )),
            MapErrorKind::InvalidInput
        );
        let ok = format!("{{\"note\":\"{}\",\"count\":3}}", "я".repeat(250));
        let e = s
            .append(
                EventDraft {
                    payload: ok,
                    ..base.clone()
                },
                Authority::Unverified,
            )
            .unwrap();
        assert!(e.draft.payload.starts_with("{\"count\":3,"));
        // `note` is caller text: the map bounds it but cannot know where it came from.
        let typed = s.append(
            EventDraft {
                payload: "{\"note\":\"pasted words\"}".into(),
                ..base.clone()
            },
            Authority::Unverified,
        );
        assert!(typed.is_ok());
        for bad in [
            "{\"content\":\"archive text\"}",
            "{\"note\":{\"nested\":1}}",
            "[1]",
            "{\"count\":1.5}",
            "{\"note\":\"a\nb\"}",
        ] {
            assert_eq!(
                kind(s.append(
                    EventDraft {
                        payload: bad.into(),
                        ..base.clone()
                    },
                    Authority::Unverified
                )),
                MapErrorKind::InvalidInput,
                "{bad}"
            );
        }
        let big = format!("{{\"note\":\"{}\"}}", " ".repeat(events::MAX_PAYLOAD));
        assert_eq!(
            kind(s.append(
                EventDraft {
                    payload: big,
                    ..base.clone()
                },
                Authority::Unverified
            )),
            MapErrorKind::InvalidInput
        );
        let control = EventDraft {
            agent: "co\u{7}dex".into(),
            ..base.clone()
        };
        assert_eq!(
            kind(s.append(control, Authority::Unverified)),
            MapErrorKind::InvalidInput
        );
        let sql = EventDraft {
            space: "sp'; DROP TABLE events;--".into(),
            ..base.clone()
        };
        assert_eq!(
            kind(s.append(sql, Authority::Unverified)),
            MapErrorKind::InvalidInput
        );
        let evidence = EventDraft {
            evidence: vec!["task:sp/1".into(); events::MAX_EVIDENCE + 1],
            ..base.clone()
        };
        assert_eq!(
            kind(s.append(evidence, Authority::Unverified)),
            MapErrorKind::InvalidInput
        );
        let no_relation = EventDraft {
            relation: String::new(),
            ..base
        };
        assert_eq!(
            kind(s.append(no_relation, Authority::Unverified)),
            MapErrorKind::InvalidInput
        );
    }

    #[test]
    fn hash_covers_every_field_and_is_stable() {
        let f = fx("hash");
        let mut s = f.open("a", A);
        let e = s
            .append(
                draft("link_proposed", "archive-session:codex/1", "codex"),
                Authority::Unverified,
            )
            .unwrap();
        assert_eq!(e.compute_hash(), e.content_hash);
        assert_eq!(e.content_hash.len(), 64);
        let stored = s.events_page("sp", 0, 1).unwrap().0.pop().unwrap();
        assert_eq!(stored, e);
        let mut t = e.clone();
        t.authority = "local_user".into();
        assert_ne!(t.compute_hash(), e.content_hash);
        let mut t = e.clone();
        t.draft.relation = "depends_on".into();
        assert_ne!(t.compute_hash(), e.content_hash);
        let mut t = e.clone();
        t.draft.evidence = vec!["task:sp/1".into()];
        assert_ne!(t.compute_hash(), e.content_hash);
        // Length prefixes keep field boundaries unambiguous.
        let (mut p, mut q) = (e.clone(), e.clone());
        p.draft.client = "ab".into();
        p.draft.agent = "c".into();
        q.draft.client = "a".into();
        q.draft.agent = "bc".into();
        assert_ne!(p.compute_hash(), q.compute_hash());
    }

    #[test]
    fn concurrent_writers_never_fork_the_sequence() {
        const THREADS: usize = 4;
        const EACH: usize = 50;
        let f = fx("concurrent");
        drop(f.open("a", A));
        let path = f.path("a");
        let ids: Vec<String> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..THREADS)
                .map(|t| {
                    let path = path.clone();
                    scope.spawn(move || {
                        let mut s = MapStore::open(&path, A).expect("open");
                        (0..EACH)
                            .map(|i| {
                                let d = EventDraft {
                                    subject: format!("task:sp/{t}-{i}"),
                                    ..draft(
                                        "link_proposed",
                                        &format!("archive-session:w{t}/{i}"),
                                        "codex",
                                    )
                                };
                                s.append(d, Authority::Unverified)
                                    .expect("append under contention")
                                    .event_id()
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|w| w.join().unwrap())
                .collect()
        });
        let total = i64::try_from(THREADS * EACH).unwrap();
        let unique: std::collections::BTreeSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), THREADS * EACH, "every append got its own id");
        let s = f.open("a", A);
        let (n, distinct, lo, hi): (i64, i64, i64, i64) = s
            .connection()
            .query_row("SELECT count(*), count(DISTINCT origin_seq), min(origin_seq), max(origin_seq) FROM events", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap();
        assert_eq!((n, distinct, lo, hi), (total, total, 1, total));
        let next: String = s
            .connection()
            .query_row("SELECT value FROM meta WHERE key = 'next_seq'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(next, (total + 1).to_string());
        let links: i64 = s
            .connection()
            .query_row("SELECT count(*) FROM links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(links, total, "every projection committed with its event");
        let dump = s.links_dump().unwrap();
        let mut s = s;
        s.rebuild().unwrap();
        assert_eq!(dump, s.links_dump().unwrap());
    }

    #[test]
    fn thousand_events_project_per_slot_and_the_cap_is_enforced() {
        let f = fx("bench");
        let mut s = f.open("a", A);
        let started = std::time::Instant::now();
        for i in 0..1_000 {
            let d = EventDraft {
                subject: format!("task:sp/{}", i % 50),
                ..draft(
                    "link_proposed",
                    &format!("archive-session:codex/{i}"),
                    "codex",
                )
            };
            s.append(d, Authority::Unverified).unwrap();
        }
        let appended = started.elapsed();
        let mut cursor: Option<LinkRow> = None;
        let mut seen = Vec::new();
        loop {
            let (page, more) = s.links_page("sp", cursor.as_ref(), 100).unwrap();
            seen.extend(
                page.iter()
                    .map(|r| (r.status.clone(), r.contested, r.current_events.len())),
            );
            cursor = page.last().cloned();
            if !more {
                break;
            }
        }
        assert_eq!(seen.len(), 1_000);
        assert!(
            seen.iter()
                .all(|(st, c, n)| st == "proposed" && !c && *n == 1)
        );
        // The per-append projection reads one slot through its index, not the space.
        let plan: Vec<String> = s
            .connection()
            .prepare(&format!("EXPLAIN QUERY PLAN SELECT * FROM events WHERE {} ORDER BY origin_machine, origin_seq", store::SLOT_FILTER))
            .unwrap()
            .query_map(params!["sp", "task:sp/1", "has_session"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(plan.iter().any(|p| p.contains("events_slot")), "{plan:?}");
        assert_eq!(
            kind(store::load_capped(
                s.connection(),
                "space = ?1",
                params!["sp"],
                999
            )),
            MapErrorKind::Conflict
        );
        assert_eq!(
            store::load_capped(s.connection(), "space = ?1", params!["sp"], 1_000)
                .unwrap()
                .len(),
            1_000
        );
        let rebuild = std::time::Instant::now();
        let dump = s.links_dump().unwrap();
        s.rebuild().unwrap();
        assert_eq!(dump, s.links_dump().unwrap());
        eprintln!(
            "map bench: 1000 appends {appended:?}, rebuild+dump {:?} (informational, not gated)",
            rebuild.elapsed()
        );
    }
}
