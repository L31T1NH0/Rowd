use rowd_app::{App, DeviceConfig};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
};

fn invoke(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rowd"))
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn background_cli_survives_parent_and_stops() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path();
    App::new(home).pair("127.0.0.1:0").unwrap();
    let mut config = DeviceConfig::load(home).unwrap();
    config.listen = "127.0.0.1:0".into();
    config.save(home).unwrap();
    let start = invoke(home, &["daemon", "start"]);
    assert!(
        start.status.success(),
        "{}",
        String::from_utf8_lossy(&start.stderr)
    );
    struct Stop<'a>(&'a Path);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            let _ = invoke(self.0, &["daemon", "stop"]);
        }
    }
    let _stop = Stop(home);
    let status = invoke(home, &["status"]);
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("manual"));
    assert!(!invoke(home, &["daemon", "start"]).status.success());
    let tui = invoke(home, &[]);
    assert!(!tui.status.success());
    assert!(String::from_utf8_lossy(&tui.stderr).contains("Daemon ativo"));
    assert!(invoke(home, &["logs"]).status.success());
    assert!(invoke(home, &["events"]).status.success());

    let fake_home = home.join("systemd-test-home");
    let bin = home.join("test-bin");
    fs::create_dir_all(&bin).unwrap();
    let mock = bin.join("systemctl");
    fs::write(
        &mock,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$ROWD_TEST_SYSTEMCTL_LOG\"\n",
    )
    .unwrap();
    fs::set_permissions(&mock, fs::Permissions::from_mode(0o755)).unwrap();
    let calls = home.join("systemctl-calls");
    let call_autostart = |action| {
        Command::new(env!("CARGO_BIN_EXE_rowd"))
            .arg("--home")
            .arg(home)
            .args(["autostart", action])
            .env("HOME", &fake_home)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("ROWD_TEST_SYSTEMCTL_LOG", &calls)
            .output()
            .unwrap()
    };
    assert!(call_autostart("enable").status.success());
    let unit = fs::read_to_string(fake_home.join(".config/systemd/user/rowd.service")).unwrap();
    assert!(unit.contains("daemon run"));
    assert!(!unit.contains("daemon start"));
    assert!(call_autostart("disable").status.success());
    let calls = fs::read_to_string(calls).unwrap();
    assert!(calls.contains("--user daemon-reload"));
    assert!(calls.contains("--user enable rowd.service"));
    assert!(calls.contains("--user disable rowd.service"));
    assert!(!calls.contains("--now"));
    assert!(!calls.contains("--user start"));
    assert!(!calls.contains("--user stop"));
    assert!(String::from_utf8_lossy(&invoke(home, &["status"]).stdout).contains("manual"));

    let stop = invoke(home, &["daemon", "stop"]);
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(String::from_utf8_lossy(&invoke(home, &["status"]).stdout).contains("parado"));
}
