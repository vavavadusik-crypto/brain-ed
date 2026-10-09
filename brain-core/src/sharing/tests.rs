//! Canonical tests for the shared-memory core. All data is synthetic, all
//! stores live under the process temp dir, and nothing here touches a real
//! HOME store. The ten-client test is a bounded in-process simulation of ten
//! independent installations — it is honest about not being ten physical
//! machines and about the network/TLS layer being pending.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;

use super::{
    CursorState, EventKind, FactKind, FactStatus, HostLocalRead, MAX_CLAIM_BYTES, MAX_PAGE,
    MAX_PAYLOAD_BYTES, Principal, ShareError, ShareEvent, ShareStore, SharedFact, SharedReader,
    TrustedPeer,
};

/// A temp dir that removes itself on drop, mirroring the packet tests.
struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("brain-share-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("temp dir");
        Self(path)
    }

    fn share(&self) -> PathBuf {
        self.0.join("share.sqlite")
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fact(id: &str, project: &str) -> SharedFact {
    SharedFact {
        fact_id: id.to_string(),
        kind: FactKind::Fact,
        space: "team".to_string(),
        payload: json!({"project": project, "note": "curated by hand"}),
        claim: Some(format!("shared fact {id} for {project}")),
    }
}

fn remote_put(origin: &str, space: &str, seq: u64, fact: SharedFact) -> ShareEvent {
    ShareEvent {
        origin: origin.to_string(),
        space: space.to_string(),
        seq,
        kind: EventKind::Put,
        target: None,
        fact: Some(fact),
        claimed_agent: Some("agent-remote".to_string()),
    }
}

/// A store with the `team` space enabled.
fn host(tag: &str) -> (Dir, ShareStore) {
    let dir = Dir::new(tag);
    let store = ShareStore::open(dir.share()).expect("open store");
    store.enable_space("team").expect("enable team");
    (dir, store)
}

fn granted(space: &str) -> Principal {
    Principal::new("machine-a", "agent-a").with_read_grant(space)
}

fn peer_of(store: &ShareStore) -> TrustedPeer {
    TrustedPeer::new(store.install_id(), "2026-10-09T00:00:00Z")
}

fn applied_cursor(store: &ShareStore, seq: u64) -> CursorState {
    CursorState::with(store.install_id(), seq)
}

#[test]
fn open_is_idempotent_and_keeps_a_stable_install_id() {
    let (dir, store) = host("open");
    let first = store.install_id().to_string();
    assert!(first.starts_with("install-"));
    let again = ShareStore::open(dir.share()).expect("reopen");
    assert_eq!(
        again.install_id(),
        first,
        "install id must be stable across opens"
    );
    assert_eq!(
        store.enabled_spaces().expect("spaces"),
        vec!["team".to_string()]
    );
}

#[test]
fn store_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, _store) = host("perms");
    let mode = std::fs::metadata(dir.share())
        .expect("db file")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "database must be 0600");
}

#[test]
fn readonly_never_creates_or_migrates() {
    let dir = Dir::new("ro-missing");
    let err = ShareStore::open_readonly(dir.share())
        .err()
        .expect("missing file");
    assert_eq!(err.kind(), "not_found");
    assert!(
        !dir.share().exists(),
        "a read-only open must not create the file"
    );

    let (live, store) = host("ro-live");
    let ro = ShareStore::open_readonly(live.share()).expect("open read-only");
    assert_eq!(ro.install_id(), store.install_id());
    let facts = ro
        .live_facts(&granted("team"), "team", MAX_PAGE)
        .expect("readonly read");
    assert!(facts.is_empty(), "nothing was published to this store");
}

#[test]
fn foreign_files_are_refused_not_migrated() {
    let dir = Dir::new("foreign");
    let other = dir.0.join("other.sqlite");
    let conn = rusqlite::Connection::open(&other).expect("raw sqlite");
    conn.pragma_update(None, "application_id", 0x4252_4E41_u32)
        .expect("app id");
    conn.pragma_update(None, "user_version", 2_u32)
        .expect("version");
    let err = ShareStore::open(&other).err().expect("foreign file");
    assert_eq!(err.kind(), "incompatible");
    let err = ShareStore::open_readonly(&other)
        .err()
        .expect("foreign file ro");
    assert_eq!(err.kind(), "incompatible");
}

#[test]
fn default_deny_without_grant_and_foreign_space() {
    let (_dir, store) = host("deny");
    // Old work credential: no memory grants at all.
    let old = Principal::new("machine-old", "agent-old").without_grants();
    let err = store
        .live_facts(&old, "team", MAX_PAGE)
        .expect_err("no grants");
    assert_eq!(err.kind(), "capability_denied");
    // A grant for one space opens nothing else (foreign/other space).
    let err = store
        .live_facts(&granted("team"), "ops", MAX_PAGE)
        .expect_err("not enabled");
    assert_eq!(err.kind(), "space_not_enabled");
    // Authorization runs before anything else, even before page-limit checks.
    let err = store
        .page(&old, "team", "install-x", 0, 0)
        .expect_err("auth first");
    assert_eq!(err.kind(), "capability_denied");
}

