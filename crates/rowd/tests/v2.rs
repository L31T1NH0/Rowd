use rowd_core::{
    config::{ShareConfig, ShareRequest, SyncMode},
    managed::LocalDevice,
    model::Invitation,
    protocol::{self, Message},
    random_id,
    storage::atomic_json,
};
use std::{
    fs,
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
};
fn cli(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rowd"))
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .unwrap()
}
fn ok(home: &Path, args: &[&str]) -> String {
    let out = cli(home, args);
    assert!(
        out.status.success(),
        "{:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn server(home: &Path, invite: &Path, once: bool) -> Server {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rowd"));
    command
        .arg("--home")
        .arg(home)
        .args(["run", "--listen", "127.0.0.1:0"]);
    if once {
        command.arg("--once");
    }
    let mut child = Server(
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    BufReader::new(child.0.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    let address = line.split_whitespace().nth(3).expect("server readiness");
    let mut i = Invitation::decode(&fs::read_to_string(invite).unwrap()).unwrap();
    i.address = address.into();
    fs::write(invite, i.encode().unwrap()).unwrap();
    child
}
fn round(home: &Path, phone: &Path, invite: &Path) {
    let _server = server(home, invite, false);
    let identity = phone.join(".rowd/client-id.json");
    fs::create_dir_all(identity.parent().unwrap()).unwrap();
    let device_id: String = if identity.exists() {
        serde_json::from_slice(&fs::read(&identity).unwrap()).unwrap()
    } else {
        let id = random_id().unwrap();
        atomic_json(&identity, &id).unwrap();
        id
    };
    let invitation = Invitation::decode(&fs::read_to_string(invite).unwrap()).unwrap();
    let config = rowd_app::DeviceConfig::load(home).unwrap();
    let mut device = LocalDevice::open(phone).unwrap();
    let bindings_path = phone.join(".rowd/dev-bindings.json");
    let existing: std::collections::BTreeMap<String, String> = fs::read(&bindings_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    for share in &config.shares {
        if !existing.contains_key(&share.share_id) {
            device.bind(&share.share_id, &share.name).unwrap();
        }
    }
    rowd_core::managed::client_round(&invitation, &device_id, &mut device).unwrap();
}
#[test]
fn two_shares_restart_rename_ignore_offline_and_single_device() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let a = dir.path().join("pc-a");
    let b = dir.path().join("pc-b");
    let phone = dir.path().join("phone");
    let invite = dir.path().join("invite.json");
    fs::create_dir_all(a.join("node_modules")).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("same.txt"), "A").unwrap();
    fs::write(b.join("same.txt"), "B").unwrap();
    fs::write(a.join(".rowdignore"), "node_modules/\n*.tmp\n").unwrap();
    fs::write(a.join("node_modules/skip"), "skip").unwrap();
    ok(
        &home,
        &[
            "pair",
            "--address",
            "127.0.0.1:43821",
            "--invite",
            invite.to_str().unwrap(),
        ],
    );
    let id = ok(
        &home,
        &[
            "share",
            "add",
            "--name",
            "Projetos",
            "--folder",
            a.to_str().unwrap(),
        ],
    )
    .trim()
    .to_string();
    ok(
        &home,
        &[
            "share",
            "add",
            "--name",
            "Docs",
            "--folder",
            b.to_str().unwrap(),
        ],
    );
    assert!(!cli(
        &home,
        &[
            "share",
            "add",
            "--name",
            "Nested",
            "--folder",
            a.join("node_modules").to_str().unwrap()
        ]
    )
    .status
    .success());
    round(&home, &phone, &invite);
    assert_eq!(fs::read(phone.join("Projetos/same.txt")).unwrap(), b"A");
    assert_eq!(fs::read(phone.join("Docs/same.txt")).unwrap(), b"B");
    assert!(!phone.join("Projetos/node_modules").exists());
    fs::write(phone.join("Projetos/skip.tmp"), "skip").unwrap();
    ok(&home, &["share", "edit", &id, "--name", "Trabalho"]);
    fs::write(a.join("same.txt"), "offline").unwrap();
    ok(&home, &["scan"]);
    let cache: serde_json::Value =
        serde_json::from_slice(&fs::read(a.join(".rowd/cache.json")).unwrap()).unwrap();
    assert_eq!(
        cache["files"]["same.txt"]["entry"]["hash"],
        rowd_core::hash_reader(&b"offline"[..]).unwrap().0
    );
    fs::write(phone.join("Docs/phone.txt"), "android").unwrap();
    round(&home, &phone, &invite);
    assert_eq!(
        fs::read(phone.join("Projetos/same.txt")).unwrap(),
        b"offline"
    );
    assert_eq!(fs::read(b.join("phone.txt")).unwrap(), b"android");
    assert!(!a.join("skip.tmp").exists());
    round(&home, &phone, &invite);
    assert!(!phone.join("Trabalho").exists());
    let mut s = server(&home, &invite, true);
    let invitation = Invitation::decode(&fs::read_to_string(&invite).unwrap()).unwrap();
    assert!(rowd_core::managed::client_round(
        &invitation,
        &random_id().unwrap(),
        &mut LocalDevice::open(&dir.path().join("other-phone")).unwrap(),
    )
    .is_err());
    assert!(!s.0.wait().unwrap().success());
    assert!(!cli(&home, &["share", "remove", &id]).status.success());
    ok(&home, &["share", "remove", &id, "--confirm"]);
    round(&home, &phone, &invite);
    assert!(a.join("same.txt").exists() && phone.join("Projetos/same.txt").exists());
}
#[test]
fn migration_preserves_pair_base_and_recovery() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    let pc = d.path().join("legacy");
    let invite = d.path().join("invite.json");
    fs::create_dir_all(pc.join(".rowd/recovery")).unwrap();
    let cert = rcgen::generate_simple_self_signed(vec!["rowd.local".into()]).unwrap();
    let old_pair_id = random_id().unwrap();
    let old_folder_id = random_id().unwrap();
    let old_secret = random_id().unwrap();
    let old_cert = hex::encode(cert.cert.der());
    let old = serde_json::json!({
        "version": rowd_core::model::VERSION,
        "address": "127.0.0.1:43821",
        "pair_id": old_pair_id,
        "folder_id": old_folder_id,
        "cert_der": old_cert,
        "secret": old_secret,
    });
    atomic_json(
        &pc.join(".rowd/server.json"),
        &serde_json::json!({
            "version": rowd_core::model::VERSION, "root": pc.canonicalize().unwrap(),
            "pair_id": old_pair_id, "folder_id": old_folder_id,
            "cert": old_cert, "key": hex::encode(cert.key_pair.serialize_der()),
            "secret": old_secret,
        }),
    )
    .unwrap();
    atomic_json(
        &pc.join(".rowd/sync-state.json"),
        &serde_json::json!({
            "version": rowd_core::model::VERSION, "pair_id": old_pair_id,
            "folder_id": old_folder_id, "peer_root": null, "files": {}
        }),
    )
    .unwrap();
    atomic_json(&invite, &old).unwrap();
    fs::write(pc.join(".rowd/recovery/retained"), "backup").unwrap();
    ok(
        &home,
        &[
            "migrate",
            "--folder",
            pc.to_str().unwrap(),
            "--address",
            "127.0.0.1:43821",
        ],
    );
    let cfg = rowd_app::DeviceConfig::load(&home).unwrap();
    assert_eq!(cfg.pair_id, old_pair_id);
    assert_eq!(cfg.secret, old_secret);
    assert_eq!(cfg.shares[0].share_id, old_folder_id);
    assert_eq!(
        fs::read(pc.join(".rowd/recovery/retained")).unwrap(),
        b"backup"
    );
    let extra = d.path().join("extra");
    fs::create_dir_all(&extra).unwrap();
    fs::write(pc.join("legacy.txt"), "legacy").unwrap();
    fs::write(extra.join("new.txt"), "new").unwrap();
    let extra_id = ok(
        &home,
        &[
            "share",
            "add",
            "--name",
            "Extra",
            "--folder",
            extra.to_str().unwrap(),
        ],
    )
    .trim()
    .to_string();
    let phone = d.path().join("phone");
    LocalDevice::open(&phone)
        .unwrap()
        .bind(&old_folder_id, "")
        .unwrap();
    round(&home, &phone, &invite);
    assert_eq!(fs::read(phone.join("legacy.txt")).unwrap(), b"legacy");
    assert_eq!(fs::read(phone.join("Extra/new.txt")).unwrap(), b"new");
    round(&home, &phone, &invite);
    assert!(!pc.join("Extra").exists());
    ok(&home, &["share", "remove", &extra_id, "--confirm"]);
    round(&home, &phone, &invite);
    assert!(!pc.join("Extra").exists());
    assert!(phone.join("Extra/new.txt").exists());
}
#[test]
fn watcher_defers_offline_hashing_until_connection() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    let pc = d.path().join("pc");
    let invite = d.path().join("invite.json");
    fs::create_dir_all(&pc).unwrap();
    ok(
        &home,
        &[
            "pair",
            "--address",
            "127.0.0.1:43821",
            "--invite",
            invite.to_str().unwrap(),
        ],
    );
    let id = ok(
        &home,
        &[
            "share",
            "add",
            "--name",
            "Files",
            "--folder",
            pc.to_str().unwrap(),
        ],
    )
    .trim()
    .to_string();
    let _s = server(&home, &invite, false);
    std::thread::sleep(std::time::Duration::from_millis(400));
    for i in 0..20 {
        fs::write(pc.join("burst"), i.to_string()).unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(800));
    if let Ok(bytes) = fs::read(pc.join(".rowd/cache.json")) {
        let cache: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            cache["files"].get("burst").is_none(),
            "offline watcher must not hash files"
        );
    }
    let hint: serde_json::Value =
        serde_json::from_slice(&fs::read(pc.join(".rowd/cache-dirty.json")).unwrap()).unwrap();
    assert!(
        hint["full"] == true || hint["paths"].as_array().unwrap().contains(&"burst".into()),
        "offline watcher must retain a durable invalidation hint"
    );
    let phone = d.path().join("phone");
    let mut device = LocalDevice::open(&phone).unwrap();
    device.bind(&id, "").unwrap();
    let invitation = Invitation::decode(&fs::read_to_string(&invite).unwrap()).unwrap();
    let device_id = random_id().unwrap();
    rowd_core::managed::client_round(&invitation, &device_id, &mut device).unwrap();
    assert_eq!(fs::read(phone.join("burst")).unwrap(), b"19");
    rowd_core::managed::client_round(&invitation, &device_id, &mut device).unwrap();
    let runtime: serde_json::Value =
        serde_json::from_slice(&fs::read(home.join(".rowd/device-runtime.json")).unwrap()).unwrap();
    let metrics = &runtime["last_round"]["per_share"][&id];
    assert_eq!(metrics["full_scans"], 2);
    assert_eq!(metrics["files_enumerated"], 2);
    assert_eq!(metrics["manifest_entries"], 1);
}

