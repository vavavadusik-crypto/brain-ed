//! The desktop shortcuts that open the settings window and the memory graph.
//!
//! ADR-001 chose a local page over a desktop toolkit, so the only native part
//! left is a launcher entry pointing at `brain ui`. It is written from the
//! binary itself rather than by an installer script, so the path it records is
//! the path that actually exists on this machine.

use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::json;

use crate::installer;

/// Fixed port the shortcut asks for, so clicking it twice reuses one server
/// instead of leaving a trail of listeners on random ports.
pub(crate) const SHORTCUT_PORT: u16 = 8787;

/// Basename shared by the launcher entry and the icon it references.
const ENTRY_NAME: &str = "brain-for-ai-settings";
/// Second launcher: the graph of what is stored, not how it is tuned.
const GRAPH_ENTRY_NAME: &str = "brain-for-ai-graph";

/// Icon name registered under the hicolor theme.
const ICON_NAME: &str = "brain-for-ai";

/// The SVG namespace identifier, named so the self-containment test can tell it
/// apart from an address a renderer would actually fetch.
#[cfg(test)]
const SVG_NAMESPACE: &str = "http://www.w3.org/2000/svg";

/// A flat mark rather than a photograph: it has to stay legible at 24 pixels
/// and carry no external reference beyond its own namespace declaration.
///
/// The delimiter is `r##` because the fill colours contain `"#`, which would
/// close an `r#` literal in the middle of the drawing.
pub(crate) const ICON_SVG: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" width="64" height="64">
  <rect width="64" height="64" rx="14" fill="#1b1f2a"/>
  <g fill="none" stroke="#6ee7b7" stroke-width="2.5" stroke-linecap="round">
    <path d="M32 14v10M32 40v10M22 24l-8 6M42 24l8 6M22 40l-8-6M42 40l8-6"/>
  </g>
  <g fill="#6ee7b7">
    <circle cx="32" cy="10" r="4"/><circle cx="32" cy="54" r="4"/>
    <circle cx="12" cy="22" r="4"/><circle cx="52" cy="22" r="4"/>
    <circle cx="12" cy="42" r="4"/><circle cx="52" cy="42" r="4"/>
  </g>
  <circle cx="32" cy="32" r="8" fill="none" stroke="#f8fafc" stroke-width="3"/>
</svg>
"##;

/// Second icon, so the two launchers can be told apart on a crowded desktop:
/// the same mark, coloured for the graph and with its links drawn as a mesh.
pub(crate) const GRAPH_ICON_SVG: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" width="64" height="64">
  <rect width="64" height="64" rx="14" fill="#14213d"/>
  <g fill="none" stroke="#7dd3fc" stroke-width="2.5" stroke-linecap="round">
    <path d="M14 18L32 32L50 18M14 46L32 32L50 46M14 18L14 46M50 18L50 46M14 18L50 18"/>
  </g>
  <g fill="#7dd3fc">
    <circle cx="14" cy="18" r="5"/><circle cx="50" cy="18" r="5"/>
    <circle cx="14" cy="46" r="5"/><circle cx="50" cy="46" r="5"/>
  </g>
  <circle cx="32" cy="32" r="7" fill="#fde68a"/>
</svg>
"##;

/// Icon name of the graph launcher.
const GRAPH_ICON_NAME: &str = "brain-for-ai-graph";

/// Every place a launcher file goes, resolved once from the environment.
///
/// Resolving here rather than inside the writer keeps tests and `--dry-run`
/// honest: a fixture passes its own layout and can never reach the real
/// desktop through an inherited `XDG_*` variable.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Layout {
    applications: PathBuf,
    icons: PathBuf,
    desktop: Option<PathBuf>,
}