#[test]
fn publish_requires_an_enabled_space_and_matching_space() {
    let (_dir, mut store) = host("publish");
    // A fact for a space the host never enabled.
    let mut ops = fact("F1", "alpha");
    ops.space = "ops".to_string();
    let err = store.publish("ops", ops, None).expect_err("ops disabled");
    assert_eq!(err.kind(), "space_not_enabled");
    // A fact whose declared space differs from the publish call.
    let mut elsewhere = fact("F2", "alpha");
    elsewhere.space = "elsewhere".to_string();
    let err = store
        .publish("team", elsewhere, None)
        .expect_err("space mismatch");
    assert_eq!(err.kind(), "invalid_data");
}

#[test]
fn two_stores_exchange_a_page_batch_atomically() {
    let (_da, mut a) = host("two-a");
    let (_db, mut b) = host("two-b");
    a.publish("team", fact("F1", "alpha"), Some("agent-src"))
        .expect("publish 1");
    a.publish("team", fact("F2", "beta"), Some("agent-src"))
        .expect("publish 2");

    // B pages from A under its authenticated principal, then applies.
    let (events, next) = a
        .page(&granted("team"), "team", a.install_id(), 0, MAX_PAGE)
        .expect("page");
    assert_eq!(events.len(), 2);
    assert_eq!(next, 2);
    let applied = b
        .apply_batch("team", &peer_of(&a), &CursorState::default(), &events)
        .expect("apply");
    assert_eq!(applied.get(a.install_id()), Some(2));

    let live = b
        .live_facts(&granted("team"), "team", MAX_PAGE)
        .expect("live");
    assert_eq!(live.len(), 2, "both shared facts must be materialized on B");
    assert_eq!(
        live[0].origin,
        a.install_id(),
        "fact references the original origin"
    );
    assert_eq!(
        live[0].origin_seq, 1,
        "materialized fact references its put event"
    );
    assert_eq!(
        live[0].peer_id,
        a.install_id(),
        "trusted peer recorded at apply"
    );
    assert_eq!(live[0].claimed_agent.as_deref(), Some("agent-src"));

    // Re-applying the identical batch is idempotent, even via another peer.
    let peer_two = TrustedPeer::new("relay-x", "2026-10-09T00:00:01Z");
    let applied = b
        .apply_batch("team", &peer_two, &CursorState::default(), &events)
        .expect("idempotent replay");
    assert_eq!(applied.get(a.install_id()), Some(2));
    assert_eq!(
        b.live_facts(&granted("team"), "team", MAX_PAGE)
            .expect("live again")
            .len(),
        2
    );
}

#[test]
fn revocation_reaches_only_peers_that_pull() {
    let (_da, mut a) = host("revoke-a");
    let (_db, mut b) = host("revoke-b");
    a.publish("team", fact("F1", "alpha"), None)
        .expect("publish");
    let (events, _) = a
        .page(&granted("team"), "team", a.install_id(), 0, MAX_PAGE)
        .expect("page");
    b.apply_batch("team", &peer_of(&a), &CursorState::default(), &events)
        .expect("apply");

    // Retract locally: an append-only event, the fact keeps its history.
    let seq = a.retract("team", "F1").expect("retract");
    assert_eq!(seq, 2, "retract is the next event on the same stream");
    let err = a.retract("team", "F1").expect_err("already retracted");
    assert_eq!(err.kind(), "no_live_fact");
    let row = a
        .fact(&granted("team"), "team", "F1")
        .expect("fact")
        .expect("row");
    assert_eq!(row.status, FactStatus::Retracted);
    assert_eq!(
        row.origin_seq, 1,
        "the fact still references its original put"
    );
    assert_eq!(row.last_seq, 2);

    // Documented limitation: B keeps its copy until it pulls the retraction.
    assert_eq!(
        b.live_facts(&granted("team"), "team", MAX_PAGE)
            .expect("live")
            .len(),
        1
    );
    let (delta, next) = a
        .page(&granted("team"), "team", a.install_id(), 1, MAX_PAGE)
        .expect("delta page");
    assert_eq!(delta.len(), 1);
    assert_eq!(next, 2);
    assert_eq!(delta[0].kind, EventKind::Retract);
    b.apply_batch("team", &peer_of(&a), &applied_cursor(&a, 1), &delta)
        .expect("apply retract");
    assert!(
        b.live_facts(&granted("team"), "team", MAX_PAGE)
            .expect("live")
            .is_empty(),
        "revocation is re-checked on every read; the fact is gone once pulled"
    );
    // The event history still holds the original put.
    let (all, _) = a
        .page(&granted("team"), "team", a.install_id(), 0, MAX_PAGE)
        .expect("page");
    assert_eq!(all.len(), 2, "history is never deleted");
}