#[test]
fn android_share_request_waits_for_pc_folder_selection() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    let pc = d.path().join("pc-share");
    let phone = d.path().join("phone");
    let invite = d.path().join("invite.json");
    fs::create_dir_all(&pc).unwrap();
    fs::write(pc.join("hello.txt"), "from pc").unwrap();
    ok(
        &home,
        &[
            "pair",
            "--address",
            "127.0.0.1:43821",
            "--invite",
            invite.to_str().unwrap(),
        ],
    );
    let request = ShareRequest {
        request_id: random_id().unwrap(),
        name: "Fotos".into(),
        mode: SyncMode::Bidirectional,
    };
    fs::create_dir_all(phone.join(".rowd")).unwrap();
    atomic_json(
        &phone.join(".rowd/share-requests.json"),
        &vec![request.clone()],
    )
    .unwrap();

    round(&home, &phone, &invite);
    let mut cfg = rowd_app::DeviceConfig::load(&home).unwrap();
    assert_eq!(cfg.share_requests, vec![request.clone()]);
    cfg.put_share(
        &home,
        ShareConfig {
            share_id: random_id().unwrap(),
            name: request.name.clone(),
            root: pc.clone(),
            binding_revision: 0,
            mode: request.mode,
            enabled: true,
            legacy_ignore: String::new(),
            request_id: Some(request.request_id.clone()),
            remap_policy: None,
        },
    )
    .unwrap();
    cfg.share_requests.clear();
    cfg.save(&home).unwrap();

    round(&home, &phone, &invite);
    assert_eq!(fs::read(phone.join("Fotos/hello.txt")).unwrap(), b"from pc");
    let remaining: Vec<ShareRequest> =
        serde_json::from_reader(fs::File::open(phone.join(".rowd/share-requests.json")).unwrap())
            .unwrap();
    assert!(remaining.is_empty());

    let reused = ShareRequest {
        name: "Outro nome".into(),
        ..request
    };
    atomic_json(&phone.join(".rowd/share-requests.json"), &vec![reused]).unwrap();
    let mut server = server(&home, &invite, true);
    let invitation = Invitation::decode(&fs::read_to_string(&invite).unwrap()).unwrap();
    let device_id: String =
        serde_json::from_slice(&fs::read(phone.join(".rowd/client-id.json")).unwrap()).unwrap();
    let mut device = LocalDevice::open(&phone).unwrap();
    assert!(rowd_core::managed::client_round(&invitation, &device_id, &mut device).is_err());
    let _ = server.0.wait();
    assert_eq!(
        rowd_app::DeviceConfig::load(&home).unwrap().shares[0].name,
        "Fotos"
    );
}

