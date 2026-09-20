use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const VERSION: u32 = 1;
pub const MAX_FILE: u64 = 8 * 1024 * 1024 * 1024;
pub const MAX_FILES: usize = 50_000;

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
    pub folder_id: String,
    pub cert_der: String,
    pub secret: String,
}

impl Invitation {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == VERSION, "unsupported Rowd version");
        validate_hash(&self.pair_id)?;
        validate_hash(&self.folder_id)?;
        validate_hash(&self.secret)?;
        ensure!(self.cert_der.len() <= 16_384, "certificate too large");
        hex::decode(&self.cert_der)?;
        Ok(())
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
}