#[test]
fn supersession_replaces_payload_but_keeps_history() {
    let (_dir, mut a) = host("supersede");
    a.publish("team", fact("F1", "alpha"), None)
        .expect("publish");
    let replacement = SharedFact {
        fact_id: "F1".to_string(),
        kind: FactKind::Fact,
        space: "team".to_string(),
        payload: json!({"project": "alpha", "note": "second curated revision"}),
        claim: Some("shared fact F1 for alpha, revised".to_string()),
    };
    let seq = a
        .supersede("team", "F1", replacement, Some("agent-src"))
        .expect("supersede");
    assert_eq!(seq, 2);
    let row = a
        .fact(&granted("team"), "team", "F1")
        .expect("fact")
        .expect("row");
    assert_eq!(row.status, FactStatus::Live);
    assert_eq!(row.origin_seq, 1, "still the original put");
    assert_eq!(row.last_seq, 2);
    assert_eq!(row.payload["note"], "second curated revision");
    let (events, _) = a
        .page(&granted("team"), "team", a.install_id(), 0, MAX_PAGE)
        .expect("page");
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].fact.as_ref().expect("v1").payload["note"],
        "curated by hand",
        "the superseded payload stays in the event history"
    );

    // A second plain put over a live id is refused: supersede is the only
    // update path, and the failed event consumes no sequence slot.
    let err = a
        .publish("team", fact("F1", "alpha"), None)
        .expect_err("re-put refused");
    assert_eq!(err.kind(), "invalid_data");
    let seq = a
        .publish("team", fact("F2", "alpha"), None)
        .expect("next publish");
    assert_eq!(seq, 3, "the refused put consumed no sequence number");

    // Renaming inside a supersede is refused.
    let renamed = SharedFact {
        fact_id: "F9".to_string(),
        kind: FactKind::Fact,
        space: "team".to_string(),
        payload: json!({"project": "alpha"}),
        claim: None,
    };
    let err = a
        .supersede("team", "F1", renamed, None)
        .expect_err("rename refused");
    assert_eq!(err.kind(), "invalid_data");
    // Superseding an unknown fact is refused.
    let err = a
        .supersede("team", "nope", fact("nope", "alpha"), None)
        .expect_err("unknown");
    assert_eq!(err.kind(), "no_live_fact");
}

#[test]
fn same_id_different_bytes_is_a_conflict_and_rolls_back_the_batch() {
    let (_da, mut a) = host("conflict-a");
    let (_db, mut b) = host("conflict-b");
    a.publish("team", fact("F1", "alpha"), None)
        .expect("publish");
    let (events, _) = a
        .page(&granted("team"), "team", a.install_id(), 0, MAX_PAGE)
        .expect("page");
    b.apply_batch("team", &peer_of(&a), &CursorState::default(), &events)
        .expect("apply");

    // A corrupted relay re-offers seq 1 with different bytes, plus a new event.
    let mut forged = remote_put(a.install_id(), "team", 1, fact("F1", "alpha"));
    forged.fact.as_mut().expect("fact").payload = json!({"project": "alpha", "note": "tampered"});
    let batch = vec![
        forged,
        remote_put(a.install_id(), "team", 2, fact("F2", "beta")),
    ];
    let err = b
        .apply_batch("team", &peer_of(&a), &applied_cursor(&a, 1), &batch)
        .expect_err("conflict");
    assert_eq!(err.kind(), "share_conflict");
    assert!(
        b.fact(&granted("team"), "team", "F2")
            .expect("fact")
            .is_none(),
        "the whole batch rolled back, the cursor did not advance"
    );
    let live = b
        .live_facts(&granted("team"), "team", MAX_PAGE)
        .expect("live");
    assert_eq!(live.len(), 1);
    let stored = b.cursor(a.install_id(), "team").expect("cursor");
    assert_eq!(
        stored.get(a.install_id()),
        Some(1),
        "cursor unchanged after conflict"
    );
    // The untampered seq 2 still applies afterwards — the stream is usable.
    let real = vec![remote_put(a.install_id(), "team", 2, fact("F2", "beta"))];
    b.apply_batch("team", &peer_of(&a), &applied_cursor(&a, 1), &real)
        .expect("apply after conflict");
    assert!(
        b.fact(&granted("team"), "team", "F2")
            .expect("fact")
            .is_some()
    );
}

#[test]
fn gaps_out_of_order_and_wrong_cursors_are_rejected() {
    let (_da, mut a) = host("gap-a");
    let (_db, mut b) = host("gap-b");
    a.publish("team", fact("F1", "alpha"), None)
        .expect("publish 1");
    a.publish("team", fact("F2", "beta"), None)
        .expect("publish 2");
    let (events, _) = a
        .page(&granted("team"), "team", a.install_id(), 0, MAX_PAGE)
        .expect("page");
    assert_eq!(events.len(), 2);

    // Offer only seq 2 while claiming the cursor is 0: a gap.
    let skip = vec![events[1].clone()];
    let err = b
        .apply_batch("team", &peer_of(&a), &CursorState::default(), &skip)
        .expect_err("gap");
    assert_eq!(err.kind(), "gap");

    // A cursor that does not match the stored value is refused.
    let wrong = vec![events[0].clone()];
    let err = b
        .apply_batch("team", &peer_of(&a), &applied_cursor(&a, 1), &wrong)
        .expect_err("cursor mismatch");
    assert_eq!(err.kind(), "cursor_mismatch");

    // Out-of-order inside one batch is refused before anything applies.
    let reversed = vec![events[1].clone(), events[0].clone()];
    let err = b
        .apply_batch("team", &peer_of(&a), &CursorState::default(), &reversed)
        .expect_err("out of order");
    assert_eq!(err.kind(), "gap");

    // And the clean batch still applies.
    b.apply_batch("team", &peer_of(&a), &CursorState::default(), &events)
        .expect("clean apply");
    assert_eq!(
        b.live_facts(&granted("team"), "team", MAX_PAGE)
            .expect("live")
            .len(),
        2
    );
}

