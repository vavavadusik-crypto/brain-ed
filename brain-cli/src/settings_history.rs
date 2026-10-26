//! Bounded, private settings audit. Search snapshots contain no archive text.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::settings::{self, QualityOverrides, StoredSettings};

pub(crate) const MAX_BYTES: u64 = 1024 * 1024;
const HISTORY: &str = "settings-history.jsonl";
const ROTATED: &str = "settings-history.jsonl.1";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct SearchSnapshot {
    profile: String,
    overrides: QualityOverrides,
}

impl From<&StoredSettings> for SearchSnapshot {
    fn from(settings: &StoredSettings) -> Self {
        Self {
            profile: settings.profile.clone(),
            overrides: settings.overrides,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Entry {
    pub timestamp: String,
    source: String,
    changes: Value,
    search: SearchSnapshot,
}

fn private_open(path: &Path, append: bool) -> io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .append(append)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("settings history must be a regular file"));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn lock(home: &Path) -> io::Result<File> {
    crate::private_fs::ensure_private_home(home)?;
    let file = private_open(&home.join("settings-history.lock"), false)?;
    file.lock()?;
    Ok(file)
}

fn read_entries(home: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for name in [ROTATED, HISTORY] {
        let path = home.join(name);
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !file.metadata()?.is_file() || file.metadata()?.len() > MAX_BYTES {
            return Err(io::Error::other(
                "settings history exceeds its bound or is not a file",
            ));
        }
        let mut bytes = Vec::new();
        (&mut file).take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(io::Error::other("settings history exceeds its bound"));
        }
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            entries.push(serde_json::from_slice(line).map_err(io::Error::other)?);
        }
    }
    Ok(entries)
}

pub(crate) fn history(home: &Path, limit: usize) -> io::Result<Vec<Entry>> {
    let mut entries = read_entries(home)?;
    entries.reverse();
    entries.truncate(limit.min(10_000));
    Ok(entries)
}

fn audit_value(key: &str, mut value: Value) -> Value {
    // URLs may contain credentials, CLI commands may contain literal tokens.
    // Never copy either into an audit trail. No environment values are read.
    if key == "llm_providers"
        && let Some(providers) = value.as_array_mut()
    {
        for provider in providers {
            for field in ["base_url", "command"] {
                if provider.get(field).is_some_and(|v| v != "") {
                    provider[field] = json!("[redacted]");
                }
            }
        }
    }
    value
}

pub(crate) fn save(home: &Path, settings: &StoredSettings, source: &str) -> io::Result<()> {
    settings
        .validate()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let _lock = lock(home)?;
    save_locked(home, settings, source)
}

fn save_locked(home: &Path, settings: &StoredSettings, source: &str) -> io::Result<()> {
    let before = settings::load(home);
    save_with_before(home, settings, source, &before, || {
        settings::write_document(home, settings)
    })
}

fn save_with_before(
    home: &Path,
    settings: &StoredSettings,
    source: &str,
    before: &StoredSettings,
    write_document: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let old = serde_json::to_value(before)?;
    let new = serde_json::to_value(settings)?;
    let changes: serde_json::Map<String, Value> = new.as_object().unwrap().iter()
        .filter(|(key, value)| old.get(*key) != Some(*value))
        .map(|(key, value)| (key.clone(), json!({"before": audit_value(key, old[key].clone()), "after": audit_value(key, value.clone())})))
        .collect();
    let timestamp = format!(
        "{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos()
    );
    let entry = Entry {
        timestamp,
        source: source.to_owned(),
        changes: Value::Object(changes),
        search: SearchSnapshot::from(settings),
    };
    let mut encoded = serde_json::to_vec(&entry)?;
    encoded.push(b'\n');
    if encoded.len() as u64 > MAX_BYTES {
        return Err(io::Error::other("settings history entry too large"));
    }
    let path = home.join(HISTORY);
    let mut file = private_open(&path, true)?;
    if file.metadata()?.len() > MAX_BYTES {
        return Err(io::Error::other("settings history exceeds its bound"));
    }
    if file.metadata()?.len() + encoded.len() as u64 > MAX_BYTES {
        fs::rename(&path, home.join(ROTATED))?;
        file = private_open(&path, true)?;
    }
    let length = file.metadata()?.len();
    // Audit is durable before replacing settings; ordinary write failures roll
    // the append back. A power loss between the two files can leave an intent
    // record: this is an audit trail, not a cross-file transaction database.
    file.write_all(&encoded)
        .and_then(|()| file.sync_all())
        .inspect_err(|_| {
            let _ = file.set_len(length);
        })?;
    if let Err(error) = write_document() {
        file.set_len(length)?;
        return Err(error);
    }
    Ok(())
}