#[test]
fn interrupted_unlink_keeps_credentials_until_android_acknowledges() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let phone = directory.path().join("phone");
    let invite = directory.path().join("invite.txt");
    ok(
        &home,
        &[
            "pair",
            "--address",
            "127.0.0.1:43821",
            "--invite",
            invite.to_str().unwrap(),
        ],
    );
    round(&home, &phone, &invite);
    let app = rowd_app::App::new(&home);
    let before = rowd_app::DeviceConfig::load(&home).unwrap();
    app.unlink_device().unwrap();
    assert!(rowd_app::DeviceConfig::load(&home).unwrap().pending_unlink);

    let mut server = server(&home, &invite, true);
    let invitation = Invitation::decode(&fs::read_to_string(&invite).unwrap()).unwrap();
    let device_id: String =
        serde_json::from_slice(&fs::read(phone.join(".rowd/client-id.json")).unwrap()).unwrap();
    let mut io = rowd_core::tls::connect(&invitation).unwrap();
    protocol::client_auth(&mut io, &invitation.pair_id, &invitation.secret, &device_id).unwrap();
    protocol::send(&mut io, &Message::StartRound).unwrap();
    assert!(matches!(
        protocol::receive(&mut io).unwrap(),
        Message::Shares { .. }
    ));
    protocol::send(
        &mut io,
        &Message::Capabilities {
            device_id,
            share_requests: Vec::new(),
            cancel_intents: Vec::new(),
            available_shares: Vec::new(),
            requested_share_ids: Vec::new(),
            audit: false,
            unlink_requested: false,
        },
    )
    .unwrap();
    assert!(matches!(
        protocol::receive(&mut io).unwrap(),
        Message::DeviceUnlinked
    ));
    drop(io); // Lost ACK: the PC must still accept the old identity.
    let _ = server.0.wait();
    let interrupted = rowd_app::DeviceConfig::load(&home).unwrap();
    assert_eq!(interrupted.pair_id, before.pair_id);
    assert!(interrupted.pending_unlink);

    round(&home, &phone, &invite);
    let completed = rowd_app::DeviceConfig::load(&home).unwrap();
    assert_ne!(completed.pair_id, before.pair_id);
    assert!(!completed.pending_unlink);
    assert!(completed.peer_device.is_none());
}

