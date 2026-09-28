//! LAN hints only. A response never establishes trust; TLS and Rowd auth do.
use anyhow::{ensure, Result};
use sha2::{Digest, Sha256};
use std::{
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    time::{Duration, Instant},
};

pub const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 42, 99);
pub const PORT: u16 = 43822;
const VERSION: u8 = 1;
const DISCOVER: u8 = 1;
const ANNOUNCE: u8 = 2;
const REQUEST_LEN: usize = 34;
const RESPONSE_LEN: usize = 68;

pub fn fingerprint(cert_der: &str) -> Result<[u8; 32]> {
    Ok(Sha256::digest(hex::decode(cert_der)?).into())
}

pub fn discover_packet(nonce: &[u8; 32]) -> [u8; REQUEST_LEN] {
    let mut packet = [0; REQUEST_LEN];
    packet[..2].copy_from_slice(&[VERSION, DISCOVER]);
    packet[2..].copy_from_slice(nonce);
    packet
}

pub fn announce_packet(
    request: &[u8],
    fingerprint: &[u8; 32],
    port: u16,
) -> Result<[u8; RESPONSE_LEN]> {
    ensure!(
        request.len() == REQUEST_LEN && request[..2] == [VERSION, DISCOVER],
        "invalid discovery request"
    );
    ensure!(port != 0, "invalid TCP port");
    let mut packet = [0; RESPONSE_LEN];
    packet[..2].copy_from_slice(&[VERSION, ANNOUNCE]);
    packet[2..34].copy_from_slice(&request[2..]);
    packet[34..66].copy_from_slice(fingerprint);
    packet[66..].copy_from_slice(&port.to_be_bytes());
    Ok(packet)
}

pub fn endpoint(
    packet: &[u8],
    nonce: &[u8; 32],
    fingerprint: &[u8; 32],
    source: SocketAddr,
) -> Result<SocketAddr> {
    ensure!(
        packet.len() == RESPONSE_LEN && packet[..2] == [VERSION, ANNOUNCE],
        "invalid discovery response"
    );
    ensure!(
        &packet[2..34] == nonce && &packet[34..66] == fingerprint,
        "discovery response mismatch"
    );
    let port = u16::from_be_bytes([packet[66], packet[67]]);
    ensure!(port != 0 && source.is_ipv4(), "invalid discovery endpoint");
    Ok(SocketAddr::new(source.ip(), port))
}

pub fn collect(
    socket: &UdpSocket,
    destination: SocketAddr,
    fingerprint: &[u8; 32],
) -> Result<Vec<SocketAddr>> {
    let mut nonce = [0; 32];
    getrandom::getrandom(&mut nonce).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    socket.send_to(&discover_packet(&nonce), destination)?;
    let until = Instant::now() + Duration::from_millis(700);
    let mut candidates = Vec::new();
    let mut packet = [0; 256];
    while Instant::now() < until && candidates.len() < 8 {
        socket.set_read_timeout(Some(
            until
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1)),
        ))?;
        match socket.recv_from(&mut packet) {
            Ok((len, source)) => {
                if let Ok(address) = endpoint(&packet[..len], &nonce, fingerprint, source) {
                    if !candidates.contains(&address) {
                        candidates.push(address);
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                break
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_packets_and_uses_source_ip() {
        let nonce = [7; 32];
        let fp = [9; 32];
        let request = discover_packet(&nonce);
        assert!(announce_packet(&request[..33], &fp, 42).is_err());
        let response = announce_packet(&request, &fp, 43821).unwrap();
        let source = "127.0.0.2:9999".parse().unwrap();
        assert_eq!(
            endpoint(&response, &nonce, &fp, source)
                .unwrap()
                .to_string(),
            "127.0.0.2:43821"
        );
        assert!(endpoint(&response[..67], &nonce, &fp, source).is_err());
        let mut bad = response;
        bad[34] ^= 1;
        assert!(endpoint(&bad, &nonce, &fp, source).is_err());
        bad = response;
        bad[67] = 0;
        bad[66] = 0;
        assert!(endpoint(&bad, &nonce, &fp, source).is_err());
    }

    #[test]
    fn collects_unicast_responses_on_loopback() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let fp = [3; 32];
        let destination = server.local_addr().unwrap();
        let responder = std::thread::spawn(move || {
            let mut buffer = [0; 256];
            let (size, peer) = server.recv_from(&mut buffer).unwrap();
            let mut false_reply = announce_packet(&buffer[..size], &[4; 32], 43821).unwrap();
            server.send_to(&false_reply, peer).unwrap();
            false_reply = announce_packet(&buffer[..size], &fp, 43821).unwrap();
            server.send_to(&false_reply, peer).unwrap();
            server.send_to(&false_reply, peer).unwrap();
        });
        let found = collect(&client, destination, &fp).unwrap();
        responder.join().unwrap();
        assert_eq!(found, vec!["127.0.0.1:43821".parse().unwrap()]);
    }
}

#[derive(Default)]
pub struct EndpointResolver {
    last: Option<SocketAddr>,
    generation: u64,
}

impl EndpointResolver {
    fn authenticate(
        invite: &crate::model::Invitation,
        device: &str,
        address: &str,
        timeout: Duration,
    ) -> Result<crate::tls::ClientStream> {
        let mut io = crate::tls::connect_to(invite, address, timeout)?;
        io.sock.set_read_timeout(Some(timeout))?;
        io.sock.set_write_timeout(Some(timeout))?;
        crate::protocol::client_auth(&mut io, &invite.pair_id, &invite.secret, device)?;
        io.sock.set_read_timeout(Some(Duration::from_secs(90)))?;
        io.sock.set_write_timeout(Some(Duration::from_secs(90)))?;
        Ok(io)
    }
    pub fn connect<F>(
        &mut self,
        invite: &crate::model::Invitation,
        device: &str,
        generation: u64,
        discover: F,
    ) -> Result<(crate::tls::ClientStream, String)>
    where
        F: FnOnce(&[u8; 32]) -> Result<Vec<SocketAddr>>,
    {
        let changed = self.generation != generation;
        self.generation = generation;
        let mut candidates = Vec::new();
        if !changed {
            if let Some(last) = self.last {
                let address = last.to_string();
                if let Ok(io) =
                    Self::authenticate(invite, device, &address, Duration::from_millis(800))
                {
                    return Ok((io, address));
                }
            }
        }
        let fingerprint = fingerprint(&invite.cert_der)?;
        if let Ok(found) = discover(&fingerprint) {
            candidates.extend(found.into_iter().take(8).map(|address| address.to_string()));
        }
        if changed {
            if let Some(last) = self.last {
                candidates.push(last.to_string());
            }
        }
        candidates.push(invite.address.clone());
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|address| seen.insert(address.clone()));
        let mut last_error = None;
        for address in candidates.into_iter().take(10) {
            let timeout = if self.last.is_some_and(|last| last.to_string() == address) {
                Duration::from_millis(800)
            } else {
                Duration::from_secs(3)
            };
            match Self::authenticate(invite, device, &address, timeout) {
                Ok(io) => {
                    self.last = address.parse().ok();
                    return Ok((io, address));
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no PC endpoint available")))
    }
}