impl Layout {
    /// Applies the XDG base-directory rules: a relative value is invalid and
    /// falls back to the default, as the specification requires.
    fn resolve(home: &Path, variable: impl Fn(&str) -> Option<String>) -> Self {
        let absolute = |name: &str| {
            variable(name)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };
        let data_home = absolute("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
        let config_home = absolute("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        Self {
            applications: data_home.join("applications"),
            icons: data_home.join("icons/hicolor/scalable/apps"),
            desktop: desktop_directory(home, &config_home, absolute("XDG_DESKTOP_DIR")),
        }
    }

    fn from_environment(home: &Path) -> Self {
        Self::resolve(home, |name| std::env::var(name).ok())
    }

    fn settings_icon(&self) -> PathBuf {
        self.icons.join(format!("{ICON_NAME}.svg"))
    }

    fn graph_icon(&self) -> PathBuf {
        self.icons.join(format!("{GRAPH_ICON_NAME}.svg"))
    }

    fn menu_entries(&self) -> [PathBuf; 2] {
        [
            self.applications.join(format!("{ENTRY_NAME}.desktop")),
            self.applications
                .join(format!("{GRAPH_ENTRY_NAME}.desktop")),
        ]
    }

    fn desktop_entries(&self) -> Option<[PathBuf; 2]> {
        self.desktop.as_ref().map(|directory| {
            [
                directory.join(format!("{ENTRY_NAME}.desktop")),
                directory.join(format!("{GRAPH_ENTRY_NAME}.desktop")),
            ]
        })
    }
}

/// Writes both launcher entries, their icons, and copies on the desktop.
pub(crate) fn install(dry_run: bool) -> Result<(), Box<dyn Error>> {
    let home = crate::user_home();
    let brain_home = crate::brain_home();
    // Before the running image or `user-dirs.dirs` is looked at.
    crate::private_fs::require_safe_homes(&home, &brain_home)?;
    let running_binary = crate::stable_brain_binary()?;
    let layout = Layout::from_environment(&home);
    let report = if dry_run {
        plan_install(&running_binary, &brain_home, &layout)?
    } else {
        install_from(&running_binary, &brain_home, &layout)?
    };
    crate::print_json(&report)?;
    Ok(())
}

/// Every path a launcher operation may touch must pass the shared HOME guard.
/// XDG values are only required to be absolute by `Layout::resolve`, so a
/// control character arriving through them is refused here too.
fn check_layout(brain_home: &Path, layout: &Layout) -> Result<(), Box<dyn Error>> {
    let mut paths = vec![
        brain_home,
        layout.applications.as_path(),
        layout.icons.as_path(),
    ];
    if let Some(desktop) = &layout.desktop {
        paths.push(desktop.as_path());
    }
    crate::private_fs::require_safe_paths(&paths)
}

/// What `install` would write, computed without touching the filesystem.
fn plan_install(
    running_binary: &Path,
    brain_home: &Path,
    layout: &Layout,
) -> Result<serde_json::Value, Box<dyn Error>> {
    check_layout(brain_home, layout)?;
    let binary = installer::stable_copy_path(brain_home);
    // Rendering validates the path, so a dry run fails exactly where a real
    // install would.
    render_entry(&binary)?;
    render_graph_entry(&binary)?;
    let [menu, graph_menu] = layout.menu_entries();
    let desktop = layout.desktop_entries();
    Ok(json!({
        "status": "dry_run",
        "would_copy": {"from": running_binary, "to": binary},
        "would_write": {
            "menu_entry": menu,
            "graph_menu_entry": graph_menu,
            "desktop_entry": desktop.as_ref().map(|[settings, _]| settings),
            "graph_desktop_entry": desktop.as_ref().map(|[_, graph]| graph),
            "icon": layout.settings_icon(),
            "graph_icon": layout.graph_icon(),
            "owner_marker": installer::desktop_copy_owner_path(brain_home),
        },
        "desktop_directory_found": layout.desktop.is_some(),
    }))
}

fn install_from(
    running_binary: &Path,
    brain_home: &Path,
    layout: &Layout,
) -> Result<serde_json::Value, Box<dyn Error>> {
    check_layout(brain_home, layout)?;
    let binary = installer::stable_copy_path(brain_home);
    installer::install_stable_copy(running_binary, &binary)?;
    probe_brain_binary(&binary)?;
    let entry = render_entry(&binary)?;
    let graph_entry = render_graph_entry(&binary)?;

    // Claim the shared stable copy before writing any launcher. If a later
    // filesystem operation fails, agent uninstall will retain the binary and
    // cannot turn a partially installed shortcut into a dead one.
    write_file(
        &installer::desktop_copy_owner_path(brain_home),
        b"brain-for-ai-settings-v1\n",
    )?;

    write_file(&layout.settings_icon(), ICON_SVG.as_bytes())?;
    write_file(&layout.graph_icon(), GRAPH_ICON_SVG.as_bytes())?;

    let [menu_path, graph_menu_path] = layout.menu_entries();
    write_file(&menu_path, entry.as_bytes())?;
    write_file(&graph_menu_path, graph_entry.as_bytes())?;

    // A launcher on the desktop only works when it is executable and, on GNOME,
    // explicitly trusted; both are reported so a silent half-install is visible.
    // `gio` may be absent (non-GNOME desktops, minimal systems): that is a
    // reported `false`, not a failure.
    let desktop_entries = layout.desktop_entries();
    let mut trusted = false;
    if let Some([settings_path, graph_path]) = &desktop_entries {
        write_file(settings_path, entry.as_bytes())?;
        make_executable(settings_path)?;
        write_file(graph_path, graph_entry.as_bytes())?;
        make_executable(graph_path)?;
        trusted = mark_trusted(settings_path) & mark_trusted(graph_path);
    }

    Ok(json!({
        "status": "shortcut_installed",
        "menu_entry": menu_path,
        "graph_menu_entry": graph_menu_path,
        "desktop_entry": desktop_entries.as_ref().map(|[settings, _]| settings),
        "graph_desktop_entry": desktop_entries.as_ref().map(|[_, graph]| graph),
        "desktop_entry_written": desktop_entries.is_some(),
        "marked_trusted": trusted,
        "icon": layout.settings_icon(),
        "graph_icon": layout.graph_icon(),
        "binary": binary,
        "opens": format!("brain ui --port {SHORTCUT_PORT}"),
        "graph_opens": format!("brain ui --port {SHORTCUT_PORT} --page graph"),
        "hint": if desktop_entries.is_none() {
            "папка рабочего стола не найдена: ярлыки есть только в меню приложений"
        } else if trusted {
            "ярлык готов"
        } else {
            "если рабочий стол просит подтверждение, разрешите запуск в контекстном меню ярлыка"
        },
    }))
}

/// Removes everything `install` wrote, reporting what was actually there.
///
/// Memory, settings and archives under the Brain home are never touched:
/// uninstalling a launcher must not cost anyone their data.
pub(crate) fn remove(dry_run: bool) -> Result<(), Box<dyn Error>> {
    let home = crate::user_home();
    let brain_home = crate::brain_home();
    crate::private_fs::require_safe_homes(&home, &brain_home)?;
    let layout = Layout::from_environment(&home);
    let report = remove_from(&brain_home, &layout, dry_run)?;
    crate::print_json(&report)?;
    Ok(())
}

fn remove_from(
    brain_home: &Path,
    layout: &Layout,
    dry_run: bool,
) -> Result<serde_json::Value, Box<dyn Error>> {
    check_layout(brain_home, layout)?;
    let mut candidates = layout.menu_entries().to_vec();
    candidates.push(layout.settings_icon());
    candidates.push(layout.graph_icon());
    if let Some(entries) = layout.desktop_entries() {
        candidates.extend(entries);
    }
    candidates.push(installer::desktop_copy_owner_path(brain_home));
    // Agent hooks have their own manifest and may still need the shared copy.
    // When no integration owns it any more, shortcut removal cleans it up.
    if !brain_home.join("install-manifest.json").exists() {
        candidates.push(installer::stable_copy_path(brain_home));
    }

    let mut removed = Vec::new();
    for path in candidates {
        if dry_run {
            if fs::symlink_metadata(&path).is_ok() {
                removed.push(path);
            }
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => removed.push(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(if dry_run {
        json!({"status": "dry_run", "would_remove": removed, "kept": brain_home})
    } else {
        json!({"status": "shortcut_removed", "removed": removed, "kept": brain_home})
    })
}

/// Builds the launcher entry text.
fn render_entry(binary: &Path) -> Result<String, String> {
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.0\n\
         Name=Brain ED settings\n\
         Name[ru]=Brain ED — настройки\n\
         Comment=Brain memory settings, connections and diagnostics\n\
         Comment[ru]=Настройки, подключения и диагностика памяти Brain\n\
         Exec={} ui --port {SHORTCUT_PORT}\n\
         Icon={ICON_NAME}\n\
         Terminal=false\n\
         Categories=Utility;Development;\n\
         Keywords=brain;memory;settings;память;настройки;\n\
         StartupNotify=false\n",
        quote_exec_argument(binary)?
    ))
}

/// The graph launcher.
///
/// Numbers in a settings window do not tell anyone whether the memory knows
/// what they think it knows; the graph does, so it gets a door of its own
/// rather than living one click inside the settings.
fn render_graph_entry(binary: &Path) -> Result<String, String> {
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.0\n\
         Name=Brain ED graph\n\
         Name[ru]=Brain ED — граф памяти\n\
         Comment=Everything the Brain memory holds: search, filters, links, timeline\n\
         Comment[ru]=Вся память Brain: поиск, фильтры, связи, хронология\n\
         Exec={} ui --port {SHORTCUT_PORT} --page graph\n\
         Icon={GRAPH_ICON_NAME}\n\
         Terminal=false\n\
         Categories=Utility;Development;\n\
         Keywords=brain;memory;graph;память;граф;\n\
         StartupNotify=false\n",
        quote_exec_argument(binary)?
    ))
}