#[test]
#[cfg(unix)]
fn one_share_failure_does_not_starve_another_share() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    let phone = directory.path().join("phone");
    let invite = directory.path().join("invite.txt");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    ok(
        &home,
        &[
            "pair",
            "--address",
            "127.0.0.1:43821",
            "--invite",
            invite.to_str().unwrap(),
        ],
    );
    ok(
        &home,
        &[
            "share",
            "add",
            "--name",
            "First",
            "--folder",
            first.to_str().unwrap(),
        ],
    );
    ok(
        &home,
        &[
            "share",
            "add",
            "--name",
            "Second",
            "--folder",
            second.to_str().unwrap(),
        ],
    );
    round(&home, &phone, &invite);

    let archived = first.join("rowd-state-preserved");
    fs::rename(first.join(".rowd"), &archived).unwrap();
    symlink(&archived, first.join(".rowd")).unwrap();
    fs::write(second.join("new.txt"), b"still synchronized").unwrap();
    let _server = server(&home, &invite, false);
    let invitation = Invitation::decode(&fs::read_to_string(&invite).unwrap()).unwrap();
    let device_id: String =
        serde_json::from_slice(&fs::read(phone.join(".rowd/client-id.json")).unwrap()).unwrap();
    let mut device = LocalDevice::open(&phone).unwrap();
    assert!(rowd_core::managed::client_round(&invitation, &device_id, &mut device).is_err());
    assert_eq!(
        fs::read(phone.join("Second/new.txt")).unwrap(),
        b"still synchronized"
    );
    drop(device);

    // A disappeared root must not be silently recreated or block a healthy Share
    // while the server advertises definitions for the next connection.
    fs::remove_file(first.join(".rowd")).unwrap();
    fs::rename(&first, directory.path().join("first-offline")).unwrap();
    fs::write(second.join("another.txt"), b"offline root isolated").unwrap();
    let mut device = LocalDevice::open(&phone).unwrap();
    let focused = rowd_core::managed::client_round(&invitation, &device_id, &mut device).unwrap();
    assert_eq!(focused.shares_processed, 1);
    assert!(!first.exists());
    assert_eq!(
        fs::read(phone.join("Second/another.txt")).unwrap(),
        b"offline root isolated"
    );
}

