//! Read-only observations, never a claim that configured clients capture chats.
use serde_json::{Value, json};
use std::fs;
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
    let search_settings = crate::settings::load(brain_home);
    let deviations = crate::settings::stricter_thresholds(&search_settings);
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
    let opencode_config = home.join(".config/opencode/opencode.jsonc");
    let opencode_plain = home.join(".config/opencode/opencode.json");
    let openclaw_config = home.join(".openclaw/openclaw.json");
    let integration = json!({
        "opencode": {
            "detected": home.join(".config/opencode").exists()
                || crate::installer::executable_on_path_pub("opencode"),
            "mcp_config_entry_present": crate::installer::mcp_entry_present(
                &[opencode_config, opencode_plain],
                "/mcp/brain",
            ),
            "session_hook": "none (OpenCode has no hook config; plugins only)",
        },
        "openclaw": {
            "detected": home.join(".openclaw").exists()
                || crate::installer::executable_on_path_pub("openclaw"),
            "mcp_config_entry_present": crate::installer::mcp_entry_present(
                &[openclaw_config],
                "/mcp/servers/brain",
            ),
            "session_hook": "none",
        },
        "kimi_code": {
            "detected": home.join(".kimi-code").exists()
                || crate::installer::executable_on_path_pub("kimi"),
            "mcp_config_entry_present": crate::installer::mcp_entry_present(
                &[home.join(".kimi-code/mcp.json")],
                "/mcpServers/brain",
            ),
            "session_hook": "none (Kimi Code's SessionStart is observation-only; it cannot add context)",
        },
        "zcode": {
            "integrated": false,
            "reason": "its own running store has no documented CLI MCP config; writing to it from within the running agent is unsafe",
        },
    });
    /// Timer state as file facts (G10): installed unit files, the enabling
    /// symlink, and the last run where an existing status file carries it. No
    /// systemctl is invoked: the doctor answers only what this process can see.
    fn timers_document(home: &Path, brain_home: &Path) -> Value {
        #[cfg(target_os = "linux")]
        let units = home.join(".config/systemd/user");
        #[cfg(target_os = "linux")]
        let wants = units.join("timers.target.wants");
        #[cfg(target_os = "linux")]
        let unit_pair = |timer: &str, service: &str| {
            json!({
                "unit_files_installed": units.join(timer).is_file() && units.join(service).is_file(),
                "enabled_at_boot": fs::symlink_metadata(wants.join(timer)).is_ok(),
            })
        };
        #[cfg(target_os = "macos")]
        let unit_pair = |timer: &str, _service: &str| {
            let label = match timer {
                "brain-archive-collect.timer" => "org.brain-ed.archive",
                "brain-backup.timer" => "org.brain-ed.backup",
                _ => "org.brain-ed.distill",
            };
            json!({"unit_files_installed":home.join("Library/LaunchAgents").join(format!("{label}.plist")).is_file(),"enabled_at_boot":null,"scheduler":"launchd"})
        };
        let distill_last = fs::read(brain_home.join("distill-schedule-status.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        // The archive collector records its runs in the database, not a file;
        // backup leaves timestamped snapshot directories. Both are named here so
        // the reader knows where the real "did it run" answer lives.
        let latest_snapshot = fs::read_dir(brain_home.join("backups"))
            .ok()
            .and_then(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.starts_with("brain-") && name.len() >= 18)
                    .max()
            });
        json!({
            "archive": unit_pair("brain-archive-collect.timer", "brain-archive-collect.service"),
            "archive_last_run": "see `brain archive status` (observed.last_run; kept in the archive database)",
            "backup": unit_pair("brain-backup.timer", "brain-backup.service"),
            "backup_latest_snapshot": latest_snapshot,
            "distill": unit_pair("brain-distill.timer", "brain-distill.service"),
            "distill_last_run": distill_last,
        })
    }

    /// The configured LLM providers with checks this process can make without
    /// calling anything: is the command executable on PATH, is the URL
    /// well-formed, is the named key variable present. Secrets never appear.
    fn llm_providers_document(providers: &[crate::llm::LlmProvider]) -> Value {
        let providers = providers
            .iter()
            .map(|provider| {
                let first_word = provider
                    .command
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                let url = url::Url::parse(provider.base_url.as_str()).ok();
                let url_ok = url.as_ref().is_some_and(|url| {
                    matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
                });
                json!({
                    "name": provider.name,
                    "priority": provider.priority,
                    "kind": if provider.command.is_empty() { "http" } else { "command" },
                    "command_on_path": if provider.command.is_empty() {
                        None
                    } else {
                        Some(crate::installer::executable_on_path_pub(&first_word))
                    },
                    "url_valid": if provider.base_url.is_empty() { None } else { Some(url_ok) },
                    "local_loopback": url.as_ref().is_some_and(|url| {
                        match url.host() {
                            Some(url::Host::Ipv4(host)) => host.is_loopback(),
                            Some(url::Host::Ipv6(host)) => host.is_loopback(),
                            Some(url::Host::Domain(host)) => host == "localhost",
                            None => false,
                        }
                    }),
                    "api_key_env_set": if provider.api_key_env.is_empty() {
                        None
                    } else {
                        Some(std::env::var_os(&provider.api_key_env).is_some())
                    },
                })
            })
            .collect::<Vec<_>>();
        json!({"providers": providers,
           "note": "read-only: nothing is invoked, no secret is read; api_key_env_set only says whether the variable exists"})
    }

    /// Free space under the brain home, queried from `df` (no unsafe, no new
    /// dependencies); the numbers describe the filesystem the directory sits on.
    #[cfg(unix)]
    fn disk_document(brain_home: &Path) -> Value {
        let queried = std::process::Command::new("df")
            .arg("-P")
            .arg("-B1")
            .arg(brain_home)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| {
                let text = String::from_utf8_lossy(&output.stdout).into_owned();
                text.lines().nth(1).and_then(|line| {
                    let columns = line.split_whitespace().collect::<Vec<_>>();
                    if columns.len() >= 4 {
                        Some((columns[1].to_owned(), columns[3].to_owned()))
                    } else {
                        None
                    }
                })
            });
        json!(match queried {
            Some((total, available)) => json!({
                "free_bytes": available.parse::<u64>().ok(),
                "total_bytes": total.parse::<u64>().ok(),
            }),
            None => json!({"free_bytes": null, "total_bytes": null,
                       "note": "the filesystem could not be queried"}),
        })
    }

    #[cfg(not(unix))]
    fn disk_document(_brain_home: &Path) -> Value {
        json!({"free_bytes": null, "note": "supported on Unix only"})
    }

    json!({
        "search_settings": {
            "message": if deviations.is_empty() { "пороги поиска соответствуют профилю или мягче" } else { "поиск настроен строже профиля по умолчанию" },
            "profile": search_settings.profile,
            "stricter_than_profile": !deviations.is_empty(),
            "deviations": deviations,
        },
        "schema_version": 1,
        "status": if damaged { "degraded" } else { "incomplete" },
        "read_only": true,
        "clients": clients,
        "agent_integrations": integration,
        "timers": timers_document(home, brain_home),
        "llm_providers": llm_providers_document(&search_settings.llm_providers),
        "disk": disk_document(brain_home),
        "settings_agents": crate::installer::agents_document(home, brain_home)["agents"],
        "activity": activity,
        // Stored memory never enters a session automatically; only a fixed notice can.
        "automatic_session_brief": "disabled_by_design",
        "session_start_notice": crate::settings::load(brain_home).session_start_notice,
        // The hook drops activity silently by design (it must never block a client);
        // this is the one place that says when, and why, nothing is being recorded.
        "hook_journal": match crate::hook::journal_refusal(brain_home) {
            None => json!({"recording": true}),
            Some(reason) => json!({"recording": false, "reason": reason}),
        },
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
    fn e65_doctor_lists_cutoffs_stricter_than_the_selected_profile() {
        let root = crate::test_support::temp_path("e65-doctor");
        let home = root.join("home");
        let brain = root.join("brain");
        let homes = crate::client_home::classify_all(&home, None, None, None);
        let mut settings = crate::settings::StoredSettings::default();
        crate::settings::save(&brain, &settings).unwrap();
        let clean = document_with(&home, &brain, 1000, &homes);
        assert_eq!(clean["search_settings"]["stricter_than_profile"], false);
        settings.profile = "max".into();
        settings.overrides.min_cosine = Some(0.99);
        settings.overrides.min_bm25 = Some(0.99);
        crate::settings::save(&brain, &settings).unwrap();
        let result = document_with(&home, &brain, 1000, &homes);
        let search = &result["search_settings"];
        assert_eq!(search["stricter_than_profile"], true);
        assert_eq!(search["profile"], "max");
        assert_eq!(search["deviations"].as_array().unwrap().len(), 2);
        assert_eq!(search["deviations"][0]["key"], "min_cosine");
        assert_eq!(search["deviations"][0]["profile_default"], 0.20);
        assert_eq!(search["deviations"][1]["effective"], 0.99);
        assert!(search["message"].as_str().unwrap().contains("строже"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn relocated_clients_report_unknown_markers_and_no_override_value() {
        let root = crate::test_support::temp_path("brain-doctor-relocated");
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
        for leaked in [
            "forbidden",
            "private/",
            "codex-profile",
            "hermes-profile",
            home.to_str().unwrap(),
        ] {
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
        let root = crate::test_support::temp_path("brain-doctor-absent");
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

    /// G10: the doctor reports timer file facts, the distill status file,
    /// LLM providers without invoking anything, and disk space.
    #[cfg(target_os = "linux")] // systemd file facts; macOS has native plist facts
    #[test]
    fn doctor_reports_timers_providers_and_disk() {
        let home = crate::test_support::temp_path("brain-doctor-g10");
        let _ = std::fs::remove_dir_all(&home);
        let brain_home = home.join(".brain");
        std::fs::create_dir_all(brain_home.join("backups")).expect("brain home");
        let now = crate::now_unix_seconds();

        // No timers installed: honest file facts.
        let first = document(&home, &brain_home, now);
        assert_eq!(
            first["timers"]["archive"]["unit_files_installed"],
            json!(false)
        );
        assert_eq!(first["timers"]["distill"]["enabled_at_boot"], json!(false));
        assert_eq!(first["timers"]["distill_last_run"], json!(null));
        assert!(
            first["timers"]["archive_last_run"]
                .as_str()
                .unwrap()
                .contains("brain archive status")
        );

        // Install the distill unit files, the enabling symlink and a status file.
        let units = home.join(".config/systemd/user");
        std::fs::create_dir_all(units.join("timers.target.wants")).expect("units");
        std::fs::write(units.join("brain-distill.service"), b"unit").expect("service");
        std::fs::write(units.join("brain-distill.timer"), b"unit").expect("timer");
        std::os::unix::fs::symlink(
            units.join("brain-distill.timer"),
            units.join("timers.target.wants/brain-distill.timer"),
        )
        .expect("enable symlink");
        std::fs::write(
            brain_home.join("distill-schedule-status.json"),
            br#"{"finished_at":123,"state":"finished"}"#,
        )
        .expect("status file");

        // A backup snapshot directory: the newest name becomes the fact.
        std::fs::create_dir_all(brain_home.join("backups/brain-20261026-010101")).expect("snap");
        std::fs::create_dir_all(brain_home.join("backups/brain-20261025-010101")).expect("old");

        let installed = document(&home, &brain_home, now);
        let timers = &installed["timers"];
        assert_eq!(timers["distill"]["unit_files_installed"], json!(true));
        assert_eq!(timers["distill"]["enabled_at_boot"], json!(true));
        assert_eq!(
            timers["distill_last_run"]["state"],
            json!("finished"),
            "the status file must be surfaced verbatim"
        );
        assert_eq!(
            timers["backup_latest_snapshot"],
            json!("brain-20261026-010101")
        );

        // LLM providers: a command provider not on PATH and an http one;
        // neither is invoked, and the report stays honest about both.
        std::fs::create_dir_all(&brain_home).expect("brain home");
        std::fs::write(
            brain_home.join("settings.json"),
            br#"{"version":1,"profile":"balanced","embedding_mode":"off",
                 "llm_providers":[
                   {"name":"local-llama","base_url":"http://127.0.0.1:8080","model":"m","priority":1,"context_tokens":8192,"timeout_seconds":60},
                   {"name":"sub","command":"claude","priority":2,"context_tokens":8192,"timeout_seconds":60}]}"#,
        )
        .expect("settings");
        let configured = document(&home, &brain_home, now);
        let providers = configured["llm_providers"]["providers"]
            .as_array()
            .expect("providers");
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0]["kind"], json!("http"));
        assert_eq!(providers[0]["url_valid"], json!(true));
        assert_eq!(providers[0]["local_loopback"], json!(true));
        assert_eq!(providers[1]["kind"], json!("command"));
        assert!(
            providers[1]["command_on_path"].as_bool().is_some(),
            "command providers report a boolean path fact, without invoking"
        );
        assert_eq!(providers[1]["url_valid"], json!(null));
        assert!(
            configured["llm_providers"]["note"]
                .as_str()
                .unwrap()
                .contains("nothing is invoked")
        );

        // Disk: df numbers for the filesystem the brain home sits on.
        let disk = &configured["disk"];
        assert!(disk["free_bytes"].as_u64().is_some_and(|free| free > 0));
        assert!(disk["total_bytes"].as_u64().is_some_and(|total| total > 0));

        std::fs::remove_dir_all(&home).ok();
    }
}
