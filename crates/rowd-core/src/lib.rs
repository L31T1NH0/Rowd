pub mod config;
pub mod discovery;
pub mod ignore;
pub mod managed;
pub mod model;
pub mod pairing;
pub mod protocol;
pub mod storage;
pub mod sync;
pub mod tls;
pub mod trace;
pub mod trace_render;

use anyhow::Result;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

pub fn hash_reader(reader: impl Read) -> Result<(String, u64)> {
    copy_and_hash(reader, std::io::sink())
}

/// Only for migrating committed SHA-256 bases and recovering pre-BLAKE3 journals.
pub fn legacy_hash_reader(reader: impl Read) -> Result<(String, u64)> {
    legacy_hash_reader_with_control(reader, || Ok(()))
}

pub fn legacy_hash_reader_with_control(
    mut reader: impl Read,
    mut control: impl FnMut() -> Result<()>,
) -> Result<(String, u64)> {
    let mut hash = Sha256::new();
    let mut size = 0;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        control()?;
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        size += n as u64;
    }
    Ok((hex::encode(hash.finalize()), size))
}

pub fn copy_and_hash(mut reader: impl Read, mut writer: impl Write) -> Result<(String, u64)> {
    let mut hash = blake3::Hasher::new();
    let mut size = 0;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buffer[..n])?;
        hash.update(&buffer[..n]);
        size += n as u64;
    }
    writer.flush()?;
    Ok((hash.finalize().to_hex().to_string(), size))
}

pub fn random_id() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    Ok(hex::encode(bytes))
}

pub mod io_retry;

pub mod internal_writes;

#[cfg(test)]
mod digest_tests {
    use super::*;
    #[test]
    fn blake3_vectors_and_copy_match() {
        for (bytes, expected) in [
            (
                b"".as_slice(),
                "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
            ),
            (
                b"abc".as_slice(),
                "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85",
            ),
        ] {
            let mut output = Vec::new();
            assert_eq!(
                copy_and_hash(bytes, &mut output).unwrap(),
                (expected.into(), bytes.len() as u64)
            );
            assert_eq!(output, bytes);
            assert_eq!(hash_reader(bytes).unwrap().0, expected);
            assert_ne!(legacy_hash_reader(bytes).unwrap().0, expected);
        }
    }
}
