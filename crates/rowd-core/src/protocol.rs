use crate::{
    model::{Entry, Manifest, MAX_FILE},
    random_id,
};
use anyhow::{ensure, Context, Result};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::io::{Read, Write};

const MAX_FRAME: usize = 16 * 1024 * 1024;
pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Scoped {
        share_id: String,
        message: Box<Message>,
    },
    Shares {
        shares: Vec<crate::config::ShareConfig>,
        removed: Vec<String>,
    },
    Capabilities {
        root_id: String,
        polling: bool,
        managed_shares: bool,
        share_requests: Vec<crate::config::ShareRequest>,
    },
    ShareRequestStatus {
        accepted: Vec<String>,
        pending: Vec<String>,
    },
    SelectShare {
        share_id: String,
    },
    SessionDone,
    Ack {
        path: String,
        entry: Entry,
    },
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

pub fn send_for(io: &mut impl Write, share_id: &str, message: Message) -> Result<()> {
    send(
        io,
        &Message::Scoped {
            share_id: share_id.into(),
            message: Box::new(message),
        },
    )
}
pub fn receive_for(io: &mut impl Read, share_id: &str) -> Result<Message> {
    let Message::Scoped {
        share_id: actual,
        message,
    } = receive(io)?
    else {
        anyhow::bail!("missing Share context")
    };
    ensure!(actual == share_id, "wrong Share context");
    if let Message::Error { message } = *message {
        anyhow::bail!("peer: {message}");
    }
    Ok(*message)
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
        peer == pair_id && folder == folder_id,
        "wrong pair or folder"
    );
    if version != PROTOCOL_VERSION {
        let message =
            format!("incompatible protocol: expected {PROTOCOL_VERSION}, received {version}");
        send(
            io,
            &Message::Error {
                message: message.clone(),
            },
        )?;
        anyhow::bail!(message);
    }
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
            version: PROTOCOL_VERSION,
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

#[cfg(test)]
mod v2_tests {
    use super::*;
    #[test]
    fn rejects_incompatible_protocol_and_cross_share_messages() {
        struct Buffer {
            input: std::io::Cursor<Vec<u8>>,
            output: Vec<u8>,
        }
        impl Read for Buffer {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                self.input.read(b)
            }
        }
        impl Write for Buffer {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.output.write(b)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let h = "a".repeat(64);
        let mut input = Vec::new();
        send(
            &mut input,
            &Message::Hello {
                version: 1,
                pair_id: h.clone(),
                folder_id: h.clone(),
                root_id: h.clone(),
            },
        )
        .unwrap();
        let mut io = Buffer {
            input: std::io::Cursor::new(input),
            output: vec![],
        };
        assert!(server_auth(&mut io, &h, &h, &h)
            .unwrap_err()
            .to_string()
            .contains("incompatible protocol"));
        assert!(receive(&mut std::io::Cursor::new(io.output))
            .unwrap_err()
            .to_string()
            .contains("incompatible protocol"));
        let mut data = Vec::new();
        send_for(&mut data, "first", Message::Scan).unwrap();
        assert!(receive_for(&mut std::io::Cursor::new(data), "second").is_err());
    }
}
