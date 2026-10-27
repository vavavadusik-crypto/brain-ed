//! launchd user-agent backend. All payload arguments are literal plist strings.
use serde_json::{Value, json};
use std::{
    error::Error,
    path::{Path, PathBuf},
};

pub(crate) const ARCHIVE: &str = "org.brain-ed.archive";
pub(crate) const BACKUP: &str = "org.brain-ed.backup";
pub(crate) const DISTILL: &str = "org.brain-ed.distill";
pub(crate) const TEAM: &str = "org.brain-ed.team-publish";

fn xml(value: &str) -> Result<String, Box<dyn Error>> {
    if value
        .chars()
        .any(|c| c.is_control() || matches!(c, '\u{fffe}' | '\u{ffff}'))
    {
        return Err("control character in LaunchAgent value".into());
    }
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}

pub(crate) fn seconds(every: &str) -> Result<u32, Box<dyn Error>> {
    crate::distill_schedule::interval(every)?;
    let (n, unit) = every.split_at(every.len() - 1);
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => unreachable!(),
    };
    Ok(n.parse::<u32>()?
        .checked_mul(scale)
        .ok_or("interval overflow")?)
}

fn agent_path(home: &Path, label: &str) -> Result<PathBuf, Box<dyn Error>> {
    if ![ARCHIVE, BACKUP, DISTILL, TEAM].contains(&label) {
        return Err("unknown LaunchAgent label".into());
    }
    crate::private_fs::require_safe_paths(&[home])?;
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist")))
}

fn plist(
    label: &str,
    args: &[String],
    every: u32,
    env: &[(&str, String)],
) -> Result<String, Box<dyn Error>> {
    if args.is_empty() || !Path::new(&args[0]).is_absolute() || every == 0 {
        return Err("LaunchAgent requires an absolute executable and positive interval".into());
    }
    let args = args
        .iter()
        .map(|s| xml(s).map(|s| format!("<string>{s}</string>")))
        .collect::<Result<Vec<_>, _>>()?
        .join("\n");
    let env = env
        .iter()
        .map(|(k, v)| {
            Ok(format!(
                "<key>{}</key><string>{}</string>",
                xml(k)?,
                xml(v)?
            ))
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?
        .join("\n");
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{}</string>\n<key>ProgramArguments</key><array>{args}</array>\n<key>EnvironmentVariables</key><dict>{env}</dict>\n<key>StartInterval</key><integer>{every}</integer>\n<key>Nice</key><integer>10</integer>\n<key>ProcessType</key><string>Background</string>\n</dict></plist>\n",
        xml(label)?
    ))
}

fn launchctl(args: &[&str]) -> Option<bool> {
    use std::process::{Command, Stdio};
    Command::new("launchctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .map(|s| s.success())
}

fn job_text(
    home: &Path,
    label: &str,
    args: &[String],
    every: u32,
) -> Result<String, Box<dyn Error>> {
    let brain_home = std::path::absolute(crate::brain_home())?;
    let env = [
        ("HOME", home.to_str().ok_or("HOME is not UTF-8")?.into()),
        (
            "BRAIN_HOME",
            brain_home.to_str().ok_or("Brain home is not UTF-8")?.into(),
        ),
    ];
    plist(label, args, every, &env)
}

/// Check each ancestor BEFORE creating any next component (never mkdir through
/// a symlink). Missing directories on removal do not need to be created.
fn parent_ready(home: &Path, create: bool) -> Result<bool, Box<dyn Error>> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    for dir in [
        home.to_path_buf(),
        home.join("Library"),
        home.join("Library/LaunchAgents"),
    ] {
        let meta = match std::fs::symlink_metadata(&dir) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
                std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
                std::fs::symlink_metadata(&dir)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        if !meta.is_dir()
            || meta.file_type().is_symlink()
            || meta.uid() != brain_core::platform::current_uid()?
            || meta.mode() & 0o022 != 0
        {
            return Err(
                "LaunchAgent parent must be owned by this user and not shared-writable".into(),
            );
        }
    }
    Ok(true)
}

fn domain() -> Result<String, Box<dyn Error>> {
    Ok(format!("gui/{}", brain_core::platform::current_uid()?))
}

pub(crate) fn status(
    home: &Path,
    label: &str,
    last: Option<Value>,
) -> Result<Value, Box<dyn Error>> {
    let installed = agent_path(home, label)?.is_file();
    let loaded = launchctl(&["print", &format!("{}/{label}", domain()?)]);
    Ok(
        json!({"status":"ok","scheduler":"launchd","unit_files_installed":installed,"enabled":loaded,"active":loaded,"last_run":last,
        "note":"active means loaded; StartInterval ticks may be coalesced during sleep"}),
    )
}

/// Write private plist only. Never contacts launchd (also used by fixture tests).
pub(crate) fn write(
    home: &Path,
    label: &str,
    job: Option<(&[String], u32)>,
) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let path = agent_path(home, label)?;
    if let Some((args, every)) = job {
        let text = job_text(home, label, args, every)?;
        parent_ready(home, true)?;
        crate::private_fs::write_private(&path, text.as_bytes())?;
    } else if parent_ready(home, false)? {
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    Ok(vec![path])
}