/// Quotes one `Exec=` argument the way the desktop entry specification asks.
///
/// A path containing a space or a quote would otherwise be split into two
/// arguments and the shortcut would launch nothing.
fn quote_exec_argument(path: &Path) -> Result<String, String> {
    let raw = path.to_string_lossy();
    if raw.contains(['\n', '\r', '=', '\0']) {
        return Err(
            "путь к brain содержит недопустимый символ и не может быть записан в Exec=".to_owned(),
        );
    }
    // Percent introduces desktop-entry field codes even inside quotes. `%%`
    // is the specified literal percent representation.
    let escaped_percent = raw.replace('%', "%%");
    if escaped_percent
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "/._-+".contains(character))
    {
        return Ok(escaped_percent);
    }
    // Exec quoting is interpreted after the desktop string escape layer.
    // Double both layers for reserved quote/dollar/backtick/backslash bytes.
    let mut escaped = String::new();
    for character in escaped_percent.chars() {
        match character {
            '\\' => escaped.push_str("\\\\\\\\"),
            '"' => escaped.push_str("\\\\\""),
            '$' => escaped.push_str("\\\\$"),
            '`' => escaped.push_str("\\\\`"),
            other => escaped.push(other),
        }
    }
    Ok(format!("\"{escaped}\""))
}

