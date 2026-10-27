//! E7.0b: one-command machine onboarding — `net init`, `net enroll --bundle-out`,
//! `net join`.
//!
//! The live stand made certificates by hand with openssl and spread the account,
//! CA and address over clients manually. These commands implement the
//! path: a private CA plus a server certificate with every name and IP in the
//! SAN (rcgen, EC P-256), a versioned join bundle that carries exactly one
//! copy of the coordinator token (plus a separate relay token in v2), and an unpack into `$BRAIN_HOME/net/client`
//! after which `net status` and `net call` need no flags.
//!
//! Safety: token and private keys are printed nowhere and written only through
//! create-new 0600 files inside 0700 directories; symlinks are never followed.

use std::error::Error;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// Direct-only format revision. Relay routes require v2; join accepts both.
const BUNDLE_VERSION: u32 = 1;

/// Where `net join` unpacks when `--state-dir` is not given.
pub(crate) fn default_client_dir(brain_home: &Path) -> PathBuf {
    brain_home.join("net/client")
}

fn sha256_hex(bytes: &[u8]) -> String {
    crate::installer::sha256(bytes)
}

/// Creates parent directories 0700 (only when missing) and writes a brand-new
/// 0600 file; an existing target is an error, never an overwrite.
fn write_secret(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.is_dir()
    {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|error| {
                format!(
                    "cannot create 0700 directory for {}: {error}",
                    path.display()
                )
            })?;
    }
    if path.symlink_metadata().is_ok() {
        return Err(format!(
            "{} already exists; secrets are never overwritten",
            path.display()
        )
        .into());
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, bytes))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(())
}

/// `brain net init`: own CA (EC P-256) and a server certificate whose SAN
/// carries every `--name` and `--ip`. Re-running without `--rotate` changes
/// nothing and says so.
pub(crate) fn init(
    state_dir: &Path,
    names: &[String],
    ips: &[String],
    rotate: bool,
) -> Result<Value, Box<dyn Error>> {
    let tls_dir = state_dir.join("tls");
    let ca_pem = tls_dir.join("ca.pem");
    let ca_key = tls_dir.join("ca.key");
    let server_pem = tls_dir.join("server.pem");
    let server_key = tls_dir.join("server.key");
    let san_json = tls_dir.join("san.json");
    let exists =
        ca_pem.is_file() && ca_key.is_file() && server_pem.is_file() && server_key.is_file();
    let partial = !exists
        && [&ca_pem, &ca_key, &server_pem, &server_key, &san_json]
            .iter()
            .any(|path| path.symlink_metadata().is_ok());
    if partial {
        return Err(format!(
            "{} holds an incomplete certificate set; move it aside before running init",
            tls_dir.display()
        )
        .into());
    }
    if exists && !rotate {
        return Ok(json!({
            "status": "exists",
            "tls_dir": tls_dir,
            "note": "certificates already present; pass --rotate to replace them",
        }));
    }
    if names.is_empty() && ips.is_empty() {
        return Err("--name or --ip is required on the first init (the certificate needs at least one SAN entry)".into());
    }
    // A rotate first moves the old files aside inside the same 0700 dir, so a
    // crash mid-generation never mixes old and new material.
    if exists {
        for path in [&ca_pem, &ca_key, &server_pem, &server_key, &san_json] {
            if path.symlink_metadata().is_err() {
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("tls");
            let retired = tls_dir.join(format!("{name}.previous"));
            let _ = std::fs::remove_file(&retired);
            std::fs::rename(path, &retired)?;
        }
    }
    // A newly created state directory will hold team gateway secrets as well
    // as TLS material; do not inherit a permissive process umask.
    crate::private_fs::create_private_dir_all(state_dir)?;
    std::fs::create_dir_all(&tls_dir)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tls_dir, std::fs::Permissions::from_mode(0o700))?;

    let mut params = rcgen::CertificateParams::new(Vec::new())?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Brain ED coordinator CA");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key_pair = rcgen::KeyPair::generate()?;
    let ca = params.self_signed(&ca_key_pair)?;

    let mut server_params = rcgen::CertificateParams::new(Vec::new())?;
    server_params.distinguished_name = rcgen::DistinguishedName::new();
    server_params.distinguished_name.push(
        rcgen::DnType::CommonName,
        names.first().cloned().unwrap_or_else(|| "brain-ed".into()),
    );
    for name in names {
        server_params
            .subject_alt_names
            .push(rcgen::SanType::DnsName(name.parse()?));
    }
    for ip in ips {
        let parsed: std::net::IpAddr = ip
            .parse()
            .map_err(|_| format!("--ip {ip:?} is not a numeric IP"))?;
        server_params
            .subject_alt_names
            .push(rcgen::SanType::IpAddress(parsed));
    }
    let server_key_pair = rcgen::KeyPair::generate()?;
    let server = server_params.signed_by(&server_key_pair, &ca, &ca_key_pair)?;

    write_secret(&ca_pem, ca.pem().as_bytes())?;
    write_secret(&ca_key, ca_key_pair.serialize_pem().as_bytes())?;
    write_secret(&server_pem, server.pem().as_bytes())?;
    write_secret(&server_key, server_key_pair.serialize_pem().as_bytes())?;
    // The SAN list as plain data: `net enroll --bundle-out` defaults
    // --server-name to the first DNS name without parsing DER.
    write_secret(
        &san_json,
        serde_json::to_vec_pretty(&json!({"dns": names, "ips": ips}))?.as_slice(),
    )?;
    let fingerprint = sha256_hex(ca.der().as_ref());
    Ok(json!({
        "status": "ok",
        "created": !exists,
        "rotated": exists,
        "tls_dir": tls_dir,
        "files": {
            "ca_pem": ca_pem, "ca_key": ca_key,
            "server_pem": server_pem, "server_key": server_key,
        },
        "san": {
            "dns": names,
            "ips": ips,
        },
        "ca_sha256_fingerprint": fingerprint,
        "note": "keys are 0600 and never printed; the fingerprint identifies the CA",
    }))
}

