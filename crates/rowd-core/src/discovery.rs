//! LAN hints only. A response never establishes trust; TLS and Rowd auth do.
use anyhow::{ensure, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    time::{Duration, Instant},
};

pub const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 42, 99);
pub const PORT: u16 = 43822;
pub const MAGIC: [u8; 4] = *b"ROWD";
const VERSION: u8 = 1;
const DISCOVER: u8 = 1;
const ANNOUNCE: u8 = 2;
const MAGIC_LEN: usize = MAGIC.len();
const HEADER_LEN: usize = MAGIC_LEN + 2;
const NONCE_LEN: usize = 32;
const FINGERPRINT_LEN: usize = 32;
const REQUEST_LEN: usize = HEADER_LEN + NONCE_LEN + FINGERPRINT_LEN;
const RESPONSE_LEN: usize = REQUEST_LEN + 2;

pub fn eligible_ipv4_addresses<I>(addresses: I) -> Vec<Ipv4Addr>
where
    I: IntoIterator<Item = Ipv4Addr>,
{
    addresses
        .into_iter()
        .filter(|address| {
            !address.is_unspecified()
                && !address.is_loopback()
                && !address.is_multicast()
                && *address != Ipv4Addr::BROADCAST
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub fn multicast_membership_changes(
    joined: &BTreeSet<Ipv4Addr>,
    available: impl IntoIterator<Item = Ipv4Addr>,
) -> (Vec<Ipv4Addr>, Vec<Ipv4Addr>) {
    let available = eligible_ipv4_addresses(available)
        .into_iter()
        .collect::<BTreeSet<_>>();
    let to_join = available.difference(joined).copied().collect();
    let to_leave = joined.difference(&available).copied().collect();
    (to_join, to_leave)
}

pub fn join_multicast_interfaces<I, E, F>(interfaces: I, mut join: F) -> Vec<Ipv4Addr>
where
    I: IntoIterator<Item = Ipv4Addr>,
    F: FnMut(&Ipv4Addr) -> Result<(), E>,
{
    interfaces
        .into_iter()
        .filter(|address| join(address).is_ok())
        .collect()
}

pub fn fingerprint(cert_der: &str) -> Result<[u8; 32]> {
    Ok(Sha256::digest(hex::decode(cert_der)?).into())
}

pub fn discover_packet(nonce: &[u8; 32], fingerprint: &[u8; 32]) -> [u8; REQUEST_LEN] {
    let mut packet = [0; REQUEST_LEN];
    packet[..MAGIC_LEN].copy_from_slice(&MAGIC);
    packet[MAGIC_LEN..HEADER_LEN].copy_from_slice(&[VERSION, DISCOVER]);
    packet[HEADER_LEN..HEADER_LEN + NONCE_LEN].copy_from_slice(nonce);
    packet[HEADER_LEN + NONCE_LEN..].copy_from_slice(fingerprint);
    packet
}

pub fn announce_packet(
    request: &[u8],
    fingerprint: &[u8; 32],
    port: u16,
) -> Result<[u8; RESPONSE_LEN]> {
    ensure!(
        request.len() == REQUEST_LEN
            && request[..MAGIC_LEN] == MAGIC
            && request[MAGIC_LEN..HEADER_LEN] == [VERSION, DISCOVER],
        "invalid discovery request"
    );
    ensure!(
        &request[HEADER_LEN + NONCE_LEN..REQUEST_LEN] == fingerprint,
        "discovery fingerprint mismatch"
    );
    ensure!(port != 0, "invalid TCP port");
    let mut packet = [0; RESPONSE_LEN];
    packet[..MAGIC_LEN].copy_from_slice(&MAGIC);
    packet[MAGIC_LEN..HEADER_LEN].copy_from_slice(&[VERSION, ANNOUNCE]);
    packet[HEADER_LEN..HEADER_LEN + NONCE_LEN]
        .copy_from_slice(&request[HEADER_LEN..HEADER_LEN + NONCE_LEN]);
    packet[HEADER_LEN + NONCE_LEN..REQUEST_LEN].copy_from_slice(fingerprint);
    packet[REQUEST_LEN..].copy_from_slice(&port.to_be_bytes());
    Ok(packet)
}

pub fn endpoint(
    packet: &[u8],
    nonce: &[u8; 32],
    fingerprint: &[u8; 32],
    source: SocketAddr,
) -> Result<SocketAddr> {
    ensure!(
        packet.len() == RESPONSE_LEN
            && packet[..MAGIC_LEN] == MAGIC
            && packet[MAGIC_LEN..HEADER_LEN] == [VERSION, ANNOUNCE],
        "invalid discovery response"
    );
    ensure!(
        &packet[HEADER_LEN..HEADER_LEN + NONCE_LEN] == nonce
            && &packet[HEADER_LEN + NONCE_LEN..REQUEST_LEN] == fingerprint,
        "discovery response mismatch"
    );
    let port = u16::from_be_bytes([packet[REQUEST_LEN], packet[REQUEST_LEN + 1]]);
    ensure!(port != 0 && source.is_ipv4(), "invalid discovery endpoint");
    Ok(SocketAddr::new(source.ip(), port))
}

pub fn collect(
    socket: &UdpSocket,
    destination: SocketAddr,
    fingerprint: &[u8; 32],
) -> Result<Vec<SocketAddr>> {
    crate::trace_event!(
        crate::trace::Level::Trace,
        crate::trace::Component::Discovery,
        "DISCOVERY_START",
        serde_json::json!({"endpoint":destination.to_string()})
    );
    let mut nonce = [0; 32];
    getrandom::getrandom(&mut nonce).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    let query = discover_packet(&nonce, fingerprint);
    let started = Instant::now();
    let until = started + Duration::from_millis(700);
    let query_delays = [
        Duration::ZERO,
        Duration::from_millis(100),
        Duration::from_millis(250),
    ];
    let mut next_query = 0;
    let mut candidates = Vec::new();
    let mut packet = [0; 256];
    while Instant::now() < until && candidates.len() < 8 {
        let now = Instant::now();
        if next_query < query_delays.len() && now >= started + query_delays[next_query] {
            socket.send_to(&query, destination)?;
            crate::trace_event!(
                crate::trace::Level::Trace,
                crate::trace::Component::Discovery,
                "DISCOVERY_QUERY_SENT",
                serde_json::json!({"endpoint":destination.to_string(),"attempt":next_query+1})
            );
            next_query += 1;
            continue;
        }
        let next_send = query_delays
            .get(next_query)
            .map(|delay| started + *delay)
            .unwrap_or(until);
        let read_until = until.min(next_send);
        socket.set_read_timeout(Some(
            read_until
                .saturating_duration_since(now)
                .max(Duration::from_millis(1)),
        ))?;
        match crate::io_retry::interrupted("discovery_recv_from", || socket.recv_from(&mut packet))
        {
            Ok((len, source)) => {
                crate::trace_event!(
                    crate::trace::Level::Trace,
                    crate::trace::Component::Discovery,
                    "DISCOVERY_REPLY_RECEIVED",
                    serde_json::json!({"endpoint":source.to_string(),"payload_size":len})
                );
                let candidate = endpoint(&packet[..len], &nonce, fingerprint, source);
                if let Err(error) = &candidate {
                    crate::trace_event!(
                        crate::trace::Level::Warn,
                        crate::trace::Component::Discovery,
                        "DISCOVERY_CANDIDATE_REJECTED",
                        serde_json::json!({"endpoint":source.to_string(),"source":"multicast","reason":"invalid_reply","error":crate::trace::TraceError::new("discovery","validate_reply",error)})
                    );
                }
                if let Ok(address) = candidate {
                    if !candidates.contains(&address) {
                        candidates.push(address);
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
    crate::trace_event!(
        crate::trace::Level::Trace,
        crate::trace::Component::Discovery,
        "DISCOVERY_END",
        serde_json::json!({"candidates":candidates.len(),"duration_us":started.elapsed().as_micros()})
    );
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_packets_and_uses_source_ip() {
        let nonce = [7; 32];
        let fp = [9; 32];
        let request = discover_packet(&nonce, &fp);
        assert!(announce_packet(&request[..REQUEST_LEN - 1], &fp, 42).is_err());
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
        bad[HEADER_LEN + NONCE_LEN] ^= 1;
        assert!(endpoint(&bad, &nonce, &fp, source).is_err());
        bad = response;
        bad[REQUEST_LEN + 1] = 0;
        bad[REQUEST_LEN] = 0;
        assert!(endpoint(&bad, &nonce, &fp, source).is_err());
    }

    #[test]
    fn rejects_wrong_magic_version_type_nonce_fingerprint_and_truncation() {
        let nonce = [7; 32];
        let fp = [9; 32];
        let request = discover_packet(&nonce, &fp);
        let response = announce_packet(&request, &fp, 43821).unwrap();
        let source = "127.0.0.2:9999".parse().unwrap();

        let mut bad_request = request;
        bad_request[0] ^= 1;
        assert!(announce_packet(&bad_request, &fp, 42).is_err());
        bad_request = request;
        bad_request[MAGIC_LEN] = 2;
        assert!(announce_packet(&bad_request, &fp, 42).is_err());
        bad_request = request;
        bad_request[MAGIC_LEN + 1] = ANNOUNCE;
        assert!(announce_packet(&bad_request, &fp, 42).is_err());

        let mut bad_response = response;
        bad_response[0] ^= 1;
        assert!(endpoint(&bad_response, &nonce, &fp, source).is_err());
        bad_response = response;
        bad_response[MAGIC_LEN] = 2;
        assert!(endpoint(&bad_response, &nonce, &fp, source).is_err());
        bad_response = response;
        bad_response[MAGIC_LEN + 1] = DISCOVER;
        assert!(endpoint(&bad_response, &nonce, &fp, source).is_err());
        assert!(endpoint(&response[..RESPONSE_LEN - 1], &nonce, &fp, source).is_err());

        let mut bad_nonce = response;
        bad_nonce[HEADER_LEN] ^= 1;
        assert!(endpoint(&bad_nonce, &nonce, &fp, source).is_err());
        let mut bad_fingerprint = response;
        bad_fingerprint[HEADER_LEN + NONCE_LEN] ^= 1;
        assert!(endpoint(&bad_fingerprint, &nonce, &fp, source).is_err());
        assert!(announce_packet(&request, &fp, 0).is_err());
    }

    #[test]
    fn selects_lan_addresses_and_membership_changes_independently() {
        let loopback = Ipv4Addr::LOCALHOST;
        let wlan = "192.168.1.20".parse().unwrap();
        let ethernet = "10.0.0.20".parse().unwrap();
        let joined = [wlan].into_iter().collect();
        assert_eq!(
            eligible_ipv4_addresses([
                loopback,
                Ipv4Addr::UNSPECIFIED,
                Ipv4Addr::BROADCAST,
                wlan,
                ethernet,
            ]),
            vec![ethernet, wlan]
        );
        assert_eq!(
            multicast_membership_changes(&joined, [loopback, wlan, ethernet]),
            (vec![ethernet], Vec::new())
        );
        assert_eq!(
            multicast_membership_changes(&[wlan, ethernet].into_iter().collect(), [wlan]),
            (Vec::new(), vec![ethernet])
        );
        assert_eq!(
            join_multicast_interfaces([wlan, ethernet], |address| {
                if *address == wlan {
                    Err("membership failed")
                } else {
                    Ok(())
                }
            }),
            vec![ethernet]
        );
    }

    #[test]
    fn collects_unicast_responses_on_loopback() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let fp = [3; 32];
        let destination = server.local_addr().unwrap();
        let responder = std::thread::spawn(move || {
            let mut buffer = [0; 256];
            let (size, _first_peer) = server.recv_from(&mut buffer).unwrap();
            let first = buffer[..size].to_vec();
            let false_reply = announce_packet(&buffer[..size], &[4; 32], 43821).unwrap_err();
            assert!(false_reply.to_string().contains("fingerprint"));
            let (size, peer) = server.recv_from(&mut buffer).unwrap();
            assert_eq!(&buffer[..size], first.as_slice());
            let mut false_reply = announce_packet(&buffer[..size], &fp, 43821).unwrap();
            false_reply[HEADER_LEN + NONCE_LEN] ^= 1;
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

#[derive(Clone, Debug)]
struct VerifiedEndpoint {
    peer_key: String,
    address: SocketAddr,
    generation: u64,
}

#[derive(Default)]
pub struct EndpointResolver {
    last: Option<VerifiedEndpoint>,
    generation: u64,
    trace_context: crate::trace::TraceContext,
}

impl EndpointResolver {
    pub fn trace_context(&self) -> crate::trace::TraceContext {
        self.trace_context.clone()
    }
    fn authenticate(
        invite: &crate::model::Invitation,
        device: &str,
        address: &str,
        timeout: Duration,
    ) -> Result<(crate::tls::ClientStream, crate::trace::TraceContext)> {
        let context = crate::trace::current_context()
            .with("connection_attempt_id", crate::trace::new_id("attempt"));
        let _scope = context.clone().enter();
        crate::trace_event!(
            crate::trace::Level::Debug,
            crate::trace::Component::Connection,
            "CONNECTION_ATTEMPT",
            serde_json::json!({"endpoint":address,"timeout_ms":timeout.as_millis()})
        );
        let result = (|| -> Result<_> {
            let mut io = crate::tls::connect_to(invite, address, timeout)?;
            io.sock.set_read_timeout(Some(timeout))?;
            io.sock.set_write_timeout(Some(timeout))?;
            crate::protocol::client_auth(&mut io, &invite.pair_id, &invite.secret, device)?;
            io.sock.set_read_timeout(Some(Duration::from_secs(90)))?;
            io.sock.set_write_timeout(Some(Duration::from_secs(90)))?;
            let context = context.with("connection_id", crate::trace::new_id("connection"));
            let _connected = context.clone().enter();
            crate::trace_event!(
                crate::trace::Level::Info,
                crate::trace::Component::Connection,
                "CONNECTION_AUTHENTICATED",
                serde_json::json!({"endpoint":address,"network_generation":crate::trace::current_context().ids.get("network_generation")})
            );
            Ok((io, context))
        })();
        if let Err(error) = &result {
            crate::trace_event!(
                crate::trace::Level::Warn,
                crate::trace::Component::Connection,
                "CONNECTION_ATTEMPT_FAILED",
                serde_json::json!({"endpoint":address,"error":crate::trace::TraceError::new("connection","authenticate_candidate",error)})
            );
        }
        result
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
        let _generation = crate::trace::current_context()
            .with("network_generation", generation)
            .enter();
        let fingerprint = fingerprint(&invite.cert_der)?;
        let peer_key = format!("{}:{}", invite.pair_id, hex::encode(fingerprint));
        let changed = self.generation != generation;
        self.generation = generation;
        if self
            .last
            .as_ref()
            .is_some_and(|cached| cached.peer_key != peer_key)
        {
            crate::trace_event!(
                crate::trace::Level::Debug,
                crate::trace::Component::Discovery,
                "DISCOVERY_CACHE_REJECTED",
                serde_json::json!({"reason":"peer_identity_changed"})
            );
            self.last = None;
        }
        let mut candidates = Vec::new();
        if !changed {
            if let Some(last) = &self.last {
                let address = last.address.to_string();
                crate::trace_event!(
                    crate::trace::Level::Debug,
                    crate::trace::Component::Discovery,
                    "DISCOVERY_CANDIDATE",
                    serde_json::json!({"endpoint":address,"source":"cache","reason":"network_generation_unchanged"})
                );
                if let Ok((io, context)) =
                    Self::authenticate(invite, device, &address, Duration::from_millis(800))
                {
                    self.trace_context = context;
                    return Ok((io, address));
                }
            }
        }
        match discover(&fingerprint) {
            Ok(found) => {
                candidates.extend(found.into_iter().take(8).map(|address| address.to_string()))
            }
            Err(error) => crate::trace_event!(
                crate::trace::Level::Warn,
                crate::trace::Component::Discovery,
                "DISCOVERY_FAILED",
                serde_json::json!({"reason":"continue_with_cached_or_invitation_endpoint","error":crate::trace::TraceError::new("discovery","discover",&error)})
            ),
        }
        if changed {
            if let Some(last) = &self.last {
                candidates.push(last.address.to_string());
            }
        }
        candidates.push(invite.address.clone());
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|address| seen.insert(address.clone()));
        let mut last_error = None;
        let cached_address = self
            .last
            .as_ref()
            .filter(|cached| cached.generation == generation)
            .map(|cached| cached.address);
        for address in candidates.into_iter().take(10) {
            crate::trace_event!(
                crate::trace::Level::Debug,
                crate::trace::Component::Discovery,
                "DISCOVERY_CANDIDATE",
                serde_json::json!({"endpoint":address,"network_generation":generation,"source":if address==invite.address {"invitation"}else if cached_address.is_some_and(|a|a.to_string()==address){"cache"}else{"multicast"}})
            );
            let timeout = if cached_address.is_some_and(|cached| cached.to_string() == address) {
                Duration::from_millis(800)
            } else {
                Duration::from_secs(3)
            };
            match Self::authenticate(invite, device, &address, timeout) {
                Ok((io, context)) => {
                    self.trace_context = context;
                    self.last = Some(VerifiedEndpoint {
                        peer_key: peer_key.clone(),
                        address: address.parse()?,
                        generation,
                    });
                    return Ok((io, address));
                }
                Err(error) => {
                    crate::trace_event!(
                        crate::trace::Level::Warn,
                        crate::trace::Component::Discovery,
                        "DISCOVERY_CANDIDATE_REJECTED",
                        serde_json::json!({"endpoint":address,"reason":"authentication_or_connection_failed","error":crate::trace::TraceError::new("connection","authenticate_candidate",&error)})
                    );
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no PC endpoint available")))
    }
}
