//! CLI relay administration and explicit relay routes.
use super::{Exit, StateDirArg, finish};
use brain_core::network::relay::{self, Route};
use brain_core::network::{KeySource, read_credential_from, write_credential_file};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::error::Error;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug, Default, Args, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RelayArgs {
    /// Explicit company relay (DNS:PORT or IP:PORT). Direct TCP is tried first when present.
    #[arg(long, value_name="HOST:PORT", requires_all=["relay_token_file","relay_name","relay_ca"])]
    pub via_relay: Option<String>,
    /// Separate relay credential (0600); register for serve, connect for clients/bundles.
    #[arg(long, value_name = "FILE", requires = "via_relay")]
    pub relay_token_file: Option<PathBuf>,
    /// Exact name authorized by the relay token, [a-z0-9-]{3,64}.
    #[arg(long, value_name = "NAME", requires = "via_relay")]
    pub relay_name: Option<String>,
    /// Pinned CA for the relay's outer TLS (independent of coordinator CA).
    #[arg(long, value_name = "FILE", requires = "via_relay")]
    pub relay_ca: Option<PathBuf>,
    /// Relay certificate SAN/SNI; default: host part of --via-relay.
    #[arg(long, value_name = "NAME", requires = "via_relay")]
    pub relay_server_name: Option<String>,
}
impl RelayArgs {
    pub fn route(&self) -> Result<Option<Route>, Box<dyn Error>> {
        let Some(address) = &self.via_relay else {
            if self.relay_token_file.is_some()
                || self.relay_name.is_some()
                || self.relay_ca.is_some()
                || self.relay_server_name.is_some()
            {
                return Err("incomplete relay route".into());
            }
            return Ok(None);
        };
        let name = self.relay_name.clone().ok_or("--relay-name required")?;
        if !relay::valid_name(&name) {
            return Err("invalid relay name".into());
        }
        let host = relay::relay_host(address)?;
        Ok(Some(Route {
            address: address.clone(),
            name,
            server_name: self.relay_server_name.clone().unwrap_or(host),
            ca: self.relay_ca.clone().ok_or("--relay-ca required")?,
            token_file: self
                .relay_token_file
                .clone()
                .ok_or("--relay-token-file required")?,
        }))
    }
    pub fn bundle(&self) -> Result<Option<Value>, Box<dyn Error>> {
        let Some(route) = self.route()? else {
            return Ok(None);
        };
        let _ = relay::Connector::new(&route)?;
        let token = read_credential_from(&KeySource::File(route.token_file))
            .map_err(|_| "invalid relay token file")?;
        let ca = std::fs::read_to_string(route.ca)?;
        Ok(Some(
            json!({"address":route.address,"name":route.name,"server_name":route.server_name,"ca_pem":ca,"credential":token}),
        ))
    }
}
#[derive(Debug, Subcommand)]
pub(crate) enum RelayCommand {
    /// Public relay with outer TLS; forwards coordinator ciphertext only. EOF on stdin stops it.
    Serve(RelayServe),
    /// Trusted local admin of the separate relay token store.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
}
#[derive(Debug, Args)]
pub(crate) struct RelayServe {
    #[command(flatten)]
    state: StateDirArg,
    /// Numeric public, wildcard or loopback relay IP; explicit, no default.
    #[arg(long)]
    listen: std::net::IpAddr,
    /// Relay TCP port; 0 selects a free port shown in the startup JSON.
    #[arg(long)]
    port: u16,
    /// Maximum live sockets, including authentication, pending slots and both tunnel ends.
    #[arg(long, default_value_t = 128)]
    max_connections: usize,
    /// Aggregate bytes per second per tunnel, both directions.
    #[arg(long, default_value_t = 4194304)]
    bytes_per_second: u64,
    /// Maximum idle time for registrations and tunnels in seconds (1..=300).
    #[arg(long, default_value_t = 30)]
    idle_secs: u64,
    /// Absolute tunnel lifetime in seconds (idle-secs..=3600).
    #[arg(long, default_value_t = 300)]
    max_session_secs: u64,
}
#[derive(Debug, Subcommand)]
pub(crate) enum TokenCommand {
    /// Issue a random relay token bound to one coordinator name and one role. Secret goes to file only.
    Add {
        #[command(flatten)]
        state: StateDirArg,
        /// Exact coordinator name this token may access, [a-z0-9-]{3,64}.
        #[arg(long)]
        name: String,
        /// Register a coordinator or connect as a client; each token has one role.
        #[arg(long, value_parser=["register","connect"])]
        role: String,
        /// New owner-only output file; the token is never printed or overwritten.
        #[arg(long)]
        token_out: PathBuf,
    },
    /// Revoke a token. Pending and active tunnels are rechecked within 100 ms plus bounded I/O.
    Revoke {
        #[command(flatten)]
        state: StateDirArg,
        /// Non-secret id from token add output; closes waiting and active uses.
        #[arg(long)]
        token_id: String,
    },
}
pub(crate) fn run(command: RelayCommand) -> Result<(), Box<dyn Error>> {
    match command {
        RelayCommand::Token { command } => {
            let document = match command {
                TokenCommand::Add {
                    state,
                    name,
                    role,
                    token_out,
                } => {
                    if !relay::valid_name(&name) {
                        return Err("invalid relay name".into());
                    }
                    if token_out.symlink_metadata().is_ok() {
                        return Err("token output already exists".into());
                    }
                    let store = relay::auth_store(&state.state_dir)?;
                    store
                        .upsert_principal(&name, &role, "relay-only", "reader", &["relay"])
                        .map_err(|_| "relay principal refused")?;
                    let token = store
                        .issue_credential(&name, &role, None)
                        .map_err(|_| "relay token issue failed")?;
                    if let Err(error) = write_credential_file(&token_out, token.as_str()) {
                        let _ = store.revoke_credential(token.token_id().as_str());
                        return Err(format!("relay token file: {}", error.status()).into());
                    }
                    json!({"status":"ok","name":name,"role":role,"token_id":token.token_id().as_str(),"token_file":token_out,"ttl_secs":7776000})
                }
                TokenCommand::Revoke { state, token_id } => {
                    relay::auth_store(&state.state_dir)?
                        .revoke_credential(&token_id)
                        .map_err(|_| "relay token not found or revoked")?;
                    json!({"status":"ok","revoked":token_id})
                }
            };
            finish(Exit::Ok, &document)
        }
        RelayCommand::Serve(args) => {
            let limits = relay::Limits {
                connections: args.max_connections,
                bytes_per_second: args.bytes_per_second,
                idle: Duration::from_secs(args.idle_secs),
                lifetime: Duration::from_secs(args.max_session_secs),
            };
            let server =
                relay::RelayServer::start(&args.state.state_dir, args.listen, args.port, limits)?;
            let address = server.address()?;
            let handle = server.serve()?;
            println!(
                "{}",
                json!({"status":"serving","protocol":"brain-relay/1","bind":address.to_string(),"end_to_end_tls":true})
            );
            std::io::stdout().flush()?;
            let mut sink = [0; 256];
            while matches!(std::io::stdin().read(&mut sink),Ok(n) if n>0) {}
            drop(handle);
            Ok(())
        }
    }
}
