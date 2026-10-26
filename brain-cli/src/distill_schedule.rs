//! Opt-in user timer. All execution is one bounded run, never a retry loop.
use crate::{RuntimeConfig, archive, distill, now_unix_seconds, settings};
use clap::Subcommand;
use serde_json::{Value, json};
use std::{
    error::Error,
    process::{Command, Stdio},
};

pub(crate) const SERVICE: &str = "brain-distill.service";
pub(crate) const TIMER: &str = "brain-distill.timer";

#[derive(Debug, Subcommand)]
pub(crate) enum ScheduleCommand {
    /// Install or refresh the systemd timer that runs distillation.
    Enable {
        /// LLM provider name the timer distills with.
        #[arg(long)]
        provider: String,
        /// Timer interval: positive integer plus s, m, h or d (default 1h).
        #[arg(long, default_value = "1h")]
        every: String,
        /// Sessions per timer run (1-1000, default 20).
        #[arg(long, default_value_t = 20)]
        max_sessions: usize,
        /// Local clock window in which no provider is invoked (start inclusive, end exclusive).
        #[arg(long)]
        quiet_hours: Option<String>,
    },
    /// Stop the distillation timer (installed files are removed).
    Disable,
    /// Show whether the timer is installed and when it last ran.
    Status,
}