#[test]
fn a_domain_failure_inside_a_batch_rolls_back_everything() {
    let (_da, mut a) = host("rollback-a");
    let (_db, mut b) = host("rollback-b");
    a.publish("team", fact("F1", "alpha"), None)
        .expect("publish");
    let (first, _) = a
        .page(&granted("team"), "team", a.install_id(), 0, MAX_PAGE)
        .expect("page");
    b.apply_batch("team", &peer_of(&a), &CursorState::default(), &first)
        .expect("apply first");

    // A valid put followed by a retraction of an unknown target: the whole
    // batch must roll back, including the valid event.
    let batch = vec![
        remote_put(a.install_id(), "team", 2, fact("F2", "beta")),
        ShareEvent {
            origin: a.install_id().to_string(),
            space: "team".to_string(),
            seq: 3,
            kind: EventKind::Retract,
            target: Some(77),
            fact: None,
            claimed_agent: None,
        },
    ];
    let err = b
        .apply_batch("team", &peer_of(&a), &applied_cursor(&a, 1), &batch)
        .expect_err("bad target");
    assert_eq!(err.kind(), "unknown_target");
    assert!(
        b.fact(&granted("team"), "team", "F2")
            .expect("fact")
            .is_none(),
        "the earlier event of the batch must not survive"
    );
    let stored = b.cursor(a.install_id(), "team").expect("cursor");
    assert_eq!(
        stored.get(a.install_id()),
        Some(1),
        "cursor never claims a partial apply"
    );
}

#[test]
fn remote_batches_cannot_claim_the_local_origin() {
    let (_da, a) = host("spoof-a");
    let (_db, mut b) = host("spoof-b");
    // A remote peer offers events that claim to be B's own installation.
    let forged = remote_put(b.install_id(), "team", 1, fact("F1", "alpha"));
    let err = b
        .apply_batch("team", &peer_of(&a), &CursorState::default(), &[forged])
        .expect_err("spoofed origin");
    assert_eq!(err.kind(), "invalid_data");
}

#[test]
fn claimed_provenance_never_raises_authority() {
    let (_da, mut a) = host("claim-a");
    let (_db, mut b) = host("claim-b");
    a.publish("team", fact("F1", "alpha"), Some("claimed-root-admin"))
        .expect("publish");
    let (events, _) = a
        .page(&granted("team"), "team", a.install_id(), 0, MAX_PAGE)
        .expect("page");
    b.apply_batch("team", &peer_of(&a), &CursorState::default(), &events)
        .expect("apply");
    let live = b
        .live_facts(&granted("team"), "team", MAX_PAGE)
        .expect("live");
    assert_eq!(
        live[0].claimed_agent.as_deref(),
        Some("claimed-root-admin"),
        "the claim is recorded as data"
    );
    assert_eq!(
        live[0].peer_id,
        a.install_id(),
        "trusted context is separate from claims"
    );
    // A client presenting an agent named `root` in its claim gets nothing: a
    // grant-less principal is denied regardless of what the payload claims.
    let nobody = Principal::new("machine-b", "root");
    let err = b
        .live_facts(&nobody, "team", MAX_PAGE)
        .expect_err("claims are not authority");
    assert_eq!(err.kind(), "capability_denied");
}

#[test]
fn payload_keys_that_name_host_context_are_refused() {
    let (_dir, mut a) = host("payload-keys");
    for key in [
        "authority",
        "grant",
        "credential",
        "token",
        "trusted_peer",
        "host_context",
    ] {
        let mut f = fact("F1", "alpha");
        f.payload = json!({"project": "alpha", key: "admin"});
        let err = a.publish("team", f, None).expect_err("reserved key");
        assert_eq!(
            err.kind(),
            "forbidden_payload_key",
            "key {key} must be refused"
        );
    }
}

#[test]
fn reserved_payload_keys_are_refused_nested_and_in_any_case() {
    let (_dir, mut a) = host("payload-keys-nested");
    for payload in [
        json!({"project": "alpha", "Token": "x"}),
        json!({"project": "alpha", "AUTHORITY": "x"}),
        json!({"project": "alpha", "meta": {"grant": "admin"}}),
        json!({"project": "alpha", "items": [{"ok": 1}, {"Credential": "x"}]}),
        json!({"project": "alpha", "a": [[{"b": {"Host_Context": {}}}]]}),
    ] {
        let mut f = fact("F1", "alpha");
        f.payload = payload.clone();
        let err = a.publish("team", f, None).expect_err("reserved key");
        assert_eq!(err.kind(), "forbidden_payload_key", "{payload}");
    }
    let mut deep = json!("leaf");
    for _ in 0..80 {
        deep = json!([deep]);
    }
    let mut f = fact("F1", "alpha");
    f.payload = json!({"project": "alpha", "deep": deep});
    assert_eq!(
        a.publish("team", f, None).expect_err("too deep").kind(),
        "invalid_data"
    );
    // Narrative values that mention reserved words remain ordinary data.
    let mut f = fact("F2", "alpha");
    f.payload = json!({
        "project": "alpha",
        "note": "rotate the token and grant authority later",
        "tags": ["credential", "Token"],
        "tokens_used": 12,
    });
    a.publish("team", f, None).expect("harmless narrative text");
}

