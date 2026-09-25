use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const VERSION: u32 = 1;
pub const INVITATION_VERSION: u32 = 2;
pub const MAX_FILE: u64 = 8 * 1024 * 1024 * 1024;
pub const MAX_FILES: usize = 100_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub hash: String,
    pub size: u64,
}
pub type Manifest = BTreeMap<String, Entry>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    ToPc,
    ToAndroid,
    Conflict,
}

// Explicit absence; no delete propagation in v1. Order covers every combination.
pub fn reconcile(base: Option<&str>, pc: Option<&str>, android: Option<&str>) -> Action {
    match (pc, android) {
        (None, None) => Action::None,
        (Some(a), Some(b)) if a == b => Action::None,
        (Some(_), None) => Action::ToAndroid,
        (None, Some(_)) => Action::ToPc,
        (Some(a), Some(_)) if Some(a) == base => Action::ToPc,
        (Some(_), Some(b)) if Some(b) == base => Action::ToAndroid,
        _ => Action::Conflict,
    }
}

pub fn validate_hash(hash: &str) -> Result<()> {
    ensure!(
        hash.len() == 64
            && hash
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "invalid SHA-256"
    );
    Ok(())
}

pub fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty() && path.len() <= 2048,
        "invalid path length"
    );
    for part in path.split('/') {
        ensure!(
            !part.is_empty() && part != "." && part != "..",
            "unsafe path"
        );
        ensure!(
            part.len() <= 240 && !part.ends_with(['.', ' ']),
            "unsupported filename"
        );
        ensure!(
            !part
                .chars()
                .any(|c| c.is_control() || "\\:*?\"<>|".contains(c)),
            "unsafe filename"
        );
        let stem = part.split('.').next().unwrap().to_ascii_uppercase();
        if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit())
        {
            bail!("reserved filename");
        }
    }
    ensure!(path.split('/').next() != Some(".rowd"), "reserved path");
    Ok(())
}

pub fn validate_manifest(files: &Manifest) -> Result<()> {
    ensure!(files.len() <= MAX_FILES, "too many files");
    let mut folded = std::collections::HashSet::new();
    for (path, entry) in files {
        validate_path(path)?;
        validate_hash(&entry.hash)?;
        ensure!(entry.size <= MAX_FILE, "file exceeds 8 GiB: {path}");
        ensure!(folded.insert(path.to_lowercase()), "case-colliding paths");
        for (index, _) in path.match_indices('/') {
            ensure!(
                !files.contains_key(&path[..index]),
                "file/directory collision"
            );
        }
    }
    Ok(())
}

pub fn validate_cross_peer_namespace(pc: &Manifest, android: &Manifest) -> Result<()> {
    let mut names = BTreeMap::new();
    for path in pc.keys().chain(android.keys()) {
        let folded = path.to_lowercase();
        if let Some(previous) = names.insert(folded, path) {
            ensure!(previous == path, "case collision: {path}");
        }
    }
    for folded in names.keys() {
        for (index, _) in folded.match_indices('/') {
            ensure!(
                !names.contains_key(&folded[..index]),
                "file/directory collision: {folded}"
            );
        }
    }
    Ok(())
}

