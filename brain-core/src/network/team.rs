//! Single-use onboarding and a read-only coordinator gateway. Source tokens
//! stay at the coordinator; every page reauthenticates both manager and worker.
use super::{
    AuthStore, Authenticated, BearerToken, CoordRequest, NetError, TlsCallResult, TlsCoordClient,
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// No Debug: this host-local descriptor contains the source credential.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub dial: String,
    pub server_name: String,
    pub ca_pem: String,
    pub credential: String,
    pub origin: String,
    #[serde(default)]
    pub allow_lan: bool,
    #[serde(default)]
    pub allow_cgnat: bool,
}
impl Source {
    fn validate(&self) -> Result<(), NetError> {
        if !crate::team::id(&self.origin) || self.ca_pem.len() > 16384 {
            return Err(NetError::invalid("invalid team source"));
        }
        TlsCoordClient::team_source(self).map(|_| ())
    }
}

impl AuthStore {
    /// Team gateway credentials are secrets (legacy auth stored only hashes).
    /// Refuse unsafe existing state rather than silently chmod an owner's files.
    fn private_team_state(&self) -> Result<(), NetError> {
        use std::os::unix::fs::MetadataExt;
        let uid =
            super::tls::current_uid().ok_or_else(|| NetError::forbidden("unknown state owner"))?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| NetError::forbidden("invalid state"))?;
        let directory = std::fs::symlink_metadata(parent)?;
        if !directory.is_dir() || directory.uid() != uid || directory.mode() & 0o022 != 0 {
            return Err(NetError::forbidden(
                "team state directory must be owned and not group/world writable",
            ));
        }
        for suffix in ["", "-wal", "-shm"] {
            let path = std::path::PathBuf::from(format!("{}{suffix}", self.path.display()));
            let metadata = match std::fs::symlink_metadata(path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && !suffix.is_empty() => {
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
                return Err(NetError::forbidden(
                    "team auth state must be owner-only regular files",
                ));
            }
        }
        Ok(())
    }

