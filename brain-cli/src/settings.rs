//! Persisted quality settings: the profile the user picked, the per-knob
//! overrides layered on top of it, and how the embedding model may run.
//!
//! Every number here was hard-coded once. Making it the user's to change is the
//! product, so the choice lives in one versioned file that every later command
//! reads, and the price of a change is computable before it is committed.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use brain_core::profile::{Profile, QualitySettings};
use serde::{Deserialize, Serialize};

/// Schema version of the settings file, bumped when the shape changes.
const SETTINGS_VERSION: u32 = 1;

/// Inclusive bounds the interface offers for each knob.
///
/// The ceilings are deliberately generous: the owner's whole point is that the
/// user decides how much to spend. They exist only so a typo cannot ask for a
/// pool of ten million candidates and stall every later command.
pub(crate) const CANDIDATES_RANGE: (usize, usize) = (10, 4_000);
pub(crate) const GRAPH_HOPS_RANGE: (u32, u32) = (0, 4);
pub(crate) const BUDGET_TOKENS_RANGE: (usize, usize) = (100, 20_000);
pub(crate) const MIN_COSINE_RANGE: (f64, f64) = (0.0, 0.99);
pub(crate) const MIN_BM25_RANGE: (f64, f64) = (0.0, 0.99);

/// How young an archive entry must be before `brain ask` stops treating it as
/// history. The conversation an agent is having right now quotes the very
/// query being asked, so without this gate its own messages outrank the
/// weeks-old fact the question is about. 0 disables the gate.
pub(crate) const DEFAULT_ARCHIVE_MIN_AGE_MINUTES: u32 = 120;

/// Hop count past which published graph-retrieval results report noise growing
/// faster than recall. Shown as a warning, never enforced.
pub(crate) const GRAPH_HOPS_DIMINISHING: u32 = 2;

/// How the local embedding model is allowed to run.
///
/// The mode is a cost decision like every other setting: resident memory versus
/// per-batch load time versus no semantic search at all.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum EmbeddingMode {
    /// Keep the model resident so answers never wait for a load.
    #[default]
    Always,
    /// Load the model per batch and release the memory afterwards.
    OnDemand,
    /// Never load the model; BM25 and exact matches only.
    Off,
}

impl fmt::Display for EmbeddingMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Always => "always",
            Self::OnDemand => "on-demand",
            Self::Off => "off",
        };
        formatter.write_str(name)
    }
}

impl FromStr for EmbeddingMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "always" => Ok(Self::Always),
            "on-demand" | "on_demand" | "ondemand" => Ok(Self::OnDemand),
            "off" | "never" => Ok(Self::Off),
            other => Err(format!(
                "unknown embedding mode {other:?}; expected always, on-demand, or off"
            )),
        }
    }
}

/// Per-knob values that win over whatever the profile says.
///
/// Absent fields inherit from the profile, so a user who tunes one number keeps
/// tracking the profile for everything else.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub(crate) struct QualityOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidates_per_signal: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_hops: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_cosine: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_bm25: Option<f64>,
}

impl QualityOverrides {
    /// True when nothing is overridden and the profile speaks for itself.
    pub(crate) fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The whole persisted settings document.
///
/// The profile is stored by name rather than as a parsed value so an unknown
/// name from a newer build degrades to the default instead of failing to load.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct StoredSettings {
    pub version: u32,
    pub profile: String,
    #[serde(default)]
    pub overrides: QualityOverrides,
    #[serde(default)]
    pub embedding_mode: EmbeddingMode,
    #[serde(default = "default_include_archive")]
    pub include_archive: bool,
    #[serde(default = "default_archive_min_age_minutes")]
    pub archive_min_age_minutes: u32,
    #[serde(default)]
    pub backup_dir: String,
    #[serde(default = "default_backup_keep")]
    pub backup_keep: u32,
    #[serde(default)]
    pub backup_timer_enabled: bool,
    /// Tell a starting Claude Code or Codex session that Brain is available (fixed text only).
    #[serde(default = "default_session_start_notice")]
    pub session_start_notice: bool,
    /// Plug-in LLM providers (compute nodes), tried by priority.
    #[serde(default)]
    pub llm_providers: Vec<crate::llm::LlmProvider>,
    /// SessionStart briefing mode: "off" (default) or "brief" (cached project brief).
    #[serde(default)]
    pub session_start_brief: String,
}

fn default_session_start_notice() -> bool {
    true
}

fn default_include_archive() -> bool {
    true
}

fn default_archive_min_age_minutes() -> u32 {
    DEFAULT_ARCHIVE_MIN_AGE_MINUTES
}

fn default_backup_keep() -> u32 {
    14
}

