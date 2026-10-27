//! Versioned coordinator capabilities. All fields are signed; no bearer Debug.
use super::{DAY, ShareResult, bad, id};
use serde::{Deserialize, Serialize};

pub const SIGNED: &str = "tls+blake3+ed25519";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rights {
    pub version: u32,
    pub issuer: String,
    pub subject: String,
    pub credential_id: String,
    pub projects: Vec<String>,
    pub permission: String,
    pub issued_at: i64,
    pub expires_at: i64,
}

impl Rights {
    pub fn verify(token: &str, authority: &str, project: &str, now: i64) -> ShareResult<Self> {
        let (body, signature) = token.split_once('.').ok_or_else(bad)?;
        let bytes = super::keys::decode(body)?;
        super::keys::verify(authority, &bytes, signature)?;
        let rights: Self = serde_json::from_slice(&bytes).map_err(|_| bad())?;
        if rights.version != 1
            || rights.issuer != authority
            || rights.permission != "read"
            || !id(&rights.subject)
            || !id(&rights.credential_id)
            || rights.projects.is_empty()
            || rights.projects.len() > 16
            || rights.projects.iter().any(|p| !id(p) || p.len() > 48)
            || !rights.projects.iter().any(|p| p == project)
            || rights.issued_at < 0
            || rights.issued_at > now
            || rights.expires_at <= now
            || rights.expires_at <= rights.issued_at
            || rights
                .expires_at
                .checked_sub(rights.issued_at)
                .is_none_or(|ttl| ttl > 90 * DAY)
        {
            return Err(bad());
        }
        Ok(rights)
    }
    pub fn sign(&self, key: &super::keys::MachineKey) -> ShareResult<String> {
        let bytes = serde_json::to_vec(self).map_err(|_| bad())?;
        Ok(format!(
            "{}.{}",
            super::keys::encode(&bytes),
            key.sign(&bytes)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::super::keys::MachineKey;
    use super::*;
    fn capability() -> (MachineKey, Rights) {
        let key = MachineKey::generate().unwrap();
        let rights = Rights {
            version: 1,
            issuer: key.public_key(),
            subject: "lead-one".into(),
            credential_id: "synthetic-token".into(),
            projects: vec!["a".into()],
            permission: "read".into(),
            issued_at: 100,
            expires_at: 200,
        };
        (key, rights)
    }
    #[test]
    fn attack_manager_project_a_cannot_read_b() {
        let (key, rights) = capability();
        let token = rights.sign(&key).unwrap();
        assert!(Rights::verify(&token, &key.public_key(), "a", 150).is_ok());
        assert!(Rights::verify(&token, &key.public_key(), "b", 150).is_err());
    }
    #[test]
    fn attack_expired_token_is_rejected_at_boundary() {
        let (key, rights) = capability();
        let token = rights.sign(&key).unwrap();
        assert!(Rights::verify(&token, &key.public_key(), "a", 199).is_ok());
        assert!(Rights::verify(&token, &key.public_key(), "a", 200).is_err());
        assert!(Rights::verify(&token, &key.public_key(), "a", 201).is_err());
    }
    #[test]
    fn capabilities_reject_forged_body_authority_permissions_and_version() {
        let (key, mut rights) = capability();
        let token = rights.sign(&key).unwrap();
        assert!(
            Rights::verify(
                &token,
                &MachineKey::generate().unwrap().public_key(),
                "a",
                150
            )
            .is_err()
        );
        let (body, signature) = token.split_once('.').unwrap();
        let mut forged: Rights =
            serde_json::from_slice(&super::super::keys::decode(body).unwrap()).unwrap();
        forged.projects.push("b".into());
        let forged = format!(
            "{}.{}",
            super::super::keys::encode(&serde_json::to_vec(&forged).unwrap()),
            signature
        );
        assert!(Rights::verify(&forged, &key.public_key(), "b", 150).is_err());
        rights.permission = "write".into();
        assert!(Rights::verify(&rights.sign(&key).unwrap(), &key.public_key(), "a", 150).is_err());
        rights.permission = "read".into();
        rights.version = 2;
        assert!(Rights::verify(&rights.sign(&key).unwrap(), &key.public_key(), "a", 150).is_err());
        assert!(Rights::verify(&token, &key.public_key(), "a", 99).is_err());
    }
}