/// The user's desktop folder, whatever it is called in their language.
///
/// Reads `user-dirs.dirs` directly, so `xdg-user-dir` need not be installed.
/// A desktop set to the home directory itself means "no desktop" in the XDG
/// convention; launchers then go only to the application menu.
fn desktop_directory(
    home: &Path,
    config_home: &Path,
    configured: Option<PathBuf>,
) -> Option<PathBuf> {
    let usable = |path: PathBuf| (path.is_dir() && path != home).then_some(path);
    if let Some(path) = configured.and_then(usable) {
        return Some(path);
    }
    if let Ok(text) = fs::read_to_string(config_home.join("user-dirs.dirs")) {
        let configured = text
            .lines()
            .filter_map(|line| line.trim().strip_prefix("XDG_DESKTOP_DIR="))
            .next_back()
            .and_then(|value| parse_user_dir(value, home));
        if let Some(path) = configured {
            return usable(path);
        }
    }
    [home.join("Desktop"), home.join("Рабочий стол")]
        .into_iter()
        .find_map(usable)
}

/// One `user-dirs.dirs` value: `"$HOME/…"` or an absolute path, in quotes.
fn parse_user_dir(value: &str, home: &Path) -> Option<PathBuf> {
    let value = value.trim().strip_prefix('"')?.strip_suffix('"')?;
    let value = value.replace("\\\"", "\"");
    if let Some(rest) = value.strip_prefix("$HOME") {
        let rest = rest.trim_start_matches('/');
        return Some(if rest.is_empty() {
            home.to_path_buf()
        } else {
            home.join(rest)
        });
    }
    let path = PathBuf::from(value);
    path.is_absolute().then_some(path)
}