pub fn conflict_path(path: &str, hash: &str) -> String {
    // Separate names by original path AND full content hash. Never truncate identity.
    let path_hash = hex::encode(Sha256::digest(path.as_bytes()));
    let name = path.rsplit('/').next().unwrap_or("arquivo");
    format!("Rowd Conflicts/{path_hash}/{hash}/{name}")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Invitation {
    pub version: u32,
    pub address: String,
    pub pair_id: String,
    pub cert_der: String,
    pub secret: String,
}

impl Invitation {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == INVITATION_VERSION,
            "unsupported invitation version"
        );
        validate_hash(&self.pair_id)?;
        validate_hash(&self.secret)?;
        ensure!(
            !self.address.is_empty() && self.address.len() <= 2048,
            "invalid address"
        );
        ensure!(self.cert_der.len() <= 16_384, "certificate too large");
        ensure!(
            !hex::decode(&self.cert_der)?.is_empty(),
            "empty certificate"
        );
        Ok(())
    }

    pub fn encode(&self) -> Result<String> {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        self.validate()?;
        let address = self.address.as_bytes();
        let certificate = hex::decode(&self.cert_der)?;
        ensure!(
            address.len() <= u16::MAX as usize && certificate.len() <= u16::MAX as usize,
            "pairing payload is too large"
        );
        let mut bytes = Vec::with_capacity(1 + 4 + 2 + address.len() + 64 + 2 + certificate.len());
        bytes.push(2);
        bytes.extend_from_slice(&self.version.to_be_bytes());
        bytes.extend_from_slice(&(address.len() as u16).to_be_bytes());
        bytes.extend_from_slice(address);
        for value in [&self.pair_id, &self.secret] {
            bytes.extend_from_slice(&hex::decode(value)?);
        }
        bytes.extend_from_slice(&(certificate.len() as u16).to_be_bytes());
        bytes.extend_from_slice(&certificate);
        Ok(format!("rowd1:{}", URL_SAFE_NO_PAD.encode(bytes)))
    }

    pub fn decode(text: &str) -> Result<Self> {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let text = text.trim();
        if let Some(encoded) = text.strip_prefix("rowd1:") {
            let bytes = URL_SAFE_NO_PAD.decode(encoded)?;
            let mut input = std::io::Cursor::new(bytes.as_slice());
            let mut marker = [0; 1];
            std::io::Read::read_exact(&mut input, &mut marker)?;
            ensure!(marker[0] == 2, "unsupported pairing payload");
            let mut version = [0; 4];
            std::io::Read::read_exact(&mut input, &mut version)?;
            let mut length = [0; 2];
            std::io::Read::read_exact(&mut input, &mut length)?;
            let address_len = u16::from_be_bytes(length) as usize;
            ensure!(address_len <= 2048, "invalid address length");
            let mut address = vec![0; address_len];
            std::io::Read::read_exact(&mut input, &mut address)?;
            let mut pair_id = [0; 32];
            let mut secret = [0; 32];
            std::io::Read::read_exact(&mut input, &mut pair_id)?;
            std::io::Read::read_exact(&mut input, &mut secret)?;
            std::io::Read::read_exact(&mut input, &mut length)?;
            let certificate_len = u16::from_be_bytes(length) as usize;
            ensure!(certificate_len <= 8192, "certificate too large");
            let mut certificate = vec![0; certificate_len];
            std::io::Read::read_exact(&mut input, &mut certificate)?;
            ensure!(
                input.position() as usize == bytes.len(),
                "trailing pairing data"
            );
            let invite = Self {
                version: u32::from_be_bytes(version),
                address: String::from_utf8(address)?,
                pair_id: hex::encode(pair_id),
                cert_der: hex::encode(certificate),
                secret: hex::encode(secret),
            };
            invite.validate()?;
            return Ok(invite);
        }
        // Migration only: old Android installations stored JSON invitations.
        #[derive(Deserialize)]
        struct LegacyInvitation {
            version: u32,
            address: String,
            pair_id: String,
            folder_id: String,
            cert_der: String,
            secret: String,
        }
        let old: LegacyInvitation = serde_json::from_str(text)?;
        ensure!(
            old.version == VERSION,
            "unsupported legacy invitation version"
        );
        validate_hash(&old.folder_id)?;
        let invite = Self {
            version: INVITATION_VERSION,
            address: old.address,
            pair_id: old.pair_id,
            cert_der: old.cert_der,
            secret: old.secret,
        };
        invite.validate()?;
        Ok(invite)
    }

    pub fn fingerprint(&self) -> Result<String> {
        Ok(hex::encode(Sha256::digest(hex::decode(&self.cert_der)?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exhaustive_reconciliation() {
        let states = [None, Some("a"), Some("b"), Some("c")];
        for base in states {
            for pc in states {
                for android in states {
                    let action = reconcile(base, pc, android);
                    if pc == android {
                        assert_eq!(action, Action::None);
                    } else if pc.is_none() {
                        assert_eq!(action, Action::ToPc);
                    } else if android.is_none() {
                        assert_eq!(action, Action::ToAndroid);
                    } else if pc == base {
                        assert_eq!(action, Action::ToPc);
                    } else if android == base {
                        assert_eq!(action, Action::ToAndroid);
                    } else {
                        assert_eq!(action, Action::Conflict);
                    }
                }
            }
        }
    }
    #[test]
    fn paths_and_conflict_identity() {
        for path in [
            "../a", "/a", "a//b", "a/../b", "a\\b", "a\0", "CON", ".rowd/x",
        ] {
            assert!(validate_path(path).is_err(), "{path}");
        }
        validate_path("documentos/ação.txt").unwrap();
        let h = "a".repeat(64);
        assert_eq!(conflict_path("a", &h), conflict_path("a", &h));
        assert_ne!(conflict_path("a", &h), conflict_path("b", &h));
    }
    #[test]
    fn cross_peer_namespace_rejects_file_directory_and_case_collisions() {
        let entry = Entry {
            hash: "a".repeat(64),
            size: 1,
        };
        let pc = Manifest::from([("a".into(), entry.clone())]);
        let android = Manifest::from([("a/b.txt".into(), entry.clone())]);
        assert!(validate_cross_peer_namespace(&pc, &android).is_err());
        let android = Manifest::from([("A".into(), entry.clone())]);
        assert!(validate_cross_peer_namespace(&pc, &android).is_err());
        let android = Manifest::from([("b/c.txt".into(), entry)]);
        validate_cross_peer_namespace(&pc, &android).unwrap();
    }

    #[test]
    fn invitation_round_trip_and_legacy_json_migration() {
        let invitation = Invitation {
            version: INVITATION_VERSION,
            address: "127.0.0.1:43821".into(),
            pair_id: "a".repeat(64),
            cert_der: "bb".repeat(80),
            secret: "c".repeat(64),
        };
        let encoded = invitation.encode().unwrap();
        assert_eq!(
            Invitation::decode(&encoded).unwrap().pair_id,
            invitation.pair_id
        );
        assert_eq!(
            Invitation::decode(&encoded)
                .unwrap()
                .fingerprint()
                .unwrap()
                .len(),
            64
        );
        let legacy = serde_json::json!({
            "version": VERSION,
            "address": invitation.address,
            "pair_id": invitation.pair_id,
            "folder_id": "d".repeat(64),
            "cert_der": invitation.cert_der,
            "secret": invitation.secret,
        });
        assert_eq!(
            Invitation::decode(&legacy.to_string()).unwrap().version,
            INVITATION_VERSION
        );
        assert!(Invitation::decode(&format!("{encoded}AA")).is_err());
        assert!(Invitation::decode("rowd1:!!").is_err());
        assert!(Invitation::decode("{}").is_err());
    }
}