impl Default for StoredSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            profile: Profile::default().to_string(),
            overrides: QualityOverrides::default(),
            embedding_mode: EmbeddingMode::default(),
            include_archive: true,
            archive_min_age_minutes: DEFAULT_ARCHIVE_MIN_AGE_MINUTES,
            session_start_notice: true,
            backup_dir: String::new(),
            backup_keep: default_backup_keep(),
            backup_timer_enabled: false,
            llm_providers: Vec::new(),
            session_start_brief: String::from("off"),
        }
    }
}

impl StoredSettings {
    /// The chosen profile, falling back to the default on an unknown name.
    pub(crate) fn profile(&self) -> Profile {
        self.profile.parse().unwrap_or_default()
    }

    /// True when at least one knob no longer tracks the profile.
    pub(crate) fn is_manual(&self) -> bool {
        !self.overrides.is_empty()
    }

    /// The settings search should actually run with.
    ///
    /// Starting from the profile and overwriting only the named knobs keeps
    /// fields this module does not know about — later additions to
    /// `QualitySettings` — on their profile values instead of on zero.
    pub(crate) fn effective(&self) -> QualitySettings {
        let mut quality = self.profile().settings();
        if let Some(value) = self.overrides.candidates_per_signal {
            quality.candidates_per_signal = value;
        }
        if let Some(value) = self.overrides.graph_hops {
            quality.graph_hops = value;
        }
        if let Some(value) = self.overrides.budget_tokens {
            quality.budget_tokens = value;
        }
        if let Some(value) = self.overrides.min_cosine {
            quality.min_cosine = value;
        }
        if let Some(value) = self.overrides.min_bm25 {
            quality.min_bm25 = value;
        }
        quality
    }

    /// Rejects values that would make later commands unusable.
    ///
    /// Returns the message the interface shows the user, so it names the knob
    /// and the accepted range rather than saying the input was invalid.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.version != SETTINGS_VERSION {
            return Err(format!(
                "версия настроек {} не поддерживается этой сборкой (ожидается {})",
                self.version, SETTINGS_VERSION
            ));
        }
        if self.profile.parse::<Profile>().is_err() {
            return Err(format!(
                "неизвестный профиль {:?}; ожидается fast, balanced, precise или max",
                self.profile
            ));
        }
        check_range(
            "число кандидатов на сигнал",
            self.overrides.candidates_per_signal,
            CANDIDATES_RANGE,
        )?;
        check_range(
            "глубина обхода графа",
            self.overrides.graph_hops,
            GRAPH_HOPS_RANGE,
        )?;
        check_range(
            "бюджет ответа в токенах",
            self.overrides.budget_tokens,
            BUDGET_TOKENS_RANGE,
        )?;
        check_range(
            "порог косинуса",
            self.overrides.min_cosine,
            MIN_COSINE_RANGE,
        )?;
        check_range("порог BM25", self.overrides.min_bm25, MIN_BM25_RANGE)?;
        Ok(())
    }
}

/// Settings together with every recovery decision made while loading them.
///
/// Search must stay usable when one hand-edited field is bad, but silently
/// changing the user's profile or quality knobs is worse than failing. Callers
/// that have a UI can render these warnings; command-line callers use `load`,
/// which writes them to stderr.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LoadedSettings {
    pub settings: StoredSettings,
    pub warnings: Vec<String>,
}

/// Rejects one optional knob that falls outside its inclusive bounds.
fn check_range<T>(name: &str, value: Option<T>, range: (T, T)) -> Result<(), String>
where
    T: PartialOrd + fmt::Display,
{
    let Some(value) = value else {
        return Ok(());
    };
    if value < range.0 || value > range.1 {
        return Err(format!(
            "{name}: {value} вне допустимого диапазона {}..{}",
            range.0, range.1
        ));
    }
    Ok(())
}

/// Parses and applies one memory-related setting from its raw text form.
///
/// `brain config set` and the settings window's POST both land here, so a
/// value one path accepts the other never rejects, and the error texts are
/// the same everywhere. Unknown keys are refused with the supported list.
pub(crate) fn apply_memory_setting(
    settings: &mut StoredSettings,
    key: &str,
    raw: &str,
) -> Result<(), String> {
    match key {
        "include_archive" => {
            settings.include_archive = raw
                .parse::<bool>()
                .map_err(|_| "include_archive must be true or false".to_owned())?;
        }
        "session_start_notice" => {
            settings.session_start_notice = raw
                .parse::<bool>()
                .map_err(|_| "session_start_notice must be true or false".to_owned())?;
        }
        "archive_min_age_minutes" => {
            settings.archive_min_age_minutes = parse_minutes(raw)?;
        }
        "backup_dir" => {
            // Empty resets to the default location; a non-empty value must be
            // an absolute path so the timer and the CLI agree.
            if raw.is_empty() {
                settings.backup_dir = String::new();
            } else if raw.starts_with('/') {
                settings.backup_dir = raw.to_owned();
            } else {
                return Err(
                    "backup_dir must be an absolute path (or empty for the default)".to_owned(),
                );
            }
        }
        "backup_keep" => {
            settings.backup_keep = raw
                .parse::<u64>()
                .ok()
                .and_then(|keep| u32::try_from(keep).ok())
                .filter(|keep| (1..=1000).contains(keep))
                .ok_or_else(|| {
                    "backup_keep must be a whole number of snapshots from 1 to 1000".to_owned()
                })?;
        }
        "backup_timer_enabled" => {
            settings.backup_timer_enabled = raw
                .parse::<bool>()
                .map_err(|_| "backup_timer_enabled must be true or false".to_owned())?;
        }
        "session_start_brief" => match raw {
            "off" | "brief" => settings.session_start_brief = raw.to_owned(),
            other => {
                return Err(format!(
                    "session_start_brief must be off or brief (got {other:?})"
                ));
            }
        },
        _ => {
            return Err(format!(
                "unknown setting {key:?}; supported: profile, include_archive, archive_min_age_minutes, session_start_notice"
            ));
        }
    }
    Ok(())
}

