use rowd_core::{
    model::{Invitation, VERSION},
    protocol, random_id,
    storage::{LocalStore, Store},
    sync::{self, State},
    tls,
};
use std::{fs, net::TcpListener};

#[test]
fn cli_invitation_server_and_client_work_together() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    struct Process(std::process::Child);
    impl Drop for Process {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let pc = dir.path().join("pc");
    let android = dir.path().join("android");
    let invitation = dir.path().join("invite.json");
    let binary = env!("CARGO_BIN_EXE_rowd");
    let init = Command::new(binary)
        .args(["init", "--folder"])
        .arg(&pc)
        .args(["--address", "127.0.0.1:0", "--invite"])
        .arg(&invitation)
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    fs::write(pc.join("hello.txt"), "Rowd CLI").unwrap();
    let mut server = Process(
        Command::new(binary)
            .args(["serve", "--folder"])
            .arg(&pc)
            .args(["--listen", "127.0.0.1:0", "--once"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut ready = String::new();
    let mut server_output = BufReader::new(server.0.stdout.take().unwrap());
    server_output.read_line(&mut ready).unwrap();
    let address = ready.split_whitespace().nth(3).unwrap();
    let mut invite: Invitation = serde_json::from_slice(&fs::read(&invitation).unwrap()).unwrap();
    invite.address = address.into();
    fs::write(&invitation, serde_json::to_vec(&invite).unwrap()).unwrap();
    let sync = Command::new(binary)
        .args(["sync", "--folder"])
        .arg(&android)
        .arg("--invite")
        .arg(&invitation)
        .output()
        .unwrap();
    assert!(
        sync.status.success(),
        "{}",
        String::from_utf8_lossy(&sync.stderr)
    );
    assert!(server.0.wait().unwrap().success());
    assert_eq!(fs::read(android.join("hello.txt")).unwrap(), b"Rowd CLI");
}

fn invitation(address: String) -> (Invitation, String) {
    let cert = rcgen::generate_simple_self_signed(vec!["rowd.local".into()]).unwrap();
    (
        Invitation {
            version: VERSION,
            address,
            pair_id: random_id().unwrap(),
            folder_id: random_id().unwrap(),
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
    std::thread::scope(|scope| {
        let server = scope.spawn(|| {
            let mut store = LocalStore::open(pc.path()).unwrap();
            let path = store.private().join("state.json");
            let mut state = State::load(&path, &invite.pair_id, &invite.folder_id).unwrap();
            let (socket, _) = listener.accept().unwrap();
            let mut io = tls::accept(socket, config).unwrap();
            let peer =
                protocol::server_auth(&mut io, &invite.pair_id, &invite.folder_id, &invite.secret)
                    .unwrap();
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
            assert!(protocol::server_auth(
                &mut io,
                &invite.pair_id,
                &invite.folder_id,
                &invite.secret
            )
            .is_err());
        });
        let mut io = tls::connect(&wrong).unwrap();
        assert!(protocol::client_auth(
            &mut io,
            &wrong.pair_id,
            &wrong.folder_id,
            &wrong.secret,
            &random_id().unwrap()
        )
        .is_err());
        server.join().unwrap();
    });
}
