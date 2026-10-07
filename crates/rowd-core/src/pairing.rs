//! Public LAN pairing hints. Only a pinned TLS connection can deliver credentials.
use crate::{discovery, model::validate_hash};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    time::{Duration, Instant},
};

pub const GROUP: Ipv4Addr = discovery::GROUP;
pub const PORT: u16 = 43823;
pub const VERSION: u32 = 1;
const MAGIC: &str = "rowd-pair";
const MAX_PACKET: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceType {
    Pc,
    Android,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Packet {
    Discover {
        nonce: String,
        device_type: DeviceType,
    },
    Announce {
        nonce: String,
        session_id: String,
        device_name: String,
        device_type: DeviceType,
        #[serde(default)]
        pair_id: Option<String>,
        #[serde(default)]
        fingerprint: Option<String>,
        #[serde(default)]
        cert_der: Option<String>,
        #[serde(default)]
        tcp_port: Option<u16>,
    },
    Offer {
        session_id: String,
        device_name: String,
        pair_id: String,
        fingerprint: String,
        cert_der: String,
        tcp_port: u16,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    magic: String,
    version: u32,
    packet: Packet,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Peer {
    pub session_id: String,
    pub device_name: String,
    pub device_type: DeviceType,
    pub pair_id: Option<String>,
    pub fingerprint: Option<String>,
    pub cert_der: Option<String>,
    pub endpoint: SocketAddr,
}

pub fn validate_pc_peer(peer: &Peer) -> Result<()> {
    ensure!(
        peer.device_type == DeviceType::Pc && peer.endpoint.is_ipv4(),
        "invalid PC peer"
    );
    validate_hash(&peer.session_id)?;
    validate_name(&peer.device_name)?;
    validate_public_pc(
        peer.pair_id.as_deref(),
        peer.fingerprint.as_deref(),
        peer.cert_der.as_deref(),
        Some(peer.endpoint.port()),
    )
}

pub fn offered_peer(packet: Packet, source: SocketAddr) -> Result<Peer> {
    let Packet::Offer {
        session_id,
        device_name,
        pair_id,
        fingerprint,
        cert_der,
        tcp_port,
    } = packet
    else {
        anyhow::bail!("not a pairing offer")
    };
    ensure!(source.is_ipv4(), "invalid offer source");
    Ok(Peer {
        session_id,
        device_name,
        device_type: DeviceType::Pc,
        pair_id: Some(pair_id),
        fingerprint: Some(fingerprint),
        cert_der: Some(cert_der),
        endpoint: SocketAddr::new(source.ip(), tcp_port),
    })
}

pub fn encode(packet: &Packet) -> Result<Vec<u8>> {
    validate(packet)?;
    let bytes = serde_json::to_vec(&Envelope {
        magic: MAGIC.into(),
        version: VERSION,
        packet: packet.clone(),
    })?;
    ensure!(bytes.len() <= MAX_PACKET, "pairing packet too large");
    Ok(bytes)
}

pub fn decode(bytes: &[u8]) -> Result<Packet> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_PACKET,
        "pairing packet size"
    );
    let envelope: Envelope = serde_json::from_slice(bytes)?;
    ensure!(
        envelope.magic == MAGIC && envelope.version == VERSION,
        "pairing magic/version"
    );
    validate(&envelope.packet)?;
    Ok(envelope.packet)
}

fn validate(packet: &Packet) -> Result<()> {
    match packet {
        Packet::Discover { nonce, .. } => validate_hash(nonce),
        Packet::Announce {
            nonce,
            session_id,
            device_name,
            device_type,
            pair_id,
            fingerprint,
            cert_der,
            tcp_port,
        } => {
            validate_hash(nonce)?;
            validate_hash(session_id)?;
            validate_name(device_name)?;
            if *device_type == DeviceType::Pc {
                validate_public_pc(
                    pair_id.as_deref(),
                    fingerprint.as_deref(),
                    cert_der.as_deref(),
                    *tcp_port,
                )
            } else {
                ensure!(
                    pair_id.is_none()
                        && fingerprint.is_none()
                        && cert_der.is_none()
                        && tcp_port.is_none(),
                    "Android announcement has PC fields"
                );
                Ok(())
            }
        }
        Packet::Offer {
            session_id,
            device_name,
            pair_id,
            fingerprint,
            cert_der,
            tcp_port,
        } => {
            validate_hash(session_id)?;
            validate_name(device_name)?;
            validate_public_pc(
                Some(pair_id),
                Some(fingerprint),
                Some(cert_der),
                Some(*tcp_port),
            )
        }
    }
}

fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.len() <= 80 && !name.chars().any(char::is_control),
        "invalid device name"
    );
    Ok(())
}

