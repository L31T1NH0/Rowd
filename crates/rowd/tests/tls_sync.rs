use rowd_core::{
    model::{Invitation, INVITATION_VERSION},
    protocol, random_id,
    storage::{LocalStore, Store},
    sync::{self, State},
    tls,
};
use std::{fs, net::TcpListener};

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
