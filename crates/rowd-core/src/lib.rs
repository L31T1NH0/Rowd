pub mod config;
pub mod ignore;
pub mod journal;
pub mod managed;
pub mod model;
pub mod protocol;
pub mod storage;
pub mod sync;
pub mod tls;

use anyhow::Result;
use sha2::{Digest, Sha256};
use std::io::Read;

pub fn hash_reader(mut reader: impl Read) -> Result<(String, u64)> {
    let mut hash = Sha256::new();
    let mut size = 0;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        size += n as u64;
    }
    Ok((hex::encode(hash.finalize()), size))
}

pub fn random_id() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    Ok(hex::encode(bytes))
}
