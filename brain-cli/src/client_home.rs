//! Classifies agent-client home overrides (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`,
//! `HERMES_HOME`) from their raw process-environment values only. Nothing here
//! touches the filesystem: an override is never canonicalized, stat'ed, read
//! or followed, and its value is never reported, only the variable name and a
//! fixed state.
use serde_json::{Value, json};
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HomeState {
    /// The variable is not set.
    Unset,
    /// Set to the empty string; treated as the default location.
    Empty,
    /// An absolute path lexically equal to the default location.
    DefaultEquivalent,
    /// An absolute path elsewhere; Brain does not follow it.
    UnsupportedOverride,
    /// Relative, non-UTF-8 or `..`-containing; cannot be resolved without
    /// filesystem access, so it is not followed either.
    UnresolvedOverride,
}

impl HomeState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Unset => "unset",
            Self::Empty => "empty",
            Self::DefaultEquivalent => "default_equivalent",
            Self::UnsupportedOverride => "unsupported_override",
            Self::UnresolvedOverride => "unresolved_override",
        }
    }

    /// True when the client's real home may differ from the default one.
    pub(crate) const fn relocated(self) -> bool {
        matches!(self, Self::UnsupportedOverride | Self::UnresolvedOverride)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ClientHome {
    pub(crate) variable: &'static str,
    pub(crate) state: HomeState,
}

impl ClientHome {
    pub(crate) fn location(self) -> Value {
        json!({"variable": self.variable, "state": self.state.as_str()})
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ClientHomes {
    pub(crate) claude: ClientHome,
    pub(crate) codex: ClientHome,
    pub(crate) hermes: ClientHome,
}

pub(crate) const CLAUDE_VARIABLE: &str = "CLAUDE_CONFIG_DIR";
pub(crate) const CODEX_VARIABLE: &str = "CODEX_HOME";
pub(crate) const HERMES_VARIABLE: &str = "HERMES_HOME";

/// Pure: the caller injects each raw variable value.
pub(crate) fn classify_all(
    home: &Path,
    claude: Option<&OsStr>,
    codex: Option<&OsStr>,
    hermes: Option<&OsStr>,
) -> ClientHomes {
    ClientHomes {
        claude: classify(CLAUDE_VARIABLE, claude, &home.join(".claude")),
        codex: classify(CODEX_VARIABLE, codex, &home.join(".codex")),
        hermes: classify(HERMES_VARIABLE, hermes, &home.join(".hermes")),
    }
}

/// Reads the three variables from this process's environment.
pub(crate) fn from_process_env(home: &Path) -> ClientHomes {
    let read = |name: &str| -> Option<OsString> { std::env::var_os(name) };
    let (claude, codex, hermes) = (
        read(CLAUDE_VARIABLE),
        read(CODEX_VARIABLE),
        read(HERMES_VARIABLE),
    );
    classify_all(home, claude.as_deref(), codex.as_deref(), hermes.as_deref())
}

pub(crate) fn classify(variable: &'static str, raw: Option<&OsStr>, default: &Path) -> ClientHome {
    let state = match raw {
        None => HomeState::Unset,
        Some(value) if value.is_empty() => HomeState::Empty,
        Some(value) => match value.to_str() {
            None => HomeState::UnresolvedOverride,
            // Component-wise equal to the default (even when HOME itself has `..`).
            Some(text) if Path::new(text).is_absolute() && Path::new(text) == default => {
                HomeState::DefaultEquivalent
            }
            Some(text) => match (lexical(Path::new(text)), lexical(default)) {
                (Some(given), Some(expected)) if given == expected => HomeState::DefaultEquivalent,
                (Some(_), _) => HomeState::UnsupportedOverride,
                (None, _) => HomeState::UnresolvedOverride,
            },
        },
    };
    ClientHome { variable, state }
}

/// Lexical form of an absolute path: repeated separators, `.` and a trailing
/// separator are dropped. `..` is refused because it is only equivalent after
/// resolving symlinks, which needs the filesystem.
fn lexical(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Normal(_) => out.push(component),
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;

    fn state(raw: Option<&OsStr>) -> HomeState {
        classify(CODEX_VARIABLE, raw, Path::new("/synthetic/home/.codex")).state
    }

    #[test]
    fn default_spellings_are_default() {
        assert_eq!(state(None), HomeState::Unset);
        assert_eq!(state(Some(OsStr::new(""))), HomeState::Empty);
        for same in [
            "/synthetic/home/.codex",
            "/synthetic/home/.codex/",
            "//synthetic//home/./.codex//",
        ] {
            assert_eq!(
                state(Some(OsStr::new(same))),
                HomeState::DefaultEquivalent,
                "{same}"
            );
        }
        for state in [
            HomeState::Unset,
            HomeState::Empty,
            HomeState::DefaultEquivalent,
        ] {
            assert!(!state.relocated());
        }
    }

    #[test]
    fn other_values_are_overrides_and_never_touch_the_filesystem() {
        // Non-existent and forbidden-looking paths classify the same way:
        // the helper has no filesystem access to differ on.
        for other in [
            "/synthetic/elsewhere",
            "/proc/self/forbidden-brain-test",
            "/",
        ] {
            assert_eq!(
                state(Some(OsStr::new(other))),
                HomeState::UnsupportedOverride
            );
        }
        for unresolved in [
            ".codex",
            "~/.codex",
            "relative/dir",
            "/synthetic/home/x/../.codex",
        ] {
            assert_eq!(
                state(Some(OsStr::new(unresolved))),
                HomeState::UnresolvedOverride
            );
        }
        #[cfg(unix)]
        assert_eq!(
            state(Some(OsStr::from_bytes(b"/synthetic/home/\xff"))),
            HomeState::UnresolvedOverride
        );
    }

    #[test]
    fn location_reports_name_and_state_only() {
        let homes = classify_all(
            Path::new("/synthetic/home"),
            Some(OsStr::new("/secret-claude-value")),
            None,
            Some(OsStr::new("profile-secret")),
        );
        let text = serde_json::to_string(&[
            homes.claude.location(),
            homes.codex.location(),
            homes.hermes.location(),
        ])
        .unwrap();
        assert!(!text.contains("secret"));
        assert_eq!(homes.claude.location()["state"], "unsupported_override");
        assert_eq!(homes.codex.location()["state"], "unset");
        assert_eq!(homes.hermes.location()["variable"], "HERMES_HOME");
        assert_eq!(homes.hermes.state, HomeState::UnresolvedOverride);
    }
}
