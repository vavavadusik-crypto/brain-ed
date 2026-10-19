//! Read-only observations, never a claim that configured clients capture chats.
use serde_json::{Value, json};
use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::client_home::ClientHomes;

const MAX_CONFIG_BYTES: u64 = 2 * 1024 * 1024;

fn bounded_text(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut text = String::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut text)
        .ok()?;
    (text.len() <= MAX_CONFIG_BYTES as usize).then_some(text)
}

fn json_file(path: &Path) -> Option<Value> {
    serde_json::from_str(&bounded_text(path)?).ok()
}

/// Version and executable state of the process answering, nothing else: no
/// environment, arguments or other processes are read.
fn this_process() -> Value {
    #[cfg(target_os = "linux")]
    let replaced = std::fs::read_link("/proc/self/exe")
        .ok()
        .map(|link| link.to_string_lossy().ends_with(" (deleted)"));
    #[cfg(not(target_os = "linux"))]
    let replaced: Option<bool> = None;
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "executable_replaced_since_start": replaced,
    })
}

pub(crate) fn document(home: &Path, brain_home: &Path, now: i64) -> Value {
    document_with(
        home,
        brain_home,
        now,
        &crate::client_home::from_process_env(home),
    )
}

/// A relocated client's default files are not read: a stale marker there says
/// nothing about the configuration the client actually uses.
pub(crate) fn document_with(
    home: &Path,
    brain_home: &Path,
    now: i64,
    homes: &ClientHomes,
) -> Value {
    let claude_relocated = homes.claude.state.relocated();
    let codex_relocated = homes.codex.state.relocated();
    let claude = (!claude_relocated)
        .then(|| json_file(&home.join(".claude.json")))
        .flatten();
    let settings = (!claude_relocated)
        .then(|| json_file(&home.join(".claude/settings.json")))
        .flatten();
    let codex = (!codex_relocated)
        .then(|| bounded_text(&home.join(".codex/config.toml")))
        .flatten();
    let codex_hooks = (!codex_relocated)
        .then(|| bounded_text(&home.join(".codex/hooks.json")))
        .flatten();
    let activity = crate::activity::document(brain_home, now);
    let damaged = activity["invalid_records"].as_u64().unwrap_or(0) > 0
        || activity["future_records"].as_u64().unwrap_or(0) > 0;
    let clients = ["claude", "codex", "hermes"].map(|client| {
        let observed = activity["agents"]
            .as_array()
            .and_then(|agents| agents.iter().find(|agent| agent["agent"] == client));
        let (mcp, hooks) = match client {
            "claude" => (
                claude
                    .as_ref()
                    .map(|v| v.pointer("/mcpServers/brain").is_some()),
                settings.as_ref().map(|v| {
                    v.get("hooks")
                        .is_some_and(|h| h.to_string().contains("brain hook"))
                }),
            ),
            "codex" => (
                codex.as_ref().map(|v| v.contains("[mcp_servers.brain]")),
                codex_hooks.as_ref().map(|v| v.contains("brain hook")),
            ),
            _ => (None, None),
        };
        let (location, support) = match client {
            "claude" => (homes.claude, "supported"),
            "codex" => (homes.codex, "supported"),
            _ => (homes.hermes, "unsupported"),
        };
        let support = if support == "supported" && location.state.relocated() {
            "relocated_not_followed"
        } else {
            support
        };
        json!({
            "client": client,
            "mcp_config_entry_present": mcp,
            "hook_config_marker_present": hooks,
            "config_location": location.location(),
            "mcp_support": support,
            "hook_support": support,
            "runtime_handshake": "not_checked",
            "last_observed_event": observed,
            "transcript_capture": crate::archive::collector_switch(brain_home),
            "transcript_coverage": "bounded; see `brain archive status` (full history is never claimed)",
        })
    });
    json!({
        "schema_version": 1,
        "status": if damaged { "degraded" } else { "incomplete" },
        "read_only": true,
        "clients": clients,
        "activity": activity,
        "automatic_session_brief": "disabled_by_design",
        "this_process": this_process(),
        "misplaced_claude_mcp_entry": settings.as_ref().is_some_and(|v| v.pointer("/mcpServers/brain").is_some()),
        "client_registry": crate::client_registry::doctor_report(home, brain_home, homes),
        "limitations": [
            "Configuration markers do not prove a successful client handshake.",
            "Activity is a bounded journal tail, not a complete conversation archive.",
            "Missing, invalid or oversized configuration is reported as unknown.",
            "Registering a home alone neither configures nor captures from it. Codex MCP configuration of a registered home is a separate explicit opt-in (`brain clients configure`); for it this command validates only Brain's own record, not the client's config.toml or a working connection. Registered homes are otherwise inspected using filesystem metadata only. Metadata on a remote mount may delay this command.",
            "A relocated home variable is a diagnostic hint only. Its directory contents are not read; explicit registrations are inspected for metadata.",
            "No database, model, network or external process is opened by this command.",
            "Hermes operational-graph import is not Hermes conversation capture.",
            "this_process describes only this doctor run; already running `brain serve` processes keep the binary they started with until their client reconnects them."
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn relocated_clients_report_unknown_markers_and_no_override_value() {
        let root =
            std::env::temp_dir().join(format!("brain-doctor-relocated-{}", std::process::id()));
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        // Stale markers in the default locations.
        std::fs::write(home.join(".claude.json"), r#"{"mcpServers":{"brain":{}}}"#).unwrap();
        std::fs::write(
            home.join(".claude/settings.json"),
            r#"{"hooks":"brain hook"}"#,
        )
        .unwrap();
        std::fs::write(home.join(".codex/config.toml"), "[mcp_servers.brain]\n").unwrap();
        std::fs::write(home.join(".codex/hooks.json"), "brain hook").unwrap();
        let brain_home = home.join(".brain");
        let unset = crate::client_home::classify_all(&home, None, None, None);
        let legacy = document_with(&home, &brain_home, 1000, &unset);
        for index in [0, 1] {
            assert_eq!(legacy["clients"][index]["mcp_config_entry_present"], true);
            assert_eq!(legacy["clients"][index]["hook_config_marker_present"], true);
            assert_eq!(legacy["clients"][index]["mcp_support"], "supported");
            assert_eq!(
                legacy["clients"][index]["config_location"]["state"],
                "unset"
            );
        }
        assert_eq!(
            legacy["clients"][2]["mcp_config_entry_present"],
            Value::Null
        );
        assert_eq!(legacy["clients"][2]["hook_support"], "unsupported");

        let forbidden = root.join("forbidden-override");
        let relocated = crate::client_home::classify_all(
            &home,
            Some(OsStr::new("/synthetic/forbidden-override")),
            Some(OsStr::new("private/codex-profile")),
            Some(OsStr::new("/private/hermes-profile")),
        );
        let result = document_with(&home, &brain_home, 1000, &relocated);
        for (index, variable, state) in [
            (0, "CLAUDE_CONFIG_DIR", "unsupported_override"),
            (1, "CODEX_HOME", "unresolved_override"),
        ] {
            let client = &result["clients"][index];
            assert_eq!(client["mcp_config_entry_present"], Value::Null);
            assert_eq!(client["hook_config_marker_present"], Value::Null);
            assert_eq!(client["config_location"]["variable"], variable);
            assert_eq!(client["config_location"]["state"], state);
            assert_eq!(client["mcp_support"], "relocated_not_followed");
            assert_eq!(client["hook_support"], "relocated_not_followed");
            assert_eq!(client["runtime_handshake"], "not_checked");
        }
        assert_eq!(
            result["clients"][2]["config_location"]["state"],
            "unsupported_override"
        );
        assert_eq!(result["clients"][2]["mcp_support"], "unsupported");
        assert_eq!(result["misplaced_claude_mcp_entry"], false);
        let text = result.to_string();
        for leaked in ["forbidden", "private/", "profile", home.to_str().unwrap()] {
            assert!(!text.contains(leaked), "{leaked}");
        }
        for client in result["clients"].as_array().unwrap() {
            for key in ["mcp_config_entry_present", "hook_config_marker_present"] {
                assert!(
                    client[key].is_boolean() || client[key].is_null(),
                    "{key} type"
                );
            }
        }
        assert!(!forbidden.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_installation_is_not_created_or_called_healthy() {
        let root = std::env::temp_dir().join(format!("brain-doctor-absent-{}", std::process::id()));
        assert!(!root.exists());
        let result = document(&root, &root.join(".brain"), 1000);
        assert_eq!(result["status"], "incomplete");
        assert_eq!(
            result["clients"][0]["mcp_config_entry_present"],
            Value::Null
        );
        assert_eq!(
            result["clients"][2]["transcript_capture"],
            "archive_not_configured"
        );
        assert_eq!(result["this_process"]["version"], env!("CARGO_PKG_VERSION"));
        #[cfg(target_os = "linux")]
        assert_eq!(
            result["this_process"]["executable_replaced_since_start"],
            false
        );
        assert!(!root.exists());
    }
}