/// Whole minutes, 0 to u32::MAX; 0 disables the age filter.
pub(crate) fn parse_minutes(raw: &str) -> Result<u32, String> {
    raw.parse::<u64>()
        .ok()
        .and_then(|minutes| u32::try_from(minutes).ok())
        .ok_or_else(|| {
            "archive_min_age_minutes must be a whole number of minutes from 0 to 4294967295 (0 disables the age filter)"
                .to_owned()
        })
}

/// Where the settings document lives.
pub(crate) fn settings_path(brain_home: &Path) -> PathBuf {
    brain_home.join("settings.json")
}

/// Where the pre-settings builds kept the bare profile name.
pub(crate) fn legacy_profile_path(brain_home: &Path) -> PathBuf {
    brain_home.join("profile")
}

/// Reads the settings and reports every fallback rather than failing silently.
///
/// A corrupt or unreadable file must not break search: the user still gets
/// working defaults instead of an error at every command. When no settings
/// document exists yet, the bare profile file written by earlier builds is
/// honoured so an upgrade does not silently reset the user's choice.
pub(crate) fn load_with_diagnostics(brain_home: &Path) -> LoadedSettings {
    let mut settings = legacy_or_default(brain_home);
    let mut warnings = Vec::new();
    let path = settings_path(brain_home);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return LoadedSettings { settings, warnings };
        }
        Err(error) => {
            warnings.push(format!(
                "не удалось прочитать settings.json: {error}; применены совместимые резервные значения"
            ));
            return LoadedSettings { settings, warnings };
        }
    };
    let document = match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(serde_json::Value::Object(document)) => document,
        Ok(_) => {
            warnings.push(
                "settings.json должен содержать JSON-объект; применены совместимые резервные значения"
                    .to_owned(),
            );
            return LoadedSettings { settings, warnings };
        }
        Err(error) => {
            warnings.push(format!(
                "settings.json повреждён ({error}); применены совместимые резервные значения"
            ));
            return LoadedSettings { settings, warnings };
        }
    };

    let version = document.get("version").and_then(serde_json::Value::as_u64);
    if version != Some(u64::from(SETTINGS_VERSION)) {
        let found = document
            .get("version")
            .map_or_else(|| "отсутствует".to_owned(), serde_json::Value::to_string);
        warnings.push(format!(
            "settings.json имеет неподдерживаемую версию {found}; документ не применён (эта сборка понимает только версию {SETTINGS_VERSION})"
        ));
        return LoadedSettings { settings, warnings };
    }

    if let Some(value) = document.get("profile") {
        match value.as_str().and_then(|name| name.parse::<Profile>().ok()) {
            Some(profile) => settings.profile = profile.to_string(),
            None => warnings.push(format!(
                "profile имеет неподдерживаемое значение; сохранён резервный профиль {}",
                settings.profile
            )),
        }
    } else {
        warnings.push(format!(
            "в settings.json отсутствует обязательный profile; сохранён резервный профиль {}",
            settings.profile
        ));
    }
    if let Some(value) = document.get("embedding_mode") {
        match value
            .as_str()
            .and_then(|name| name.parse::<EmbeddingMode>().ok())
        {
            Some(mode) => settings.embedding_mode = mode,
            None => warnings.push(format!(
                "embedding_mode имеет неподдерживаемое значение; применён режим {}",
                settings.embedding_mode
            )),
        }
    }
    if let Some(value) = document.get("session_start_brief") {
        match value.as_str() {
            Some(mode @ ("off" | "brief")) => settings.session_start_brief = mode.to_owned(),
            _ => warnings
                .push("session_start_brief должен быть off или brief; применено off".to_owned()),
        }
    }
    if let Some(value) = document.get("session_start_notice") {
        match value.as_bool() {
            Some(notice) => settings.session_start_notice = notice,
            None => warnings.push(
                "session_start_notice must be a boolean; the default (on) is used".to_owned(),
            ),
        }
    }
    if let Some(value) = document.get("include_archive") {
        match value.as_bool() {
            Some(include) => settings.include_archive = include,
            None => {
                // A malformed privacy switch must not silently enable archive reads.
                settings.include_archive = false;
                warnings
                    .push("include_archive must be a boolean; archive search disabled".to_owned());
            }
        }
    }
    if let Some(value) = document.get("archive_min_age_minutes") {
        match value
            .as_u64()
            .and_then(|minutes| u32::try_from(minutes).ok())
        {
            Some(minutes) => settings.archive_min_age_minutes = minutes,
            None => warnings.push(format!(
                "archive_min_age_minutes must be a whole number of minutes from 0 to {}; kept {}",
                u32::MAX,
                settings.archive_min_age_minutes
            )),
        }
    }
    if let Some(value) = document.get("backup_dir") {
        match value.as_str() {
            Some(dir) if dir.is_empty() || dir.starts_with('/') => {
                settings.backup_dir = dir.to_owned();
            }
            _ => warnings.push(
                "backup_dir должен быть абсолютным путём или пустым; применено значение по умолчанию"
                    .to_owned(),
            ),
        }
    }
    if let Some(value) = document.get("backup_keep") {
        match value.as_u64().and_then(|keep| u32::try_from(keep).ok()) {
            Some(keep) if (1..=1000).contains(&keep) => settings.backup_keep = keep,
            _ => warnings.push(
                "backup_keep должен быть целым числом от 1 до 1000; применено значение по умолчанию"
                    .to_owned(),
            ),
        }
    }
    if let Some(value) = document.get("backup_timer_enabled") {
        match value.as_bool() {
            Some(enabled) => settings.backup_timer_enabled = enabled,
            None => warnings.push(
                "backup_timer_enabled должен быть true или false; применено значение по умолчанию"
                    .to_owned(),
            ),
        }
    }
    if let Some(value) = document.get("llm_providers") {
        match value.as_array() {
            Some(items) => {
                let mut providers = Vec::new();
                for (index, item) in items.iter().enumerate() {
                    match serde_json::from_value::<crate::llm::LlmProvider>(item.clone()) {
                        Ok(provider) if crate::llm::validate_provider(&provider).is_ok() => {
                            providers.push(provider);
                        }
                        _ => warnings.push(format!(
                            "llm_providers[{index}] не прошёл проверку (имя, https/loopback, модель); элемент отброшен"
                        )),
                    }
                }
                settings.llm_providers = providers;
            }
            None => warnings
                .push("llm_providers должен быть массивом; список провайдеров отброшен".to_owned()),
        }
    }
    if let Some(value) = document.get("overrides") {
        if let Some(overrides) = value.as_object() {
            settings.overrides.candidates_per_signal = recover_unsigned(
                overrides,
                "candidates_per_signal",
                CANDIDATES_RANGE,
                &mut warnings,
            );
            settings.overrides.graph_hops =
                recover_unsigned(overrides, "graph_hops", GRAPH_HOPS_RANGE, &mut warnings);
            settings.overrides.budget_tokens = recover_unsigned(
                overrides,
                "budget_tokens",
                BUDGET_TOKENS_RANGE,
                &mut warnings,
            );
            settings.overrides.min_cosine =
                recover_fraction(overrides, "min_cosine", MIN_COSINE_RANGE, &mut warnings);
            settings.overrides.min_bm25 =
                recover_fraction(overrides, "min_bm25", MIN_BM25_RANGE, &mut warnings);
        } else {
            warnings
                .push("overrides должен быть объектом; все переопределения отброшены".to_owned());
        }
    }
    settings.version = SETTINGS_VERSION;
    // A typo in a key would silently do nothing: name it instead.
    const KNOWN: [&str; 12] = [
        "version",
        "profile",
        "embedding_mode",
        "session_start_brief",
        "session_start_notice",
        "include_archive",
        "archive_min_age_minutes",
        "backup_dir",
        "backup_keep",
        "backup_timer_enabled",
        "llm_providers",
        "overrides",
    ];
    let mut unknown = document
        .keys()
        .filter(|key| !KNOWN.contains(&key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    unknown.sort();
    if !unknown.is_empty() {
        warnings.push(format!(
            "settings.json содержит неизвестные поля {unknown:?}; они проигнорированы"
        ));
    }
    LoadedSettings { settings, warnings }
}

/// Loads settings for a command-line consumer and makes recovery visible.
pub(crate) fn load(brain_home: &Path) -> StoredSettings {
    let loaded = load_with_diagnostics(brain_home);
    for warning in &loaded.warnings {
        eprintln!("warning: settings: {warning}");
    }
    loaded.settings
}

fn legacy_or_default(brain_home: &Path) -> StoredSettings {
    let legacy = fs::read_to_string(legacy_profile_path(brain_home))
        .ok()
        .and_then(|text| text.trim().parse::<Profile>().ok());
    match legacy {
        Some(profile) => StoredSettings {
            profile: profile.to_string(),
            ..StoredSettings::default()
        },
        None => StoredSettings::default(),
    }
}

fn recover_unsigned<T>(
    document: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    range: (T, T),
    warnings: &mut Vec<String>,
) -> Option<T>
where
    T: Copy + PartialOrd + fmt::Display + TryFrom<u64>,
{
    let value = document.get(key)?;
    if value.is_null() {
        return None;
    }
    let parsed = value.as_u64().and_then(|number| T::try_from(number).ok());
    match parsed {
        Some(number) if number >= range.0 && number <= range.1 => Some(number),
        _ => {
            warnings.push(format!(
                "overrides.{key} вне допустимого диапазона {}..{} или имеет неверный тип; поле отброшено",
                range.0, range.1
            ));
            None
        }
    }
}

fn recover_fraction(
    document: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    range: (f64, f64),
    warnings: &mut Vec<String>,
) -> Option<f64> {
    let value = document.get(key)?;
    if value.is_null() {
        return None;
    }
    match value
        .as_f64()
        .filter(|number| number.is_finite() && *number >= range.0 && *number <= range.1)
    {
        Some(number) => Some(number),
        None => {
            warnings.push(format!(
                "overrides.{key} вне допустимого диапазона {}..{} или имеет неверный тип; поле отброшено",
                range.0, range.1
            ));
            None
        }
    }
}

/// Writes the settings so the next command picks them up.
///
/// The document is written to a sibling temporary file and renamed, so a crash
/// mid-write leaves the previous settings intact rather than a truncated file
/// that would silently reset every knob. The bare profile file is kept in sync
/// for any code path that has not moved to the settings document yet.
pub(crate) fn save(brain_home: &Path, settings: &StoredSettings) -> io::Result<()> {
    crate::settings_history::save(brain_home, settings, "cli")
}

pub(crate) fn write_document(brain_home: &Path, settings: &StoredSettings) -> io::Result<()> {
    let mut encoded = serde_json::to_vec_pretty(settings).map_err(io::Error::other)?;
    encoded.push(b'\n');
    crate::private_fs::write_private(&settings_path(brain_home), &encoded)?;
    // settings.json is authoritative. A failed compatibility mirror must not
    // erase the audit record for an already committed settings document.
    if let Err(error) = crate::private_fs::write_private(
        &legacy_profile_path(brain_home),
        settings.profile().to_string().as_bytes(),
    ) {
        eprintln!("warning: settings saved, but legacy profile mirror failed: {error}");
    }
    Ok(())
}

/// Any increase of either cutoff above the chosen profile reduces recall.
pub(crate) fn stricter_thresholds(settings: &StoredSettings) -> Vec<serde_json::Value> {
    let base = settings.profile().settings();
    let live = settings.effective();
    [("min_cosine", base.min_cosine, live.min_cosine), ("min_bm25", base.min_bm25, live.min_bm25)]
        .into_iter().filter(|(_, before, after)| after > before)
        .map(|(key, before, after)| serde_json::json!({"key": key, "profile_default": before, "effective": after})).collect()
}

/// Rough relative cost of arbitrary settings against the default profile.
///
/// This mirrors `Profile::relative_cost` so a hand-tuned point on the curve can
/// be priced the same way a named profile is; `cost_matches_profile_costs`
/// keeps the two definitions from drifting apart. Like the original it prices
/// the candidate pool and the answer budget only — graph hops are reported to
/// the user separately rather than folded into a number that would imply a
/// precision this estimate does not have.
pub(crate) fn relative_cost(settings: &QualitySettings) -> f64 {
    let base = Profile::Balanced.settings();
    (settings.candidates_per_signal as f64 / base.candidates_per_signal as f64).mul_add(
        0.6,
        settings.budget_tokens as f64 / base.budget_tokens as f64 * 0.4,
    )
}

#[cfg(test)]
mod tests {
    /// G12: the settings.json FILE table — unknown fields, wrong types and
    /// huge values never abort the load; they warn and fall back.
    #[test]
    fn settings_files_follow_the_table() {
        let cases: &[(&str, &str, usize)] = &[
            // (file content, warning fragment expected, number of warnings)
            ("not json at all", "повреждён", 1),
            ("[1,2,3]", "должен содержать JSON-объект", 1),
            (
                "{\"version\":99,\"profile\":\"balanced\"}",
                "неподдерживаемую версию",
                1,
            ),
            ("{}", "неподдерживаемую версию отсутствует", 1),
            (
                "{\"version\":1,\"profile\":\"balanced\",\"mystery_key\":true}",
                "неизвестные поля",
                1,
            ),
            // Duplicate keys collapse to the last value at the serde_json
            // layer (standard JSON behavior); nothing to warn about here.
            (
                "{\"version\":1,\"profile\":\"balanced\",\"include_archive\":\"yes\"}",
                "include_archive",
                1,
            ),
            (
                "{\"version\":1,\"profile\":\"balanced\",\"archive_min_age_minutes\":99999999999}",
                "archive_min_age_minutes",
                1,
            ),
            (
                "{\"version\":1,\"profile\":\"balanced\",\"session_start_notice\":1}",
                "session_start_notice",
                1,
            ),
            (
                "{\"version\":1,\"profile\":\"balanced\",\"embedding_mode\":\"turbo\"}",
                "embedding_mode",
                1,
            ),
            (
                "{\"version\":1,\"profile\":\"balanced\",\"session_start_brief\":true}",
                "session_start_brief",
                1,
            ),
            ("{\"version\":1,\"profile\":\"balanced\"}", "", 0),
        ];
        for (content, expected, warning_count) in cases {
            let home = crate::test_support::temp_path("settings-table");
            let _ = std::fs::remove_dir_all(&home);
            std::fs::create_dir_all(&home).expect("home");
            std::fs::write(home.join("settings.json"), content).expect("settings");
            let loaded = load_with_diagnostics(&home);
            assert_eq!(
                loaded.warnings.len(),
                *warning_count,
                "{content}: warnings were {:?}",
                loaded.warnings
            );
            if !expected.is_empty() {
                assert!(
                    loaded.warnings.iter().any(|w| w.contains(expected)),
                    "{content}: expected {expected:?} in {:?}",
                    loaded.warnings
                );
            }
            std::fs::remove_dir_all(&home).ok();
        }
        // A valid file keeps its values; a huge-but-valid minutes value loads.
        let home = crate::test_support::temp_path("settings-table-ok");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            home.join("settings.json"),
            br#"{"version":1,"profile":"balanced","embedding_mode":"off",
                 "include_archive":true,"archive_min_age_minutes":4294967295}"#,
        )
        .expect("settings");
        let loaded = load_with_diagnostics(&home);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert!(loaded.settings.include_archive);
        assert_eq!(loaded.settings.archive_min_age_minutes, 4_294_967_295);
        std::fs::remove_dir_all(&home).ok();
    }

    /// G12: every memory-setting key accepts and refuses the same values on
    /// both paths (CLI and window share this function).
    #[test]
    fn memory_settings_follow_the_value_table() {
        let mut settings = StoredSettings::default();
        let cases: &[(&str, &str, bool)] = &[
            ("include_archive", "true", true),
            ("include_archive", "false", true),
            ("include_archive", "1", false),
            ("include_archive", "yes", false),
            ("include_archive", "", false),
            ("session_start_notice", "true", true),
            ("session_start_notice", "false", true),
            ("session_start_notice", "TRUE", false),
            ("session_start_notice", "on", false),
            ("archive_min_age_minutes", "0", true),
            ("archive_min_age_minutes", "90", true),
            ("archive_min_age_minutes", "4294967295", true),
            ("archive_min_age_minutes", "4294967296", false),
            ("archive_min_age_minutes", "-5", false),
            ("archive_min_age_minutes", "1.5", false),
            ("archive_min_age_minutes", "", false),
            ("backup_keep", "1", true),
            ("backup_keep", "1000", true),
            ("backup_keep", "0", false),
            ("backup_keep", "1001", false),
            ("backup_keep", "abc", false),
            ("backup_timer_enabled", "true", true),
            ("backup_timer_enabled", "false", true),
            ("backup_timer_enabled", "1", false),
            ("session_start_brief", "off", true),
            ("session_start_brief", "brief", true),
            ("session_start_brief", "on", false),
            ("session_start_brief", "", false),
            ("backup_dir", "", true),
            ("backup_dir", "/tmp/brain-backups", true),
            ("backup_dir", "relative/path", false),
            ("no-such-key", "true", false),
        ];
        for (key, raw, expected) in cases {
            let outcome = apply_memory_setting(&mut settings, key, raw);
            assert_eq!(outcome.is_ok(), *expected, "{key}={raw:?}: {outcome:?}");
        }
        let unsupported = apply_memory_setting(&mut settings, "no-such-key", "true").unwrap_err();
        assert!(unsupported.contains("supported:"), "{unsupported}");
    }
    use super::*;

    fn temp_home(_tag: &str) -> PathBuf {
        let root = crate::test_support::temp_path("brain-settings-test");
        fs::create_dir_all(&root).expect("create temp brain home");
        root
    }

    #[test]
    fn cost_matches_profile_costs() {
        for profile in Profile::all() {
            let mirrored = relative_cost(&profile.settings());
            assert!(
                (mirrored - profile.relative_cost()).abs() < 1e-12,
                "{profile}: settings.rs must price a profile exactly as profile.rs does"
            );
        }
    }

    #[test]
    fn overrides_win_over_the_profile_but_keep_the_rest() {
        let settings = StoredSettings {
            profile: "fast".to_owned(),
            overrides: QualityOverrides {
                candidates_per_signal: Some(750),
                ..QualityOverrides::default()
            },
            ..StoredSettings::default()
        };
        let effective = settings.effective();
        assert_eq!(effective.candidates_per_signal, 750);
        assert_eq!(
            effective.budget_tokens,
            Profile::Fast.settings().budget_tokens
        );
        assert!(settings.is_manual());
    }

    #[test]
    fn saved_settings_round_trip() {
        let home = temp_home("round-trip");
        let settings = StoredSettings {
            profile: "precise".to_owned(),
            overrides: QualityOverrides {
                graph_hops: Some(2),
                min_cosine: Some(0.41),
                ..QualityOverrides::default()
            },
            embedding_mode: EmbeddingMode::OnDemand,
            ..StoredSettings::default()
        };
        save(&home, &settings).expect("settings save");
        assert_eq!(load(&home), settings);
        assert_eq!(
            fs::read_to_string(legacy_profile_path(&home)).expect("legacy profile mirror"),
            "precise",
            "older code paths must keep reading the same choice"
        );
        fs::remove_dir_all(home).expect("remove temp brain home");
    }

    #[test]
    fn the_shared_memory_setting_parser_is_strict_and_names_its_errors() {
        let mut settings = StoredSettings::default();
        assert!(apply_memory_setting(&mut settings, "include_archive", "false").is_ok());
        assert!(!settings.include_archive);
        assert!(apply_memory_setting(&mut settings, "session_start_notice", "true").is_ok());
        assert!(settings.session_start_notice);
        // Minutes: zero and the ceiling are fine, everything else is one text.
        assert_eq!(
            apply_memory_setting(&mut settings, "archive_min_age_minutes", "0"),
            Ok(())
        );
        assert_eq!(settings.archive_min_age_minutes, 0);
        assert!(
            apply_memory_setting(&mut settings, "archive_min_age_minutes", "4294967295").is_ok()
        );
        for bad in ["-5", "1.5", "soon", "4294967296"] {
            let error =
                apply_memory_setting(&mut settings, "archive_min_age_minutes", bad).expect_err(bad);
            assert!(
                error.starts_with("archive_min_age_minutes must be a whole number of minutes"),
                "{bad}: {error}"
            );
        }
        // Booleans refuse anything but true/false, with the same text everywhere.
        assert_eq!(
            apply_memory_setting(&mut settings, "include_archive", "sometimes").unwrap_err(),
            "include_archive must be true or false"
        );
        // Unknown keys name the supported list.
        assert!(
            apply_memory_setting(&mut settings, "profile", "fast")
                .unwrap_err()
                .starts_with("unknown setting")
        );
    }

    #[test]
    fn archive_min_age_round_trips_and_a_malformed_value_keeps_the_default() {
        let home = temp_home("min-age");
        let tuned = StoredSettings {
            archive_min_age_minutes: 15,
            ..StoredSettings::default()
        };
        save(&home, &tuned).expect("settings save");
        assert_eq!(load(&home).archive_min_age_minutes, 15);

        fs::write(
            settings_path(&home),
            r#"{"version":1,"profile":"balanced","archive_min_age_minutes":"soon"}"#,
        )
        .expect("write malformed min age");
        let loaded = load_with_diagnostics(&home);
        assert_eq!(
            loaded.settings.archive_min_age_minutes,
            DEFAULT_ARCHIVE_MIN_AGE_MINUTES
        );
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("archive_min_age_minutes"));
        fs::remove_dir_all(home).expect("remove temp brain home");
    }

    #[test]
    fn corrupt_document_falls_back_to_the_legacy_profile() {
        let home = temp_home("corrupt");
        fs::write(settings_path(&home), "{ not json").expect("write corrupt settings");
        fs::write(legacy_profile_path(&home), "max").expect("write legacy profile");
        let loaded = load_with_diagnostics(&home);
        assert_eq!(loaded.settings.profile(), Profile::Max);
        assert_eq!(loaded.warnings.len(), 1);
        fs::remove_dir_all(home).expect("remove temp brain home");
    }

    #[test]
    fn one_bad_override_does_not_discard_valid_settings() {
        let home = temp_home("partial-recovery");
        fs::write(
            settings_path(&home),
            r#"{
  "version": 1,
  "profile": "max",
  "embedding_mode": "off",
  "overrides": {
    "graph_hops": 3,
    "budget_tokens": 999999,
    "min_cosine": 0.33
  }
}"#,
        )
        .expect("write mixed settings");

        let loaded = load_with_diagnostics(&home);
        assert_eq!(loaded.settings.profile(), Profile::Max);
        assert_eq!(loaded.settings.embedding_mode, EmbeddingMode::Off);
        assert_eq!(loaded.settings.overrides.graph_hops, Some(3));
        assert_eq!(loaded.settings.overrides.min_cosine, Some(0.33));
        assert_eq!(loaded.settings.overrides.budget_tokens, None);
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("budget_tokens"));
        fs::remove_dir_all(home).expect("remove temp brain home");
    }

    #[test]
    fn a_future_document_is_rejected_with_a_warning() {
        let home = temp_home("future-version");
        fs::write(legacy_profile_path(&home), "precise").expect("write legacy profile");
        fs::write(
            settings_path(&home),
            r#"{
  "version": 999,
  "profile": "max",
  "embedding_mode": "off",
  "overrides": {"graph_hops": 3}
}"#,
        )
        .expect("write future settings");

        let loaded = load_with_diagnostics(&home);
        assert_eq!(loaded.settings.profile(), Profile::Precise);
        assert_eq!(loaded.settings.embedding_mode, EmbeddingMode::Always);
        assert_eq!(loaded.settings.overrides.graph_hops, None);
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("999"));
        assert!(loaded.warnings[0].contains("не применён"));
        fs::remove_dir_all(home).expect("remove temp brain home");
    }

    #[test]
    fn malformed_fields_are_recovered_independently() {
        let home = temp_home("wrong-types");
        fs::write(
            settings_path(&home),
            r#"{
  "version": 1,
  "profile": "precise",
  "embedding_mode": 7,
  "overrides": {
    "candidates_per_signal": "many",
    "graph_hops": 2,
    "min_bm25": 0.44
  }
}"#,
        )
        .expect("write malformed settings");

        let loaded = load_with_diagnostics(&home);
        assert_eq!(loaded.settings.profile(), Profile::Precise);
        assert_eq!(loaded.settings.embedding_mode, EmbeddingMode::Always);
        assert_eq!(loaded.settings.overrides.candidates_per_signal, None);
        assert_eq!(loaded.settings.overrides.graph_hops, Some(2));
        assert_eq!(loaded.settings.overrides.min_bm25, Some(0.44));
        assert_eq!(loaded.warnings.len(), 2);
        fs::remove_dir_all(home).expect("remove temp brain home");
    }

    #[test]
    fn a_missing_required_profile_is_visible() {
        let home = temp_home("missing-profile");
        fs::write(
            settings_path(&home),
            r#"{"version":1,"embedding_mode":"off","overrides":{"graph_hops":2}}"#,
        )
        .expect("write settings without profile");

        let loaded = load_with_diagnostics(&home);
        assert_eq!(loaded.settings.profile(), Profile::Balanced);
        assert_eq!(loaded.settings.embedding_mode, EmbeddingMode::Off);
        assert_eq!(loaded.settings.overrides.graph_hops, Some(2));
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("profile"));
        fs::remove_dir_all(home).expect("remove temp brain home");
    }

    #[test]
    fn missing_files_produce_the_shipped_defaults() {
        let home = temp_home("missing");
        let loaded = load(&home);
        assert_eq!(loaded.profile(), Profile::Balanced);
        assert_eq!(loaded.embedding_mode, EmbeddingMode::Always);
        assert!(!loaded.is_manual());
        fs::remove_dir_all(home).expect("remove temp brain home");
    }

    #[test]
    fn out_of_range_values_are_rejected_with_the_range_named() {
        let settings = StoredSettings {
            overrides: QualityOverrides {
                budget_tokens: Some(1_000_000),
                ..QualityOverrides::default()
            },
            ..StoredSettings::default()
        };
        let error = settings.validate().expect_err("must reject");
        assert!(
            error.contains("20000"),
            "error should quote the accepted range"
        );
    }

    #[test]
    fn future_versions_cannot_be_saved_as_current_settings() {
        let home = temp_home("future-save");
        let settings = StoredSettings {
            version: SETTINGS_VERSION + 1,
            ..StoredSettings::default()
        };
        let error = save(&home, &settings).expect_err("future schema must not be written");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!settings_path(&home).exists());
        fs::remove_dir_all(home).expect("remove temp brain home");
    }

    #[test]
    fn embedding_mode_names_round_trip() {
        for mode in [
            EmbeddingMode::Always,
            EmbeddingMode::OnDemand,
            EmbeddingMode::Off,
        ] {
            let parsed: EmbeddingMode = mode.to_string().parse().expect("name parses back");
            assert_eq!(parsed, mode);
        }
    }
}
