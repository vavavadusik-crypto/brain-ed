//! Bounded user-timer publication; local policy is checked again on every run.
use super::{Local, Publish, TeamCommand, dir, selected};
use crate::{
    RuntimeConfig, archive,
    distill_schedule::{interval, systemctl},
};
use brain_core::{
    network::SHARE_STORE_FILE,
    sharing::{ShareResult, ShareStore},
    team,
};
use clap::Subcommand;
use serde_json::{Value, json};
use std::error::Error;

const SERVICE: &str = "brain-team-publish.service";
const TIMER: &str = "brain-team-publish.timer";

#[derive(Debug, Subcommand)]
pub(crate) enum ScheduleCommand {
    /// Install a user timer that republishes this machine's slice (only while publication is on).
    Enable {
        /// How often to publish a fresh slice (systemd interval, e.g. 15m, 1h).
        #[arg(long, default_value = "15m")]
        every: String,
        #[command(flatten)]
        local: Local,
    },
    /// Remove the publication timer; already published slices stay until they age out.
    Disable,
    /// Timer state and the last scheduled publication result.
    Status,
}

fn enabled(local: &Local) -> ShareResult<bool> {
    let path = dir(local).join(SHARE_STORE_FILE);
    if !path.exists() {
        return Ok(false);
    }
    let store = ShareStore::open_readonly(path)?;
    let space = selected(&store, local)?;
    Ok(store.team_manifest(&space)?.is_some_and(|m| m.enabled))
}

pub(super) fn publish(config: &RuntimeConfig, args: Publish) -> ShareResult<Value> {
    let result = match enabled(&args.local) {
        Ok(false) => Ok(json!({"status":"publication_disabled","published":false})),
        Ok(true) => super::execute(
            config,
            TeamCommand::Publish(Publish {
                local: args.local,
                scheduled: false,
            }),
        ),
        Err(error) => Err(error),
    };
    // Persist only bounded diagnostics, never source text or credentials.
    let status = match &result {
        Ok(value) if value["status"] == "publication_disabled" => "publication_disabled",
        Ok(_) => "published",
        Err(_) => "failed",
    };
    let home = crate::brain_home();
    std::fs::create_dir_all(&home).map_err(|_| team::bad())?;
    archive::write_private(
        &home.join("team-schedule-status.json"),
        &serde_json::to_vec(&json!({"at":crate::now_unix_seconds(),"status":status}))
            .map_err(|_| team::bad())?,
    )
    .map_err(|_| team::bad())?;
    result
}

pub(crate) fn command(
    config: &RuntimeConfig,
    command: ScheduleCommand,
) -> Result<Value, Box<dyn Error>> {
    let home = crate::user_home();
    let units = home.join(".config/systemd/user");
    let (files, switched, reloaded) = match command {
        ScheduleCommand::Status => {
            let last = std::fs::read(crate::brain_home().join("team-schedule-status.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            return Ok(
                json!({"status":"ok","unit_files_installed":units.join(SERVICE).is_file() && units.join(TIMER).is_file(),
                "enabled":systemctl(&["is-enabled",TIMER]),"active":systemctl(&["is-active",TIMER]),"last_run":last}),
            );
        }
        ScheduleCommand::Enable { every, local } => {
            interval(&every)?;
            if !enabled(&local)? {
                return Err("enable local publication first: brain team enable".into());
            }
            let state = std::path::absolute(dir(&local))?;
            let store = ShareStore::open_readonly(state.join(SHARE_STORE_FILE))?;
            let space = selected(&store, &local)?;
            let binary = crate::stable_brain_binary()?;
            archive::timer_units(&binary)?;
            let database = std::path::absolute(&config.database)?;
            let args = [
                binary.to_str().ok_or("binary path is not UTF-8")?,
                "--offline",
                "--database",
                database.to_str().ok_or("database path is not UTF-8")?,
                "--space",
                &config.space,
                "team",
                "publish",
                "--scheduled",
                "--state-dir",
                state.to_str().ok_or("state path is not UTF-8")?,
                "--team-space",
                &space,
            ];
            let exec = args
                .iter()
                .map(|s| archive::systemd_argument(s))
                .collect::<Result<Vec<_>, _>>()?
                .join(" ");
            let brain_home = std::path::absolute(crate::brain_home())?;
            let user_home = std::path::absolute(&home)?;
            let env_home = archive::systemd_argument(&format!("HOME={}", user_home.display()))?;
            let env_brain =
                archive::systemd_argument(&format!("BRAIN_HOME={}", brain_home.display()))?;
            let service = format!(
                "[Unit]\nDescription=Brain ED: bounded team publication\n\n[Service]\nType=oneshot\nEnvironment={env_home} {env_brain}\nExecStart={exec}\nNice=10\nIOSchedulingClass=idle\nTimeoutStartSec=90\n"
            );
            let timer = format!(
                "[Unit]\nDescription=Brain ED: scheduled team publication\n\n[Timer]\nOnBootSec={every}\nOnUnitInactiveSec={every}\nAccuracySec=30s\n\n[Install]\nWantedBy=timers.target\n"
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
