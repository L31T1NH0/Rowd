use rowd_core::{
    model::{Invitation, INVITATION_VERSION},
    protocol, random_id,
    storage::{LocalStore, Store},
    sync::{self, State},
    tls,
};
use std::{
    fs,
    net::TcpListener,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

fn invitation(address: String) -> (Invitation, String) {
    let cert = rcgen::generate_simple_self_signed(vec!["rowd.local".into()]).unwrap();
    (
        Invitation {
            version: INVITATION_VERSION,
            address,
            pair_id: random_id().unwrap(),
            secret: random_id().unwrap(),
            cert_der: hex::encode(cert.cert.der()),
        },
        hex::encode(cert.key_pair.serialize_der()),
    )
}

#[test]
fn tls_and_hmac_sync_in_both_directions() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let (invite, key) = invitation(listener.local_addr().unwrap().to_string());
    let config = tls::server_config(&invite.cert_der, &key).unwrap();
    let pc = tempfile::tempdir().unwrap();
    let phone = tempfile::tempdir().unwrap();
    fs::write(pc.path().join("empty"), []).unwrap();
    fs::write(pc.path().join("large.bin"), vec![0xab; 1024 * 1024 + 37]).unwrap();
    fs::write(phone.path().join("ação.txt"), "Olá do celular").unwrap();
    let root = random_id().unwrap();
    let share_id = random_id().unwrap();
    std::thread::scope(|scope| {
        let server = scope.spawn(|| {
            let mut store = LocalStore::open(pc.path()).unwrap();
            let path = store.private().join("state.json");
            let mut state = State::load(&path, &invite.pair_id, &share_id).unwrap();
            let (socket, _) = listener.accept().unwrap();
            let mut io = tls::accept(socket, config).unwrap();
            let peer = protocol::server_auth(&mut io, &invite.pair_id, &invite.secret).unwrap();
            assert_eq!(peer, root);
            sync::coordinate(&mut io, &mut store, &mut state, &path).unwrap()
        });
        let mut client = LocalStore::open(phone.path()).unwrap();
        let report = sync::client_round(&invite, &root, &mut client).unwrap();
        assert_eq!(report.transferred, 3);
        server.join().unwrap();
    });
    assert_eq!(
        LocalStore::open(pc.path()).unwrap().scan().unwrap(),
        LocalStore::open(phone.path()).unwrap().scan().unwrap()
    );
}

#[test]
fn incorrect_secret_cannot_read_manifest() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let (invite, key) = invitation(listener.local_addr().unwrap().to_string());
    let config = tls::server_config(&invite.cert_der, &key).unwrap();
    let mut wrong = invite.clone();
    wrong.secret = random_id().unwrap();
    std::thread::scope(|scope| {
        let server = scope.spawn(|| {
            let (socket, _) = listener.accept().unwrap();
            let mut io = tls::accept(socket, config).unwrap();
            assert!(protocol::server_auth(&mut io, &invite.pair_id, &invite.secret).is_err());
        });
        let mut io = tls::connect(&wrong).unwrap();
        assert!(protocol::client_auth(
            &mut io,
            &wrong.pair_id,
            &wrong.secret,
            &random_id().unwrap()
        )
        .is_err());
        server.join().unwrap();
    });
}

#[test]
fn resolver_rejects_false_candidate_then_authenticates_discovered_pc() {
    let good = TcpListener::bind("127.0.0.1:0").unwrap();
    let false_pc = TcpListener::bind("127.0.0.1:0").unwrap();
    let (invite, key) = invitation("127.0.0.1:1".into());
    let (false_invite, false_key) = invitation(false_pc.local_addr().unwrap().to_string());
    let good_config = tls::server_config(&invite.cert_der, &key).unwrap();
    let false_config = tls::server_config(&false_invite.cert_der, &false_key).unwrap();
    let device = random_id().unwrap();
    let candidate = good.local_addr().unwrap();
    let spoofed = false_pc.local_addr().unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let (socket, _) = false_pc.accept().unwrap();
            let mut io = tls::accept(socket, false_config).unwrap();
            let _ = protocol::server_auth(&mut io, &false_invite.pair_id, &false_invite.secret);
        });
        scope.spawn(|| {
            let (socket, _) = good.accept().unwrap();
            let mut io = tls::accept(socket, good_config).unwrap();
            assert_eq!(
                protocol::server_auth(&mut io, &invite.pair_id, &invite.secret).unwrap(),
                device
            );
        });
        let mut resolver = rowd_core::discovery::EndpointResolver::default();
        let (_io, address) = resolver
            .connect(&invite, &device, 0, |_| Ok(vec![spoofed, candidate]))
            .unwrap();
        assert_eq!(address, candidate.to_string());
    });
}

#[test]
fn resolver_reuses_authenticated_endpoint_until_network_changes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let candidate = listener.local_addr().unwrap();
    let (invite, key) = invitation("127.0.0.1:1".into());
    let config = tls::server_config(&invite.cert_der, &key).unwrap();
    let device = random_id().unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for _ in 0..3 {
                let (socket, _) = listener.accept().unwrap();
                let mut io = tls::accept(socket, config.clone()).unwrap();
                assert_eq!(
                    protocol::server_auth(&mut io, &invite.pair_id, &invite.secret).unwrap(),
                    device
                );
            }
        });
        let mut resolver = rowd_core::discovery::EndpointResolver::default();
        let (first, _) = resolver
            .connect(&invite, &device, 0, |_| Ok(vec![candidate]))
            .unwrap();
        drop(first);
        let (second, _) = resolver
            .connect(&invite, &device, 0, |_| {
                panic!("healthy cached endpoint must skip discovery")
            })
            .unwrap();
        drop(second);
        let (_third, _) = resolver
            .connect(&invite, &device, 1, |_| Ok(vec![candidate]))
            .unwrap();
    });
}

