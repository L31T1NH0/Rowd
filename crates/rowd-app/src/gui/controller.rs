use anyhow::{ensure, Context, Result};
use fs2::FileExt;
use rowd_app::{App, AppSignal, AppSnapshot, PendingPairRequest};
use rowd_core::{config::SyncMode, pairing::Peer};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use super::desktop::{self, Preferences};

pub enum Command {
    SaveShare {
        id: Option<String>,
        request: Option<String>,
        name: String,
        path: PathBuf,
        mode: SyncMode,
    },
    SetShareEnabled(String, bool),
    SetPaused(bool),
    RejectRequest(String),
    RemoveShare(String),
    SyncShare(String),
    StartPairing,
    StopPairing,
    Offer(String),
    DecidePairing(String, bool),
    CheckConnection,
    Unlink,
    Restore(String, String),
    ExportRecovery(String, String),
    KeepRecovery(String, String),
    PickFolder,
    SavePreferences(Preferences),
    ExportDiagnostic,
    RetryServer,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    General,
    SaveShare,
    Preferences,
    NativeDialog,
    Pairing,
}

pub enum Event {
    Snapshot(AppSnapshot, CoreState, Option<PendingPairRequest>, bool),
    Finished(Completion, Result<String, String>),
    FolderSelected(Option<PathBuf>),
    Peers(Vec<Peer>),
    Preferences(Preferences),
    Fatal(String),
    TrayAvailable(bool),
    ShowWindow,
    TogglePause,
    Quit,
}

#[derive(Clone, Default)]
pub struct CoreState {
    pub running: bool,
    pub connected: bool,
    pub syncing: BTreeSet<String>,
    pub error: Option<String>,
    attention: bool,
}

pub struct Runtime {
    pub commands: SyncSender<Command>,
    pub events: Receiver<Event>,
    pub stop: Arc<AtomicBool>,
    pub engine_stop: Arc<AtomicBool>,
    pub tray_available: Arc<AtomicBool>,
    pub worker: JoinHandle<()>,
}

impl Runtime {
    pub fn start(app: App, icon: Vec<u8>) -> Self {
        let (commands, jobs) = mpsc::sync_channel(16);
        let (events_tx, events) = mpsc::sync_channel(32);
        let stop = Arc::new(AtomicBool::new(false));
        let engine_stop = Arc::new(AtomicBool::new(false));
        let tray_available = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let service_stop = engine_stop.clone();
        let tray_flag = tray_available.clone();
        let worker = thread::spawn(move || {
            if let Err(error) = run(
                app,
                jobs,
                &events_tx,
                worker_stop,
                service_stop,
                tray_flag,
                icon,
            ) {
                let _ = events_tx.send(Event::Fatal(format!("{error:#}")));
            }
        });
        Self {
            commands,
            events,
            stop,
            engine_stop,
            tray_available,
            worker,
        }
    }
}

fn start_engine(app: App, stop: Arc<AtomicBool>, state: Arc<Mutex<CoreState>>) -> JoinHandle<()> {
    stop.store(false, Ordering::Release);
    *state.lock().unwrap() = CoreState::default();
    thread::spawn(move || {
        let signals = state.clone();
        let result = app.serve_observed(
            stop,
            |_| {},
            move |signal: AppSignal| {
                let mut state = signals.lock().unwrap();
                match signal.kind {
                    "ready" => state.running = true,
                    "connected" => state.connected = true,
                    "disconnected" => {
                        state.connected = false;
                        state.syncing.clear();
                    }
                    "sync_start" => {
                        if let Some(id) = signal.share_id {
                            state.syncing.insert(id);
                        }
                    }
                    "sync_done" | "share_error" => {
                        if let Some(id) = signal.share_id {
                            state.syncing.remove(&id);
                        }
                        state.attention |= signal.kind == "share_error";
                    }
                    _ => {}
                }
            },
        );
        let mut state = state.lock().unwrap();
        state.running = false;
        state.connected = false;
        state.syncing.clear();
        state.error = result.err().map(|error| format!("{error:#}"));
    })
}

fn completion(command: &Command) -> Completion {
    match command {
        Command::SaveShare { .. } => Completion::SaveShare,
        Command::SavePreferences(_) => Completion::Preferences,
        Command::PickFolder | Command::ExportRecovery(..) | Command::ExportDiagnostic => {
            Completion::NativeDialog
        }
        Command::DecidePairing(..) => Completion::Pairing,
        _ => Completion::General,
    }
}