#[test]
fn focused_round_scans_only_the_requested_share_then_full_audit_catches_the_rest() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    let phone = directory.path().join("phone");
    let invite = directory.path().join("invite.txt");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    ok(
        &home,
        &[
            "pair",
            "--address",
            "127.0.0.1:43821",
            "--invite",
            invite.to_str().unwrap(),
        ],
    );
    ok(
        &home,
        &[
            "share",
            "add",
            "--name",
            "First",
            "--folder",
            first.to_str().unwrap(),
        ],
    );
    let second_id = ok(
        &home,
        &[
            "share",
            "add",
            "--name",
            "Second",
            "--folder",
            second.to_str().unwrap(),
        ],
    );
    let second_id = second_id.trim().to_owned();
    round(&home, &phone, &invite);
    fs::write(first.join("changed.txt"), b"first later").unwrap();
    fs::write(second.join("changed.txt"), b"second now").unwrap();

    let _server = server(&home, &invite, false);
    let invitation = Invitation::decode(&fs::read_to_string(&invite).unwrap()).unwrap();
    let device_id: String =
        serde_json::from_slice(&fs::read(phone.join(".rowd/client-id.json")).unwrap()).unwrap();
    let mut device = LocalDevice::open(&phone).unwrap();
    device.focus(vec![second_id]);
    let focused = rowd_core::managed::client_round(&invitation, &device_id, &mut device).unwrap();
    assert_eq!(focused.shares_processed, 1);
    assert_eq!(
        fs::read(phone.join("Second/changed.txt")).unwrap(),
        b"second now"
    );
    assert!(!phone.join("First/changed.txt").exists());
    drop(device);

    let mut device = LocalDevice::open(&phone).unwrap();
    let full = rowd_core::managed::client_round(&invitation, &device_id, &mut device).unwrap();
    assert_eq!(full.shares_processed, 2);
    assert_eq!(
        fs::read(phone.join("First/changed.txt")).unwrap(),
        b"first later"
    );
}

