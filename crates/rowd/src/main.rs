mod tui;

use anyhow::{ensure, Context, Result};
use clap::{Parser, Subcommand};
use rowd_app::App;
use rowd_core::config::{RemapPolicy, SyncMode};
use rowd_core::trace;
use rowd_daemon as daemon;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "rowd",
    version,
    about = "Seus arquivos, entre seus dispositivos, sem nuvem intermediária."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(long, global = true)]
    home: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    Trace {
        #[command(subcommand)]
        command: TraceCommand,
    },
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    Autostart {
        #[command(subcommand)]
        command: AutostartCommand,
    },
    Status,
    Logs {
        #[arg(short, long)]
        follow: bool,
    },
    Events {
        #[arg(short, long)]
        follow: bool,
    },
    Pair {
        #[arg(long)]
        address: Option<String>,
        #[arg(long)]
        invite: Option<PathBuf>,
    },
    Migrate {
        #[arg(long)]
        folder: PathBuf,
        #[arg(long)]
        address: String,
    },
    Share {
        #[command(subcommand)]
        command: ShareCommand,
    },
    Request {
        #[command(subcommand)]
        command: RequestCommand,
    },
    Device {
        #[command(subcommand)]
        command: DeviceCommand,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Run {
        #[arg(long)]
        listen: Option<String>,
        #[arg(long)]
        once: bool,
        #[arg(long)]
        trace: bool,
    },
    Scan,
    Shares,
    Recovery {
        #[arg(long)]
        share: Option<String>,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        action: Option<String>,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    Diagnostic {
        #[arg(long)]
        output: PathBuf,
    },
    Reset {
        #[arg(long, value_parser = ["share", "unlink", "initial", "all"])]
        level: String,
        #[arg(long)]
        share: Option<String>,
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(Subcommand)]
enum TraceCommand {
    Show {
        trace: PathBuf,
        #[arg(long)]
        component: Option<String>,
        #[arg(long)]
        share: Option<String>,
        #[arg(long)]
        errors: bool,
        #[arg(long)]
        connection: Option<String>,
        #[arg(long)]
        round: Option<String>,
        #[arg(long)]
        file: Option<String>,
        #[arg(long)]
        request: Option<String>,
    },
}

#[derive(Subcommand)]
enum DaemonCommand {
    Run,
    Start,
    Stop,
    Restart,
    Trace {
        #[command(subcommand)]
        command: DaemonTraceCommand,
    },
}

#[derive(Subcommand)]
enum DaemonTraceCommand {
    Start {
        #[arg(long)]
        output_dir: Option<PathBuf>,
    },
    Status,
    Stop,
    Flush,
}

#[derive(Subcommand)]
enum AutostartCommand {
    Enable,
    Disable,
    Status,
}

#[derive(Subcommand)]
enum ShareCommand {
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        folder: PathBuf,
        #[arg(long, default_value = "bidirectional")]
        mode: String,
    },
    Edit {
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        folder: Option<PathBuf>,
        #[arg(long)]
        mode: Option<String>,
    },
    Remove {
        id: String,
        #[arg(long)]
        confirm: bool,
    },
    Pause {
        id: String,
    },
    Resume {
        id: String,
    },
    Reindex {
        id: String,
    },
    Remap {
        id: String,
        #[arg(long)]
        policy: String,
    },
    Ignore {
        id: String,
        #[arg(long)]
        file: PathBuf,
    },
    Sync {
        id: String,
    },
}

#[derive(Subcommand)]
enum RequestCommand {
    List,
    Accept {
        id: String,
        #[arg(long)]
        folder: PathBuf,
    },
    Reject {
        id: String,
    },
}

#[derive(Subcommand)]
enum DeviceCommand {
    Test,
    Pause,
    Resume,
    Unlink {
        #[arg(long)]
        confirm: bool,
    },
    Revoke {
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    ExportProfile {
        output: PathBuf,
    },
    ImportProfile {
        input: PathBuf,
    },
    ExportBackup {
        output: PathBuf,
        #[arg(long, env = "ROWD_BACKUP_PASSPHRASE")]
        passphrase: String,
    },
    ImportBackup {
        input: PathBuf,
        #[arg(long, env = "ROWD_BACKUP_PASSPHRASE")]
        passphrase: String,
    },
}

pub(crate) fn parse_mode(mode: &str) -> Result<SyncMode> {
    match mode {
        "bidirectional" => Ok(SyncMode::Bidirectional),
        "to_android" => Ok(SyncMode::ToAndroid),
        "to_pc" => Ok(SyncMode::ToPc),
        _ => anyhow::bail!("mode must be bidirectional, to_android or to_pc"),
    }
}

fn parse_policy(policy: &str) -> Result<RemapPolicy> {
    match policy {
        "pc" => Ok(RemapPolicy::Pc),
        "android" => Ok(RemapPolicy::Android),
        "compare" => Ok(RemapPolicy::Compare),
        _ => anyhow::bail!("policy must be pc, android or compare"),
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let home = cli.home.unwrap_or_else(rowd_app::default_home);
    let app = App::new(&home);
    let Some(command) = cli.command else {
        return tui::run(&home);
    };
    match command {
        Command::Trace {
            command:
                TraceCommand::Show {
                    trace,
                    component,
                    share,
                    errors,
                    connection,
                    round,
                    file,
                    request,
                },
        } => {
            rowd_core::trace_render::show(
                &trace,
                &rowd_core::trace_render::Filter {
                    component,
                    share,
                    errors,
                    connection,
                    round,
                    file,
                    request,
                },
                &mut std::io::stdout().lock(),
            )?;
        }
        Command::Daemon { command } => match command {
            DaemonCommand::Run => daemon::run(&home)?,
            DaemonCommand::Start => daemon::start(&home)?,
            DaemonCommand::Stop => daemon::stop(&home)?,
            DaemonCommand::Restart => daemon::restart(&home)?,
            DaemonCommand::Trace { command } => {
                let (command, args) = match command {
                    DaemonTraceCommand::Start { output_dir } => {
                        if let Some(path) = &output_dir {
                            ensure!(path.is_absolute(), "--output-dir must be absolute");
                        }
                        (
                            "trace_start",
                            output_dir
                                .map(|p| serde_json::json!({"output_dir":p}))
                                .unwrap_or(serde_json::json!({})),
                        )
                    }
                    DaemonTraceCommand::Status => ("trace_status", serde_json::json!({})),
                    DaemonTraceCommand::Stop => ("trace_stop", serde_json::json!({})),
                    DaemonTraceCommand::Flush => ("trace_flush", serde_json::json!({})),
                };
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &daemon::request_args(&home, command, args)?.data
                    )?
                );
            }
        },
        Command::Autostart { command } => match command {
            AutostartCommand::Enable => daemon::autostart_enable(&home)?,
            AutostartCommand::Disable => daemon::autostart_disable()?,
            AutostartCommand::Status => println!("{}", daemon::autostart_status()?),
        },
        Command::Status => print_daemon_status(&home)?,
        Command::Logs { follow } => print_stream(&home, "logs", follow)?,
        Command::Events { follow } => print_stream(&home, "events", follow)?,
        Command::Pair { address, invite } => {
            app.pair(address.as_deref().unwrap_or(""))?;
            if let Some(path) = invite {
                app.export_invitation(&path)?;
            }
            let info = app.pairing_info()?;
            println!("{}", info.qr);
            println!("QR privado: {}", info.qr_image.display());
            println!("Certificado SHA-256: {}", info.device.fingerprint);
        }
        Command::Migrate { folder, address } => app.migrate(&folder, &address)?,
        Command::Share { command } => match command {
            ShareCommand::Add { name, folder, mode } => {
                println!("{}", app.add_share(name, folder, parse_mode(&mode)?)?)
            }
            ShareCommand::Edit {
                id,
                name,
                folder,
                mode,
            } => app.patch_share(
                &id,
                name,
                folder,
                mode.as_deref().map(parse_mode).transpose()?,
            )?,
            ShareCommand::Remove { id, confirm } => {
                ensure!(
                    confirm,
                    "use --confirm; files and recovery will be retained"
                );
                app.remove_share(&id)?;
            }
            ShareCommand::Pause { id } => app.set_share_enabled(&id, false)?,
            ShareCommand::Resume { id } => app.set_share_enabled(&id, true)?,
            ShareCommand::Reindex { id } => app.reindex_share(&id)?,
            ShareCommand::Remap { id, policy } => app.remap_share(&id, parse_policy(&policy)?)?,
            ShareCommand::Ignore { id, file } => {
                app.set_ignore_text(&id, &std::fs::read_to_string(file)?)?
            }
            ShareCommand::Sync { id } => app.request_share_sync(&id)?,
        },
        Command::Request { command } => match command {
            RequestCommand::List => {
                println!("{}", serde_json::to_string_pretty(&app.share_requests()?)?)
            }
            RequestCommand::Accept { id, folder } => app.accept_share_request(&id, &folder)?,
            RequestCommand::Reject { id } => app.reject_share_request(&id)?,
        },
        Command::Device { command } => match command {
            DeviceCommand::Test => {
                println!("{}", serde_json::to_string_pretty(&app.connection_test()?)?)
            }
            DeviceCommand::Pause => app.set_sync_paused(true)?,
            DeviceCommand::Resume => app.set_sync_paused(false)?,
            DeviceCommand::Unlink { confirm } => {
                ensure!(confirm, "use --confirm to revoke the Android pairing");
                app.unlink_device()?;
            }
            DeviceCommand::Revoke { confirm } => {
                ensure!(confirm, "use --confirm to revoke the Android pairing");
                app.revoke_device()?;
            }
        },
        Command::Config { command } => match command {
            ConfigCommand::ExportProfile { output } => app.export_profile(&output)?,
            ConfigCommand::ImportProfile { input } => app.import_profile(&input)?,
            ConfigCommand::ExportBackup { output, passphrase } => {
                app.export_backup(&output, &passphrase)?
            }
            ConfigCommand::ImportBackup { input, passphrase } => {
                app.import_backup(&input, &passphrase)?
            }
        },
        Command::Run {
            listen,
            once,
            trace: tracing,
        } => {
            if tracing {
                std::fs::create_dir_all(home.join(".rowd"))?;
                trace::enable(&home.join(".rowd/performance-trace-pc.jsonl"), "pc")?;
            }
            let result = app.serve(
                listen.as_deref(),
                once,
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                |event| println!("{event}"),
            );
            if tracing {
                trace::stop("process_exit")?;
            }
            result?;
        }
        Command::Scan => app.scan(true)?,
        Command::Shares => println!("{}", serde_json::to_string_pretty(&app.status()?)?),
        Command::Recovery {
            share,
            id,
            action,
            output,
        } => {
            if let Some(id) = id {
                let share = share.context("--share is required with --id")?;
                match action.as_deref().context("--action is required")? {
                    "keep" => app.keep_recovery(&share, &id)?,
                    "restore" => app.restore_recovery(&share, &id)?,
                    "export" => app.export_recovery(
                        &share,
                        &id,
                        output.as_deref().context("--output is required")?,
                    )?,
                    "cleanup" => app.cleanup_recovery(&share, &id)?,
                    _ => anyhow::bail!("action must be keep, restore, export or cleanup"),
                }
            } else {
                println!("{}", serde_json::to_string_pretty(&app.recovery()?)?);
            }
        }
        Command::Diagnostic { output } => app.export_diagnostic(&output)?,
        Command::Reset {
            level,
            share,
            confirm,
        } => {
            ensure!(confirm, "use --confirm for reset operations");
            match level.as_str() {
                "share" => app.reindex_share(&share.context("--share is required")?)?,
                "unlink" => app.unlink_device()?,
                "initial" => app.reset_device_configuration()?,
                "all" => app.archive_all_application_data()?,
                _ => anyhow::bail!("level must be share, unlink, initial or all"),
            }
        }
    }
    Ok(())
}

fn print_stream(home: &std::path::Path, name: &str, follow: bool) -> Result<()> {
    if follow {
        daemon::follow(home, name)?;
    } else {
        for line in daemon::history(home, name)? {
            println!("{line}");
        }
    }
    Ok(())
}

fn print_daemon_status(home: &std::path::Path) -> Result<()> {
    if !daemon::ipc_present(home) {
        println!("Daemon: parado");
        return Ok(());
    }
    let reply = daemon::request(home, "status")?;
    let data = reply.data.context("daemon returned no status")?;
    let uptime = data["uptime_seconds"].as_u64().unwrap_or(0);
    println!("Daemon        ativo\nModo          {}\nPID           {}\nUptime        {:02}h{:02}m\nIPC           v{}\nRowd          {}",
        data["launch_mode"].as_str().unwrap_or("?"), data["pid"], uptime / 3600, uptime % 3600 / 60, data["ipc_version"], data["rowd_version"].as_str().unwrap_or("?"));
    println!(
        "Celular       {}",
        if data["connected"].as_bool() == Some(true) {
            "conectado"
        } else {
            "desconectado"
        }
    );
    if let Some(last) = data["app"]["device"]["last_connection"].as_u64() {
        println!("Última conexão {last}");
    }
    if let Some(shares) = data["app"]["shares"].as_array() {
        println!("\nShares");
        for item in shares {
            let share = &item["share"];
            let name = share["name"].as_str().unwrap_or("?");
            let state = if share["enabled"].as_bool() == Some(false) {
                "pausado"
            } else if item["root_available"].as_bool() == Some(false) {
                "indisponível"
            } else if item["error"].is_string() || item["last_error"].is_object() {
                "erro"
            } else {
                "ativo"
            };
            let direction = match share["mode"].as_str() {
                Some("bidirectional") => "↔",
                Some("to_android") => "→",
                Some("to_pc") => "←",
                _ => "?",
            };
            println!("{}  {}  {}", name, direction, state);
        }
    }
    Ok(())
}

fn main() {
    trace::process_start();
    let result = run();
    rowd_core::trace_event!(
        trace::Level::Info,
        trace::Component::CLI,
        "PROCESS_STOP",
        serde_json::json!({"result":if result.is_ok(){"success"}else{"failed"}})
    );
    if let Err(error) = trace::stop("process_exit") {
        eprintln!("TRACE_WRITER_FAILED: {error:#}");
    }
    if let Err(error) = result {
        eprintln!("Rowd: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trace_commands_parse() {
        assert!(Cli::try_parse_from([
            "rowd",
            "daemon",
            "trace",
            "start",
            "--output-dir",
            "/tmp/traces"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "rowd",
            "trace",
            "show",
            "/tmp/traces",
            "--component",
            "watcher",
            "--share",
            "camera",
            "--errors",
            "--connection",
            "31",
            "--round",
            "812",
            "--file",
            "file-id",
            "--request",
            "93"
        ])
        .is_ok());
    }
    #[test]
    fn pair_address_is_optional() {
        let cli = Cli::try_parse_from(["rowd", "pair"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Pair { address: None, .. })
        ));
    }
}