    /// Local admin only. Existing identities are never overwritten by an invitation.
    pub fn team_invite(
        &self,
        name: &str,
        role: &str,
        project: &str,
        ttl: i64,
    ) -> Result<BearerToken, NetError> {
        self.private_team_state()?;
        if !["worker", "manager"].contains(&role) || !crate::team::id(project) || project.len() > 48
        {
            return Err(NetError::invalid("invalid team role or project"));
        }
        let exists: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM principals WHERE machine_id=?1)",
            [name],
            |r| r.get(0),
        )?;
        if exists {
            return Err(NetError::invalid(
                "team name already registered; choose a new name",
            ));
        }
        let tx = self.connection.unchecked_transaction()?;
        let space = format!("team.{project}");
        self.upsert_principal(name, "team", name, role, &[&space])?;
        let id: i64 = tx.query_row(
            "SELECT id FROM principals WHERE machine_id=?1 AND agent_id='team'",
            [name],
            |r| r.get(0),
        )?;
        let token = self.issue_credential(name, "team", Some(ttl))?;
        tx.execute(
            "INSERT INTO team_members(principal_id,project,space) VALUES(?1,?2,?3)",
            params![id, project, space],
        )?;
        tx.execute(
            "INSERT INTO team_invitations(token_id,principal_id) VALUES(?1,?2)",
            params![token.token_id().as_str(), id],
        )?;
        if role == "manager" {
            self.grant_memory_read(name, "team", &space)?;
        }
        tx.commit()?;
        Ok(token)
    }

    pub(super) fn team_is_invitation(&self, auth: &Authenticated) -> Result<bool, NetError> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM team_invitations WHERE token_id=?1)",
            [auth.token_id.as_str()],
            |r| r.get(0),
        )?)
    }

    /// Safe membership projection: no tokens, paths, CA material or addresses.
    pub fn team_who(&self, project: Option<&str>, now: i64) -> Result<Value, NetError> {
        self.private_team_state()?;
        let mut st = self.connection.prepare("SELECT p.machine_id,p.agent_id,p.role,p.disabled_at,m.project,m.accepted_at,m.last_slice_at,
            (SELECT max(c.last_used_at) FROM credentials c WHERE c.principal_id=p.id)
            FROM principals p LEFT JOIN team_members m ON m.principal_id=p.id
            WHERE (?1 IS NULL OR m.project=?1) ORDER BY p.machine_id,p.agent_id LIMIT 200")?;
        let members = st.query_map([project], |r| {
            let slice: Option<i64> = r.get(6)?;
            let role: String = r.get(2)?;
            let project: Option<String> = r.get(4)?;
            Ok(json!({"name":r.get::<_,String>(0)?,"agent":r.get::<_,String>(1)?,
                "role":if role=="reader" {"manager"} else {&role},
                "legacy":project.is_none(),"project":project,"removed":r.get::<_,Option<i64>>(3)?.is_some(),
                "accepted_at":r.get::<_,Option<i64>>(5)?,"last_seen_at":r.get::<_,Option<i64>>(7)?,
                "generated_at":slice,"freshness": match slice { None=>"unknown",Some(t) if now-t>crate::team::DAY=>"stale",_=>"fresh"}}))
        })?.collect::<Result<Vec<_>,_>>()?;
        Ok(
            json!({"status":"ok","admin":{"role":"admin","authority":"local coordinator owner"},"members":members,"limit":200}),
        )
    }

    fn team_project(&self, auth: &Authenticated) -> Result<(String, String), NetError> {
        self.connection.query_row("SELECT project,space FROM team_members WHERE principal_id=?1 AND accepted_at IS NOT NULL", [auth.principal.id], |r|Ok((r.get(0)?,r.get(1)?)))
            .optional()?.ok_or_else(||NetError::forbidden("not an accepted team member"))
    }

    fn team_accept(
        &self,
        auth: &Authenticated,
        req: &CoordRequest,
        now: i64,
    ) -> Result<Value, NetError> {
        if !self.team_is_invitation(auth)? {
            return Err(NetError::forbidden("not an invitation"));
        }
        let role = if auth.principal.role == "reader" {
            "manager"
        } else {
            "worker"
        };
        if req.arguments["expected_role"] != role
            || req.arguments["expected_name"] != auth.principal.machine_id
        {
            return Err(NetError::forbidden("invitation identity or role mismatch"));
        }
        let source = match req.arguments.get("source") {
            Some(v) if auth.principal.role == "worker" => {
                let source: Source = serde_json::from_value(v.clone())
                    .map_err(|_| NetError::invalid("invalid source"))?;
                // Address scope is approved by host-local invite, never by the remote payload.
                let policy: String = self.connection.query_row(
                    "SELECT value FROM team_route_policy WHERE principal_id=?1",
                    [auth.principal.id],
                    |r| r.get(0),
                )?;
                let policy: Value = serde_json::from_str(&policy)
                    .map_err(|_| NetError::invalid("invalid route policy"))?;
                if (source.allow_lan && policy["allow_lan"] != true)
                    || (source.allow_cgnat && policy["allow_cgnat"] != true)
                {
                    return Err(NetError::forbidden("source address scope not approved"));
                }
                source.validate()?;
                Some(
                    serde_json::to_string(&source)
                        .map_err(|_| NetError::invalid("invalid source"))?,
                )
            }
            None => None,
            _ => {
                return Err(NetError::invalid(
                    "worker requires source; manager may not publish",
                ));
            }
        };
        let tx = self.connection.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE team_invitations SET consumed_at=?2 WHERE token_id=?1 AND consumed_at IS NULL",
            params![auth.token_id.as_str(), now],
        )?;
        if changed != 1 {
            return Err(NetError::unauthorized());
        }
        self.revoke_credential(auth.token_id.as_str())?;
        let token = self.issue_credential(&auth.principal.machine_id, auth.agent_id(), None)?;
        tx.execute(
            "UPDATE team_members SET accepted_at=?2,source=?3 WHERE principal_id=?1",
            params![auth.principal.id, now, source],
        )?;
        tx.commit()?;
        // Returned only in the encrypted accept response, written 0600 by the CLI.
        Ok(
            json!({"credential":token.as_str(),"expires_at":now+super::auth::DEFAULT_CREDENTIAL_TTL_SECS}),
        )
    }

    pub fn team_route_policy(
        &self,
        name: &str,
        allow_lan: bool,
        allow_cgnat: bool,
    ) -> Result<(), NetError> {
        self.connection.execute("INSERT INTO team_route_policy SELECT id,?2 FROM principals WHERE machine_id=?1 AND agent_id='team' ON CONFLICT(principal_id) DO UPDATE SET value=excluded.value",
            params![name,json!({"allow_lan":allow_lan,"allow_cgnat":allow_cgnat}).to_string()])?;
        Ok(())
    }

    pub(super) fn team_call(
        &self,
        auth: &Authenticated,
        req: &CoordRequest,
        now: i64,
    ) -> Result<Value, NetError> {
        self.private_team_state()?;
        if req.tool == "team_accept" {
            return self.team_accept(auth, req, now);
        }
        let (project, space) = self.team_project(auth)?;
        if req.tool == "team_directory" {
            return self.team_directory(auth, &project, &space);
        }
        if req.tool == "team_register" {
            if auth.principal.role != "worker" || req.space() != Some(space.as_str()) {
                return Err(NetError::forbidden("worker only"));
            }
            let source: Source = serde_json::from_value(
                req.arguments
                    .get("source")
                    .cloned()
                    .ok_or_else(|| NetError::invalid("source required"))?,
            )
            .map_err(|_| NetError::invalid("invalid source"))?;
            let policy: String = self.connection.query_row(
                "SELECT value FROM team_route_policy WHERE principal_id=?1",
                [auth.principal.id],
                |r| r.get(0),
            )?;
            let policy: Value = serde_json::from_str(&policy)
                .map_err(|_| NetError::invalid("invalid route policy"))?;
            if (source.allow_lan && policy["allow_lan"] != true)
                || (source.allow_cgnat && policy["allow_cgnat"] != true)
            {
                return Err(NetError::forbidden("route scope not approved"));
            }
            source.validate()?;
            self.connection.execute(
                "UPDATE team_members SET source=?2 WHERE principal_id=?1",
                params![
                    auth.principal.id,
                    serde_json::to_string(&source)
                        .map_err(|_| NetError::invalid("invalid source"))?
                ],
            )?;
            return Ok(json!({"status":"ok"}));
        }
        if auth.principal.role != "reader"
            || !auth.memory_read_spaces().contains(&space)
            || req.space() != Some(space.as_str())
        {
            return Err(NetError::forbidden("manager read grant required"));
        }
        let name = req.arguments["source_name"].as_str().unwrap_or_default();
        let source:Option<String>=self.connection.query_row("SELECT m.source FROM team_members m JOIN principals p ON p.id=m.principal_id
            WHERE p.machine_id=?1 AND m.project=?2 AND m.space=?3 AND p.disabled_at IS NULL AND p.role='worker' AND m.accepted_at IS NOT NULL
            AND EXISTS(SELECT 1 FROM credentials c WHERE c.principal_id=p.id AND c.revoked_at IS NULL AND c.not_after>?4)",params![name,project,space,now],|r|r.get(0)).optional()?.flatten();
        let source: Source =
            serde_json::from_str(&source.ok_or_else(|| NetError::forbidden("source unavailable"))?)
                .map_err(|_| NetError::unavailable("source config"))?;
        let client = TlsCoordClient::team_source(&source)?;
        let mut args = req.arguments.clone();
        args.remove("source_name");
        let body = match client.call("share_pull", "", args, req.request_id.clone()) {
            TlsCallResult::Ok(v) => v,
            TlsCallResult::Refused(_) => return Err(NetError::forbidden("source denied")),
            _ => return Err(NetError::unavailable("source offline")),
        };
        if body["result"]["share"]["origin"] != source.origin {
            return Err(NetError::forbidden("source origin mismatch"));
        }
        let share = body["result"]["share"].clone();
        if let Some(events) = share["events"].as_array() {
            for event in events {
                if let Some(t) = event["fact"]["payload"]["generated_at"].as_i64() {
                    self.connection.execute("UPDATE team_members SET last_slice_at=?2 WHERE principal_id IN (SELECT id FROM principals WHERE machine_id=?1)",params![name,t])?;
                }
            }
        }
        Ok(json!({"status":"ok","share":share}))
    }

    fn team_directory(
        &self,
        auth: &Authenticated,
        project: &str,
        space: &str,
    ) -> Result<Value, NetError> {
        let mut st=self.connection.prepare("SELECT p.machine_id,m.source FROM team_members m JOIN principals p ON p.id=m.principal_id WHERE m.project=?1 AND p.disabled_at IS NULL AND p.role='worker' AND m.accepted_at IS NOT NULL AND m.source IS NOT NULL ORDER BY p.machine_id LIMIT 200")?;
        let mut peers = vec![];
        if auth.principal.role == "reader" && auth.memory_read_spaces().contains(&space.to_string())
        {
            for row in st.query_map([project], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })? {
                let (name, text) = row?;
                let source: Source = serde_json::from_str(&text)
                    .map_err(|_| NetError::unavailable("source config"))?;
                peers.push(
                    json!({"machine":name,"origin":source.origin,"project":project,"space":space}),
                );
            }
        }
        Ok(
            json!({"status":"ok","peers":peers,"team":self.team_who(Some(project),super::auth::now_unix())?}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn store() -> (AuthStore, std::path::PathBuf) {
        let dir = crate::test_support::temp_path("team-roles");
        (AuthStore::open(&dir).unwrap(), dir)
    }
    #[test]
    fn v3_migration_preserves_tokens_identity_scopes_and_grants() {
        let (auth, dir) = store();
        auth.upsert_principal(
            "legacy-host",
            "bound-agent",
            "Existing",
            "reader",
            &["team.demo"],
        )
        .unwrap();
        auth.grant_memory_read("legacy-host", "bound-agent", "team.demo")
            .unwrap();
        let token = auth
            .issue_credential("legacy-host", "bound-agent", None)
            .unwrap();
        auth.connection.execute_batch("DROP TABLE team_invitations; DROP TABLE team_members; DROP TABLE team_route_policy; PRAGMA user_version=3;").unwrap();
        drop(auth);
        let auth = AuthStore::open(&dir).unwrap();
        let current = auth
            .authenticate(&token, super::super::auth::now_unix(), "test")
            .unwrap();
        assert_eq!(current.agent_id(), "bound-agent");
        assert_eq!(current.principal.role, "reader");
        assert!(!current.principal.may_write());
        assert_eq!(current.memory_read_spaces(), ["team.demo"]);
        let who = auth.team_who(None, super::super::auth::now_unix()).unwrap();
        assert_eq!(who["members"][0]["role"], "manager");
        assert_eq!(who["members"][0]["legacy"], true);
        assert!(auth.team_project(&current).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn redemption_is_single_use_and_wrong_role_cannot_consume_it() {
        let (auth, dir) = store();
        let token = auth
            .team_invite("manager-one", "manager", "demo", 60)
            .unwrap();
        let now = super::super::auth::now_unix();
        let pending = auth.authenticate(&token, now, "test").unwrap();
        let mut request = CoordRequest {
            tool: "team_accept".into(),
            agent: String::new(),
            arguments: serde_json::from_value(
                json!({"space":"team.demo","expected_role":"worker","expected_name":"manager-one"}),
            )
            .unwrap(),
            request_id: None,
        };
        assert!(auth.team_call(&pending, &request, now).is_err());
        request
            .arguments
            .insert("expected_role".into(), json!("manager"));
        let response = auth.team_call(&pending, &request, now).unwrap();
        assert!(auth.authenticate(&token, now, "test").is_err());
        assert!(auth.team_call(&pending, &request, now).is_err());
        let member = BearerToken::parse(response["credential"].as_str().unwrap()).unwrap();
        let live = auth.authenticate(&member, now, "test").unwrap();
        assert!(!auth.team_is_invitation(&live).unwrap());
        assert!(!live.principal.may_write());
        auth.disable_principal("manager-one").unwrap();
        assert!(auth.authenticate(&member, now, "test").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn worker_invitation_has_no_read_grants_and_expiry_is_server_enforced() {
        let (auth, dir) = store();
        let token = auth
            .team_invite("worker-one", "worker", "demo", 60)
            .unwrap();
        let now = super::super::auth::now_unix();
        let pending = auth.authenticate(&token, now, "test").unwrap();
        assert!(pending.memory_read_spaces().is_empty());
        assert!(auth.authenticate(&token, now + 61, "test").is_err());
        assert!(
            auth.team_invite("worker-one", "manager", "demo", 60)
                .is_err()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
