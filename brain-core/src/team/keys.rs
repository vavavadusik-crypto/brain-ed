//! Owner-only Ed25519 machine key. Secret material has no Debug or Display.
use super::{ShareResult, bad};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub const KEY_FILE: &str = "team-ed25519.key";
pub struct MachineKey {
    seed: [u8; 32],
}

pub fn encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn decode(text: &str) -> ShareResult<Vec<u8>> {
    if text.len() > 32768
        || !text.len().is_multiple_of(2)
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(bad());
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|s| {
            let n = |b: u8| if b <= b'9' { b - b'0' } else { b - b'a' + 10 };
            Ok(n(s[0]) * 16 + n(s[1]))
        })
        .collect()
}
/// Bounded no-symlink read of the coordinator trust anchor installed by accept.
pub fn read_authority(state: &Path) -> ShareResult<String> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(state.join("team-authority.pub"))
        .map_err(|_| bad())?;
    let m = file.metadata().map_err(|_| bad())?;
    if !m.is_file()
        || m.len() != 64
        || m.mode() & 0o777 != 0o600
        || Some(m.uid()) != crate::network::tls::current_uid()
    {
        return Err(bad());
    }
    let mut bytes = [0; 64];
    file.read_exact(&mut bytes).map_err(|_| bad())?;
    let public = String::from_utf8(bytes.to_vec()).map_err(|_| bad())?;
    validate_public(&public)?;
    Ok(public)
}
fn check_state(state: &Path) -> ShareResult<()> {
    let metadata = std::fs::symlink_metadata(state).map_err(|_| bad())?;
    if !metadata.is_dir()
        || metadata.mode() & 0o022 != 0
        || Some(metadata.uid()) != crate::network::tls::current_uid()
    {
        return Err(bad());
    }
    Ok(())
}
fn key_path(state: &Path) -> PathBuf {
    state.join(KEY_FILE)
}
impl MachineKey {
    pub fn generate() -> ShareResult<Self> {
        let mut seed = [0; 32];
        std::fs::File::open("/dev/urandom")
            .map_err(|_| bad())?
            .read_exact(&mut seed)
            .map_err(|_| bad())?;
        Ok(Self { seed })
    }
    pub fn ensure(state: &Path) -> ShareResult<Self> {
        check_state(state)?;
        if key_path(state).symlink_metadata().is_ok() {
            return Self::load(state);
        }
        let key = Self::generate()?;
        match key.write_new(&key_path(state)) {
            Ok(()) => Ok(key),
            Err(_) if key_path(state).exists() => Self::load(state),
            Err(e) => Err(e),
        }
    }
    pub fn load_pending(state: &Path) -> ShareResult<Self> {
        Self::read(&state.join("team-ed25519.pending"))
    }
    pub fn load(state: &Path) -> ShareResult<Self> {
        Self::read(&key_path(state))
    }
    fn read(path: &Path) -> ShareResult<Self> {
        check_state(path.parent().ok_or_else(bad)?)?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|_| bad())?;
        let m = file.metadata().map_err(|_| bad())?;
        if !m.is_file()
            || m.len() != 32
            || m.mode() & 0o777 != 0o600
            || Some(m.uid()) != crate::network::tls::current_uid()
        {
            return Err(bad());
        }
        let mut seed = [0; 32];
        file.read_exact(&mut seed).map_err(|_| bad())?;
        Ok(Self { seed })
    }
    pub fn write_new(&self, path: &Path) -> ShareResult<()> {
        check_state(path.parent().ok_or_else(bad)?)?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|_| bad())?;
        file.write_all(&self.seed).map_err(|_| bad())?;
        file.sync_all().map_err(|_| bad())?;
        Ok(())
    }
    /// Promote a prewritten pending key only after the coordinator confirmed it.
    pub fn promote_pending(state: &Path) -> ShareResult<()> {
        Self::read(&state.join("team-ed25519.pending"))?;
        std::fs::rename(state.join("team-ed25519.pending"), key_path(state)).map_err(|_| bad())?;
        std::fs::File::open(state)
            .map_err(|_| bad())?
            .sync_all()
            .map_err(|_| bad())?;
        Ok(())
    }
}

impl MachineKey {
    fn key_pair(&self) -> ring::signature::Ed25519KeyPair {
        // Every 32-byte seed is valid for this fixed-size API.
        ring::signature::Ed25519KeyPair::from_seed_unchecked(&self.seed)
            .expect("32-byte Ed25519 seed")
    }
    pub fn public_key(&self) -> String {
        use ring::signature::KeyPair;
        encode(self.key_pair().public_key().as_ref())
    }
    pub fn sign(&self, message: &[u8]) -> String {
        encode(self.key_pair().sign(message).as_ref())
    }
}
pub fn validate_public(public: &str) -> ShareResult<()> {
    let bytes: [u8; 32] = decode(public)?.try_into().map_err(|_| bad())?;
    if bytes.iter().all(|b| *b == 0) {
        return Err(bad());
    }
    Ok(())
}
pub fn verify(public: &str, message: &[u8], signature: &str) -> ShareResult<()> {
    validate_public(public)?;
    let public = decode(public)?;
    let signature = decode(signature)?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public)
        .verify(message, &signature)
        .map_err(|_| bad())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rfc8032_vector_and_private_key_file_contract() {
        let key = MachineKey {
            seed: decode("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
                .unwrap()
                .try_into()
                .unwrap(),
        };
        assert_eq!(
            key.public_key(),
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        );
        let signature = key.sign(b"");
        assert_eq!(
            signature,
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        );
        verify(&key.public_key(), b"", &signature).unwrap();
        assert!(verify(&key.public_key(), b"tamper", &signature).is_err());
        let state = crate::test_support::temp_path("ed25519-key");
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&state)
            .unwrap();
        key.write_new(&key_path(&state)).unwrap();
        assert_eq!(
            std::fs::metadata(key_path(&state)).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(
            MachineKey::load(&state).unwrap().public_key(),
            key.public_key()
        );
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(key_path(&state), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(MachineKey::ensure(&state).is_err());
        std::fs::remove_file(key_path(&state)).unwrap();
        std::os::unix::fs::symlink("/dev/zero", key_path(&state)).unwrap();
        assert!(MachineKey::ensure(&state).is_err());
        std::fs::remove_dir_all(state).unwrap();
    }
}
