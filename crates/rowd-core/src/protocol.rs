use crate::{
    model::{Entry, Manifest, MAX_FILE, VERSION},
    random_id,
};
use anyhow::{ensure, Context, Result};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::io::{Read, Write};

const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Hello {
        version: u32,
        pair_id: String,
        folder_id: String,
        root_id: String,
    },
    Challenge {
        nonce: String,
    },
    Proof {
        mac: String,
    },
    Ready,
    Scan,
    Files {
        files: Manifest,
    },
    Get {
        path: String,
        entry: Entry,
    },
    Blob {
        entry: Entry,
    },
    Put {
        path: String,
        entry: Entry,
        expected: Option<String>,
    },
    Accept,
    Done {
        transferred: usize,
        conflicts: usize,
    },
    Error {
        message: String,
    },
}

pub fn send(io: &mut impl Write, message: &Message) -> Result<()> {
    let bytes = serde_json::to_vec(message)?;
    ensure!(bytes.len() <= MAX_FRAME, "frame too large");
    io.write_all(&(bytes.len() as u32).to_be_bytes())?;
    io.write_all(&bytes)?;
    io.flush()?;
    Ok(())
}

pub fn receive(io: &mut impl Read) -> Result<Message> {
    let mut len = [0; 4];
    io.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    ensure!(len <= MAX_FRAME, "frame too large");
    let mut data = vec![0; len];
    io.read_exact(&mut data)?;
    let message: Message = serde_json::from_slice(&data)?;
    if let Message::Error { message } = message {
        anyhow::bail!("peer: {message}");
    }
    Ok(message)
}

pub fn copy_exact(reader: &mut impl Read, writer: &mut impl Write, size: u64) -> Result<()> {
    ensure!(size <= MAX_FILE, "file too large");
    let n = std::io::copy(&mut reader.take(size), writer)?;
    ensure!(n == size, "truncated transfer: {n}/{size}");
    writer.flush()?;
    Ok(())
}

fn auth_mac(
    secret: &str,
    nonce: &str,
    pair_id: &str,
    folder_id: &str,
    root_id: &str,
) -> Result<Hmac<Sha256>> {
    let key = hex::decode(secret)?;
    ensure!(key.len() == 32, "invalid pairing secret");
    let mut mac = Hmac::<Sha256>::new_from_slice(&key)?;
    // Fixed-length identities and a domain label avoid ambiguous concatenations.
    mac.update(b"rowd-auth-v1\0");
    for value in [nonce, pair_id, folder_id, root_id] {
        crate::model::validate_hash(value)?;
        mac.update(&hex::decode(value)?);
    }
    Ok(mac)
}

pub fn server_auth(
    io: &mut (impl Read + Write),
    pair_id: &str,
    folder_id: &str,
    secret: &str,
) -> Result<String> {
    let Message::Hello {
        version,
        pair_id: peer,
        folder_id: folder,
        root_id,
    } = receive(io)?
    else {
        anyhow::bail!("expected hello")
    };
    ensure!(
        version == VERSION && peer == pair_id && folder == folder_id,
        "wrong pair or folder"
    );
    let nonce = random_id()?;
    send(
        io,
        &Message::Challenge {
            nonce: nonce.clone(),
        },
    )?;
    let Message::Proof { mac } = receive(io)? else {
        anyhow::bail!("expected auth proof")
    };
    auth_mac(secret, &nonce, pair_id, folder_id, &root_id)?
        .verify_slice(&hex::decode(mac)?)
        .context("authentication failed")?;
    send(io, &Message::Ready)?;
    Ok(root_id)
}

pub fn client_auth(
    io: &mut (impl Read + Write),
    pair_id: &str,
    folder_id: &str,
    secret: &str,
    root_id: &str,
) -> Result<()> {
    send(
        io,
        &Message::Hello {
            version: VERSION,
            pair_id: pair_id.into(),
            folder_id: folder_id.into(),
            root_id: root_id.into(),
        },
    )?;
    let Message::Challenge { nonce } = receive(io)? else {
        anyhow::bail!("expected challenge")
    };
    let mac = auth_mac(secret, &nonce, pair_id, folder_id, root_id)?
        .finalize()
        .into_bytes();
    send(
        io,
        &Message::Proof {
            mac: hex::encode(mac),
        },
    )?;
    ensure!(
        matches!(receive(io)?, Message::Ready),
        "authentication rejected"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_and_truncation() {
        assert!(receive(&mut std::io::Cursor::new(u32::MAX.to_be_bytes())).is_err());
        assert!(copy_exact(&mut &b"abc"[..], &mut Vec::new(), 4).is_err());
    }
    #[test]
    fn proof_is_bound_to_nonce_and_root() {
        let h = "a".repeat(64);
        let other = "b".repeat(64);
        let proof = auth_mac(&h, &h, &h, &h, &h)
            .unwrap()
            .finalize()
            .into_bytes();
        assert!(auth_mac(&h, &other, &h, &h, &h)
            .unwrap()
            .verify_slice(&proof)
            .is_err());
        assert!(auth_mac(&h, &h, &h, &h, &other)
            .unwrap()
            .verify_slice(&proof)
            .is_err());
    }
}
