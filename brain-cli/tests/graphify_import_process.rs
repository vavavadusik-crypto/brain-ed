//! `brain import <graphify export>` as a process: labels are masked before they are
//! stored or compared, and a changed label becomes a new version of the node instead of
//! a second insert that aborts the import. Only synthetic, assembled "secrets".

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

struct Home(PathBuf);

impl Home {
    fn new(tag: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "brain-graphify-import-{tag}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn brain(&self, args: &[&str]) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_brain"))
            .env("HOME", &self.0)
            .env("BRAIN_OFFLINE", "true")
            .env_remove("BRAIN_DATABASE")
            .args(["--offline"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap_or(Value::Null)
    }
    fn export(&self, labels: &[&str]) -> PathBuf {
        let nodes: Vec<Value> = labels
            .iter()
            .enumerate()
            .map(|(i, label)| json!([format!("n{i}"), label, 0]))
            .collect();
        let path = self.0.join("brain-data.json");
        let doc = json!({"format": "brain-data/1", "nodes": nodes, "links": [],
                         "dictionaries": {"types": ["concept"]}});
        fs::write(&path, doc.to_string()).unwrap();
        path
    }
    fn labels(&self) -> Vec<(String, i64, bool)> {
        let db = rusqlite::Connection::open(self.0.join(".brain/brain.sqlite")).unwrap();
        let mut statement = db
            .prepare(
                "SELECT label, revision, superseded_at IS NULL AND valid_to IS NULL FROM node_versions
                 WHERE node_id = 'g:n0' ORDER BY revision",
            )
            .unwrap();
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn import(home: &Home, path: &Path) -> Value {
    home.brain(&["import", path.to_str().unwrap()])
}

#[test]
fn a_secret_in_a_graph_label_is_masked_and_a_changed_label_is_a_new_version() {
    let home = Home::new("labels");
    home.brain(&["init"]);
    let secret = format!("ghp_{}", "Ab1".repeat(10));
    let export = home.export(&[&format!("deploy notes {secret}"), "plain node"]);
    import(&home, &export);
    let first = home.labels();
    assert_eq!(first.len(), 1);
    assert!(
        !first[0].0.contains(&secret),
        "the secret must be masked on the way in"
    );
    assert!(first[0].0.contains("[REDACTED]"));

    // The same export again changes nothing: the masked label compares equal.
    let again = import(&home, &export);
    assert_eq!(again["nodes"]["changed"], 0, "{again}");
    assert_eq!(home.labels().len(), 1);

    // A renamed node is a new version of the same node, with history kept: exactly one
    // version is current now, and it carries the new label.
    let renamed = home.export(&["deploy notes, rotated", "plain node"]);
    import(&home, &renamed);
    let history = home.labels();
    let current: Vec<_> = history.iter().filter(|(_, _, now)| *now).collect();
    assert_eq!(current.len(), 1, "{history:?}");
    assert_eq!(current[0].0, "deploy notes, rotated");
    assert!(
        history
            .iter()
            .any(|(label, _, now)| !now && label.contains("[REDACTED]"))
    );
}