#[test]
fn page_and_batch_bounds_hold() {
    let (_da, mut a) = host("bounds-a");
    let (_db, mut b) = host("bounds-b");
    for i in 1..=3 {
        a.publish("team", fact(&format!("F{i}"), "alpha"), None)
            .expect("publish");
    }
    let err = a
        .page(&granted("team"), "team", a.install_id(), 0, 0)
        .expect_err("zero");
    assert_eq!(err.kind(), "page_limit");
    let err = a
        .page(&granted("team"), "team", a.install_id(), 0, MAX_PAGE + 1)
        .expect_err("too big");
    assert_eq!(err.kind(), "page_limit");

    let (p1, next) = a
        .page(&granted("team"), "team", a.install_id(), 0, 2)
        .expect("page 1");
    assert_eq!(p1.len(), 2);
    assert_eq!(next, 2);
    let (p2, next) = a
        .page(&granted("team"), "team", a.install_id(), next, 2)
        .expect("page 2");
    assert_eq!(p2.len(), 1);
    assert_eq!(next, 3);
    let (empty, next) = a
        .page(&granted("team"), "team", a.install_id(), next, 2)
        .expect("page 3");
    assert!(empty.is_empty());
    assert_eq!(next, 3);

    // Batch bounds: too many events, and too many encoded bytes.
    let many: Vec<ShareEvent> = (1..=101)
        .map(|i| remote_put(a.install_id(), "team", i, fact(&format!("F{i}"), "alpha")))
        .collect();
    let err = b
        .apply_batch("team", &peer_of(&a), &CursorState::default(), &many)
        .expect_err("too many events");
    assert_eq!(err.kind(), "batch_too_large");

    let big_payload = "x".repeat(MAX_PAYLOAD_BYTES);
    let heavy: Vec<ShareEvent> = (1..=66)
        .map(|i| {
            let mut f = fact(&format!("F{i}"), "alpha");
            f.payload = json!({"project": "alpha", "blob": big_payload});
            remote_put(a.install_id(), "team", i, f)
        })
        .collect();
    let err = b
        .apply_batch("team", &peer_of(&a), &CursorState::default(), &heavy)
        .expect_err("too many bytes");
    assert_eq!(err.kind(), "batch_too_large");

    // An oversized single payload is refused at validation.
    let mut f = fact("F-big", "alpha");
    f.payload = json!({"project": "alpha", "blob": format!("y{}", "y".repeat(MAX_PAYLOAD_BYTES))});
    let err = a.publish("team", f, None).expect_err("payload bound");
    assert_eq!(err.kind(), "payload_too_large");
}

#[test]
fn unicode_claims_and_ids_are_bounded_and_survive_the_round_trip() {
    let (_da, mut a) = host("uni-a");
    let (_db, mut b) = host("uni-b");
    let unicode = fact("F1", "alpha");
    a.publish("team", unicode, None).expect("publish unicode");
    // 300 two-byte characters: 600 bytes is the exact bound.
    let mut edge = fact("F2", "alpha");
    edge.claim = Some("é".repeat(MAX_CLAIM_BYTES / 2));
    a.publish("team", edge, None).expect("publish edge claim");
    // 301 characters: 602 bytes, over the bound.
    let mut over = fact("F3", "alpha");
    over.claim = Some("é".repeat(MAX_CLAIM_BYTES / 2 + 1));
    let err = a.publish("team", over, None).expect_err("claim bound");
    assert_eq!(err.kind(), "claim_too_large");

    let mut bad = fact("F4", "alpha");
    bad.fact_id = "has space".to_string();
    let err = a.publish("team", bad, None).expect_err("bad id");
    assert_eq!(err.kind(), "invalid_data");

    let (events, _) = a
        .page(&granted("team"), "team", a.install_id(), 0, MAX_PAGE)
        .expect("page");
    assert_eq!(events.len(), 2, "the refused publishes consumed no slots");
    b.apply_batch("team", &peer_of(&a), &CursorState::default(), &events)
        .expect("apply");
    let live = b
        .live_facts(&granted("team"), "team", MAX_PAGE)
        .expect("live");
    assert_eq!(live.len(), 2);
    let claims: Vec<_> = live.iter().filter(|f| f.fact_id == "F1").collect();
    assert_eq!(
        claims[0].claim,
        fact("F1", "alpha").claim,
        "unicode survives the round trip"
    );
}

