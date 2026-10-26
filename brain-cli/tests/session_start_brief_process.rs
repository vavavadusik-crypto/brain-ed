//! `session_start_brief` as a real process: off is byte-for-byte the old
//! notice, brief adds the cached project brief (≤ 2000 tokens, untrusted,
//! no archive text), no cache is notice-only, and both clients read the same
//! wire. Synthetic data in temp HOMEs only.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

struct Fixture(PathBuf);

impl Fixture {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("brain-ssb-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(root.join(".brain")).unwrap();
        Self(root)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }

    fn write_setting(&self, body: &str) {
        fs::write(self.0.join(".brain/settings.json"), body).unwrap();
    }

    fn hook(&self, client: &str, cwd: &Path) -> (String, Duration) {
        let started = Instant::now();
        let mut child = Command::new(env!("CARGO_BIN_EXE_brain"))
            .env("HOME", self.path())
            .env("BRAIN_OFFLINE", "true")
            .args(["hook", "--event", "SessionStart", "--client", client])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn hook");
        let payload = json!({"cwd": cwd.to_string_lossy()});
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(serde_json::to_vec(&payload).unwrap().as_slice())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        (
            String::from_utf8_lossy(&output.stdout).to_string(),
            started.elapsed(),
        )
    }

    /// A synthetic project: one markdown note (imported), so the cached brief
    /// has real memory text; the database lives in this HOME.
    fn seed_project(&self) -> PathBuf {
        let vault = self.0.join("vault");
        fs::create_dir_all(&vault).unwrap();
        fs::write(
            vault.join("note.md"),
            "# Synthetic note\nDeploy uses cargo --locked and the synthetic deploy key marker.",
        )
        .unwrap();
        let (ok, _, _, _) = crate_helper_run(
            self.path(),
            &[
                "--offline",
                "import",
                "--from",
                "markdown",
                &vault.to_string_lossy(),
                "--tag",
                "perf",
            ],
        );
        assert!(ok, "import failed");
        // Build (and cache) the brief for the vault cwd, exactly as the hook
        // will look it up later.
        // A memory record the brief will carry (the agent's own decision).
        let (ok, _, _, _) = crate_helper_run(
            self.path(),
            &[
                "--offline",
                "remember",
                "Deploy uses cargo --locked",
                "--kind",
                "decision",
            ],
        );
        assert!(ok, "remember failed");
        // Build (and cache) the brief for the vault cwd, exactly as the hook
        // will look it up later.
        let (ok, _, _, _) = crate_helper_run(
            self.path(),
            &["--offline", "brief", "--cwd", &vault.to_string_lossy()],
        );
        assert!(ok, "brief build failed: the seed must leave a fresh cache");
        vault
    }
}

fn crate_helper_run(home: &Path, args: &[&str]) -> (bool, String, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_brain"))
        .env("HOME", home)
        .args(args)
        .output()
        .expect("run brain");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::new(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn parse(response: &str) -> Value {
    serde_json::from_str(response).expect("hook response is valid JSON")
}

fn context(response: &Value) -> &str {
    response["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext")
}

#[test]
fn off_is_byte_for_byte_the_old_notice() {
    let fixture = Fixture::new("off");
    fixture.write_setting(
        r#"{"version":1,"profile":"balanced","session_start_notice":true,"session_start_brief":"off"}"#,
    );
    let cwd = fixture.path().join("vault");
    fs::create_dir_all(&cwd).unwrap();
    fixture.seed_project();

    let (response_off, _) = fixture.hook("claude", &cwd);
    let parsed = parse(&response_off);
    let context = context(&parsed);
    // The fixed notice, no brief text, no untrusted marker.
    assert!(
        context.starts_with("Brain ED, the shared local memory"),
        "{context}"
    );
    assert!(!context.contains("Untrusted context"), "{context}");
    assert!(!context.contains("Synthetic note"), "{context}");
    // Byte-for-byte: the same run with the setting file absent (off by default).
    let bare = Fixture::new("off-bare");
    let (response_default, _) = bare.hook("claude", &cwd);
    assert_eq!(response_off, response_default, "off must equal the default");
    // And Codex reads the same wire in off mode.
    let (codex_off, _) = fixture.hook("codex", &cwd);
    assert_eq!(codex_off, response_off);
}

#[test]
fn brief_mode_injects_the_cached_brief_marked_untrusted() {
    let fixture = Fixture::new("brief");
    fixture.write_setting(
        r#"{"version":1,"profile":"balanced","session_start_notice":true,"session_start_brief":"brief"}"#,
    );
    let cwd = fixture.path().join("vault");
    fs::create_dir_all(&cwd).unwrap();
    fixture.seed_project();

    let (response, elapsed) = fixture.hook("claude", &cwd);
    let parsed = parse(&response);
    let context = context(&parsed);
    assert!(context.contains("Untrusted context"), "{context}");
    // The brief carries the seeded decision in its HISTORY section.
    assert!(context.contains("Deploy uses cargo --locked"), "{context}");
    assert!(
        context.starts_with("Brain ED, the shared local memory"),
        "{context}"
    );
    // The project's own estimator caps the injection.
    assert!(
        context.len() <= 2000,
        "context is {} bytes, over the 2000-token cap",
        context.len()
    );
    // The hook answers from the cache in well under a second.
    assert!(elapsed < Duration::from_secs(1), "hook took {elapsed:?}");
    // Codex reads the same wire in brief mode.
    let (codex_response, _) = fixture.hook("codex", &cwd);
    assert_eq!(codex_response, response);
}

#[test]
fn no_cache_is_notice_only_and_fast() {
    let fixture = Fixture::new("no-cache");
    fixture.write_setting(
        r#"{"version":1,"profile":"balanced","session_start_notice":true,"session_start_brief":"brief"}"#,
    );
    // A cwd whose cache does not exist: the hook falls back to the notice.
    let cwd = fixture.path().join("vault-without-cache");
    fs::create_dir_all(&cwd).unwrap();

    let (response, elapsed) = fixture.hook("claude", &cwd);
    let parsed = parse(&response);
    let context = context(&parsed);
    assert!(
        context.starts_with("Brain ED, the shared local memory"),
        "{context}"
    );
    assert!(!context.contains("Untrusted context"), "{context}");
    assert!(
        elapsed < Duration::from_secs(1),
        "cache-only must be fast: {elapsed:?}"
    );
}