#[test]
fn resolver_tries_discovery_after_cached_endpoint_fails() {
    let old = TcpListener::bind("127.0.0.1:0").unwrap();
    let next = TcpListener::bind("127.0.0.1:0").unwrap();
    let old_address = old.local_addr().unwrap();
    let next_address = next.local_addr().unwrap();
    let (invite, key) = invitation(old_address.to_string());
    let config = tls::server_config(&invite.cert_der, &key).unwrap();
    let device = random_id().unwrap();
    let mut resolver = rowd_core::discovery::EndpointResolver::default();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let (socket, _) = old.accept().unwrap();
            let mut io = tls::accept(socket, config.clone()).unwrap();
            protocol::server_auth(&mut io, &invite.pair_id, &invite.secret).unwrap();
        });
        let (io, _) = resolver
            .connect(&invite, &device, 0, |_| Ok(vec![]))
            .unwrap();
        drop(io);
    });
    drop(old);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            next.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            for _ in 0..2 {
                let socket = loop {
                    match next.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(10))
                        }
                        Err(error) => panic!("next endpoint did not accept: {error}"),
                    }
                };
                socket.set_nonblocking(false).unwrap();
                let mut io = tls::accept(socket, config.clone()).unwrap();
                protocol::server_auth(&mut io, &invite.pair_id, &invite.secret).unwrap();
            }
        });
        let (io, address) = resolver
            .connect(&invite, &device, 0, |_| Ok(vec![next_address]))
            .unwrap();
        assert_eq!(address, next_address.to_string());
        drop(io);
        let (_io, cached_address) = resolver
            .connect(&invite, &device, 0, |_| {
                panic!("new endpoint should be cached")
            })
            .unwrap();
        assert_eq!(cached_address, address);
    });
}

#[test]
fn resolver_drops_cached_endpoint_when_invitation_identity_changes() {
    let first_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let second_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let first_address = first_listener.local_addr().unwrap();
    let second_address = second_listener.local_addr().unwrap();
    let (first_invite, first_key) = invitation(first_address.to_string());
    let (second_invite, second_key) = invitation(second_address.to_string());
    let first_config = tls::server_config(&first_invite.cert_der, &first_key).unwrap();
    let second_config = tls::server_config(&second_invite.cert_der, &second_key).unwrap();
    let device = random_id().unwrap();
    let first_connections = Arc::new(AtomicUsize::new(0));
    let first_connections_for_server = first_connections.clone();
    let first_pair_id = first_invite.pair_id.clone();
    let first_secret = first_invite.secret.clone();

    std::thread::scope(|scope| {
        scope.spawn(move || {
            let (socket, _) = first_listener.accept().unwrap();
            first_connections_for_server.fetch_add(1, Ordering::Relaxed);
            let mut io = tls::accept(socket, first_config).unwrap();
            protocol::server_auth(&mut io, &first_pair_id, &first_secret).unwrap();

            first_listener.set_nonblocking(true).unwrap();
            let until = Instant::now() + Duration::from_secs(2);
            while Instant::now() < until {
                match first_listener.accept() {
                    Ok((socket, _)) => {
                        first_connections_for_server.fetch_add(1, Ordering::Relaxed);
                        drop(socket);
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        scope.spawn(|| {
            let (socket, _) = second_listener.accept().unwrap();
            let mut io = tls::accept(socket, second_config).unwrap();
            assert_eq!(
                protocol::server_auth(&mut io, &second_invite.pair_id, &second_invite.secret)
                    .unwrap(),
                device
            );
        });

        let mut resolver = rowd_core::discovery::EndpointResolver::default();
        let (first_io, _) = resolver
            .connect(&first_invite, &device, 7, |_| Ok(vec![first_address]))
            .unwrap();
        drop(first_io);
        let (_second_io, address) = resolver
            .connect(&second_invite, &device, 7, |_| Ok(vec![second_address]))
            .unwrap();
        assert_eq!(address, second_address.to_string());
    });

    assert_eq!(first_connections.load(Ordering::Relaxed), 1);
}

#[test]
fn resolver_uses_invitation_when_discovery_is_unavailable() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let (invite, key) = invitation(listener.local_addr().unwrap().to_string());
    let config = tls::server_config(&invite.cert_der, &key).unwrap();
    let device = random_id().unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let (socket, _) = listener.accept().unwrap();
            let mut io = tls::accept(socket, config).unwrap();
            assert_eq!(
                protocol::server_auth(&mut io, &invite.pair_id, &invite.secret).unwrap(),
                device
            );
        });
        let mut resolver = rowd_core::discovery::EndpointResolver::default();
        let (_io, address) = resolver
            .connect(&invite, &device, 0, |_| {
                Err(anyhow::anyhow!("UDP unavailable"))
            })
            .unwrap();
        assert_eq!(address, invite.address);
    });
}
