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
pub const MANIFEST_CHUNK_FILES: usize = 1024;
pub const PROTOCOL_VERSION: u32 = 9;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Scoped {
        share_id: String,
        message: Box<Message>,
    },
    Shares {
        shares: Vec<crate::config::ShareDefinition>,
    },
    Capabilities {
        device_id: String,
        share_requests: Vec<crate::config::ShareRequest>,
        cancel_intents: Vec<String>,
        available_shares: Vec<String>,
        requested_share_ids: Vec<String>,
        audit: bool,
        #[serde(default)]
        unlink_requested: bool,
    },
    ShareRequestStatus {
        accepted: Vec<String>,
        #[serde(default)]
        rejected: Vec<String>,
        #[serde(default)]
        cancelled: Vec<String>,
    },
    DeviceUnlinked,
    UnlinkAck,
    UnlinkComplete,
    SelectShare {
        share_id: String,
    },
    StartRound,
    WakeShare {
        share_id: String,
    },
    ShareSkipped {
        share_id: String,
        reason: String,
    },
    SessionDone,
    RoundDeferred {
        shares: Vec<String>,
    },
    AckBatch {
        entries: Manifest,
    },
    Hello {
        version: u32,
        pair_id: String,
        device_id: String,
    },
    Challenge {
        nonce: String,
    },
    Proof {
        mac: String,
    },
    Ready,
    Scan,
    DeltaScan {
        base_token: String,
        paths: std::collections::BTreeSet<String>,
    },
    DeltaManifest {
        paths: std::collections::BTreeSet<String>,
        files: Manifest,
        metrics: crate::storage::StoreMetrics,
    },
    NeedFullScan,
    ManifestBegin {
        count: usize,
    },
    ManifestChunk {
        files: Manifest,
    },
    ManifestEnd {
        metrics: crate::storage::StoreMetrics,
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
    PutBatchEnd,
    Accept,
    Done {
        transferred: usize,
        conflicts: usize,
        base_token: String,
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

pub fn receive_after_first(io: &mut impl Read, first: u8) -> Result<Message> {
    struct Prefixed<'a, R>(&'a mut R, Option<u8>);
    impl<R: Read> Read for Prefixed<'_, R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !buf.is_empty() {
                if let Some(first) = self.1.take() {
                    buf[0] = first;
                    return Ok(1);
                }
            }
            self.0.read(buf)
        }
    }
    receive(&mut Prefixed(io, Some(first)))
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

pub fn send_manifest(io: &mut impl Write, share_id: &str, files: &Manifest) -> Result<()> {
    send_manifest_with_metrics(io, share_id, files, Default::default())
}

pub fn send_manifest_with_metrics(
    io: &mut impl Write,
    share_id: &str,
    files: &Manifest,
    metrics: crate::storage::StoreMetrics,
) -> Result<()> {
    crate::model::validate_manifest(files)?;
    send_for(io, share_id, Message::ManifestBegin { count: files.len() })?;
    let mut chunk = Manifest::new();
    for (path, entry) in files {
        chunk.insert(path.clone(), entry.clone());
        if chunk.len() == MANIFEST_CHUNK_FILES {
            send_for(
                io,
                share_id,
                Message::ManifestChunk {
                    files: std::mem::take(&mut chunk),
                },
            )?;
        }
    }
    if !chunk.is_empty() {
        send_for(io, share_id, Message::ManifestChunk { files: chunk })?;
    }
    send_for(io, share_id, Message::ManifestEnd { metrics })
}

pub fn receive_manifest(io: &mut impl Read, share_id: &str) -> Result<Manifest> {
    Ok(receive_manifest_with_metrics(io, share_id)?.0)
}

pub fn receive_manifest_with_metrics(
    io: &mut impl Read,
    share_id: &str,
) -> Result<(Manifest, crate::storage::StoreMetrics, u64)> {
    struct Counted<'a, R>(&'a mut R, u64);
    impl<R: Read> Read for Counted<'_, R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let size = self.0.read(buf)?;
            self.1 += size as u64;
            Ok(size)
        }
    }
    let mut io = Counted(io, 0);
    let Message::ManifestBegin { count } = receive_for(&mut io, share_id)? else {
        anyhow::bail!("expected manifest begin")
    };
    ensure!(count <= crate::model::MAX_FILES, "too many files");
    let mut files = Manifest::new();
    while files.len() < count {
        let Message::ManifestChunk { files: chunk } = receive_for(&mut io, share_id)? else {
            anyhow::bail!("expected manifest chunk")
        };
        ensure!(
            !chunk.is_empty() && chunk.len() <= MANIFEST_CHUNK_FILES,
            "invalid manifest chunk"
        );
        for (path, entry) in chunk {
            ensure!(
                files.insert(path.clone(), entry).is_none(),
                "duplicate manifest path: {path}"
            );
        }
        ensure!(files.len() <= count, "manifest exceeds declared count");
    }
    let Message::ManifestEnd { metrics } = receive_for(&mut io, share_id)? else {
        anyhow::bail!("expected manifest end")
    };
    crate::model::validate_manifest(&files)?;
    Ok((files, metrics, io.1))
}