#[test]
fn the_adapter_yields_project_scoped_snippets() {
    use crate::map::packet::{MemoryKind, MemoryScope};
    let (_da, mut a) = host("adapt-a");
    let (_db, mut b) = host("adapt-b");
    a.publish("team", fact("P1", "alpha"), Some("codex-claimed"))
        .expect("P1");
    let mut link = fact("L1", "alpha");
    link.kind = FactKind::Link;
    link.payload = json!({"project": "alpha", "from": "task:team/1", "to": "task:team/2"});
    a.publish("team", link, None).expect("L1");
    a.publish("team", fact("P2", "beta"), None).expect("P2");
    let mut no_project = fact("P3", "gamma");
    no_project.payload = json!({"note": "no project declared"});
    a.publish("team", no_project, None).expect("P3");
    let mut space_wide = fact("S1", "x");
    space_wide.payload = json!({"scope": "space", "note": "everyone"});
    a.publish("team", space_wide, None).expect("S1");

    let (events, _) = a
        .page(&granted("team"), "team", a.install_id(), 0, MAX_PAGE)
        .expect("page");
    assert_eq!(events.len(), 5);
    b.apply_batch("team", &peer_of(&a), &CursorState::default(), &events)
        .expect("apply");

    let remote = granted("team");
    let reader = SharedReader::Remote(&remote);
    let alpha = b
        .shared_snippets(&reader, "team", Some("alpha"), None, MAX_PAGE)
        .expect("snippets");
    let mut texts: Vec<_> = alpha.iter().map(|s| (s.kind, s.scope.clone())).collect();
    texts.sort_by_key(|(kind, scope)| (kind.as_str(), format!("{scope:?}")));
    assert_eq!(
        texts,
        vec![
            (MemoryKind::Fact, MemoryScope::Project("alpha".into())),
            (MemoryKind::Fact, MemoryScope::Space),
            (MemoryKind::Link, MemoryScope::Project("alpha".into())),
        ],
        "facts stay facts, links stay links, space-wide facts join; never decisions or gotchas"
    );
    for snippet in &alpha {
        assert!(snippet.id.starts_with("shared-") && snippet.id.len() <= 64);
        let shared = snippet.shared.as_ref().expect("shared provenance");
        assert_eq!(
            shared.origin,
            a.install_id(),
            "origin as the event names it"
        );
        assert_eq!(
            shared.received_from_peer,
            a.install_id(),
            "peer as this host recorded it"
        );
    }
    let p1 = alpha
        .iter()
        .find(|s| s.shared.as_ref().is_some_and(|p| p.fact_id == "P1"))
        .expect("P1");
    assert_eq!(
        p1.shared.as_ref().and_then(|p| p.claimed_agent.as_deref()),
        Some("codex-claimed"),
        "the claimed agent is kept as a claim, apart from the peer"
    );
    let link = alpha
        .iter()
        .find(|s| s.kind == MemoryKind::Link)
        .expect("link");
    assert!(link.text.contains("task:team/2"), "a link carries its refs");

    assert_eq!(
        b.shared_snippets(&reader, "team", Some("gamma"), None, MAX_PAGE)
            .expect("gamma")
            .iter()
            .map(|s| s.scope.clone())
            .collect::<Vec<_>>(),
        vec![MemoryScope::Space],
        "a fact without a declared project is never project-scoped"
    );
    let queried = b
        .shared_snippets(&reader, "team", Some("alpha"), Some("everyone"), MAX_PAGE)
        .expect("query");
    assert_eq!(queried.len(), 1, "a query keeps only matching facts");

    // Default deny: no grant, a disabled space, and the host before enabling.
    let bare = Principal::new("machine-a", "agent-a").without_grants();
    assert!(matches!(
        b.shared_snippets(
            &SharedReader::Remote(&bare),
            "team",
            Some("alpha"),
            None,
            MAX_PAGE
        ),
        Err(ShareError::CapabilityDenied { .. })
    ));
    let host_read = SharedReader::HostLocal(HostLocalRead::from_host_configuration());
    assert_eq!(
        b.shared_snippets(&host_read, "team", Some("alpha"), None, MAX_PAGE)
            .expect("host")
            .len(),
        3,
        "the host reads its own store without a network grant"
    );
    assert!(matches!(
        b.shared_snippets(&host_read, "other", Some("alpha"), None, MAX_PAGE),
        Err(ShareError::SpaceNotEnabled(_))
    ));

    // A retracted fact disappears from the retrieval path on the next pull.
    a.retract("team", "P1").expect("retract P1");
    let (delta, _) = a
        .page(&granted("team"), "team", a.install_id(), 5, MAX_PAGE)
        .expect("delta");
    b.apply_batch("team", &peer_of(&a), &applied_cursor(&a, 5), &delta)
        .expect("apply retract");
    let alpha = b
        .shared_snippets(&host_read, "team", Some("alpha"), None, MAX_PAGE)
        .expect("snippets");
    assert_eq!(alpha.len(), 2, "retracted facts never reach map_packet");
    assert!(
        alpha
            .iter()
            .all(|s| s.shared.as_ref().is_some_and(|p| p.fact_id != "P1"))
    );
}

#[test]
fn many_other_project_facts_never_crowd_out_this_project() {
    let (_da, mut a) = host("crowd-a");
    for i in 0..30 {
        a.publish("team", fact(&format!("B{i}"), "beta"), None)
            .expect("beta");
    }
    a.publish("team", fact("A0", "alpha"), None).expect("alpha");
    for i in 30..60 {
        a.publish("team", fact(&format!("B{i}"), "beta"), None)
            .expect("beta");
    }
    let host_read = SharedReader::HostLocal(HostLocalRead::from_host_configuration());
    let alpha = a
        .shared_snippets(&host_read, "team", Some("alpha"), Some("shared fact"), 5)
        .expect("alpha");
    assert_eq!(alpha.len(), 1);
    assert_eq!(
        alpha[0].shared.as_ref().map(|p| p.fact_id.as_str()),
        Some("A0")
    );
}