pub(crate) fn manage(
    home: &Path,
    label: &str,
    job: Option<(&[String], u32)>,
) -> Result<Value, Box<dyn Error>> {
    manage_with(home, label, job, launchctl)
}

fn manage_with(
    home: &Path,
    label: &str,
    job: Option<(&[String], u32)>,
    mut run: impl FnMut(&[&str]) -> Option<bool>,
) -> Result<Value, Box<dyn Error>> {
    let path = agent_path(home, label)?;
    let domain = domain()?;
    let target = format!("{domain}/{label}");
    // Unload before replacing/removing the file. A loaded job that cannot be
    // unloaded must remain intact and visible to status.
    // Validate before unloading a working job; malformed updates must not stop it.
    if let Some((args, every)) = job {
        job_text(home, label, args, every)?;
    }
    parent_ready(home, false)?;
    let loaded =
        run(&["print", &target]).ok_or("launchctl is unavailable; LaunchAgent unchanged")?;
    if loaded && run(&["bootout", &target]) != Some(true) {
        return Err("launchctl bootout failed; LaunchAgent unchanged".into());
    }
    let files = write(home, label, job)?;
    let switched = job.is_none()
        || run(&[
            "bootstrap",
            &domain,
            path.to_str().ok_or("plist path is not UTF-8")?,
        ]) == Some(true);
    Ok(
        json!({"status":if switched {"ok"} else {"files_written_launchctl_failed"},"scheduler":"launchd","unit_files":files,"launchctl_switched":switched}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plist_arguments_are_literal_and_intervals_are_bounded() {
        let args = vec!["/tmp/a & b/brain".into(), "a<$HOME>\"'".into()];
        let text = plist(
            DISTILL,
            &args,
            seconds("30m").unwrap(),
            &[("HOME", "/tmp/a & b".into())],
        )
        .unwrap();
        assert!(text.contains("a&lt;$HOME&gt;&quot;&apos;"));
        assert!(text.contains("<integer>1800</integer>"));
        assert!(!text.contains("/bin/sh"));
        assert!(plist(DISTILL, &["relative".into()], 1, &[]).is_err());
        assert!(plist(DISTILL, &["/bin/brain\n".into()], 1, &[]).is_err());
        assert!(plist(DISTILL, &["/bin/brain\u{fffe}".into()], 1, &[]).is_err());
        for bad in ["0h", "1;sh", "10001d", ""] {
            assert!(seconds(bad).is_err());
        }
        assert_eq!(seconds("10000d").unwrap(), 864000000);
    }
    #[test]
    fn failed_manager_and_malformed_update_preserve_existing_agent() {
        let root = crate::test_support::temp_path("launch-agent-errors");
        crate::private_fs::create_private_dir_all(&root).unwrap();
        let args = vec!["/usr/bin/true".into()];
        let path = write(&root, BACKUP, Some((&args, 60))).unwrap().remove(0);
        let before = std::fs::read(&path).unwrap();
        assert!(manage_with(&root, BACKUP, None, |_| None).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(manage_with(&root, BACKUP, None, |args| Some(args[0] == "print")).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(
            manage_with(&root, BACKUP, Some((&["relative".into()], 60)), |_| panic!(
                "invalid update contacted launchctl"
            ))
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let result = manage_with(&root, BACKUP, Some((&args, 120)), |args| {
            Some(args[0] != "bootstrap")
        })
        .unwrap();
        assert_eq!(result["status"], "files_written_launchctl_failed");
        assert!(path.exists());
        assert_eq!(
            manage_with(&root, BACKUP, None, |_| Some(false)).unwrap()["status"],
            "ok"
        );
        assert!(!path.exists());
    }
    #[test]
    fn agent_writer_never_creates_through_a_library_symlink() {
        use std::os::unix::fs::symlink;
        let root = crate::test_support::temp_path("launch-agent-symlink");
        let outside = crate::test_support::temp_path("launch-agent-outside");
        crate::private_fs::create_private_dir_all(&root).unwrap();
        crate::private_fs::create_private_dir_all(&outside).unwrap();
        symlink(&outside, root.join("Library")).unwrap();
        let args = vec!["/usr/bin/true".into()];
        assert!(write(&root, BACKUP, Some((&args, 60))).is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        assert!(write(&root, BACKUP, None).is_err());
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_plist_passes_plutil_and_writer_removes_only_agent() {
        let root = crate::test_support::temp_path("launch-agent");
        crate::private_fs::create_private_dir_all(&root).unwrap();
        let args = vec!["/usr/bin/true".into(), "literal & < >".into()];
        let paths = write(&root, BACKUP, Some((&args, 86400))).unwrap();
        assert!(
            std::process::Command::new("/usr/bin/plutil")
                .args(["-lint", "--"])
                .arg(&paths[0])
                .status()
                .unwrap()
                .success()
        );
        assert!(write(&root, "../bad", None).is_err());
        std::fs::write(root.join("keep"), b"keep").unwrap();
        write(&root, BACKUP, None).unwrap();
        assert!(!paths[0].exists());
        assert!(root.join("keep").exists());
    }
}