/// A backup restore stages a whole home using hard links. Detach audit files
/// before appending so the live journal is untouched until directory exchange.
pub(crate) fn stage_restore(live: &Path, staged: &Path, document: &[u8]) -> io::Result<()> {
    for name in [HISTORY, ROTATED] {
        let path = staged.join(name);
        if path.exists() {
            if fs::metadata(&path)?.len() > MAX_BYTES {
                return Err(io::Error::other("settings history exceeds its bound"));
            }
            crate::private_fs::write_private(&path, &fs::read(&path)?)?;
        }
    }
    let lock_path = staged.join("settings-history.lock");
    if lock_path.exists() {
        fs::remove_file(lock_path)?;
    }
    let _lock = lock(staged)?;
    // Backup restore preserves bytes, including unknown/old schema fields.
    // Audit only the known effective settings, never arbitrary restored JSON
    // which could hold a token. Staging failures never publish this directory.
    crate::private_fs::write_private(&settings::settings_path(staged), document)?;
    let candidate = settings::load(staged);
    save_with_before(staged, &candidate, "cli", &settings::load(live), || Ok(()))
}

pub(crate) fn revert(home: &Path, timestamp: &str, yes: bool) -> io::Result<()> {
    if !yes {
        return Err(io::Error::other(
            "подтвердите возврат блока поиска флагом --yes",
        ));
    }
    let _lock = lock(home)?;
    let entry = read_entries(home)?
        .into_iter()
        .find(|e| e.timestamp == timestamp)
        .ok_or_else(|| io::Error::other("timestamp not found in retained settings history"))?;
    let mut current = settings::load(home);
    current.profile = entry.search.profile;
    current.overrides = entry.search.overrides;
    current.validate().map_err(io::Error::other)?;
    save_locked(home, &current, "cli")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn home() -> std::path::PathBuf {
        let home = crate::test_support::temp_path("e65-history");
        fs::create_dir_all(&home).unwrap();
        home
    }

    #[test]
    fn e65_restore_stages_audit_without_mutating_live_hard_links() {
        let live = home();
        let staged = home();
        let initial = StoredSettings::default();
        save(&live, &initial, "cli").unwrap();
        for name in [HISTORY, "settings-history.lock", "settings.json", "profile"] {
            fs::hard_link(live.join(name), staged.join(name)).unwrap();
        }
        let before = fs::read(live.join(HISTORY)).unwrap();
        let candidate = StoredSettings {
            profile: "max".into(),
            ..initial.clone()
        };
        stage_restore(&live, &staged, &serde_json::to_vec(&candidate).unwrap()).unwrap();
        assert_eq!(fs::read(live.join(HISTORY)).unwrap(), before);
        assert_eq!(settings::load(&live), initial);
        assert_eq!(settings::load(&staged), candidate);
        assert_eq!(history(&staged, 10).unwrap().len(), 2);
        assert_eq!(
            history(&staged, 1).unwrap()[0].changes["profile"]["before"],
            "balanced"
        );
        fs::remove_dir_all(live).unwrap();
        fs::remove_dir_all(staged).unwrap();
    }

    #[test]
    fn e65_settings_write_failure_rolls_back_the_journal_append() {
        let home = home();
        fs::create_dir(home.join("settings.json")).unwrap();
        assert!(save(&home, &StoredSettings::default(), "cli").is_err());
        assert!(history(&home, 10).unwrap().is_empty());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn e65_history_records_each_save_source_diff_and_search_snapshot_privately() {
        let home = home();
        let mut settings = StoredSettings::default();
        settings::save(&home, &settings).unwrap();
        settings.profile = "max".into();
        settings.overrides.min_cosine = Some(0.99);
        save(&home, &settings, "window").unwrap();
        let entries = history(&home, 10).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].source, "window");
        assert_eq!(entries[1].source, "cli");
        assert_eq!(entries[0].changes["profile"]["before"], "balanced");
        assert_eq!(entries[0].changes["profile"]["after"], "max");
        assert_eq!(entries[0].changes["overrides"]["after"]["min_cosine"], 0.99);
        assert_eq!(entries[0].search.profile, "max");
        assert_ne!(entries[0].timestamp, entries[1].timestamp);
        assert_eq!(history(&home, 1).unwrap().len(), 1);
        for file in [HISTORY, "settings.json", "profile"] {
            assert_eq!(
                fs::metadata(home.join(file)).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn e65_revert_requires_yes_restores_search_only_and_journals_itself() {
        let home = home();
        let mut settings = StoredSettings {
            profile: "precise".into(),
            ..StoredSettings::default()
        };
        settings.overrides.min_cosine = Some(0.31);
        save(&home, &settings, "window").unwrap();
        let target = history(&home, 1).unwrap()[0].timestamp.clone();
        settings.profile = "max".into();
        settings.overrides.min_cosine = Some(0.99);
        settings.include_archive = false;
        settings.backup_keep = 19;
        settings.embedding_mode = settings::EmbeddingMode::Off;
        settings.session_start_notice = false;
        save(&home, &settings, "cli").unwrap();
        let bytes = fs::read(home.join("settings.json")).unwrap();
        assert!(revert(&home, &target, false).is_err());
        assert!(revert(&home, "missing", true).is_err());
        assert_eq!(fs::read(home.join("settings.json")).unwrap(), bytes);
        assert_eq!(history(&home, 10).unwrap().len(), 2);
        revert(&home, &target, true).unwrap();
        let restored = settings::load(&home);
        settings.profile = "precise".into();
        settings.overrides.min_cosine = Some(0.31);
        assert_eq!(restored, settings);
        assert_eq!(history(&home, 10).unwrap().len(), 3);
        assert_eq!(history(&home, 1).unwrap()[0].source, "cli");
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn e65_history_rotates_with_bound_and_retained_timestamps_remain_revertible() {
        let home = home();
        let settings = StoredSettings::default();
        save(&home, &settings, "cli").unwrap();
        let line = fs::read(home.join(HISTORY)).unwrap();
        let repeated = line.repeat(MAX_BYTES as usize / line.len());
        fs::write(home.join(HISTORY), &repeated).unwrap();
        save(&home, &settings, "cli").unwrap();
        assert!(home.join(ROTATED).exists());
        assert_eq!(
            fs::metadata(home.join(ROTATED)).unwrap().len(),
            repeated.len() as u64
        );
        let timestamp = history(&home, 1).unwrap()[0].timestamp.clone();
        revert(&home, &timestamp, true).unwrap();
        for file in [HISTORY, ROTATED] {
            assert!(fs::metadata(home.join(file)).unwrap().len() <= MAX_BYTES);
            assert_eq!(
                fs::metadata(home.join(file)).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // A second rotation keeps only one previous generation.
        fs::write(home.join(HISTORY), &repeated).unwrap();
        save(&home, &settings, "window").unwrap();
        assert_eq!(history(&home, 1).unwrap()[0].source, "window");
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn e65_history_refuses_unsafe_paths_and_invalid_settings_without_applying() {
        let home = home();
        let settings = StoredSettings::default();
        let victim = home.join("untouched");
        fs::write(&victim, "keep me").unwrap();
        std::os::unix::fs::symlink(&victim, home.join(HISTORY)).unwrap();
        assert!(save(&home, &settings, "cli").is_err());
        assert!(!home.join("settings.json").exists());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep me");
        fs::remove_file(home.join(HISTORY)).unwrap();
        let mut invalid = settings;
        invalid.overrides.min_cosine = Some(2.0);
        assert!(save(&home, &invalid, "cli").is_err());
        assert!(!home.join(HISTORY).exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn e65_history_never_copies_provider_url_credentials_or_command_tokens() {
        let home = home();
        let mut settings = StoredSettings::default();
        let provider: crate::llm::LlmProvider = serde_json::from_value(json!({
            "name":"test", "base_url":"https://user:secret@example.com/?key=secret", "model":"test",
            "api_key_env":"E65_API_KEY", "command":"tool --token secret"
        }))
        .unwrap();
        settings.llm_providers.push(provider);
        save(&home, &settings, "cli").unwrap();
        let text = fs::read_to_string(home.join(HISTORY)).unwrap();
        assert!(!text.contains("secret"));
        assert!(text.contains("E65_API_KEY"));
        assert!(text.contains("[redacted]"));
        fs::remove_dir_all(home).unwrap();
    }
}
