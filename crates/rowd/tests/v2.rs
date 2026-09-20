use rowd_core::{
    config::{ShareConfig, ShareRequest, SyncMode},
    journal::ShareState,
    model::Invitation,
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
    let mut i: Invitation = serde_json::from_slice(&fs::read(invite).unwrap()).unwrap();
    i.address = address.into();
    fs::write(invite, serde_json::to_vec(&i).unwrap()).unwrap();
    child
}
fn round(home: &Path, phone: &Path, invite: &Path) {
    let mut s = server(home, invite, true);
    ok(
        home,
        &[
            "device-sync",
            "--folder",
            phone.to_str().unwrap(),
            "--invite",
            invite.to_str().unwrap(),
        ],
    );
    assert!(s.0.wait().unwrap().success());
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
    let journal = home.join(".rowd/shares").join(&id).join("journal.json");
    assert_eq!(ShareState::load(&journal, &id).unwrap().pending.len(), 1);
    fs::write(phone.join("Docs/phone.txt"), "android").unwrap();
    round(&home, &phone, &invite);
    assert_eq!(
        fs::read(phone.join("Projetos/same.txt")).unwrap(),
        b"offline"
    );
    assert_eq!(fs::read(b.join("phone.txt")).unwrap(), b"android");
    assert!(!a.join("skip.tmp").exists());
    assert!(ShareState::load(&journal, &id).unwrap().pending.is_empty());
    fs::write(&journal, "interrupted invalid JSON").unwrap();
    ok(&home, &["scan"]);
    assert!(ShareState::load(&journal, &id).is_ok());
    round(&home, &phone, &invite);
    assert!(!phone.join("Trabalho").exists());
    let mut s = server(&home, &invite, true);
    assert!(!cli(
        &home,
        &[
            "device-sync",
            "--folder",
            dir.path().join("other-phone").to_str().unwrap(),
            "--invite",
            invite.to_str().unwrap()
        ]
    )
    .status
    .success());
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
    ok(
        &home,
        &[
            "init",
            "--folder",
            pc.to_str().unwrap(),
            "--address",
            "127.0.0.1:43821",
            "--invite",
            invite.to_str().unwrap(),
        ],
    );
    let old: Invitation = serde_json::from_slice(&fs::read(&invite).unwrap()).unwrap();
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
    let cfg = rowd_core::config::DeviceConfig::load(&home).unwrap();
    assert_eq!(cfg.pair_id, old.pair_id);
    assert_eq!(cfg.secret, old.secret);
    assert_eq!(cfg.shares[0].share_id, old.folder_id);
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
fn watcher_records_bursts_while_phone_is_offline() {
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
    let journal = home.join(".rowd/shares").join(&id).join("journal.json");
    let start = std::time::Instant::now();
    loop {
        if let Ok(state) = ShareState::load(&journal, &id) {
            if state.pending.contains_key("burst") {
                assert_eq!(state.pending.len(), 1);
                assert_eq!(
                    state.pending["burst"].entry.as_ref().unwrap().hash,
                    rowd_core::hash_reader(&b"19"[..]).unwrap().0
                );
                break;
            }
        }
        assert!(
            start.elapsed().as_secs() < 5,
            "watcher did not persist pending change"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
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
    let mut cfg = rowd_core::config::DeviceConfig::load(&home).unwrap();
    assert_eq!(cfg.share_requests, vec![request.clone()]);
    cfg.put_share(
        &home,
        ShareConfig {
            share_id: random_id().unwrap(),
            name: request.name.clone(),
            root: pc.clone(),
            android_path: request.name.clone(),
            mode: request.mode,
            ignore: String::new(),
            request_id: Some(request.request_id.clone()),
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
}
