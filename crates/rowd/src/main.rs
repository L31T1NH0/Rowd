mod tui;

use anyhow::{ensure, Context, Result};
use clap::{Parser, Subcommand};
use rowd_app::{App, ResetLevel};
use rowd_core::config::{RemapPolicy, SyncMode};
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
    Pair {
        #[arg(long)]
        address: String,
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
    },
    DeviceSync {
        #[arg(long)]
        folder: PathBuf,
        #[arg(long)]
        invite: PathBuf,
        #[arg(long)]
        watch: bool,
    },
    Scan,
    Shares,
    Recovery {
        #[arg(long)]
        folder: Option<PathBuf>,
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
        #[arg(long)]
        level: String,
        #[arg(long)]
        share: Option<String>,
        #[arg(long)]
        confirm: bool,
    },
    Init {
        #[arg(long)]
        folder: PathBuf,
        #[arg(long)]
        address: String,
        #[arg(long)]
        invite: PathBuf,
    },
    Serve {
        #[arg(long)]
        folder: PathBuf,
        #[arg(long, default_value = "0.0.0.0:43821")]
        listen: String,
        #[arg(long)]
        once: bool,
    },
    Sync {
        #[arg(long)]
        folder: PathBuf,
        #[arg(long)]
        invite: PathBuf,
        #[arg(long)]
        watch: bool,
    },
    Status {
        #[arg(long)]
        folder: PathBuf,
    },
}

#[derive(Subcommand)]
enum ShareCommand {
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        folder: PathBuf,
        #[arg(long)]
        android: Option<String>,
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
        android: String,
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
        Command::Pair { address, invite } => {
            app.pair(&address)?;
            if let Some(path) = invite {
                app.export_invitation(&path)?;
            }
            let info = app.pairing_info()?;
            println!("{}", info.qr);
            println!("QR privado: {}", info.qr_image.display());
            println!("Certificado SHA-256: {}", info.fingerprint);
        }
        Command::Migrate { folder, address } => rowd_app::migrate(&home, &folder, &address)?,
        Command::Share { command } => match command {
            ShareCommand::Add {
                name,
                folder,
                android,
                mode,
            } => println!(
                "{}",
                app.add_share(name, folder, android, parse_mode(&mode)?)?
            ),
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
            ShareCommand::Remap {
                id,
                android,
                policy,
            } => app.remap_share(&id, android, parse_policy(&policy)?)?,
            ShareCommand::Ignore { id, file } => {
                app.set_ignore_text(&id, &std::fs::read_to_string(file)?)?
            }
            ShareCommand::Sync { id } => app.request_share_sync(&id)?,
        },
        Command::Request { command } => match command {
            RequestCommand::List => println!(
                "{}",
                serde_json::to_string_pretty(&rowd_app::share_requests(&home)?)?
            ),
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
                ensure!(confirm, "use --confirm to revoke the current pairing");
                app.unlink_device()?;
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
        Command::Run { listen, once } => rowd_app::serve(
            &home,
            listen.as_deref(),
            once,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            |event| println!("{event}"),
        )?,
        Command::DeviceSync {
            folder,
            invite,
            watch,
        } => rowd_app::device_sync(&folder, &invite, watch)?,
        Command::Scan => rowd_app::scan(&home, true)?,
        Command::Shares => println!("{}", serde_json::to_string_pretty(&app.status()?)?),
        Command::Recovery {
            folder,
            share,
            id,
            action,
            output,
        } => {
            if let Some(folder) = folder {
                if let Some(entries) = rowd_app::legacy_recovery(
                    &folder,
                    id.as_deref(),
                    action.as_deref(),
                    output.as_deref(),
                )? {
                    println!("{}", serde_json::to_string_pretty(&entries)?);
                }
            } else if let Some(id) = id {
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
            let level = match level.as_str() {
                "interface" => ResetLevel::Interface,
                "share" => ResetLevel::Share(share.context("--share is required")?),
                "unlink" => ResetLevel::Unlink,
                "initial" => ResetLevel::Initial,
                "all" => ResetLevel::AllData,
                _ => anyhow::bail!("level must be interface, share, unlink, initial or all"),
            };
            app.reset(level)?;
        }
        Command::Init {
            folder,
            address,
            invite,
        } => rowd_app::legacy_init(&folder, &address, &invite)?,
        Command::Serve {
            folder,
            listen,
            once,
        } => rowd_app::legacy_serve(&folder, &listen, once)?,
        Command::Sync {
            folder,
            invite,
            watch,
        } => rowd_app::legacy_sync(&folder, &invite, watch)?,
        Command::Status { folder } => println!(
            "{}",
            serde_json::to_string_pretty(&rowd_app::legacy_status(&folder)?)?
        ),
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Rowd: {error:#}");
        std::process::exit(1);
    }
}
