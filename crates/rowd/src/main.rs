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
    command: Command,
}
#[derive(Subcommand)]
enum Command {
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
    match Cli::parse().command {
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