#[test]
fn persistent_connection_wakes_a_focused_share_and_reuses_auth_for_two_shares() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    let phone = directory.path().join("phone");
    let invite = directory.path().join("invite.txt");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    ok(
        &home,
        &[
            "pair",
            "--address",
            "127.0.0.1:43821",
            "--invite",
            invite.to_str().unwrap(),
        ],
    );
    let first_id = ok(
        &home,
        &[
            "share",
            "add",
            "--name",
            "First",
            "--folder",
            first.to_str().unwrap(),
        ],
    )
    .trim()
    .to_owned();
    let second_id = ok(
        &home,
        &[
            "share",
            "add",
            "--name",
            "Second",
            "--folder",
            second.to_str().unwrap(),
        ],
    )
    .trim()
    .to_owned();
    round(&home, &phone, &invite);
    let _server = server(&home, &invite, false);
    let invitation = Invitation::decode(&fs::read_to_string(&invite).unwrap()).unwrap();
    let device_id: String =
        serde_json::from_slice(&fs::read(phone.join(".rowd/client-id.json")).unwrap()).unwrap();
    let mut device = LocalDevice::open(&phone).unwrap();
    let mut io = rowd_core::tls::connect(&invitation).unwrap();
    io.sock
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    protocol::client_auth(&mut io, &invitation.pair_id, &invitation.secret, &device_id).unwrap();
    device.focus(vec![first_id.clone()]);
    assert_eq!(
        rowd_core::managed::client_round_on(&mut io, &device_id, &mut device)
            .unwrap()
            .shares_processed,
        1
    );
    fs::write(first.join("new.txt"), b"wake").unwrap();
    for index in 0..3 {
        fs::write(first.join(format!("burst-{index}.txt")), b"burst").unwrap();
    }
    assert!(
        matches!(protocol::receive(&mut io).unwrap(), Message::WakeShare { share_id } if share_id == first_id)
    );
    assert_eq!(
        rowd_core::managed::client_round_on(&mut io, &device_id, &mut device)
            .unwrap()
            .shares_processed,
        1
    );
    assert_eq!(fs::read(phone.join("First/new.txt")).unwrap(), b"wake");
    for index in 0..3 {
        assert_eq!(
            fs::read(phone.join(format!("First/burst-{index}.txt"))).unwrap(),
            b"burst"
        );
    }
    assert!(!phone.join("Second/new.txt").exists());
    fs::write(first.join("new.txt"), b"wake again").unwrap();
    fs::write(second.join("new.txt"), b"second").unwrap();
    device.focus(vec![first_id, second_id.clone()]);
    assert_eq!(
        rowd_core::managed::client_round_on(&mut io, &device_id, &mut device)
            .unwrap()
            .shares_processed,
        2
    );
    assert_eq!(fs::read(phone.join("Second/new.txt")).unwrap(), b"second");
    assert_eq!(
        fs::read(phone.join("First/new.txt")).unwrap(),
        b"wake again"
    );
    let mut replacement = rowd_core::tls::connect(&invitation).unwrap();
    protocol::client_auth(
        &mut replacement,
        &invitation.pair_id,
        &invitation.secret,
        &device_id,
    )
    .unwrap();
    device.focus(vec![second_id]);
    assert_eq!(
        rowd_core::managed::client_round_on(&mut replacement, &device_id, &mut device)
            .unwrap()
            .shares_processed,
        1
    );
}