pub fn copy_exact(reader: &mut impl Read, writer: &mut impl Write, size: u64) -> Result<()> {
    ensure!(size <= MAX_FILE, "file too large");
    let n = std::io::copy(&mut reader.take(size), writer)?;
    ensure!(n == size, "truncated transfer: {n}/{size}");
    writer.flush()?;
    Ok(())
}

fn auth_mac(secret: &str, nonce: &str, pair_id: &str, device_id: &str) -> Result<Hmac<Sha256>> {
    let key = hex::decode(secret)?;
    ensure!(key.len() == 32, "invalid pairing secret");
    let mut mac = Hmac::<Sha256>::new_from_slice(&key)?;
    // Fixed-length identities and a domain label avoid ambiguous concatenations.
    mac.update(b"rowd-auth-v2\0");
    for value in [nonce, pair_id, device_id] {
        crate::model::validate_hash(value)?;
        mac.update(&hex::decode(value)?);
    }
    Ok(mac)
}

pub fn server_auth(io: &mut (impl Read + Write), pair_id: &str, secret: &str) -> Result<String> {
    let Message::Hello {
        version,
        pair_id: peer,
        device_id,
    } = receive(io)?
    else {
        anyhow::bail!("expected hello")
    };
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
    ensure!(peer == pair_id, "wrong pairing identity");
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
    auth_mac(secret, &nonce, pair_id, &device_id)?
        .verify_slice(&hex::decode(mac)?)
        .context("authentication failed")?;
    send(io, &Message::Ready)?;
    Ok(device_id)
}

pub fn client_auth(
    io: &mut (impl Read + Write),
    pair_id: &str,
    secret: &str,
    device_id: &str,
) -> Result<()> {
    send(
        io,
        &Message::Hello {
            version: PROTOCOL_VERSION,
            pair_id: pair_id.into(),
            device_id: device_id.into(),
        },
    )?;
    let Message::Challenge { nonce } = receive(io)? else {
        anyhow::bail!("expected challenge")
    };
    let mac = auth_mac(secret, &nonce, pair_id, device_id)?
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
    fn proof_is_bound_to_nonce_and_device() {
        let h = "a".repeat(64);
        let other = "b".repeat(64);
        let proof = auth_mac(&h, &h, &h, &h).unwrap().finalize().into_bytes();
        assert!(auth_mac(&h, &other, &h, &h)
            .unwrap()
            .verify_slice(&proof)
            .is_err());
        assert!(auth_mac(&h, &h, &h, &other)
            .unwrap()
            .verify_slice(&proof)
            .is_err());
    }
    #[test]
    fn chunked_manifest_round_trips_and_rejects_invalid_sequences() {
        let entry = Entry {
            hash: "a".repeat(64),
            size: 1,
        };
        let files: Manifest = (0..2050)
            .map(|index| (format!("file-{index:05}"), entry.clone()))
            .collect();
        let mut bytes = Vec::new();
        send_manifest(&mut bytes, "share", &files).unwrap();
        assert_eq!(
            receive_manifest(&mut std::io::Cursor::new(bytes), "share").unwrap(),
            files
        );

        let mut missing = Vec::new();
        send_for(&mut missing, "share", Message::ManifestBegin { count: 1 }).unwrap();
        send_for(
            &mut missing,
            "share",
            Message::ManifestEnd {
                metrics: Default::default(),
            },
        )
        .unwrap();
        assert!(receive_manifest(&mut std::io::Cursor::new(missing), "share").is_err());

        let mut duplicate = Vec::new();
        send_for(&mut duplicate, "share", Message::ManifestBegin { count: 2 }).unwrap();
        for _ in 0..2 {
            send_for(
                &mut duplicate,
                "share",
                Message::ManifestChunk {
                    files: Manifest::from([("a".into(), entry.clone())]),
                },
            )
            .unwrap();
        }
        assert!(receive_manifest(&mut std::io::Cursor::new(duplicate), "share").is_err());
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
                version: 4,
                pair_id: h.clone(),
                device_id: h.clone(),
            },
        )
        .unwrap();
        let mut io = Buffer {
            input: std::io::Cursor::new(input),
            output: vec![],
        };
        assert!(server_auth(&mut io, &h, &h)
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
