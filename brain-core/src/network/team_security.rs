//! Coordinator key pins, scoped capability verification and audited rotation.
use super::{AuthStore, Authenticated, CoordRequest, NetError};
use crate::team::{keys::MachineKey, security::Rights};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

fn denied() -> NetError {
    NetError::forbidden(
        "rights token expired, invalid or project not granted / токен просрочен или проект не разрешён",
    )
}
impl AuthStore {
    pub fn team_authority(&self) -> Result<String, NetError> {
        self.private_team_state()?;
        MachineKey::ensure(self.path.parent().ok_or_else(denied)?)
            .map(|k| k.public_key())
            .map_err(|_| denied())
    }
    pub(super) fn team_capability(
        &self,
        auth: &Authenticated,
        project: &str,
        now: i64,
    ) -> Result<String, NetError> {
        let token: Option<String> = self
            .connection
            .query_row(
                "SELECT rights FROM team_security WHERE principal_id=?1",
                [auth.principal.id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let token = token.ok_or_else(denied)?;
        let authority = MachineKey::load(self.path.parent().ok_or_else(denied)?)
            .map_err(|_| denied())?
            .public_key();
        let rights = Rights::verify(&token, &authority, project, now).map_err(|_| denied())?;
        if rights.subject != auth.principal.machine_id
            || rights.credential_id != auth.token_id.as_str()
        {
            return Err(denied());
        }
        Ok(token)
    }
    pub(super) fn team_projects(&self, auth: &Authenticated) -> Result<Vec<String>, NetError> {
        let text: String = self
            .connection
            .query_row(
                "SELECT projects FROM team_security WHERE principal_id=?1",
                [auth.principal.id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(denied)?;
        serde_json::from_str(&text).map_err(|_| denied())
    }
    pub(super) fn team_pin(&self, name: &str) -> Result<String, NetError> {
        self.connection.query_row("SELECT s.public_key FROM team_security s JOIN principals p ON p.id=s.principal_id WHERE p.machine_id=?1 AND p.disabled_at IS NULL", [name], |r|r.get(0)).optional()?.flatten().ok_or_else(||NetError::forbidden("подпись не прошла / machine key not pinned"))
    }
    pub(super) fn team_rekey(
        &self,
        auth: &Authenticated,
        req: &CoordRequest,
        now: i64,
    ) -> Result<Value, NetError> {
        let old = req
            .arguments
            .get("old_key")
            .and_then(Value::as_str)
            .ok_or_else(denied)?;
        let new = req
            .arguments
            .get("public_key")
            .and_then(Value::as_str)
            .ok_or_else(denied)?;
        let proof = req
            .arguments
            .get("proof")
            .and_then(Value::as_str)
            .ok_or_else(denied)?;
        let bytes = serde_json::to_vec(&json!([
            "brain-team-rekey/1",
            auth.principal.machine_id,
            old,
            new
        ]))
        .map_err(|_| denied())?;
        crate::team::keys::validate_public(new).map_err(|_| denied())?;
        crate::team::keys::verify(old, &bytes, proof).map_err(|_| denied())?;
        let tx = self.connection.unchecked_transaction()?;
        let pinned = self.team_pin(&auth.principal.machine_id)?;
        if pinned == new {
            // Retry after an uncertain response: only the exact journaled transition.
            let recorded: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM team_key_audit WHERE principal_id=?1 AND old_key=?2 AND new_key=?3 AND epoch=(SELECT key_epoch FROM team_security WHERE principal_id=?1))",params![auth.principal.id,old,new], |r|r.get(0))?;
            if !recorded {
                return Err(denied());
            }
        } else {
            if pinned != old || new == old {
                return Err(denied());
            }
            tx.execute("UPDATE team_security SET public_key=?2,key_epoch=key_epoch+1 WHERE principal_id=?1",params![auth.principal.id,new])?;
            tx.execute(
                "UPDATE team_members SET last_slice_at=NULL WHERE principal_id=?1",
                [auth.principal.id],
            )?;
            tx.execute("INSERT INTO team_key_audit(principal_id,at,old_key,new_key,epoch) SELECT principal_id,?2,?3,?4,key_epoch FROM team_security WHERE principal_id=?1",params![auth.principal.id,now,old,new])?;
        }
        tx.commit()?;
        Ok(json!({"status":"ok","public_key":new,"rotation":"journaled"}))
    }
    /// A joined source checks the coordinator signature, read permission, project and clock.
    /// TLS credential/recipient authorization still runs independently.
    pub(super) fn team_source_rights(&self, req: &CoordRequest, now: i64) -> Result<(), NetError> {
        let Some(space) = req.space().filter(|s| crate::team::is_team(s)) else {
            return Ok(());
        };
        let path = self
            .path
            .parent()
            .ok_or_else(denied)?
            .join("team-authority.pub");
        if !path.exists()
            && !self
                .path
                .parent()
                .ok_or_else(denied)?
                .join("membership.json")
                .exists()
        {
            return Ok(());
        } // explicit manually pinned peers have local grants
        let authority = crate::team::keys::read_authority(self.path.parent().ok_or_else(denied)?)
            .map_err(|_| denied())?;
        let project = space.strip_prefix("team.").ok_or_else(denied)?;
        let token = req
            .arguments
            .get("rights")
            .and_then(Value::as_str)
            .ok_or_else(denied)?;
        Rights::verify(token, &authority, project, now).map_err(|_| denied())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::BearerToken;
    fn request(token: &str, space: &str) -> CoordRequest {
        CoordRequest {
            tool: "share_pull".into(),
            agent: String::new(),
            arguments: serde_json::from_value(json!({"space":space,"rights":token})).unwrap(),
            request_id: None,
        }
    }
    #[test]
    fn source_independently_checks_projects_expiry_signature_and_missing_token() {
        let dir = crate::test_support::temp_path("source-rights");
        let auth = AuthStore::open(&dir).unwrap();
        let key = MachineKey::generate().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(dir.join("team-authority.pub"), key.public_key()).unwrap();
        std::fs::set_permissions(
            dir.join("team-authority.pub"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let rights = Rights {
            version: 1,
            issuer: key.public_key(),
            subject: "manager-one".into(),
            credential_id: "token-one".into(),
            projects: vec!["a".into()],
            permission: "read".into(),
            issued_at: 100,
            expires_at: 200,
        };
        let token = rights.sign(&key).unwrap();
        assert!(
            auth.team_source_rights(&request(&token, "team.a"), 150)
                .is_ok()
        );
        assert!(
            auth.team_source_rights(&request(&token, "team.b"), 150)
                .is_err()
        );
        assert!(
            auth.team_source_rights(&request(&token, "team.a"), 200)
                .is_err()
        );
        assert!(
            auth.team_source_rights(&request("", "team.a"), 150)
                .is_err()
        );
        let forged = rights.sign(&MachineKey::generate().unwrap()).unwrap();
        assert!(
            auth.team_source_rights(&request(&forged, "team.a"), 150)
                .is_err()
        );
        drop(auth);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn coordinator_checks_signed_scope_expiry_and_credential_binding() {
        let dir = crate::test_support::temp_path("coordinator-rights");
        let auth = AuthStore::open(&dir).unwrap();
        let now = super::super::auth::now_unix();
        let token = auth
            .team_invite_projects("lead-one", "manager", &["a".into(), "b".into()], 60)
            .unwrap();
        let pending = auth.authenticate(&token, now, "test").unwrap();
        let key = MachineKey::generate().unwrap();
        let req=CoordRequest{tool:"team_accept".into(),agent:String::new(),arguments:serde_json::from_value(json!({"space":"team.a","expected_role":"manager","expected_name":"lead-one","public_key":key.public_key()})).unwrap(),request_id:None};
        let response = auth.team_call(&pending, &req, now).unwrap();
        let token = BearerToken::parse(response["credential"].as_str().unwrap()).unwrap();
        let current = auth.authenticate(&token, now, "test").unwrap();
        assert!(auth.team_capability(&current, "a", now).is_ok());
        assert!(auth.team_capability(&current, "b", now).is_ok());
        assert!(auth.team_capability(&current, "c", now).is_err());
        // Invitation issuance reads the wall clock independently; it can cross
        // a second boundary after the test captured `now`.
        let expires_at = response["expires_at"].as_i64().unwrap();
        assert!(auth.team_capability(&current, "a", expires_at - 1).is_ok());
        assert!(auth.team_capability(&current, "a", expires_at).is_err());
        // Token remains valid at the transport layer; signed rights have expired.
        assert!(auth.authenticate(&token, expires_at, "test").is_ok());
        let mut rights: Rights = serde_json::from_slice(
            &crate::team::keys::decode(
                response["rights"]
                    .as_str()
                    .unwrap()
                    .split_once('.')
                    .unwrap()
                    .0,
            )
            .unwrap(),
        )
        .unwrap();
        rights.credential_id = "another-credential".into();
        let signed = rights.sign(&MachineKey::load(&dir).unwrap()).unwrap();
        auth.connection
            .execute("UPDATE team_security SET rights=?1", [signed])
            .unwrap();
        assert!(auth.team_capability(&current, "a", now).is_err());
        drop(auth);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn rekey_retries_only_the_last_proven_transition_without_duplicate_audit() {
        let dir = crate::test_support::temp_path("rekey-retry");
        let auth = AuthStore::open(&dir).unwrap();
        let now = super::super::auth::now_unix();
        let invitation = auth.team_invite("worker-one", "worker", "a", 60).unwrap();
        let pending = auth.authenticate(&invitation, now, "test").unwrap();
        let old = MachineKey::generate().unwrap();
        let accept = CoordRequest {
            tool: "team_accept".into(),
            agent: String::new(),
            arguments: serde_json::from_value(json!({"space":"team.a",
                "expected_role":"worker","expected_name":"worker-one",
                "public_key":old.public_key()}))
            .unwrap(),
            request_id: None,
        };
        let response = auth.team_call(&pending, &accept, now).unwrap();
        let token = BearerToken::parse(response["credential"].as_str().unwrap()).unwrap();
        let current = auth.authenticate(&token, now, "test").unwrap();
        let new = MachineKey::generate().unwrap();
        let transition = |old: &MachineKey, new: &MachineKey| {
            let bytes = serde_json::to_vec(&json!([
                "brain-team-rekey/1",
                "worker-one",
                old.public_key(),
                new.public_key()
            ]))
            .unwrap();
            CoordRequest {
                tool: "team_rekey".into(),
                agent: String::new(),
                arguments: serde_json::from_value(json!({"space":"team.a",
                    "old_key":old.public_key(),"public_key":new.public_key(),
                    "proof":old.sign(&bytes)}))
                .unwrap(),
                request_id: None,
            }
        };
        let request = transition(&old, &new);
        let mut forged = request.clone();
        forged
            .arguments
            .insert("proof".into(), json!(new.sign(b"forged")));
        assert!(auth.team_call(&current, &forged, now).is_err());
        assert_eq!(auth.team_pin("worker-one").unwrap(), old.public_key());
        auth.team_call(&current, &request, now).unwrap();
        // The client did not receive the response and retries its pending key.
        auth.team_call(&current, &request, now + 1).unwrap();
        assert_eq!(auth.team_pin("worker-one").unwrap(), new.public_key());
        let rotations: i64 = auth
            .connection
            .query_row(
                "SELECT count(*) FROM team_key_audit WHERE old_key IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rotations, 1);
        let third = MachineKey::generate().unwrap();
        assert!(
            auth.team_call(&current, &transition(&old, &third), now + 2)
                .is_err()
        );
        auth.team_call(&current, &transition(&new, &third), now + 2)
            .unwrap();
        assert!(auth.team_call(&current, &request, now + 3).is_err());
        assert_eq!(auth.team_pin("worker-one").unwrap(), third.public_key());
        drop(auth);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn attack_revoked_machine_cannot_authenticate_even_with_signed_rights() {
        let dir = crate::test_support::temp_path("revoked-signed-machine");
        let auth = AuthStore::open(&dir).unwrap();
        auth.upsert_principal("worker-one", "team", "Worker", "worker", &["team.a"])
            .unwrap();
        let token = auth.issue_credential("worker-one", "team", None).unwrap();
        let now = super::super::auth::now_unix();
        assert!(auth.authenticate(&token, now, "test").is_ok());
        auth.disable_principal("worker-one").unwrap();
        assert!(auth.authenticate(&token, now, "test").is_err());
        drop(auth);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