fn execute(
    app: &App,
    command: Command,
    peers: &mut Vec<Peer>,
    prefs: &mut Preferences,
    events: &SyncSender<Event>,
) -> Result<String> {
    match command {
        Command::SaveShare {
            id,
            request,
            name,
            path,
            mode,
        } => {
            if let Some(request) = request {
                app.accept_share_request(&request, &path)?;
                Ok("Pasta vinculada ao pedido do celular.".into())
            } else if let Some(id) = id {
                app.edit_share(&id, name, path, mode)?;
                Ok("Alterações da pasta salvas.".into())
            } else {
                app.add_share(name, path, mode)?;
                Ok("Pasta adicionada. Vincule a pasta correspondente no Android.".into())
            }
        }
        Command::SetShareEnabled(id, enabled) => {
            app.set_share_enabled(&id, enabled)?;
            Ok(if enabled {
                "Pasta retomada."
            } else {
                "Pasta pausada."
            }
            .into())
        }
        Command::SetPaused(paused) => {
            app.set_sync_paused(paused)?;
            Ok(if paused {
                "Sincronização pausada."
            } else {
                "Sincronização retomada."
            }
            .into())
        }
        Command::RejectRequest(id) => {
            app.reject_share_request(&id)?;
            Ok("Pedido recusado.".into())
        }
        Command::RemoveShare(id) => {
            app.remove_share(&id)?;
            Ok("Vínculo removido. Arquivos e recuperação foram preservados.".into())
        }
        Command::SyncShare(id) => {
            app.request_share_sync(&id)?;
            Ok("Sincronização solicitada. Acompanhe o estado da pasta.".into())
        }
        Command::StartPairing => {
            app.start_pairing_mode()?;
            *peers = app.discover_phones()?;
            let _ = events.send(Event::Peers(peers.clone()));
            Ok(if peers.is_empty() {
                "Busca aberta por 2 minutos. Abra Encontrar na rede no Android."
            } else {
                "Selecione o celular encontrado para enviar o convite."
            }
            .into())
        }
        Command::StopPairing => {
            app.stop_pairing_mode();
            peers.clear();
            let _ = events.send(Event::Peers(Vec::new()));
            Ok("Busca de pareamento encerrada.".into())
        }
        Command::Offer(id) => {
            let peer = peers
                .iter()
                .find(|peer| peer.session_id == id)
                .context("celular não está mais na lista; busque novamente")?;
            app.offer_pairing(peer)?;
            Ok("Convite enviado. Aguarde o código de confirmação do celular.".into())
        }
        Command::DecidePairing(id, accept) => {
            let pending = app
                .pending_pairing()
                .filter(|pending| pending.request_id == id)
                .context("pedido de pareamento expirou")?;
            app.decide_pairing(&id, accept)?;
            if accept {
                prefs.peer_id = pending.device_id;
                prefs.peer_name = pending.device_name;
                if let Err(error) = prefs.save(app.home()) {
                    return Ok(format!(
                        "Código confirmado; não foi possível salvar o nome do celular: {error:#}"
                    ));
                }
                let _ = events.send(Event::Preferences(prefs.clone()));
            }
            Ok(if accept {
                "Código confirmado. Aguardando autenticação do celular."
            } else {
                "Pareamento recusado."
            }
            .into())
        }
        Command::CheckConnection => {
            let test = app.connection_test()?;
            Ok(test
                .checks
                .into_iter()
                .map(|check| {
                    format!(
                        "{}: {} — {}",
                        check.label,
                        if check.ok { "OK" } else { "atenção" },
                        check.detail
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        Command::Unlink => {
            app.unlink_device()?;
            Ok("Celular desvinculado. Arquivos locais foram preservados.".into())
        }
        Command::Restore(share, id) => {
            app.restore_recovery(&share, &id)?;
            Ok("Versão restaurada. Consulte a pasta e o registro de recuperação.".into())
        }
        Command::KeepRecovery(share, id) => {
            app.keep_recovery(&share, &id)?;
            Ok("Versão atual mantida. A cópia de recuperação foi preservada.".into())
        }
        Command::ExportRecovery(share, id) => {
            let Some(path) = rfd::FileDialog::new()
                .set_title("Exportar versão preservada")
                .save_file()
            else {
                return Ok("Exportação cancelada.".into());
            };
            app.export_recovery(&share, &id, &path)?;
            Ok(format!("Versão exportada para {}.", path.display()))
        }
        Command::PickFolder => {
            let path = rfd::FileDialog::new()
                .set_title("Escolher pasta para sincronizar")
                .pick_folder();
            if let Some(path) = &path {
                ensure!(
                    path.to_str().is_some(),
                    "o caminho da pasta precisa ser UTF-8"
                );
            }
            let _ = events.send(Event::FolderSelected(path));
            Ok(String::new())
        }
        Command::SavePreferences(next) => {
            if next.launch_on_login != prefs.launch_on_login {
                desktop::set_autostart(app.home(), next.launch_on_login)?;
            }
            if let Err(error) = next.save(app.home()) {
                if next.launch_on_login != prefs.launch_on_login {
                    desktop::set_autostart(app.home(), prefs.launch_on_login)
                        .context(format!("preferências não foram salvas ({error:#}); falha ao reverter inicialização automática"))?;
                }
                return Err(error);
            }
            *prefs = next;
            let _ = events.send(Event::Preferences(prefs.clone()));
            Ok("Preferências salvas.".into())
        }
        Command::ExportDiagnostic => {
            let Some(path) = rfd::FileDialog::new()
                .set_title("Salvar diagnóstico do Rowd")
                .set_file_name("rowd-diagnostic.json")
                .save_file()
            else {
                return Ok("Exportação cancelada.".into());
            };
            app.export_diagnostic(&path)?;
            Ok(format!("Diagnóstico salvo em {}.", path.display()))
        }
        Command::RetryServer => Ok("Reiniciando o servidor.".into()),
    }
}

fn run(
    app: App,
    jobs: Receiver<Command>,
    events: &SyncSender<Event>,
    stop: Arc<AtomicBool>,
    engine_stop: Arc<AtomicBool>,
    tray_available: Arc<AtomicBool>,
    icon: Vec<u8>,
) -> Result<()> {
    fs::create_dir_all(app.home())?;
    let app = App::new(app.home().canonicalize()?);
    fs::create_dir_all(app.home().join(".rowd"))?;
    let gui_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(app.home().join(".rowd/gui.lock"))?;
    gui_lock
        .try_lock_exclusive()
        .context("outra janela do Rowd já está usando este perfil")?;
    app.pair("")?;
    let mut prefs = Preferences::load(app.home())?;
    prefs.launch_on_login = desktop::autostart_enabled(app.home())?;
    let _ = events.send(Event::Preferences(prefs.clone()));

    #[cfg(target_os = "linux")]
    let tray = {
        use ksni::blocking::TrayMethods;
        if icon.len() == 32 * 32 * 4 {
            super::desktop::Tray {
                events: events.clone(),
                available: tray_available.clone(),
                icon,
            }
            .spawn()
            .ok()
        } else {
            None
        }
    };

    #[cfg(windows)]
    let _tray = super::windows::Tray::start(events.clone(), tray_available.clone(), icon).ok();
    #[cfg(not(any(target_os = "linux", windows)))]
    let _ = icon;
    let _ = events.send(Event::TrayAvailable(tray_available.load(Ordering::Acquire)));

    let state = Arc::new(Mutex::new(CoreState::default()));
    let mut engine = start_engine(app.clone(), engine_stop.clone(), state.clone());
    let mut peers = Vec::new();
    let mut last_snapshot = Instant::now() - Duration::from_secs(2);
    let mut last_notification = None::<Instant>;
    let mut previous_conflicts = BTreeSet::new();
    while !stop.load(Ordering::Acquire) {
        match jobs.recv_timeout(Duration::from_millis(200)) {
            Ok(command) => {
                let kind = completion(&command);
                if matches!(command, Command::RetryServer) && engine.is_finished() {
                    let _ = engine.join();
                    engine = start_engine(app.clone(), engine_stop.clone(), state.clone());
                }
                let result = execute(&app, command, &mut peers, &mut prefs, events)
                    .map_err(|error| format!("{error:#}"));
                let _ = events.send(Event::Finished(kind, result));
                last_snapshot = Instant::now() - Duration::from_secs(2);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if last_snapshot.elapsed() >= Duration::from_secs(1) {
            let (core, attention) = {
                let mut state = state.lock().unwrap();
                let attention = state.attention;
                state.attention = false;
                (state.clone(), attention)
            };
            match app.snapshot() {
                Ok(snapshot) => {
                    let conflicts: BTreeSet<_> = snapshot
                        .shares
                        .iter()
                        .flat_map(|status| {
                            status
                                .conflicts
                                .iter()
                                .map(|path| (status.share.share_id.clone(), path.clone()))
                        })
                        .collect();
                    if (attention || !conflicts.is_subset(&previous_conflicts))
                        && prefs.notify_errors
                        && last_notification
                            .is_none_or(|last| last.elapsed() >= Duration::from_secs(30))
                    {
                        desktop::notify_attention();
                        last_notification = Some(Instant::now());
                    }
                    previous_conflicts = conflicts;
                    let _ = events.try_send(Event::Snapshot(
                        snapshot,
                        core,
                        app.pending_pairing(),
                        app.pairing_mode_active(),
                    ));
                }
                Err(error) => {
                    let _ = events.try_send(Event::Fatal(format!(
                        "Não foi possível ler o perfil: {error:#}"
                    )));
                }
            }
            last_snapshot = Instant::now();
        }
    }
    engine_stop.store(true, Ordering::Release);
    app.stop_pairing_mode();
    let _ = engine.join();
    #[cfg(target_os = "linux")]
    if let Some(tray) = tray {
        tray.shutdown().wait();
    }
    Ok(())
}