#[test]
fn ten_clients_pull_from_one_host_a_bounded_simulation() {
    // Ten independent installations (each its own store file and install id)
    // pull one host's stream and apply it into their own stores. This is an
    // in-process simulation of ten clients — NOT ten physical machines —
    // and no network is involved: the TLS/wire layer is pending.
    let (_dh, mut hub) = host("ten-host");
    hub.publish("team", fact("F1", "alpha"), Some("agent-src"))
        .expect("publish");
    let (events, next) = hub
        .page(&granted("team"), "team", hub.install_id(), 0, MAX_PAGE)
        .expect("host pages its stream");
    assert_eq!(events.len(), 1);
    assert_eq!(next, 1);

    let mut applied = 0;
    let mut denied = 0;
    for i in 0..10 {
        let dir = Dir::new(&format!("ten-client-{i}"));
        let mut client = ShareStore::open(dir.share()).expect("client store");
        client
            .enable_space("team")
            .expect("client enables the space locally");
        // Client 3's credential has no memory grants: an old work credential.
        let principal = if i == 3 {
            Principal::new("machine-c3", "agent-c3").without_grants()
        } else {
            Principal::new(format!("machine-c{i}"), format!("agent-c{i}")).with_read_grant("team")
        };
        // The host authorizes the pull before any byte is served.
        match hub.page(&principal, "team", hub.install_id(), 0, MAX_PAGE) {
            Ok((served, _)) => {
                assert_eq!(served.len(), 1, "the granted client is served the event");
                // On its own store the client must authorize before replay too.
                let cursor = client
                    .apply_batch("team", &peer_of(&hub), &CursorState::default(), &events)
                    .expect("client applies");
                assert_eq!(cursor.get(hub.install_id()), Some(1));
                let live = client
                    .live_facts(&principal, "team", MAX_PAGE)
                    .expect("live");
                assert_eq!(live.len(), 1);
                assert_eq!(live[0].fact_id, "F1");
                assert_eq!(live[0].origin, hub.install_id());
                applied += 1;
            }
            Err(_) => {
                denied += 1;
            }
        }
    }
    assert_eq!(applied, 9, "nine granted clients apply the fact");
    assert_eq!(denied, 1, "the grant-less client is denied by the host");
}

#[test]
fn apply_requires_local_opt_in_before_replay_even_for_empty_batches() {
    let (_da, mut a) = host("auth-empty");
    let err = a
        .apply_batch(
            "ops",
            &TrustedPeer::new("p", "t"),
            &CursorState::default(),
            &[],
        )
        .expect_err("space the local host never enabled");
    assert_eq!(err.kind(), "space_not_enabled");
    // An empty batch into a locally enabled space returns the stored cursor.
    let cursor = a
        .apply_batch(
            "team",
            &TrustedPeer::new("p", "t"),
            &CursorState::default(),
            &[],
        )
        .expect("empty ok");
    assert!(cursor.entries.is_empty());
    assert!(a.disable_space("team").expect("disable"));
    let err = a
        .apply_batch(
            "team",
            &TrustedPeer::new("p", "t"),
            &CursorState::default(),
            &[],
        )
        .expect_err("disabled locally");
    assert_eq!(err.kind(), "space_not_enabled");
}

#[test]
fn claimed_agent_is_part_of_event_identity() {
    let (_da, a) = host("claimed-digest");
    let mut first = remote_put("origin-x", "team", 1, fact("F1", "alpha"));
    let digest = first.digest();
    first.claimed_agent = Some("someone-else".into());
    assert_ne!(
        digest,
        first.digest(),
        "a different claimed agent is a different event"
    );
    drop(a);
}

#[test]
fn replay_with_altered_claimed_agent_is_a_conflict() {
    let (_da, a) = host("claimed-conflict-src");
    drop(a);
    let (_db, mut b) = host("claimed-conflict-dst");
    let event = remote_put("origin-x", "team", 1, fact("F1", "alpha"));
    let peer = TrustedPeer::new("peer-x", "t");
    b.apply_batch(
        "team",
        &peer,
        &CursorState::default(),
        std::slice::from_ref(&event),
    )
    .expect("first apply");
    let mut forged = event.clone();
    forged.claimed_agent = Some("admin".into());
    let err = b
        .apply_batch(
            "team",
            &TrustedPeer::new("peer-y", "t"),
            &CursorState::default(),
            &[forged],
        )
        .expect_err("same slot, different claimed provenance");
    assert_eq!(err.kind(), "share_conflict");
}

#[test]
fn fact_space_must_match_event_space() {
    let (_db, mut b) = host("space-mismatch");
    let mut event = remote_put("origin-x", "team", 1, fact("F1", "alpha"));
    if let Some(f) = event.fact.as_mut() {
        f.space = "ops".into();
    }
    let err = b
        .apply_batch(
            "team",
            &TrustedPeer::new("p", "t"),
            &CursorState::default(),
            &[event],
        )
        .expect_err("mismatched fact space");
    assert_eq!(err.kind(), "invalid_data");
}

