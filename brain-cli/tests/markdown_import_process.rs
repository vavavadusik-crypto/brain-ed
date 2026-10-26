//! Real CLI regressions for Markdown rules 1-7. All inputs, HOME, databases
//! and model directories are synthetic and isolated in temporary directories.
#[path = "common/test_support.rs"]
mod test_support;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};
use serde_json::Value;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let _nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = test_support::temp_path("brain-markdown");
        fs::create_dir_all(root.join("vault")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        Self(root)
    }
    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join("vault").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        path
    }
    fn db_path(&self) -> PathBuf {
        self.0.join("state/brain.sqlite")
    }
    fn db(&self) -> Connection {
        Connection::open(self.db_path()).unwrap()
    }
    fn cli(&self, space: &str, args: &[&str]) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_brain"))
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("home/.config"))
            .env("BRAIN_OFFLINE", "true")
            .env_remove("BRAIN_DATABASE")
            .env_remove("BRAIN_SHARE_STATE_DIR")
            .args([
                "--database",
                self.db_path().to_str().unwrap(),
                "--models-dir",
                self.0.join("models").to_str().unwrap(),
                "--space",
                space,
                "--agent",
                "markdown-test",
                "--offline",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&output.stdout)))
    }
    fn import(&self, extra: &[&str]) -> Value {
        let vault = self.0.join("vault");
        let mut args = vec!["import", "--from", "markdown", vault.to_str().unwrap()];
        args.extend_from_slice(extra);
        self.cli("notes", &args)
    }
    fn count(&self, sql: &str) -> i64 {
        self.db().query_row(sql, [], |r| r.get(0)).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn reasons(report: &Value, field: &str) -> Vec<String> {
    report[field]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["reason"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn rule1_recursive_md_only_bounds_and_symlinks() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let main = f.write("main.md", "# Main\nhello");
    f.write("nested/MEMORY.md", "nested knowledge");
    f.write("ignore.txt", "not markdown");
    f.write("uppercase.MD", "not *.md");
    for dir in [".hidden", ".obsidian", ".trash", "node_modules"] {
        f.write(&format!("{dir}/private.md"), "skip");
    }
    f.write("big.md", &"x".repeat(1024 * 1024 + 1));
    fs::write(f.0.join("outside.md"), "outside").unwrap();
    symlink(f.0.join("outside.md"), f.0.join("vault/escape.md")).unwrap();
    symlink(&main, f.0.join("vault/alias.md")).unwrap();
    symlink(f.0.join("vault"), f.0.join("vault/nested/loop")).unwrap();
    symlink(f.0.join("vault/.hidden"), f.0.join("vault/disguised")).unwrap();
    let r = f.import(&[]);
    assert_eq!(r["imported"], 2, "{r}");
    let skips = reasons(&r, "skipped");
    assert!(skips.contains(&"ignored_directory".to_owned()));
    assert!(skips.contains(&"larger_than_1_mib".to_owned()));
    assert!(skips.contains(&"symlink_outside_root".to_owned()));
    assert_eq!(f.count("SELECT count(*) FROM nodes"), 2);
    let single = f.write("single.md", "one file");
    let r = f.cli(
        "notes",
        &["import", "--from", "markdown", single.to_str().unwrap()],
    );
    assert_eq!(r["files_seen"], 1);
    assert_eq!(r["imported"], 1);
}

#[test]
fn rule2_documents_frontmatter_sections_and_links() {
    let f = Fixture::new();
    let a = f.write("nested/alpha.md", &format!("---\ntags: [architecture, 'local notes']\ntype: decision\ndate: 2020-01-02\n---\n# Alpha\n[[Beta|alias]] [same](../Beta.md#part)\n## First\n{}\n```markdown\n## fake section\n```\n## Second\nneedlequartz\n", "long text\n".repeat(500)));
    let b = f.write(
        "Beta.md",
        "---\ntags:\n  - knowledge\n  - 'agent memory'\ntype: note\n---\nNo heading, use the stem.",
    );
    let r = f.import(&["--tag", "extra"]);
    assert_eq!(r["imported"], 2);
    assert_eq!(r["edges"], 3);
    let db = f.db();
    let (label, body, meta, source): (String,String,String,String) = db.query_row("SELECT label,body,metadata,source FROM node_versions WHERE source=?1 AND json_extract(metadata,'$.parent_id') IS NULL", [a.to_str().unwrap()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    assert_eq!(label, "Alpha");
    assert!(body.starts_with("# Alpha"));
    assert!(!body.contains("tags:"));
    assert_eq!(source, a.to_str().unwrap());
    let meta: Value = serde_json::from_str(&meta).unwrap();
    assert_eq!(meta["type"], "decision");
    assert_eq!(
        meta["tags"],
        serde_json::json!(["architecture", "extra", "local notes"])
    );
    assert_eq!(meta["content_hash"].as_str().unwrap().len(), 64);
    assert_eq!(meta["untrusted"], true);
    let label: String = db
        .query_row(
            "SELECT label FROM node_versions WHERE source=?1",
            [b.to_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(label, "Beta");
    assert_eq!(
        f.count("SELECT count(*) FROM node_versions WHERE kind='document'"),
        4
    );
    assert_eq!(
        f.count("SELECT count(*) FROM edge_versions WHERE relation='contains'"),
        2
    );
    assert_eq!(
        f.count("SELECT count(*) FROM edge_versions WHERE relation='links_to'"),
        1
    );
    assert_eq!(f.count("SELECT count(*) FROM node_versions WHERE json_extract(metadata,'$.tags') LIKE '%extra%'"),4);
}

#[test]
fn rule3_event_time_prefers_date_then_created_then_mtime() {
    let f = Fixture::new();
    f.write(
        "date.md",
        "---\ndate: '2020-01-02'\ncreated: 2021-01-01\n---\ndated",
    );
    f.write(
        "created.md",
        "---\ncreated: 2020-01-02T03:04:05+02:00\n---\ncreated",
    );
    let m = f.write("mtime.md", "modified");
    let bad = f.write("invalid.md", "---\ndate: nonsense\n---\nfallback");
    for p in [&m, &bad] {
        fs::File::open(p)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(1_500_000_000))
            .unwrap();
    }
    let r = f.import(&[]);
    assert!(
        reasons(&r, "warnings")
            .iter()
            .any(|r| r.contains("invalid_front_matter_date"))
    );
    let db = f.db();
    let time = |stem: &str| -> i64 {
        db.query_row(
            "SELECT valid_from FROM node_versions WHERE label=?1",
            [stem],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(time("date"), 1_577_923_200);
    assert_eq!(time("created"), 1_577_927_045);
    assert_eq!(time("mtime"), 1_500_000_000);
    assert_eq!(time("invalid"), 1_500_000_000);
    assert_eq!(
        f.count("SELECT count(*) FROM node_versions WHERE recorded_at > valid_from"),
        4
    );
}

#[test]
fn rule4_reimport_supersedes_and_retains_deleted_sources_and_old_sections() {
    let f = Fixture::new();
    let a = f.write(
        "a.md",
        &format!(
            "---\ndate: 2020-01-02\n---\n# A\n[[b]]\n## Old\n{}",
            "content ".repeat(700)
        ),
    );
    let b = f.write("b.md", "# B\nkept history");
    assert_eq!(f.import(&[])["imported"], 2);
    let before = fs::read(f.db_path()).unwrap();
    let r = f.import(&[]);
    assert_eq!(r["unchanged"], 2);
    assert_eq!(r["edges"], 0);
    assert_eq!(
        fs::read(f.db_path()).unwrap(),
        before,
        "unchanged import must not write"
    );
    assert_eq!(f.count("SELECT count(*) FROM sessions"), 1);
    fs::write(&a, "---\ndate: 2019-01-01\n---\n# A revised\nnew assertion").unwrap();
    fs::remove_file(b).unwrap();
    let r = f.import(&[]);
    assert_eq!(r["updated"], 1);
    assert!(
        reasons(&r, "warnings")
            .iter()
            .any(|r| r.starts_with("deleted_source"))
    );
    assert_eq!(f.count("SELECT count(*) FROM nodes"), 3);
    assert_eq!(
        f.count("SELECT count(*) FROM node_versions WHERE superseded_at IS NOT NULL"),
        2
    );
    assert_eq!(
        f.count("SELECT count(*) FROM edge_versions WHERE superseded_at IS NULL"),
        0
    );
    assert_eq!(
        f.count("SELECT count(*) FROM node_versions WHERE superseded_at IS NULL"),
        2
    );
    assert_eq!(
        f.count("SELECT valid_from FROM node_versions WHERE label='A revised'"),
        1_546_300_800
    );
    let db = f.db();
    let id: String = db
        .query_row(
            "SELECT node_id FROM node_versions WHERE label='A'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let old: String = db.query_row("SELECT body FROM node_versions WHERE node_id=?1 AND superseded_at IS NOT NULL AND json_extract(metadata,'$.parent_id') IS NULL", [id], |r| r.get(0)).unwrap();
    assert!(old.contains("[[b]]"));
    assert_eq!(f.import(&[])["unchanged"], 1);
    fs::write(a, "---\ndate: 2022-01-01\n---\n# A latest\nforward date").unwrap();
    assert_eq!(f.import(&[])["updated"], 1);
}

#[test]
fn rule5_tags_and_dry_run_do_not_create_or_modify_database() {
    let f = Fixture::new();
    f.write("a.md", "---\ntags: [one]\n---\n# A\n[[b]]");
    f.write("b.md", "# B");
    let planned = f.import(&["--dry-run", "--tag", "team", "--tag", "one"]);
    assert_eq!(planned["imported"], 2);
    assert_eq!(planned["edges"], 1);
    assert!(!f.db_path().parent().unwrap().exists());
    assert_eq!(fs::read_dir(f.0.join("home")).unwrap().count(), 0);
    f.import(&["--tag", "team", "--tag", "one"]);
    let before = fs::read(f.db_path()).unwrap();
    f.write("a.md", "changed");
    let planned = f.import(&["--dry-run", "--tag", "team", "--tag", "one"]);
    assert_eq!(planned["updated"], 1);
    assert_eq!(fs::read(f.db_path()).unwrap(), before);
    assert_eq!(f.count("SELECT count(*) FROM sessions"), 1);
    let db = f.db();
    let mut stmt = db.prepare("SELECT metadata FROM node_versions").unwrap();
    for meta in stmt.query_map([], |r| r.get::<_, String>(0)).unwrap() {
        let meta: Value = serde_json::from_str(&meta.unwrap()).unwrap();
        assert_eq!(meta["tags"], serde_json::json!(["one", "team"]));
    }
}

#[test]
fn rule6_secret_paths_and_content_are_never_stored() {
    let f = Fixture::new();
    for p in [
        ".secrets/a.md",
        "credentials/a.md",
        "auth.json.md",
        ".env.md",
    ] {
        f.write(p, "forbidden content");
    }
    // Obviously synthetic markers, not real credentials.
    f.write("safe.md", "---\ntags: [safe]\ntype: sk-synthetic-secret\n---\n# Safe\nkey: sk-synthetic-secret\ntoken: ghp_synthetic-secret-token\naws: AKIAFAKEEXAMPLEONLY1\n-----BEGIN PRIVATE KEY-----\nsynthetic-pem-body\n-----END PRIVATE KEY-----\nordinary knowledge\n");
    let r = f.import(&[]);
    assert_eq!(r["imported"], 1);
    assert_eq!(
        reasons(&r, "skipped")
            .iter()
            .filter(|r| *r == "secret_path")
            .count(),
        4
    );
    assert!(reasons(&r, "warnings").contains(&"secret_lines_redacted".to_owned()));
    let db = f.db();
    let all: String = db
        .query_row(
            "SELECT label || body || metadata FROM node_versions",
            [],
            |r| r.get(0),
        )
        .unwrap();
    for secret in [
        "sk-synthetic",
        "ghp_synthetic",
        "AKIAFAKE",
        "ghp_synthetic-secret-token",
        "AKIAFAKEEXAMPLEONLY1",
        "-----BEGIN",
        "synthetic-pem-body",
        "forbidden content",
    ] {
        assert!(!all.contains(secret), "leaked {secret}");
    }
    assert!(all.contains("ordinary knowledge"));
    assert!(all.contains("[REDACTED]"));
    let raw = fs::read(f.db_path()).unwrap();
    assert!(
        !raw.windows(b"synthetic-secret".len())
            .any(|w| w == b"synthetic-secret")
    );
}

#[test]
fn rule7_json_counts_and_ask_retrieval_are_space_scoped() {
    let f = Fixture::new();
    f.write(
        "knowledge.md",
        "# Quartz Knowledge\nneedlequartz identifies the migration decision.",
    );
    let r = f.import(&[]);
    for key in ["files_seen", "imported", "updated", "unchanged", "edges"] {
        assert!(r[key].is_u64(), "{r}");
    }
    for key in ["skipped", "warnings"] {
        assert!(r[key].is_array());
    }
    let result = f.cli("notes", &["ask", "needlequartz"]);
    assert!(
        result["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["text"].as_str().unwrap().contains("needlequartz")),
        "{result}"
    );
    let result = f.cli("other", &["ask", "needlequartz"]);
    assert_eq!(result["found"], 0);
    let vault = f.0.join("vault");
    let r = f.cli(
        "other",
        &["import", "--from", "markdown", vault.to_str().unwrap()],
    );
    assert_eq!(r["imported"], 1);
    assert_eq!(f.count("SELECT count(*) FROM nodes"), 2);
}

#[test]
fn links_resolve_encoded_relative_paths_and_reject_ambiguous_wikilinks() {
    let f = Fixture::new();
    f.write("a.md","# A\n[space](folder/My%20Note.md) [[unique]] [[duplicate]]\n```\n[[hidden]]\n```\n`[[hidden]]`");
    f.write("folder/My Note.md", "# Note");
    f.write("folder/unique.md", "# Unique");
    f.write("one/duplicate.md", "# Duplicate1");
    f.write("two/duplicate.md", "# Duplicate2");
    f.write("hidden.md", "# Hidden");
    let r = f.import(&[]);
    assert_eq!(r["edges"], 2, "{r}");
    assert!(reasons(&r, "warnings").contains(&"unresolved_or_ambiguous_local_link".to_owned()));
    assert_eq!(f.import(&[])["edges"], 0);
}

#[test]
fn crlf_bom_unicode_and_missing_single_file() {
    let f = Fixture::new();
    let p=f.write("Память.md","\u{feff}---\r\ntags:\r\n  - память\r\ntype: заметка\r\ncreated: '2020-01-02'\r\n---\r\n# Решение\r\nСохранить происхождение.\r\n");
    f.import(&[]);
    let db = f.db();
    let (label, body): (String, String) = db
        .query_row("SELECT label,body FROM node_versions", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(label, "Решение");
    assert!(body.starts_with("# Решение"));
    fs::remove_file(&p).unwrap();
    let r = f.cli(
        "notes",
        &["import", "--from", "markdown", p.to_str().unwrap()],
    );
    assert!(
        reasons(&r, "warnings")
            .iter()
            .any(|r| r.starts_with("source_missing"))
    );
    assert_eq!(f.count("SELECT count(*) FROM nodes"), 1);
}

#[test]
fn tag_changes_update_metadata_once_and_new_targets_add_edges_without_duplicate_nodes() {
    let f = Fixture::new();
    let a = f.write("a.md", "# A\n[[b]]");
    f.import(&[]);
    assert_eq!(f.import(&["--tag", "added"])["updated"], 1);
    assert_eq!(f.import(&[])["unchanged"], 1);
    f.write("b.md", "# B");
    let r = f.import(&["--tag", "added"]);
    assert_eq!(r["edges"], 1);
    assert_eq!(r["unchanged"], 1);
    let db = f.db();
    let n: i64 = db
        .query_row(
            "SELECT count(*) FROM node_versions WHERE source=?1",
            params![a.to_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 2);
    assert!(
        db.prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none()
    );
}

#[test]
fn updated_text_is_immediately_searchable_without_a_future_recording_time() {
    let f = Fixture::new();
    let p = f.write("a.md", "# Before\noldtokenquartz");
    f.import(&[]);
    fs::write(p, "# After\nnewtokenquartz").unwrap();
    f.import(&[]);
    let r = f.cli("notes", &["ask", "newtokenquartz"]);
    assert_eq!(r["found"], 1, "{r}");
    let r = f.cli("notes", &["ask", "oldtokenquartz"]);
    assert_eq!(r["found"], 0, "{r}");
}

#[test]
fn explicit_symlink_file_cannot_escape_its_selected_parent() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    fs::write(f.0.join("outside.md"), "outside").unwrap();
    let link = f.0.join("vault/escape.md");
    symlink(f.0.join("outside.md"), &link).unwrap();
    let r = f.cli(
        "notes",
        &["import", "--from", "markdown", link.to_str().unwrap()],
    );
    assert_eq!(r["imported"], 0);
    assert_eq!(reasons(&r, "skipped"), ["symlink_outside_root"]);
    assert!(!f.db_path().exists());
}

#[test]
fn invalid_calendar_dates_and_now_fall_back_to_mtime() {
    let f = Fixture::new();
    for (name, date) in [
        ("now", "now"),
        ("invalid", "2023-02-29"),
        ("month", "2020-13-01"),
    ] {
        let p = f.write(
            &format!("{name}.md"),
            &format!("---\ndate: {date}\n---\n# {name}"),
        );
        fs::File::open(p)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(1_500_000_000))
            .unwrap();
    }
    f.write("leap.md", "---\ndate: 2024-02-29\n---\n# Leap");
    let r = f.import(&[]);
    assert_eq!(reasons(&r, "warnings").len(), 3);
    assert_eq!(
        f.count("SELECT count(*) FROM node_versions WHERE valid_from=1500000000"),
        3
    );
    assert_eq!(
        f.count("SELECT valid_from FROM node_versions WHERE label='Leap'"),
        1_709_164_800
    );
}

#[test]
fn removing_links_invalidates_current_path_cache_but_keeps_edge_history() {
    let f = Fixture::new();
    let a = f.write("a.md", "# A\n[[b]]");
    f.write("b.md", "# B\n[[c]]");
    f.write("c.md", "# C");
    f.import(&[]);
    f.db().execute("INSERT INTO materialized_paths(space_id,a,b,c,ab_edge_id,bc_edge_id,path_weight,path_kind,created_at,updated_at)
        SELECT x.space_id,x.from_node_id,x.to_node_id,y.to_node_id,x.id,y.id,0.49,'two_hop',0,0
        FROM edges x JOIN edges y ON x.space_id=y.space_id AND x.to_node_id=y.from_node_id",[]).unwrap();
    assert_eq!(f.count("SELECT count(*) FROM materialized_paths"), 1);
    fs::write(a, "# A\nremoved link").unwrap();
    f.import(&[]);
    assert_eq!(f.count("SELECT count(*) FROM materialized_paths"), 0);
    assert_eq!(f.count("SELECT count(*) FROM edge_versions"), 2);
    assert_eq!(
        f.count("SELECT count(*) FROM edge_versions WHERE superseded_at IS NULL"),
        1
    );
}

#[test]
fn exclude_skips_named_directories_at_any_depth_but_not_files_or_prefixes() {
    let f = Fixture::new();
    f.write("notes/keep.md", "kept");
    f.write("09-New-Generation-Brain/node.md", "generated mirror");
    f.write("deep/09-New-Generation-Brain/node.md", "generated mirror");
    f.write(
        "09-New-Generation-Brain-notes/keep.md",
        "a prefix is not the name",
    );
    f.write("09-New-Generation-Brain.md", "a file with the name is kept");
    let r = f.import(&["--exclude", "09-New-Generation-Brain"]);
    assert_eq!(r["imported"], 3, "{r}");
    let skips = reasons(&r, "skipped");
    assert_eq!(
        skips.iter().filter(|s| *s == "excluded_by_user").count(),
        2,
        "{r}"
    );
}

#[test]
fn stats_counts_every_markdown_import_by_its_own_tag() {
    let f = Fixture::new();
    let one = f.write("a/one.md", "---\ntags: [zzz, forest]\n---\n# One\nfirst");
    let two = f.write("a/two.md", "# Two\nsecond");
    for path in [one, two] {
        fs::File::open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(1_500_000_000))
            .unwrap();
    }
    f.import(&["--tag", "vault"]);
    let memory = f.0.join("memory");
    fs::create_dir_all(&memory).unwrap();
    fs::write(memory.join("MEMORY.md"), "# Memory\nan agent fact").unwrap();
    f.cli(
        "notes",
        &[
            "import",
            "--from",
            "markdown",
            memory.to_str().unwrap(),
            "--tag",
            "memory-x",
        ],
    );
    let stats = f.cli("notes", &["stats"]);
    let count = |source: &str| {
        stats["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["source"] == source)
            .map(|s| s["nodes"].as_i64().unwrap())
    };
    // Front-matter tags sort after "vault" and must not split the import.
    assert_eq!(count("markdown:vault"), Some(2), "{stats}");
    assert_eq!(count("markdown:memory-x"), Some(1), "{stats}");
    assert_eq!(stats["nodes"], 3);
    // Every numeric *_at gets a readable UTC *_date sibling and keeps its value.
    for source in stats["sources"].as_array().unwrap() {
        for [at, date] in [
            ["latest_event_at", "latest_event_date"],
            ["latest_recorded_at", "latest_recorded_date"],
        ] {
            let seconds = source[at].as_i64().unwrap();
            let text = source[date].as_str().unwrap_or_default();
            assert_eq!(
                text.len(),
                16,
                "{source}: {date} should be YYYY-MM-DD HH:MM"
            );
            assert_eq!(text.as_bytes()[4], b'-', "{source}: {date} shape");
            assert_eq!(text.as_bytes()[7], b'-', "{source}: {date} shape");
            assert_eq!(text.as_bytes()[10], b' ', "{source}: {date} shape");
            assert_eq!(text.as_bytes()[13], b':', "{source}: {date} shape");
            assert!(seconds > 0, "{source}: {at} stays numeric");
        }
    }
    let vault = stats["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source"] == "markdown:vault")
        .unwrap();
    assert_eq!(vault["latest_event_at"], 1_500_000_000);
    assert_eq!(vault["latest_event_date"], "2017-07-14 02:40");
}

#[test]
fn graph_commands_explain_a_missing_graphify_instead_of_panicking() {
    let f = Fixture::new();
    let output = Command::new(env!("CARGO_BIN_EXE_brain"))
        .env("HOME", f.0.join("home"))
        .env("PATH", f.0.join("empty-path"))
        .args(["query", "anything"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("graphify could not be started"), "{stderr}");
}

// Regressions from the Codex review of 3cc4e50/c4fcdcd (2026-10-24, R1).

#[test]
fn r1_a_symlinked_file_cannot_bring_an_excluded_directory_back() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    f.write("generated/node.md", "generated mirror");
    symlink("generated/node.md", f.0.join("vault/alias.md")).unwrap();
    f.write("generated.md", "a file named like the directory stays");
    let r = f.import(&["--exclude", "generated"]);
    assert_eq!(r["imported"], 1, "{r}");
    assert_eq!(f.count("SELECT count(*) FROM nodes"), 1);
}

#[test]
fn r1_stats_counts_only_what_is_valid_now() {
    let f = Fixture::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let tomorrow = (now + 86_400).to_string();
    f.cli(
        "notes",
        &["remember", "starts tomorrow", "--valid-from", &tomorrow],
    );
    f.cli(
        "notes",
        &[
            "remember",
            "valid now, ends tomorrow",
            "--valid-to",
            &tomorrow,
        ],
    );
    let stats = f.cli("notes", &["stats"]);
    assert_eq!(stats["nodes"], 1, "{stats}");
    let latest = stats["sources"][0]["latest_event_at"].as_i64().unwrap();
    assert!(latest <= now + 5, "{stats}");
}

#[test]
fn r1_reimport_keeps_a_source_in_its_first_tag_group() {
    let f = Fixture::new();
    let note = f.write("note.md", "# Note\nfirst text");
    f.import(&["--tag", "zzz", "--tag", "aaa"]);
    let group = |f: &Fixture| {
        f.cli("notes", &["stats"])["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["source"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(group(&f), ["markdown:zzz"]);
    fs::write(&note, "# Note\nchanged text").unwrap();
    f.import(&["--tag", "zzz", "--tag", "aaa"]);
    assert_eq!(group(&f), ["markdown:zzz"]);
}

#[test]
fn r1_graphify_exit_status_reaches_the_caller() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let bin = f.0.join("fake-bin");
    fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("graphify");
    fs::write(&fake, "#!/bin/sh\necho fake graphify >&2\nexit 37\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_brain"))
        .env("HOME", f.0.join("home"))
        .env("PATH", &bin)
        .args(["query", "anything", "--graph=explicit.json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(37));
}

// Regressions from the Codex re-check of 2ef516e (2026-10-24, R2).

#[test]
fn r2_an_import_tag_already_in_front_matter_is_still_recorded() {
    let f = Fixture::new();
    let note = f.write("note.md", "---\ntags: [vault]\n---\n# Note\nbody");
    f.import(&[]);
    let r = f.import(&["--tag", "vault"]);
    assert_eq!(r["updated"], 1, "{r}");
    let group = f.cli("notes", &["stats"])["sources"][0]["source"].clone();
    assert_eq!(group, "markdown:vault");
    fs::write(&note, "# Note\nbody without front matter").unwrap();
    f.import(&[]);
    let group = f.cli("notes", &["stats"])["sources"][0]["source"].clone();
    assert_eq!(
        group, "markdown:vault",
        "the import tag must survive front-matter edits"
    );
}

#[test]
fn r2_stats_uses_both_time_axes_like_status() {
    let f = Fixture::new();
    f.cli("notes", &["remember", "recorded in the future"]);
    f.cli("notes", &["remember", "superseded only in the future"]);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let db = f.db();
    db.execute(
        "UPDATE node_versions SET recorded_at = ?1 WHERE body LIKE 'recorded in the future%'",
        params![now + 86_400],
    )
    .unwrap();
    db.execute(
        "UPDATE node_versions SET superseded_at = ?1 WHERE body LIKE 'superseded only%'",
        params![now + 86_400],
    )
    .unwrap();
    drop(db);
    let stats = f.cli("notes", &["stats"]);
    let status = f.cli("notes", &["status"]);
    assert_eq!(stats["nodes"], status["active_nodes"], "{stats} {status}");
    assert_eq!(stats["nodes"], 1, "{stats}");
    assert_eq!(stats["active_edges"], status["active_edges"]);
}

// Regression from the Codex re-check of a657834 (2026-10-24, R3).

#[test]
fn r3_active_edges_count_like_status_when_revisions_overlap() {
    let f = Fixture::new();
    f.write("n1.md", "node one");
    f.write("hub.md", "revision one\n[[n1]]");
    f.import(&[]);
    f.write("hub.md", "revision two\n[[n1]]");
    f.import(&[]);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let overlapped = f
        .db()
        .execute(
            "UPDATE edge_versions SET superseded_at = ?1 WHERE superseded_at IS NOT NULL",
            params![now + 86_400],
        )
        .unwrap();
    assert!(
        overlapped > 0,
        "the second import must have revised the edge"
    );
    let stats = f.cli("notes", &["stats"]);
    let status = f.cli("notes", &["status"]);
    assert_eq!(
        stats["active_edges"], status["active_edges"],
        "{stats} {status}"
    );
}

// Sectioning (2026-10-24): mid-size files were one long node that ranked poorly, and
// above the search budget could never be returned at all.

fn section_rows(f: &Fixture, source: &Path) -> Vec<(String, String)> {
    let db = f.db();
    let mut statement = db
        .prepare(
            "SELECT label, body FROM node_versions WHERE source = ?1 AND superseded_at IS NULL
             AND json_extract(metadata, '$.section') IS NOT NULL
             ORDER BY json_extract(metadata, '$.section')",
        )
        .unwrap();
    statement
        .query_map([source.to_str().unwrap()], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn a_mid_size_file_is_split_by_its_headings_and_intro() {
    let f = Fixture::new();
    let intro = "Why this file exists. ".repeat(20);
    let text = format!(
        "# Rules\n{intro}\n## Role\n{}\n## Safety\nnever print secrets\n",
        "senior architect duties\n".repeat(200)
    );
    let path = f.write("rules.md", &text);
    f.import(&[]);
    let labels = section_rows(&f, &path)
        .into_iter()
        .map(|(label, _)| label)
        .collect::<Vec<_>>();
    assert_eq!(labels, ["Rules (intro)", "Role", "Safety"]);
}

#[test]
fn h3_only_and_headingless_files_are_still_cut_into_bounded_parts() {
    let f = Fixture::new();
    let h3 = f.write(
        "h3.md",
        &format!(
            "# Notes\n### One\n{}\n### Two\nshort\n",
            "line one\n".repeat(500)
        ),
    );
    let plain = "paragraph words here\n".repeat(400) + "\n" + &"x".repeat(9000);
    let flat = f.write("flat.md", &plain);
    f.import(&[]);
    let h3_labels = section_rows(&f, &h3)
        .into_iter()
        .map(|(label, _)| label)
        .collect::<Vec<_>>();
    assert_eq!(h3_labels, ["One", "Two"]);
    let parts = section_rows(&f, &flat);
    assert!(parts.len() >= 4, "{}", parts.len());
    assert!(parts.iter().all(|(_, body)| body.len() <= 4 * 1024));
    assert!(
        parts
            .iter()
            .all(|(label, _)| label.starts_with("flat (intro) (part "))
    );
    let joined = parts
        .iter()
        .map(|(_, body)| body.as_str())
        .collect::<String>();
    assert_eq!(joined, plain, "parts must cover the text exactly once");
}

#[test]
fn files_stored_under_the_old_sectioning_are_cut_again_once() {
    let f = Fixture::new();
    let path = f.write(
        "old.md",
        &format!("# Old\n## A\n{}\n## B\nend\n", "body line\n".repeat(500)),
    );
    f.import(&[]);
    // A database written before sectioning was versioned has no marker.
    f.db()
        .execute(
            "UPDATE node_versions SET metadata = json_remove(metadata, '$.sectioning')",
            [],
        )
        .unwrap();
    assert_eq!(f.import(&[])["updated"], 1);
    assert_eq!(f.import(&[])["unchanged"], 1);
    assert_eq!(section_rows(&f, &path).len(), 2);
}