pub(crate) fn systemctl(args: &[&str]) -> bool {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub(crate) fn interval(value: &str) -> Result<(), Box<dyn Error>> {
    if !value.is_ascii() {
        return Err("every must use ASCII digits and units".into());
    }
    let (number, unit) = value.split_at(value.len().saturating_sub(1));
    if number.is_empty()
        || !number.bytes().all(|b| b.is_ascii_digit())
        || !matches!(unit, "s" | "m" | "h" | "d")
        || number
            .parse::<u32>()
            .ok()
            .is_none_or(|n| n == 0 || n > 10000)
    {
        return Err(
            "every must be a positive integer followed by s, m, h or d (at most 10000 units)"
                .into(),
        );
    }
    Ok(())
}

fn quiet_range(value: &str) -> Result<(u8, u8), Box<dyn Error>> {
    let (start, end) = value.split_once('-').ok_or("quiet-hours must be HH-HH")?;
    if start.is_empty()
        || end.is_empty()
        || start.len() > 2
        || end.len() > 2
        || !start.bytes().chain(end.bytes()).all(|b| b.is_ascii_digit())
    {
        return Err("quiet-hours must be HH-HH".into());
    }
    let (start, end): (u8, u8) = (start.parse()?, end.parse()?);
    if start > 23 || end > 24 || start == end {
        return Err("quiet-hours requires distinct hours in 00-24 (start <=23); example: --quiet-hours 9-18 keeps the provider quiet during work hours".into());
    }
    Ok((start, end))
}

fn quiet_hour((start, end): (u8, u8), hour: u8) -> bool {
    if start < end {
        (start..end).contains(&hour)
    } else {
        hour >= start || hour < end
    }
}

#[allow(unsafe_code)]
fn local_hour(timestamp: i64) -> Result<u8, Box<dyn Error>> {
    let timestamp: libc::time_t = timestamp as libc::time_t;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: both pointers reference live, correctly aligned objects; localtime_r
    // initializes the destination on success and does not retain either pointer.
    if unsafe { libc::localtime_r(&timestamp, local.as_mut_ptr()) }.is_null() {
        return Err("cannot determine local clock hour (the system time zone is unusable; check TZ / /etc/localtime and re-run the command)".into());
    }
    // SAFETY: localtime_r succeeded and initialized local.
    Ok(unsafe { local.assume_init() }.tm_hour as u8)
}

pub(crate) fn run(
    config: &RuntimeConfig,
    args: distill::RunArgs,
    scheduled: bool,
    quiet: Option<&str>,
) -> Result<Value, Box<dyn Error>> {
    let now = now_unix_seconds();
    let quiet = quiet.map(quiet_range).transpose()?;
    let skip = quiet
        .map(|range| local_hour(now).map(|hour| quiet_hour(range, hour)))
        .transpose()?;
    let result = if skip == Some(true) {
        Ok(json!({"status":"quiet_hours","calls":0}))
    } else {
        distill::run(config, args)
    };
    if !scheduled {
        return result;
    }
    let report = match &result {
        Ok(value) => json!({"at":now,"result":value}),
        // Do not persist provider output or archive content in timer diagnostics.
        Err(_) => json!({"at":now,"result":{"status":"failed"}}),
    };
    let home = crate::brain_home();
    std::fs::create_dir_all(&home)?;
    archive::write_private(
        &home.join("distill-schedule-status.json"),
        &serde_json::to_vec(&report)?,
    )?;
    result
}

pub(crate) fn command(
    config: &RuntimeConfig,
    command: ScheduleCommand,
) -> Result<Value, Box<dyn Error>> {
    let home = crate::user_home();
    let dir = home.join(".config/systemd/user");
    let (files, switched, reloaded) = match command {
        ScheduleCommand::Status => {
            let last = std::fs::read(crate::brain_home().join("distill-schedule-status.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            return Ok(
                json!({"status":"ok", "unit_files_installed":dir.join(SERVICE).is_file() && dir.join(TIMER).is_file(),
                "enabled":systemctl(&["is-enabled", TIMER]),"active":systemctl(&["is-active", TIMER]),"last_run":last}),
            );
        }
        ScheduleCommand::Enable {
            provider,
            every,
            max_sessions,
            quiet_hours,
        } => {
            interval(&every)?;
            if !(1..=1000).contains(&max_sessions) {
                return Err("max-sessions must be between 1 and 1000".into());
            }
            if let Some(q) = &quiet_hours {
                quiet_range(q)?;
            }
            let configured = settings::load(&crate::brain_home()).llm_providers;
            let names: Vec<&str> = provider.split(',').map(str::trim).collect();
            if names.iter().any(|name| name.is_empty())
                || names
                    .iter()
                    .any(|name| !configured.iter().any(|p| p.name == *name))
            {
                return Err("no LLM provider configured with this name (configure one first: `brain llm add <name> …`; list the known ones with `brain llm list`)".into());
            }
            let binary = crate::stable_brain_binary()?;
            archive::timer_units(&binary)?;
            let database = std::path::absolute(&config.database)?;
            let mut args = vec![
                binary
                    .to_str()
                    .ok_or("binary path is not UTF-8")?
                    .to_owned(),
                "--offline".into(),
                "--database".into(),
                database
                    .to_str()
                    .ok_or("database path is not UTF-8")?
                    .into(),
                "--space".into(),
                config.space.clone(),
                "--agent".into(),
                "distill-timer".into(),
                "distill".into(),
                "run".into(),
                "--scheduled".into(),
                "--provider".into(),
                provider,
                "--max-sessions".into(),
                max_sessions.to_string(),
            ];
            if let Some(q) = quiet_hours {
                args.extend(["--quiet-hours".into(), q]);
            }
            let exec = args
                .iter()
                .map(|a| archive::systemd_argument(a))
                .collect::<Result<Vec<_>, _>>()?
                .join(" ");
            let service = format!(
                "[Unit]\nDescription=Brain ED: bounded scheduled distillation\n\n[Service]\nType=oneshot\nExecStart={exec}\nNice=10\nIOSchedulingClass=idle\nTimeoutStartSec=infinity\n"
            );
            let timer = format!(
                "[Unit]\nDescription=Brain ED: scheduled distillation\n\n[Timer]\nOnBootSec={every}\nOnUnitInactiveSec={every}\nAccuracySec=30s\n\n[Install]\nWantedBy=timers.target\n"
            );
            let files =
                archive::write_named_timer_units(&home, SERVICE, TIMER, Some((service, timer)))?;
            let reloaded = systemctl(&["daemon-reload"]);
            (files, systemctl(&["enable", "--now", TIMER]), reloaded)
        }
        ScheduleCommand::Disable => {
            let switched = systemctl(&["disable", "--now", TIMER]);
            let files = archive::write_named_timer_units(&home, SERVICE, TIMER, None)?;
            (files, switched, systemctl(&["daemon-reload"]))
        }
    };
    Ok(
        json!({"status":if switched && reloaded {"ok"} else {"files_written_systemctl_failed"},
        "unit_files":files,"systemctl_daemon_reload":reloaded,"systemctl_enable_or_disable":switched}),
    )
}

#[cfg(test)]
mod tests {
    /// G12: quiet-hours table — HH-HH, start <= 23, end <= 24, distinct,
    /// ASCII digits only; overnight windows are legal.
    #[test]
    fn quiet_hours_follow_the_table() {
        let cases: &[(&str, bool)] = &[
            ("9-18", true),
            ("23-7", true),
            ("0-24", true),
            ("7-23", true),
            ("23-24", true),
            ("12-12", false),
            ("24-1", false),
            ("25-3", false),
            ("9-25", false),
            ("", false),
            ("9-", false),
            ("-18", false),
            ("1-2-3", false),
            ("ab-cd", false),
            (" 9-18", false),
            ("9-18 ", false),
            ("999-8", false),
            ("9-999", false),
            ("+9-18", false),
            ("9–18", false),
        ];
        for (value, expected) in cases {
            let outcome = quiet_range(value);
            assert_eq!(outcome.is_ok(), *expected, "{value:?}: {outcome:?}");
        }
    }

    /// G12: the schedule interval table — a positive integer (<= 10000) with
    /// one ASCII unit s/m/h/d.
    #[test]
    fn intervals_follow_the_table() {
        let cases: &[(&str, bool)] = &[
            ("1h", true),
            ("30m", true),
            ("90s", true),
            ("7d", true),
            ("10000h", true),
            ("0h", false),
            ("10001h", false),
            ("", false),
            ("h", false),
            ("1x", false),
            ("1.5h", false),
            ("1H", false),
            ("10 h", false),
            ("1h ", false),
            ("-1h", false),
            ("١h", false),
        ];
        for (value, expected) in cases {
            let outcome = interval(value);
            assert_eq!(outcome.is_ok(), *expected, "{value:?}: {outcome:?}");
        }
    }
    use super::*;
    #[test]
    fn quiet_hours_fixed_clock_boundaries_and_overnight() {
        let day = quiet_range("09-23").unwrap();
        for (hour, expected) in [
            (8, false),
            (9, true),
            (12, true),
            (22, true),
            (23, false),
            (0, false),
        ] {
            assert_eq!(quiet_hour(day, hour), expected);
        }
        let night = quiet_range("23-09").unwrap();
        for (hour, expected) in [(8, true), (9, false), (22, false), (23, true), (0, true)] {
            assert_eq!(quiet_hour(night, hour), expected);
        }
        assert!(quiet_hour(quiet_range("00-24").unwrap(), 12));
        for bad in ["24-09", "9-9", "25-26", "9;sh-23", "x", "é"] {
            assert!(quiet_range(bad).is_err());
        }
    }
    #[test]
    fn timer_arguments_and_intervals_fail_closed() {
        for bad in ["name%h", "name$HOME", "a\nb", "a\\b", "a\"b"] {
            assert!(archive::systemd_argument(bad).is_err());
        }
        for bad in ["0h", "-1h", "1h\n", "1;sh", "é", "9999999999999h", ""] {
            assert!(interval(bad).is_err());
        }
        for good in ["1h", "30m", "1d"] {
            interval(good).unwrap();
        }
    }
}
