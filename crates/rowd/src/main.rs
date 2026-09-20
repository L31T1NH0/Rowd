mod service;
mod tui;
use anyhow::{ensure, Context, Result};
use clap::{Parser, Subcommand};
use rowd_core::{
    model::{Invitation, VERSION},
    protocol, random_id,
    storage::{atomic_json, LocalStore, Store},
    sync::{self, State},
    tls,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Parser)]
#[command(
    name = "rowd",
    version,
    about = "Uma pasta. Dois dispositivos. Sem nuvem intermediária."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(long, global = true)]
    home: Option<PathBuf>,
}
#[derive(Subcommand)]
enum Command {
    /// Crie ou exiba o pareamento único e seu QR.
    Pair {
        #[arg(long)]
        address: String,
        #[arg(long)]
        invite: Option<PathBuf>,
    },
    /// Migre explicitamente a V1 preservando identidade e recovery.
    Migrate {
        #[arg(long)]
        folder: PathBuf,
        #[arg(long)]
        address: String,
    },
    /// Administre os Shares persistentes.
    Share {
        #[command(subcommand)]
        command: ShareCommand,
    },
    /// Sirva todos os Shares com watcher e fallback periódico.
    Run {
        #[arg(long)]
        listen: Option<String>,
        #[arg(long)]
        once: bool,
    },
    /// Simule o Android gerenciado em uma raiz local.
    DeviceSync {
        #[arg(long)]
        folder: PathBuf,
        #[arg(long)]
        invite: PathBuf,
        #[arg(long)]
        watch: bool,
    },
    /// Reconstrua os manifestos sem usar o cache.
    Scan,
    /// Mostre pendências, conflitos e último sincronismo.
    Shares,
    /// Liste, restaure ou exporte versões preservadas.
    Recovery {
        #[arg(long)]
        folder: PathBuf,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        action: Option<String>,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Prepare uma pasta e exporte um convite privado para o Android.
    Init {
        #[arg(long)]
        folder: PathBuf,
        #[arg(long)]
        address: String,
        #[arg(long)]
        invite: PathBuf,
    },
    /// Aguarde o celular na rede local (interrompa com Ctrl+C).
    Serve {
        #[arg(long)]
        folder: PathBuf,
        #[arg(long, default_value = "0.0.0.0:43821")]
        listen: String,
        #[arg(long)]
        once: bool,
    },
    /// Cliente de teste para simular o celular em outro diretório.
    Sync {
        #[arg(long)]
        folder: PathBuf,
        #[arg(long)]
        invite: PathBuf,
        #[arg(long)]
        watch: bool,
    },
    /// Calcule e mostre os hashes atuais da pasta.
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
}
fn parse_mode(mode: &str) -> Result<rowd_core::config::SyncMode> {
    match mode {
        "bidirectional" => Ok(rowd_core::config::SyncMode::Bidirectional),
        "to_android" => Ok(rowd_core::config::SyncMode::ToAndroid),
        "to_pc" => Ok(rowd_core::config::SyncMode::ToPc),
        _ => anyhow::bail!("mode must be bidirectional, to_android or to_pc"),
    }
}

#[derive(Serialize, Deserialize)]
struct Config {
    version: u32,
    root: String,
    pair_id: String,
    folder_id: String,
    cert: String,
    key: String,
    secret: String,
}

fn config(store: &LocalStore) -> Result<Config> {
    let cfg: Config = serde_json::from_reader(
        File::open(store.private().join("server.json")).context("execute rowd init first")?,
    )?;
    ensure!(
        cfg.version == VERSION && cfg.root == store.root().to_string_lossy(),
        "folder moved: configuration root mismatch"
    );
    Ok(cfg)
}
fn save_private(path: &Path, value: &impl Serialize) -> Result<()> {
    // create_new prevents accidentally replacing another invitation or pairing.
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let home = cli.home.unwrap_or_else(service::default_home);
    let Some(command) = cli.command else {
        return tui::run(&home);
    };
    match command {
        Command::Pair { address, invite } => {
            let cfg = service::pair(&home, &address)?;
            if let Some(path) = invite {
                save_private(&path, &service::invitation(&cfg))?;
            }
            println!("{}", service::qr(&cfg)?);
            println!("QR privado: {}", service::export_qr(&home, &cfg)?.display());
            println!(
                "Certificado SHA-256: {}",
                hex::encode(Sha256::digest(hex::decode(&cfg.cert)?))
            );
        }
        Command::Migrate { folder, address } => service::migrate(&home, &folder, &address)?,
        Command::Share { command } => service::update(&home, |cfg| {
            match command {
                ShareCommand::Add {
                    name,
                    folder,
                    android,
                    mode,
                } => {
                    println!(
                        "{}",
                        cfg.add_share(&home, name, folder, android, parse_mode(&mode)?)?
                    );
                }
                ShareCommand::Edit {
                    id,
                    name,
                    folder,
                    mode,
                } => {
                    let mut share = cfg
                        .shares
                        .iter()
                        .find(|s| s.share_id == id)
                        .context("unknown Share")?
                        .clone();
                    if let Some(name) = name {
                        share.name = name;
                    }
                    if let Some(folder) = folder {
                        share.root = folder;
                    }
                    if let Some(mode) = mode {
                        share.mode = parse_mode(&mode)?;
                    }
                    cfg.put_share(&home, share)?;
                }
                ShareCommand::Remove { id, confirm } => {
                    ensure!(
                        confirm,
                        "use --confirm; files and recovery will be retained"
                    );
                    cfg.remove_share(&home, &id)?;
                }
            }
            Ok(())
        })?,
        Command::Run { listen, once } => service::serve(
            &home,
            listen.as_deref(),
            once,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            |e| println!("{e}"),
        )?,
        Command::DeviceSync {
            folder,
            invite,
            watch,
        } => {
            std::fs::create_dir_all(folder.join(".rowd"))?;
            let id_path = folder.join(".rowd/client-id.json");
            let id: String = if id_path.exists() {
                serde_json::from_reader(File::open(&id_path)?)?
            } else {
                let id = random_id()?;
                atomic_json(&id_path, &id)?;
                id
            };
            let invitation = serde_json::from_reader(File::open(invite)?)?;
            let mut device = rowd_core::managed::LocalDevice::open(&folder)?;
            loop {
                match rowd_core::managed::client_round(&invitation, &id, &mut device) {
                    Ok(r) => println!("{}", serde_json::to_string(&r)?),
                    Err(e) if watch => eprintln!("{e:#}"),
                    Err(e) => return Err(e),
                }
                if !watch {
                    break;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
        Command::Scan => service::scan(&home, true)?,
        Command::Shares => println!(
            "{}",
            serde_json::to_string_pretty(&service::status(&home)?)?
        ),
        Command::Recovery {
            folder,
            id,
            action,
            output,
        } => {
            let mut store = LocalStore::open_recovery(&folder)?;
            if let Some(id) = id {
                store.resolve_recovery(
                    &id,
                    action
                        .as_deref()
                        .context("--action required: keep, restore, export")?,
                    output.as_deref(),
                )?;
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&store.recovery_entries()?)?
                );
            }
        }
        Command::Init {
            folder,
            address,
            invite,
        } => {
            let store = LocalStore::open(&folder)?;
            ensure!(
                !store.private().join("server.json").exists(),
                "folder already paired; existing identity preserved"
            );
            ensure!(!invite.exists(), "invitation file already exists");
            let invite_parent = invite
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
                .canonicalize()?;
            ensure!(
                !invite_parent.starts_with(store.root()),
                "save private invitation OUTSIDE the shared folder"
            );
            let cert = rcgen::generate_simple_self_signed(vec!["rowd.local".into()])?;
            let cfg = Config {
                version: VERSION,
                root: store.root().to_string_lossy().into(),
                pair_id: random_id()?,
                folder_id: random_id()?,
                secret: random_id()?,
                cert: hex::encode(cert.cert.der()),
                key: hex::encode(cert.key_pair.serialize_der()),
            };
            let invitation = Invitation {
                version: VERSION,
                address,
                pair_id: cfg.pair_id.clone(),
                folder_id: cfg.folder_id.clone(),
                cert_der: cfg.cert.clone(),
                secret: cfg.secret.clone(),
            };
            invitation.validate()?;
            save_private(&invite, &invitation)?;
            save_private(&store.private().join("server.json"), &cfg)?;
            println!("Rowd pronto. Pasta: {}\nConvite privado: {}\nImporte esse arquivo no Android por USB ou outro canal confiável. Ele permite acesso à pasta.\nCertificado SHA-256: {}",store.root().display(),invite.display(),hex::encode(Sha256::digest(hex::decode(cfg.cert)?)));
        }
        Command::Serve {
            folder,
            listen,
            once,
        } => {
            let mut store = LocalStore::open(&folder)?;
            let cfg = config(&store)?;
            let tls_config = tls::server_config(&cfg.cert, &cfg.key)?;
            let state_path = store.private().join("sync-state.json");
            let mut state = State::load(&state_path, &cfg.pair_id, &cfg.folder_id)?;
            let listener = TcpListener::bind(&listen)?;
            println!(
                "Rowd ouvindo em {} • {}",
                listener.local_addr()?,
                store.root().display()
            );
            for socket in listener.incoming() {
                let result = (|| -> Result<sync::Report> {
                    let mut stream = tls::accept(socket?, tls_config.clone())?;
                    let root = protocol::server_auth(
                        &mut stream,
                        &cfg.pair_id,
                        &cfg.folder_id,
                        &cfg.secret,
                    )?;
                    if let Some(expected) = &state.peer_root {
                        ensure!(
                            expected == &root,
                            "Android folder changed; pairing belongs to a different root"
                        );
                    } else {
                        state.peer_root = Some(root);
                        atomic_json(&state_path, &state)?;
                    }
                    let result = sync::coordinate(&mut stream, &mut store, &mut state, &state_path);
                    if let Err(ref e) = result {
                        let _ = protocol::send(
                            &mut stream,
                            &protocol::Message::Error {
                                message: e.to_string(),
                            },
                        );
                    }
                    result
                })();
                match result {
                    Ok(report) => println!(
                        "Sincronizado: {} transferências, {} conflitos",
                        report.transferred, report.conflicts
                    ),
                    Err(e) => {
                        eprintln!("Rodada interrompida: {e:#}");
                        if once {
                            return Err(e);
                        }
                    }
                }
                if once {
                    break;
                }
            }
        }
        Command::Sync {
            folder,
            invite,
            watch,
        } => {
            let invitation: Invitation = serde_json::from_reader(File::open(invite)?)?;
            let mut store = LocalStore::open(&folder)?;
            let id_path = store.private().join("client-id.json");
            let id: String = if id_path.exists() {
                serde_json::from_reader(File::open(id_path)?)?
            } else {
                let id = random_id()?;
                atomic_json(&id_path, &id)?;
                id
            };
            loop {
                match sync::client_round(&invitation, &id, &mut store) {
                    Ok(report) => println!("{}", serde_json::to_string(&report)?),
                    Err(e) if watch => eprintln!("Aguardando próxima tentativa: {e:#}"),
                    Err(e) => return Err(e),
                }
                if !watch {
                    break;
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        }
        Command::Status { folder } => {
            let mut store = LocalStore::open(&folder)?;
            store.invalidate();
            println!("{}", serde_json::to_string_pretty(&store.scan()?)?);
        }
    }
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("Rowd: {e:#}");
        std::process::exit(1);
    }
}