/// The first DNS name of the server certificate, recorded by `net init` in
/// `tls/san.json`; `--server-name` defaults to it.
pub(crate) fn first_dns_name(state_dir: &Path) -> Option<String> {
    let san: Value =
        serde_json::from_str(&std::fs::read_to_string(state_dir.join("tls/san.json")).ok()?)
            .ok()?;
    san.get("dns")
        .and_then(Value::as_array)
        .and_then(|names| names.first())
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// `brain net enroll --bundle-out`: one JSON file carrying everything `net
/// join` needs. The token exists exactly here and in the credential file the
/// caller pointed at (the usual enroll output continues to work).
#[allow(clippy::too_many_arguments)] // one flat call mirroring the enroll flags
pub(crate) fn write_bundle(
    state_dir: &Path,
    server_name: &str,
    dial: &[String],
    credential_path: &Path,
    machine: &str,
    agent_id: &str,
    expires_at: i64,
    bundle_out: &Path,
    relay: Option<Value>,
) -> Result<Value, Box<dyn Error>> {
    let ca_pem = std::fs::read_to_string(state_dir.join("tls/ca.pem"))
        .map_err(|e| format!("cannot read the CA (run brain net init first): {e}"))?;
    let token = std::fs::read_to_string(credential_path)
        .map_err(|e| format!("cannot read the credential file: {e}"))?;
    let version = if relay.is_some() { 2 } else { BUNDLE_VERSION };
    let mut bundle = json!({
        "version": version,
        "server_name": server_name,
        "dial": dial,
        "ca_pem": ca_pem,
        "credential": token.trim(),
        "machine": machine,
        "agent_id": agent_id,
        "expires_at": expires_at,
    });
    if let Some(relay) = relay {
        bundle["relay"] = relay;
    }
    write_secret(bundle_out, serde_json::to_vec_pretty(&bundle)?.as_slice())?;
    Ok(json!({
        "status": "ok",
        "bundle": bundle_out,
        "version": version,
        "note": "the bundle carries the token once, in a 0600 file; nothing is printed",
    }))
}

/// The unpacked client endpoint: what `net status`/`net call` read when the
/// flags are omitted.
pub(crate) fn endpoint_path(client_dir: &Path) -> PathBuf {
    client_dir.join("endpoint.json")
}

/// `brain net join FILE`: verify, unpack CA + credential + endpoint into the
/// client directory, optionally consume the bundle.
pub(crate) fn join(
    bundle_path: &Path,
    client_dir: &Path,
    allow_lan_dial: bool,
    allow_cgnat_dial: bool,
    consume: bool,
    replace: bool,
) -> Result<Value, Box<dyn Error>> {
    // Symlinked bundles are refused: the file's content decides a machine's
    // identity, so a swapped target must never be followed. O_NOFOLLOW closes
    // the gap between a separate check and the read.
    let raw = {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(bundle_path)
            .map_err(|e| {
                if e.raw_os_error() == Some(libc::ELOOP) {
                    "the bundle is a symlink; refusing to follow it".to_owned()
                } else {
                    format!("cannot read the bundle {}: {e}", bundle_path.display())
                }
            })?;
        let mut raw = Vec::new();
        file.by_ref().take(1 << 20).read_to_end(&mut raw)?;
        raw
    };
    let bundle: Value =
        serde_json::from_slice(&raw).map_err(|e| format!("the bundle is not valid JSON: {e}"))?;
    let version = bundle.get("version").and_then(Value::as_u64).unwrap_or(0);
    if version != u64::from(BUNDLE_VERSION) && version != 2 {
        return Err(format!(
            "bundle version {version} is not supported (expected 1 or 2); nothing was written"
        )
        .into());
    }
    let required = [
        "server_name",
        "dial",
        "ca_pem",
        "credential",
        "machine",
        "agent_id",
        "expires_at",
    ];
    for key in required {
        if bundle.get(key).is_none_or(Value::is_null) {
            return Err(format!("bundle field {key:?} is missing; nothing was written").into());
        }
    }
    let server_name = bundle["server_name"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if server_name.is_empty() {
        return Err("bundle server_name is empty; nothing was written (pass --server-name matching the server certificate SAN, as `net init --name` printed)".into());
    }
    let dial: Vec<String> = bundle["dial"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        })
        .unwrap_or_default();
    validate_relay_bundle(&bundle)?;
    let has_relay = bundle.get("relay").is_some_and(|v| !v.is_null());
    if dial.is_empty() && !has_relay {
        return Err("bundle dial list is empty; nothing was written (pass --dial IP:PORT, the address `net serve` printed on its bind line)".into());
    }
    // Each dial is checked by the same address policy the client applies, so a
    // machine never unpacks a bundle it cannot call. All refused → nothing is
    // written and the hint names the flag; some refused → warnings and only
    // the allowed dials reach endpoint.json.
    let mut allowed = Vec::new();
    let mut refused = Vec::new();
    for target in &dial {
        match brain_core::network::tls::parse_dial_target_with(
            target,
            allow_lan_dial,
            allow_cgnat_dial,
        ) {
            Ok(_) => allowed.push(target.clone()),
            Err(error) => refused.push(json!({
                "dial": target,
                "error": error.to_string(),
            })),
        }
    }
    if allowed.is_empty() && !has_relay {
        let needs = if refused.iter().any(|entry| {
            entry["error"]
                .as_str()
                .unwrap_or_default()
                .contains("CGNAT")
        }) {
            "--allow-cgnat-dial"
        } else {
            "--allow-lan-dial"
        };
        return Err(format!(
            "none of the bundle dial targets {dial:?} are allowed by this client's address policy; \
             nothing was written — pass {needs} to join if that is intended"
        )
        .into());
    }
    let expires_at = bundle["expires_at"].as_i64().unwrap_or(0);
    if expires_at <= crate::now_unix_seconds() {
        return Err(
            "the bundle's credential has expired; enroll the machine again (nothing was written)"
                .into(),
        );
    }
    let ca_pem = bundle["ca_pem"].as_str().unwrap_or_default().to_owned();
    if !ca_pem
        .trim_start()
        .starts_with("-----BEGIN CERTIFICATE-----")
    {
        return Err("bundle ca_pem is not a PEM certificate; nothing was written (the file is damaged; issue a fresh bundle with `net enroll --bundle-out`)".into());
    }
    let credential = bundle["credential"].as_str().unwrap_or_default().to_owned();
    if credential.is_empty() {
        return Err("bundle credential is empty; nothing was written (issue a fresh bundle with `net enroll --bundle-out`)".into());
    }
    // Unpack into a fresh sibling directory and rename it into place, so a
    // failed join leaves either the previous client or nothing — never a mix.
    if client_dir.symlink_metadata().is_ok() && !replace {
        return Err(format!(
            "{} already holds a joined endpoint; pass --replace to swap it (the old one is kept as .previous); nothing was written",
            client_dir.display()
        )
        .into());
    }
    let parent = client_dir
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or("client directory needs a parent directory")?;
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    let leaf = client_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("client directory name must be UTF-8")?;
    let staging = parent.join(format!(".{leaf}.joining-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&staging)?;
    }
    let unpacked = unpack(
        &staging,
        &ca_pem,
        &credential,
        &server_name,
        &allowed,
        allow_lan_dial,
        allow_cgnat_dial,
        &bundle,
    );
    if let Err(error) = unpacked {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    let mut previous = None;
    if client_dir.symlink_metadata().is_ok() {
        let retired = parent.join(format!("{leaf}.previous"));
        let _ = std::fs::remove_dir_all(&retired);
        std::fs::rename(client_dir, &retired)?;
        previous = Some(retired);
    }
    std::fs::rename(&staging, client_dir)?;
    if consume {
        std::fs::remove_file(bundle_path)?;
    }
    Ok(json!({
        "status": "ok",
        "client_dir": client_dir,
        "server_name": server_name,
        "dial": allowed,
        "refused_dials": refused,
        "bundle_consumed": consume,
        "previous_client_dir": previous,
        "note": "net status and net call now work without --dial/--ca/--server-name/--credential",
    }))
}

#[allow(clippy::too_many_arguments)] // the verified bundle fields, written once
fn unpack(
    client_dir: &Path,
    ca_pem: &str,
    credential: &str,
    server_name: &str,
    dial: &[String],
    allow_lan_dial: bool,
    allow_cgnat_dial: bool,
    bundle: &Value,
) -> Result<(), Box<dyn Error>> {
    write_secret(&client_dir.join("ca.pem"), ca_pem.as_bytes())?;
    write_secret(&client_dir.join("credential.pem"), credential.as_bytes())?;
    let mut endpoint = json!({
        "version": BUNDLE_VERSION,
        "server_name": server_name,
        "dial": dial,
        "allow_lan_dial": allow_lan_dial,
        "allow_cgnat_dial": allow_cgnat_dial,
        "machine": bundle["machine"],
        "agent_id": bundle["agent_id"],
        "expires_at": bundle["expires_at"],
        "ca": "ca.pem",
        "credential": "credential.pem",
    });
    if let Some(relay) = bundle.get("relay").filter(|v| !v.is_null()) {
        write_secret(
            &client_dir.join("relay-ca.pem"),
            relay["ca_pem"]
                .as_str()
                .ok_or("invalid relay CA")?
                .as_bytes(),
        )?;
        write_secret(
            &client_dir.join("relay-token"),
            relay["credential"]
                .as_str()
                .ok_or("invalid relay credential")?
                .as_bytes(),
        )?;
        endpoint["version"] = json!(2);
        endpoint["relay"] = json!({"address":relay["address"],"name":relay["name"],"server_name":relay["server_name"],"ca":"relay-ca.pem","token_file":"relay-token"});
    }
    write_secret(
        &endpoint_path(client_dir),
        serde_json::to_vec_pretty(&endpoint)?.as_slice(),
    )?;
    Ok(())
}

/// The endpoint as `net status`/`net call` should use it: `None` when the
/// client directory has no endpoint (the caller then requires the flags).
pub(crate) fn load_endpoint(client_dir: &Path) -> Option<Value> {
    let raw = std::fs::read(endpoint_path(client_dir)).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Validate relay v2 before creating any client files. No network/DNS on join.
fn validate_relay_bundle(bundle: &Value) -> Result<(), Box<dyn Error>> {
    let Some(relay) = bundle.get("relay").filter(|v| !v.is_null()) else {
        return Ok(());
    };
    if bundle["version"] != 2 {
        return Err("relay route requires bundle v2".into());
    }
    for key in ["address", "name", "server_name", "ca_pem", "credential"] {
        if relay[key].as_str().is_none_or(str::is_empty) {
            return Err("invalid relay bundle".into());
        }
    }
    if !brain_core::network::relay::valid_name(relay["name"].as_str().unwrap_or_default())
        || brain_core::network::relay::relay_host(relay["address"].as_str().unwrap_or_default())
            .is_err()
        || !relay["ca_pem"]
            .as_str()
            .unwrap_or_default()
            .starts_with("-----BEGIN CERTIFICATE-----")
        || brain_core::network::BearerToken::parse(relay["credential"].as_str().unwrap_or_default())
            .is_err()
    {
        return Err("invalid relay bundle".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FUTURE: i64 = 4_000_000_000;

    /// A bundle that passes every layer; the table overrides single fields.
    fn valid_bundle() -> Value {
        json!({
            "version": BUNDLE_VERSION,
            "server_name": "localhost",
            "dial": ["127.0.0.1:7443"],
            "ca_pem": "-----BEGIN CERTIFICATE-----\nX\n-----END CERTIFICATE-----\n",
            "credential": "SYNTHETIC-CREDENTIAL",
            "machine": "machine-one",
            "agent_id": "claude",
            "expires_at": FUTURE,
        })
    }

    /// G12: table of malformed bundles — every refusal names its layer and
    /// nothing is ever written to the client directory.
    #[test]
    fn bundle_refusals_follow_the_table_and_write_nothing() {
        let root = crate::test_support::temp_path("bundle-table");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let bundle_path = root.join("bundle.json");
        let client_dir = root.join("client");

        let mut cases: Vec<(String, Value, String)> = vec![
            (
                "version 0".into(),
                {
                    let mut bundle = valid_bundle();
                    bundle["version"] = json!(0);
                    bundle
                },
                "version 0 is not supported".into(),
            ),
            (
                // v2 carries the relay route (E7.7); the next unknown version is refused.
                "version 3".into(),
                {
                    let mut bundle = valid_bundle();
                    bundle["version"] = json!(3);
                    bundle
                },
                "version 3 is not supported".into(),
            ),
            (
                "version 99".into(),
                {
                    let mut bundle = valid_bundle();
                    bundle["version"] = json!(99);
                    bundle
                },
                "version 99 is not supported".into(),
            ),
            (
                "version missing".into(),
                {
                    let mut bundle = valid_bundle();
                    bundle.as_object_mut().unwrap().remove("version");
                    bundle
                },
                "version 0 is not supported".into(),
            ),
            (
                "version as string".into(),
                {
                    let mut bundle = valid_bundle();
                    bundle["version"] = json!("1");
                    bundle
                },
                "version 0 is not supported".into(),
            ),
            (
                "expired".into(),
                {
                    let mut bundle = valid_bundle();
                    bundle["expires_at"] = json!(1);
                    bundle
                },
                "credential has expired".into(),
            ),
            (
                "expires_at not a number".into(),
                {
                    let mut bundle = valid_bundle();
                    bundle["expires_at"] = json!("soon");
                    bundle
                },
                "credential has expired".into(),
            ),
            (
                "ca_pem garbage".into(),
                {
                    let mut bundle = valid_bundle();
                    bundle["ca_pem"] = json!("x");
                    bundle
                },
                "ca_pem is not a PEM certificate".into(),
            ),
            (
                "credential empty".into(),
                {
                    let mut bundle = valid_bundle();
                    bundle["credential"] = json!("");
                    bundle
                },
                "credential is empty".into(),
            ),
            (
                "credential as number".into(),
                {
                    let mut bundle = valid_bundle();
                    bundle["credential"] = json!(42);
                    bundle
                },
                "credential is empty".into(),
            ),
            (
                "server_name empty".into(),
                {
                    let mut bundle = valid_bundle();
                    bundle["server_name"] = json!("");
                    bundle
                },
                "server_name is empty".into(),
            ),
            (
                "server_name as number".into(),
                {
                    let mut bundle = valid_bundle();
                    bundle["server_name"] = json!(123);
                    bundle
                },
                "server_name is empty".into(),
            ),
            (
                "dial empty".into(),
                {
                    let mut bundle = valid_bundle();
                    bundle["dial"] = json!([]);
                    bundle
                },
                "dial list is empty".into(),
            ),
            (
                "dial not an array".into(),
                {
                    let mut bundle = valid_bundle();
                    bundle["dial"] = json!("127.0.0.1:7443");
                    bundle
                },
                "dial list is empty".into(),
            ),
            (
                "dial hostname".into(),
                {
                    let mut bundle = valid_bundle();
                    bundle["dial"] = json!(["example.com:7443"]);
                    bundle
                },
                "--allow-lan-dial".into(),
            ),
            (
                "dial lan without opt-in".into(),
                {
                    let mut bundle = valid_bundle();
                    bundle["dial"] = json!(["10.0.0.5:7443"]);
                    bundle
                },
                "--allow-lan-dial".into(),
            ),
        ];
        for key in [
            "server_name",
            "dial",
            "ca_pem",
            "credential",
            "machine",
            "agent_id",
            "expires_at",
        ] {
            let mut bundle = valid_bundle();
            bundle.as_object_mut().unwrap().remove(key);
            cases.push((
                format!("{key} missing"),
                bundle,
                format!("field {key:?} is missing"),
            ));
        }

        for (name, bundle, expected) in &cases {
            std::fs::write(&bundle_path, bundle.to_string()).expect("bundle");
            let error = join(&bundle_path, &client_dir, false, false, false, false)
                .expect_err(&format!("{name} must be refused"));
            let text = error.to_string();
            assert!(
                text.contains(expected.as_str()),
                "{name}: expected {expected:?} in {text:?}"
            );
            assert!(
                !client_dir.exists(),
                "{name}: nothing may be written for a refused bundle"
            );
        }

        // A bundle that passes every layer unpacks and marks the client dir.
        std::fs::write(&bundle_path, valid_bundle().to_string()).expect("bundle");
        join(&bundle_path, &client_dir, false, false, false, false).expect("valid bundle joins");
        assert!(client_dir.join("endpoint.json").is_file());

        // Joining again without --replace is refused and keeps the first.
        let error = join(&bundle_path, &client_dir, false, false, false, false)
            .expect_err("second join without replace must be refused");
        assert!(
            error
                .to_string()
                .contains("already holds a joined endpoint")
        );

        // A symlinked bundle is never followed.
        std::fs::write(&bundle_path, valid_bundle().to_string()).expect("bundle");
        let link = root.join("link.json");
        std::os::unix::fs::symlink(&bundle_path, &link).expect("symlink");
        let other = root.join("client-link");
        let error = join(&link, &other, false, false, false, false)
            .expect_err("symlinked bundle must be refused");
        assert!(error.to_string().contains("symlink"));

        // Not JSON at all.
        std::fs::write(&bundle_path, b"{ not json").expect("bundle");
        let error = join(
            &bundle_path,
            &root.join("client-bad"),
            false,
            false,
            false,
            false,
        )
        .expect_err("garbage must be refused");
        assert!(error.to_string().contains("not valid JSON"));

        std::fs::remove_dir_all(&root).ok();
    }
}
