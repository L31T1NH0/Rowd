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

    let trace_output = home.join("trace-output");
    assert!(!invoke(
        home,
        &["daemon", "trace", "start", "--output-dir", "relative"]
    )
    .status
    .success());
    let trace_start = invoke(
        home,
        &[
            "daemon",
            "trace",
            "start",
            "--output-dir",
            trace_output.to_str().unwrap(),
        ],
    );
    assert!(
        trace_start.status.success(),
        "{}",
        String::from_utf8_lossy(&trace_start.stderr)
    );
    assert!(invoke(home, &["daemon", "trace", "status"])
        .status
        .success());
    assert!(invoke(home, &["status"]).status.success());
    assert!(invoke(home, &["daemon", "trace", "flush"]).status.success());
    let latest = trace_output.join("Latest-trace");
    assert!(fs::metadata(latest.join("trace-0001.jsonl")).unwrap().len() > 0);
    assert!(invoke(home, &["daemon", "trace", "stop"]).status.success());
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(latest.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(metadata["complete"], true);
    assert_eq!(
        fs::read_dir(trace_output.join("traces")).unwrap().count(),
        1
    );
    let show = invoke(
        home,
        &[
            "trace",
            "show",
            latest.to_str().unwrap(),
            "--component",
            "Daemon-IPC",
        ],
    );
    assert!(show.status.success());
    assert!(String::from_utf8_lossy(&show.stdout).contains("IPC_REQUEST_PARSED"));
    assert!(invoke(
        home,
        &[
            "daemon",
            "trace",
            "start",
            "--output-dir",
            trace_output.to_str().unwrap()
        ]
    )
    .status
    .success());
    // Kill the daemon with an active trace. No stop/Drop/flush hook is run.
    let pid = rowd_daemon::request(home, "status").unwrap().data.unwrap()["pid"]
        .as_u64()
        .unwrap();
    let session: serde_json::Value =
        serde_json::from_slice(&fs::read(latest.join("metadata.json")).unwrap()).unwrap();
    let saved = fs::read(latest.join("trace-0001.jsonl")).unwrap();
    assert!(Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .unwrap()
        .success());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while rowd_daemon::active(home) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(fs::read(latest.join("trace-0001.jsonl"))
        .unwrap()
        .starts_with(&saved));
    let restart = invoke(home, &["daemon", "start"]);
    assert!(
        restart.status.success(),
        "{}",
        String::from_utf8_lossy(&restart.stderr)
    );
    assert!(invoke(
        home,
        &[
            "daemon",
            "trace",
            "start",
            "--output-dir",
            trace_output.to_str().unwrap()
        ]
    )
    .status
    .success());
    let recovered: serde_json::Value = serde_json::from_slice(
        &fs::read(
            trace_output
                .join("traces")
                .join(format!(
                    "trace-{}",
                    session["trace_session_id"].as_str().unwrap()
                ))
                .join("metadata.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(recovered["complete"], false);
    assert_eq!(recovered["recovered"], true);
    assert!(invoke(home, &["daemon", "trace", "stop"]).status.success());

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