#[cfg(unix)]
#[test]
fn store_refuses_symlinked_database_and_sidecars() {
    let dir = Dir::new("symlink-store");
    let real = dir.0.join("elsewhere.sqlite");
    drop(ShareStore::open(&real).expect("real store"));
    let link = dir.0.join("share.sqlite");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");
    assert_eq!(
        ShareStore::open(&link).err().expect("refused").kind(),
        "invalid_data"
    );
    assert_eq!(
        ShareStore::open_readonly(&link)
            .err()
            .expect("refused ro")
            .kind(),
        "invalid_data"
    );
    let target = dir.0.join("victim");
    std::fs::write(&target, b"x").expect("victim");
    let other = dir.0.join("fresh.sqlite");
    std::os::unix::fs::symlink(&target, dir.0.join("fresh.sqlite-wal")).expect("wal link");
    assert_eq!(
        ShareStore::open(&other).err().expect("wal refused").kind(),
        "invalid_data"
    );
}

#[cfg(unix)]
#[test]
fn opening_never_chmods_an_existing_parent_directory() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Dir::new("parent-mode");
    let parent = dir.0.join("shared-parent");
    std::fs::create_dir(&parent).expect("parent");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let store = ShareStore::open(parent.join("share.sqlite")).expect("open");
    let mode = std::fs::metadata(&parent)
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o755, "existing parent keeps its own mode");
    let file_mode = std::fs::metadata(store.path())
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(file_mode, 0o600);
    let fresh = dir.0.join("new-a/new-b/share.sqlite");
    drop(ShareStore::open(&fresh).expect("open fresh"));
    for created in [dir.0.join("new-a"), dir.0.join("new-a/new-b")] {
        let mode = std::fs::metadata(&created)
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "{} created private", created.display());
    }
}

#[test]
fn wire_round_trip_is_strict_and_digest_checked() {
    let event = remote_put("origin-x", "team", 1, fact("F1", "alpha"));
    let wire = event.to_wire();
    assert_eq!(ShareEvent::from_wire(&wire).expect("round trip"), event);
    let mut tampered = wire.clone();
    tampered["fact"]["payload"]["project"] = serde_json::json!("beta");
    assert_eq!(
        ShareEvent::from_wire(&tampered).unwrap_err().kind(),
        "invalid_data"
    );
    let mut extra = wire.clone();
    extra["trusted_peer"] = serde_json::json!("host");
    assert_eq!(
        ShareEvent::from_wire(&extra).unwrap_err().kind(),
        "invalid_data"
    );
    let mut float_seq = wire;
    float_seq["seq"] = serde_json::json!(1.0);
    assert_eq!(
        ShareEvent::from_wire(&float_seq).unwrap_err().kind(),
        "invalid_data"
    );
    assert_eq!(
        ShareEvent::from_wire(&serde_json::json!([]))
            .unwrap_err()
            .kind(),
        "invalid_data"
    );
}

#[test]
fn own_page_serves_only_this_origin() {
    let (_da, mut a) = host("own-page");
    a.publish("team", fact("F1", "alpha"), Some("agent-a"))
        .expect("publish");
    let relayed = remote_put("origin-x", "team", 1, fact("R1", "alpha"));
    a.apply_batch(
        "team",
        &TrustedPeer::new("p", "t"),
        &CursorState::default(),
        &[relayed],
    )
    .expect("apply relayed");
    let page = a.page_own(&granted("team"), "team", 0, 10).expect("page");
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].origin, a.install_id());
    assert!(!page.has_more);
    let facts = a
        .own_live_facts(&granted("team"), "team", 10)
        .expect("facts");
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].fact_id, "F1");
    let denied = a.page_own(&Principal::new("m", "a").without_grants(), "team", 0, 10);
    assert_eq!(denied.unwrap_err().kind(), "capability_denied");
}

#[test]
fn disabled_spaces_stop_serving_immediately() {
    let (_dir, mut a) = host("disable");
    a.publish("team", fact("F1", "alpha"), None)
        .expect("publish");
    assert!(a.disable_space("team").expect("disable"));
    let err = a
        .page(&granted("team"), "team", a.install_id(), 0, MAX_PAGE)
        .expect_err("disabled");
    assert_eq!(err.kind(), "space_not_enabled");
    let err = a
        .live_facts(&granted("team"), "team", MAX_PAGE)
        .expect_err("disabled live");
    assert_eq!(err.kind(), "space_not_enabled");
    assert!(
        !a.disable_space("team").expect("already disabled"),
        "idempotent opt-out"
    );
    a.enable_space("team").expect("re-enable");
    assert!(
        a.page(&granted("team"), "team", a.install_id(), 0, MAX_PAGE)
            .is_ok()
    );
}

#[test]
fn served_events_are_re_digest_and_tampering_is_caught() {
    let (_da, mut a) = host("digest-a");
    a.publish("team", fact("F1", "alpha"), None)
        .expect("publish");
    // Corrupt the stored payload directly; the page path must refuse to serve.
    a.conn
        .execute(
            "UPDATE events SET payload = '{\"project\":\"alpha\",\"note\":\"evil\"}' WHERE seq = 1",
            [],
        )
        .expect("tamper");
    let err = a
        .page(&granted("team"), "team", a.install_id(), 0, MAX_PAGE)
        .expect_err("tampered event must not travel");
    assert_eq!(err.kind(), "invalid_data");
}