fn validate_public_pc(
    pair_id: Option<&str>,
    fingerprint: Option<&str>,
    cert_der: Option<&str>,
    port: Option<u16>,
) -> Result<()> {
    let pair_id = pair_id.ok_or_else(|| anyhow::anyhow!("missing pair id"))?;
    let fingerprint = fingerprint.ok_or_else(|| anyhow::anyhow!("missing fingerprint"))?;
    let cert_der = cert_der.ok_or_else(|| anyhow::anyhow!("missing certificate"))?;
    validate_hash(pair_id)?;
    validate_hash(fingerprint)?;
    ensure!(
        cert_der.len() <= 3072 && !cert_der.is_empty(),
        "invalid certificate size"
    );
    let cert = hex::decode(cert_der)?;
    ensure!(
        !cert.is_empty() && hex::encode(Sha256::digest(cert)) == fingerprint,
        "certificate fingerprint mismatch"
    );
    ensure!(port.is_some_and(|port| port != 0), "invalid TCP port");
    Ok(())
}

pub fn peer(packet: Packet, nonce: &str, source: SocketAddr) -> Result<Peer> {
    let Packet::Announce {
        nonce: echoed,
        session_id,
        device_name,
        device_type,
        pair_id,
        fingerprint,
        cert_der,
        tcp_port,
    } = packet
    else {
        anyhow::bail!("not an announcement")
    };
    ensure!(
        echoed == nonce && source.is_ipv4(),
        "announcement nonce/source mismatch"
    );
    Ok(Peer {
        session_id,
        device_name,
        device_type,
        pair_id,
        fingerprint,
        cert_der,
        endpoint: SocketAddr::new(source.ip(), tcp_port.unwrap_or(source.port())),
    })
}

