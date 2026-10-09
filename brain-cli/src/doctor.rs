//! Read-only observations, never a claim that configured clients capture chats.
use serde_json::{Value, json};
use std::fs::File;
use std::io::Read;
use std::path::Path;

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
    let claude = json_file(&home.join(".claude.json"));
    let settings = json_file(&home.join(".claude/settings.json"));
    let codex = bounded_text(&home.join(".codex/config.toml"));
    let codex_hooks = bounded_text(&home.join(".codex/hooks.json"));
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
        json!({
            "client": client,
            "mcp_config_entry_present": mcp,
            "hook_config_marker_present": hooks,
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
        "limitations": [
            "Configuration markers do not prove a successful client handshake.",
            "Activity is a bounded journal tail, not a complete conversation archive.",
            "Missing, invalid or oversized configuration is reported as unknown.",
            "No database, model, network or external process is opened by this command.",
            "Hermes operational-graph import is not Hermes conversation capture.",
            "this_process describes only this doctor run; already running `brain serve` processes keep the binary they started with until their client reconnects them."
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