fn write_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)
}

fn probe_brain_binary(path: &Path) -> Result<(), Box<dyn Error>> {
    let output = run_version_probe(path)
        .map_err(|error| format!("не удалось запустить стабильную копию brain: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "стабильная копия brain не прошла проверку --version ({}): {}",
            output.status,
            stderr.chars().take(300).collect::<String>()
        )
        .into());
    }
    Ok(())
}

/// Runs `--version`, retrying briefly while the fresh copy is "text busy".
///
/// A child forked by another thread inherits the descriptor the copy was
/// written through until that child execs, and execve(2) refuses a file open
/// for writing with ETXTBSY. The window is milliseconds; failing the install
/// on it would report a broken binary that is merely young.
fn run_version_probe(path: &Path) -> io::Result<std::process::Output> {
    const TEXT_FILE_BUSY: i32 = 26;
    let mut attempt = 0;
    loop {
        let result = Command::new(path)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();
        match result {
            Err(error) if error.raw_os_error() == Some(TEXT_FILE_BUSY) && attempt < 20 => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            other => return other,
        }
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Asks the desktop to trust the launcher, and reports whether it agreed.
///
/// GNOME refuses to run an untrusted entry, so skipping this leaves a shortcut
/// that looks installed and does nothing when clicked.
fn mark_trusted(path: &Path) -> bool {
    Command::new("gio")
        .arg("set")
        .arg(path)
        .arg("metadata::trusted")
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(all(test, unix))]
mod home_guard_tests {
    use super::*;
    use crate::private_fs::UNSAFE_HOME;
    use crate::private_fs::tests::{snapshot, unsafe_home_pairs};
    use std::os::unix::fs::PermissionsExt;

    /// Built directly: `Layout::resolve` would look for a desktop folder,
    /// which for a relative HOME means the current directory.
    fn layout_for(home: &Path) -> Layout {
        Layout {
            applications: home.join(".local/share/applications"),
            icons: home.join(".local/share/icons/hicolor/scalable/apps"),
            desktop: Some(home.join("Desktop")),
        }
    }

    #[test]
    fn unusable_homes_refuse_every_launcher_operation_before_any_io() {
        let _nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = crate::test_support::temp_path("brain-desktop-guard");
        let good = root.join("ok");
        let good_brain = good.join(".brain");
        fs::create_dir_all(good.join("Desktop")).unwrap();
        fs::create_dir_all(good_brain.join("bin")).unwrap();
        fs::write(good_brain.join("brain.sqlite"), b"user data").unwrap();
        fs::write(good_brain.join("bin/brain"), b"#!/bin/sh\n").unwrap();
        fs::write(
            good.join(format!("Desktop/{ENTRY_NAME}.desktop")),
            b"[Desktop Entry]\n",
        )
        .unwrap();
        let source = root.join("build/brain");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
        let before = snapshot(&root);

        let mut cases = unsafe_home_pairs(&root)
            .into_iter()
            .map(|(home, brain_home)| (brain_home, layout_for(&home)))
            .collect::<Vec<_>>();
        // A healthy HOME whose absolute XDG locations carry control characters.
        let mut data = layout_for(&good);
        data.applications = root.join("data\u{7f}/applications");
        cases.push((good_brain.clone(), data));
        let mut desktop = layout_for(&good);
        desktop.desktop = Some(root.join("Desk\ntop"));
        cases.push((good_brain.clone(), desktop));

        for (brain_home, layout) in &cases {
            for result in [
                plan_install(&source, brain_home, layout),
                install_from(&source, brain_home, layout),
                remove_from(brain_home, layout, true),
                remove_from(brain_home, layout, false),
            ] {
                assert_eq!(
                    result.unwrap_err().to_string(),
                    UNSAFE_HOME,
                    "{brain_home:?}"
                );
            }
        }
        assert_eq!(
            snapshot(&root),
            before,
            "no stable copy, icon, menu entry, desktop launcher or data change"
        );

        // A Unicode HOME with spaces still plans normally (and writes nothing).
        let unicode = root.join("дом ñ space");
        let plan = plan_install(&source, &unicode.join(".brain"), &layout_for(&unicode)).unwrap();
        assert_eq!(plan["status"], "dry_run");
        assert_eq!(snapshot(&root), before);
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(_tag: &str) -> PathBuf {
        let _nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = crate::test_support::temp_path("brain-desktop");
        fs::create_dir_all(&root).expect("create desktop test root");
        root
    }

    /// Serialises the tests that write a fake binary and then execute it, so
    /// one test's spawn cannot inherit another's still-open write descriptor.
    static EXECUTABLE_FIXTURES: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn fixture_lock() -> std::sync::MutexGuard<'static, ()> {
        EXECUTABLE_FIXTURES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A layout derived only from the fixture home, never from `XDG_*`.
    fn fixture_layout(home: &Path) -> Layout {
        Layout::resolve(home, |_| None)
    }

    #[test]
    fn the_entry_points_at_the_ui_command() {
        let entry = render_entry(Path::new("/usr/local/bin/brain")).expect("valid path");
        assert!(entry.contains(&format!(
            "Exec=/usr/local/bin/brain ui --port {SHORTCUT_PORT}"
        )));
        assert!(
            entry.contains("Name[ru]="),
            "the launcher must be readable in Russian"
        );
    }

    #[test]
    fn a_path_with_a_space_stays_one_argument() {
        let quoted = quote_exec_argument(Path::new("/home/a b/bin/brain")).expect("valid path");
        assert_eq!(quoted, "\"/home/a b/bin/brain\"");
    }

    #[test]
    fn percent_is_literal_and_newlines_are_rejected() {
        assert_eq!(
            quote_exec_argument(Path::new("/home/100%/brain")).expect("percent path"),
            "\"/home/100%%/brain\""
        );
        assert!(quote_exec_argument(Path::new("/tmp/bad\nbrain")).is_err());
    }

    #[test]
    fn desktop_exec_escapes_both_string_layers() {
        assert_eq!(
            quote_exec_argument(Path::new("/tmp/a$`\\\"b/brain")).expect("reserved path"),
            "\"/tmp/a\\\\$\\\\`\\\\\\\\\\\\\"b/brain\""
        );
        assert!(quote_exec_argument(Path::new("/tmp/a=b/brain")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn shortcut_uses_a_copy_that_survives_the_build_tree() {
        let _fixtures = fixture_lock();
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("stable-copy");
        let home = root.join("home");
        let brain_home = home.join(".brain");
        let source = root.join("target/release/brain");
        fs::create_dir_all(source.parent().expect("source parent")).expect("create target dir");
        fs::create_dir_all(home.join("Desktop")).expect("create desktop dir");
        fs::write(&source, b"#!/bin/sh\nprintf 'brain-test\\n'\n").expect("write fake brain");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755))
            .expect("make source executable");

        let report =
            install_from(&source, &brain_home, &fixture_layout(&home)).expect("install shortcut");
        let stable = installer::stable_copy_path(&brain_home);
        assert_eq!(report["binary"], json!(stable));
        assert!(stable.is_file());
        assert!(
            !fs::symlink_metadata(&stable)
                .expect("stable metadata")
                .file_type()
                .is_symlink(),
            "the stable copy must not be a link back into target"
        );
        let entry = fs::read_to_string(
            home.join(".local/share/applications")
                .join(format!("{ENTRY_NAME}.desktop")),
        )
        .expect("read launcher");
        assert!(entry.contains(&format!("Exec={} ui", stable.display())));

        // Removing the build artifact models `cargo clean`: the launcher must
        // still point at a regular executable with independently copied bytes.
        fs::remove_file(&source).expect("remove build artifact");
        let output = Command::new(&stable)
            .arg("--version")
            .output()
            .expect("launch stable copy");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "brain-test\n");
        fs::remove_dir_all(root).expect("remove desktop fixture");
    }

    #[cfg(unix)]
    #[test]
    fn failed_copy_or_probe_writes_no_launcher_and_reports_no_success() {
        let _fixtures = fixture_lock();
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("failed-install");
        let home = root.join("home");
        let brain_home = home.join(".brain");
        let source = root.join("target/release/brain");
        fs::create_dir_all(source.parent().expect("source parent")).expect("create target dir");
        fs::write(&source, b"#!/bin/sh\nexit 9\n").expect("write failing brain");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755))
            .expect("make source executable");

        let error = install_from(&source, &brain_home, &fixture_layout(&home))
            .expect_err("a failing --version probe must abort installation");
        assert!(error.to_string().contains("--version"));
        assert!(!installer::desktop_copy_owner_path(&brain_home).exists());
        assert!(
            !home
                .join(".local/share/applications")
                .join(format!("{ENTRY_NAME}.desktop"))
                .exists()
        );

        let blocked_home = root.join("blocked-home");
        let blocked_brain_home = blocked_home.join(".brain");
        fs::create_dir_all(&blocked_brain_home).expect("blocked brain home");
        fs::write(blocked_brain_home.join("bin"), b"not a directory").expect("block bin directory");
        assert!(
            install_from(&source, &blocked_brain_home, &fixture_layout(&blocked_home)).is_err()
        );
        assert!(!installer::desktop_copy_owner_path(&blocked_brain_home).exists());
        assert!(
            !blocked_home
                .join(".local/share/applications")
                .join(format!("{ENTRY_NAME}.desktop"))
                .exists()
        );
        fs::remove_dir_all(root).expect("remove failed-install fixture");
    }

    #[test]
    fn the_icon_references_nothing_outside_itself() {
        // The SVG namespace is an identifier a renderer compares, never an
        // address it fetches, so it is the one URL the icon may carry.
        let drawn = ICON_SVG.replace(SVG_NAMESPACE, "");
        for marker in ["http://", "https://", "xlink:href", "<image"] {
            assert!(
                !drawn.contains(marker),
                "icon must be self-contained, found {marker:?}"
            );
        }
    }

    #[test]
    fn layout_follows_xdg_variables_and_ignores_relative_ones() {
        let root = temp_root("layout");
        let home = root.join("my home");
        let desktop = home.join("Рабочий стол");
        fs::create_dir_all(&desktop).expect("desktop");
        let config = root.join("config");
        fs::create_dir_all(&config).expect("config");
        fs::write(
            config.join("user-dirs.dirs"),
            "# written by xdg-user-dirs-update\nXDG_DESKTOP_DIR=\"$HOME/Рабочий стол\"\n",
        )
        .expect("user dirs");
        let data = root.join("data");
        let variables = |name: &str| match name {
            "XDG_DATA_HOME" => Some(data.to_string_lossy().into_owned()),
            "XDG_CONFIG_HOME" => Some(config.to_string_lossy().into_owned()),
            _ => None,
        };
        let layout = Layout::resolve(&home, variables);
        assert_eq!(layout.applications, data.join("applications"));
        assert_eq!(layout.icons, data.join("icons/hicolor/scalable/apps"));
        assert_eq!(layout.desktop.as_deref(), Some(desktop.as_path()));

        // Relative values are invalid by the specification and fall back.
        let relative = Layout::resolve(&home, |_| Some("relative/dir".to_owned()));
        assert_eq!(
            relative.applications,
            home.join(".local/share/applications")
        );
        fs::remove_dir_all(root).expect("remove layout fixture");
    }

    #[test]
    fn a_missing_or_disabled_desktop_means_menu_only() {
        let root = temp_root("no-desktop");
        let home = root.join("home");
        fs::create_dir_all(home.join(".config")).expect("config");
        assert_eq!(
            fixture_layout(&home).desktop,
            None,
            "no desktop folder exists"
        );

        // XDG: a desktop equal to $HOME disables the desktop.
        fs::write(
            home.join(".config/user-dirs.dirs"),
            "XDG_DESKTOP_DIR=\"$HOME/\"\n",
        )
        .expect("user dirs");
        fs::create_dir_all(home.join("Desktop")).expect("desktop");
        assert_eq!(
            fixture_layout(&home).desktop,
            None,
            "configured value wins over the guess"
        );
        fs::remove_dir_all(root).expect("remove no-desktop fixture");
    }

    #[test]
    fn user_dir_values_are_parsed_as_the_specification_writes_them() {
        let home = Path::new("/h");
        assert_eq!(
            parse_user_dir("\"$HOME/Desktop\"", home),
            Some(PathBuf::from("/h/Desktop"))
        );
        assert_eq!(
            parse_user_dir("\"/srv/desk top\"", home),
            Some(PathBuf::from("/srv/desk top"))
        );
        assert_eq!(parse_user_dir("\"relative\"", home), None);
        assert_eq!(
            parse_user_dir("$HOME/Desktop", home),
            None,
            "unquoted is malformed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_dry_run_and_uninstall_round_trip_keeps_user_data() {
        let _fixtures = fixture_lock();
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("round trip");
        let home = root.join("home with space");
        let brain_home = home.join(".brain");
        let desktop = home.join("Desktop");
        fs::create_dir_all(&desktop).expect("desktop");
        fs::create_dir_all(&brain_home).expect("brain home");
        fs::write(brain_home.join("brain.sqlite"), b"user data").expect("data");
        fs::write(brain_home.join("settings.json"), b"{}").expect("settings");
        let source = root.join("build dir/brain");
        fs::create_dir_all(source.parent().expect("parent")).expect("build dir");
        fs::write(&source, b"#!/bin/sh\nprintf 'brain-test\\n'\n").expect("fake brain");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).expect("chmod");
        let layout = fixture_layout(&home);

        let plan = plan_install(&source, &brain_home, &layout).expect("plan");
        assert_eq!(plan["status"], "dry_run");
        assert!(
            !installer::stable_copy_path(&brain_home).exists(),
            "dry run copied the binary"
        );
        assert!(!layout.applications.exists(), "dry run wrote a menu entry");
        assert!(fs::read_dir(&desktop).expect("desktop").next().is_none());

        let report = install_from(&source, &brain_home, &layout).expect("install");
        let [menu, graph_menu] = layout.menu_entries();
        let [desktop_settings, desktop_graph] = layout.desktop_entries().expect("desktop found");
        for path in [&menu, &graph_menu, &desktop_settings, &desktop_graph] {
            assert!(path.is_file(), "{} missing", path.display());
        }
        for path in [&desktop_settings, &desktop_graph] {
            let mode = fs::metadata(path).expect("mode").permissions().mode();
            assert_ne!(mode & 0o111, 0, "desktop launcher must be executable");
        }
        assert!(layout.settings_icon().is_file() && layout.graph_icon().is_file());
        let graph_entry = fs::read_to_string(&desktop_graph).expect("graph entry");
        let stable = installer::stable_copy_path(&brain_home);
        assert!(graph_entry.contains(&format!(
            "Exec=\"{}\" ui --port {SHORTCUT_PORT} --page graph",
            stable.display()
        )));
        assert!(graph_entry.contains(&format!("Icon={GRAPH_ICON_NAME}\n")));
        assert_eq!(report["desktop_entry_written"], json!(true));

        let preview = remove_from(&brain_home, &layout, true).expect("remove preview");
        assert_eq!(preview["would_remove"].as_array().expect("list").len(), 8);
        assert!(menu.is_file(), "removal dry run deleted a file");

        let removed = remove_from(&brain_home, &layout, false).expect("remove");
        assert_eq!(removed["removed"].as_array().expect("list").len(), 8);
        for path in [
            &menu,
            &graph_menu,
            &desktop_settings,
            &desktop_graph,
            &stable,
        ] {
            assert!(!path.exists(), "{} survived uninstall", path.display());
        }
        assert_eq!(
            fs::read(brain_home.join("brain.sqlite")).expect("data"),
            b"user data"
        );
        assert!(brain_home.join("settings.json").is_file());
        fs::remove_dir_all(root).expect("remove round-trip fixture");
    }

    #[test]
    fn the_graph_icon_references_nothing_outside_itself() {
        let drawn = GRAPH_ICON_SVG.replace(SVG_NAMESPACE, "");
        for marker in ["http://", "https://", "xlink:href", "<image"] {
            assert!(
                !drawn.contains(marker),
                "icon must be self-contained, found {marker:?}"
            );
        }
        assert_ne!(
            GRAPH_ICON_SVG, ICON_SVG,
            "the two launchers need distinct icons"
        );
    }
}