pub fn discover(
    socket: &UdpSocket,
    target: DeviceType,
    destination: SocketAddr,
) -> Result<Vec<Peer>> {
    let nonce = crate::random_id()?;
    let query = encode(&Packet::Discover {
        nonce: nonce.clone(),
        device_type: target,
    })?;
    let start = Instant::now();
    let delays = [0, 100, 250];
    let mut sent = 0;
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    let mut buf = [0u8; MAX_PACKET + 1];
    while start.elapsed() < Duration::from_millis(700) && found.len() < 16 {
        if sent < delays.len() && start.elapsed() >= Duration::from_millis(delays[sent]) {
            socket.send_to(&query, destination)?;
            sent += 1;
            continue;
        }
        socket.set_read_timeout(Some(Duration::from_millis(50)))?;
        match crate::io_retry::poll("recv_from", || socket.recv_from(&mut buf)) {
            Ok((len, source)) => {
                if let Ok(candidate) =
                    decode(&buf[..len]).and_then(|packet| peer(packet, &nonce, source))
                {
                    if candidate.device_type == target
                        && seen.insert((candidate.session_id.clone(), candidate.endpoint.ip()))
                    {
                        found.push(candidate);
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(found)
}

pub fn verification_code(request_id: &str, fingerprint: &str, device_id: &str) -> Result<String> {
    for value in [request_id, fingerprint, device_id] {
        validate_hash(value)?;
    }
    let mut hash = Sha256::new();
    hash.update(b"rowd-pair-code-v1\0");
    for value in [request_id, fingerprint, device_id] {
        hash.update(hex::decode(value)?);
    }
    let digest = hash.finalize();
    let number = u32::from_be_bytes(digest[..4].try_into()?) % 1_000_000;
    Ok(format!("{number:06}"))
}

pub fn request(
    io: &mut (impl Read + Write),
    peer: &Peer,
    device_id: &str,
    name: &str,
) -> Result<(String, String)> {
    validate_pc_peer(peer)?;
    validate_hash(device_id)?;
    validate_name(name)?;
    let request_id = crate::random_id()?;
    crate::protocol::send(
        io,
        &crate::protocol::Message::PairRequest {
            version: VERSION,
            request_id: request_id.clone(),
            device_id: device_id.into(),
            device_name: name.into(),
        },
    )?;
    let crate::protocol::Message::PairPending {
        request_id: echoed,
        verification_code: code,
    } = crate::protocol::receive(io)?
    else {
        anyhow::bail!("pairing request rejected")
    };
    ensure!(echoed == request_id, "pairing request mismatch");
    let expected = verification_code(
        &request_id,
        peer.fingerprint
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("missing fingerprint"))?,
        device_id,
    )?;
    ensure!(code == expected, "verification code mismatch");
    Ok((request_id, code))
}

pub fn finish(
    io: &mut (impl Read + Write),
    peer: &Peer,
    device_id: &str,
) -> Result<crate::model::Invitation> {
    let crate::protocol::Message::PairAccepted { mut invitation } = crate::protocol::receive(io)?
    else {
        anyhow::bail!("pairing rejected")
    };
    invitation.validate()?;
    ensure!(
        Some(&invitation.pair_id) == peer.pair_id.as_ref()
            && Some(&invitation.cert_der) == peer.cert_der.as_ref(),
        "invitation differs from pinned certificate"
    );
    invitation.address = peer.endpoint.to_string();
    invitation.validate()?;
    crate::protocol::client_auth(io, &invitation.pair_id, &invitation.secret, device_id)?;
    Ok(invitation)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_packets_are_bounded_and_validated() {
        let nonce = "a".repeat(64);
        let packet = Packet::Discover {
            nonce: nonce.clone(),
            device_type: DeviceType::Pc,
        };
        let bytes = encode(&packet).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("secret"));
        assert!(decode(&bytes).is_ok());
        let mut wrong = bytes.clone();
        wrong[3] = b'X';
        assert!(decode(&wrong).is_err());
        let mut version: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        version["version"] = 2.into();
        assert!(decode(&serde_json::to_vec(&version).unwrap()).is_err());
        assert!(decode(&vec![0; MAX_PACKET + 1]).is_err());
        assert!(encode(&Packet::Discover {
            nonce: "x".into(),
            device_type: DeviceType::Pc
        })
        .is_err());
    }

    #[test]
    fn announcement_nonce_source_ip_and_deduplication() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let destination = server.local_addr().unwrap();
        let responder = std::thread::spawn(move || {
            let mut buf = [0u8; 4097];
            for _ in 0..3 {
                let (len, source) = server.recv_from(&mut buf).unwrap();
                let Packet::Discover { nonce, .. } = decode(&buf[..len]).unwrap() else {
                    panic!("discover expected")
                };
                let reply = Packet::Announce {
                    nonce: nonce.clone(),
                    session_id: "b".repeat(64),
                    device_name: "Phone".into(),
                    device_type: DeviceType::Android,
                    pair_id: None,
                    fingerprint: None,
                    cert_der: None,
                    tcp_port: None,
                };
                let bytes = encode(&reply).unwrap();
                server.send_to(&bytes, source).unwrap();
                server.send_to(&bytes, source).unwrap();
                let wrong = Packet::Announce {
                    nonce: "c".repeat(64),
                    session_id: "b".repeat(64),
                    device_name: "Phone".into(),
                    device_type: DeviceType::Android,
                    pair_id: None,
                    fingerprint: None,
                    cert_der: None,
                    tcp_port: None,
                };
                server.send_to(&encode(&wrong).unwrap(), source).unwrap();
            }
        });
        let found = discover(&client, DeviceType::Android, destination).unwrap();
        responder.join().unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].endpoint.ip().to_string(), "127.0.0.1");
        let cert = "ab";
        let announce = Packet::Announce {
            nonce: "a".repeat(64),
            session_id: "b".repeat(64),
            device_name: "Desktop".into(),
            device_type: DeviceType::Pc,
            pair_id: Some("c".repeat(64)),
            fingerprint: Some(hex::encode(Sha256::digest(hex::decode(cert).unwrap()))),
            cert_der: Some(cert.into()),
            tcp_port: Some(43821),
        };
        let pc = peer(
            decode(&encode(&announce).unwrap()).unwrap(),
            &"a".repeat(64),
            "127.0.0.3:9999".parse().unwrap(),
        )
        .unwrap();
        assert_eq!(pc.endpoint.to_string(), "127.0.0.3:43821");
        assert!(peer(announce, &"z".repeat(64), "127.0.0.3:9999".parse().unwrap()).is_err());
    }

    #[test]
    fn offer_has_only_public_fields_and_pinned_invitation_must_match() {
        let cert = "ab";
        let fingerprint = hex::encode(Sha256::digest(hex::decode(cert).unwrap()));
        let offer = Packet::Offer {
            session_id: "a".repeat(64),
            device_name: "Desktop".into(),
            pair_id: "b".repeat(64),
            fingerprint: fingerprint.clone(),
            cert_der: cert.into(),
            tcp_port: 43821,
        };
        let bytes = encode(&offer).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        for forbidden in ["secret", "private", "shares", "paths", "key"] {
            assert!(!text.contains(&format!("\"{forbidden}\"")));
        }
        let peer =
            offered_peer(decode(&bytes).unwrap(), "127.0.0.2:9999".parse().unwrap()).unwrap();
        assert_eq!(peer.endpoint.to_string(), "127.0.0.2:43821");
        let invitation = crate::model::Invitation {
            version: crate::model::INVITATION_VERSION,
            address: "127.0.0.1:43821".into(),
            pair_id: "b".repeat(64),
            cert_der: "cd".into(),
            secret: "e".repeat(64),
        };
        let mut frame = Vec::new();
        crate::protocol::send(
            &mut frame,
            &crate::protocol::Message::PairAccepted { invitation },
        )
        .unwrap();
        assert!(finish(&mut std::io::Cursor::new(frame), &peer, &"f".repeat(64)).is_err());
        assert!(verification_code(&"a".repeat(64), &fingerprint, &"f".repeat(64)).is_ok());
    }

    #[test]
    fn phone_collects_public_pc_announcement() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let destination = server.local_addr().unwrap();
        let responder = std::thread::spawn(move || {
            let cert = "ab";
            let mut buf = [0u8; 4097];
            for _ in 0..3 {
                let (len, source) = server.recv_from(&mut buf).unwrap();
                let Packet::Discover {
                    nonce,
                    device_type: DeviceType::Pc,
                } = decode(&buf[..len]).unwrap()
                else {
                    panic!("PC query expected")
                };
                let reply = Packet::Announce {
                    nonce,
                    session_id: "b".repeat(64),
                    device_name: "Desktop".into(),
                    device_type: DeviceType::Pc,
                    pair_id: Some("c".repeat(64)),
                    fingerprint: Some(hex::encode(Sha256::digest(hex::decode(cert).unwrap()))),
                    cert_der: Some(cert.into()),
                    tcp_port: Some(43821),
                };
                server.send_to(&encode(&reply).unwrap(), source).unwrap();
            }
        });
        let found = discover(&client, DeviceType::Pc, destination).unwrap();
        responder.join().unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].endpoint.to_string(), "127.0.0.1:43821");
        validate_pc_peer(&found[0]).unwrap();
    }
}
