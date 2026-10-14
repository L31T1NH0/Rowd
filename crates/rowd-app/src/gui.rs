//! Native desktop frontend. The controller owns all filesystem and network work.
mod controller;
mod desktop;
#[cfg(windows)]
mod windows;

use controller::{Command, Completion, Event, Runtime};
use desktop::Preferences;
use rowd_app::{App, AppSnapshot, PendingPairRequest};
use rowd_core::config::SyncMode;
use slint::{ComponentHandle, Model, VecModel};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{atomic::Ordering, mpsc::SyncSender},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

slint::include_modules!();

const DIRECTIONS: [&str; 3] = [
    "Nos dois sentidos",
    "Do PC para o celular",
    "Do celular para o PC",
];
fn direction(mode: SyncMode) -> usize {
    match mode {
        SyncMode::Bidirectional => 0,
        SyncMode::ToAndroid => 1,
        SyncMode::ToPc => 2,
    }
}
fn mode(index: i32) -> SyncMode {
    match index {
        1 => SyncMode::ToAndroid,
        2 => SyncMode::ToPc,
        _ => SyncMode::Bidirectional,
    }
}

fn display_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    #[cfg(windows)]
    {
        if let Some(path) = path.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{path}");
        }
        path.strip_prefix(r"\\?\").unwrap_or(&path).to_owned()
    }
    #[cfg(not(windows))]
    {
        path.into_owned()
    }
}

#[derive(Default)]
struct ViewState {
    preferences: Preferences,
    edit_id: Option<String>,
    request_id: Option<String>,
    pair_id: Option<String>,
    decided_pair: Option<String>,
    confirmation: Option<Command>,
}

fn submit(ui: &AppWindow, commands: &SyncSender<Command>, command: Command) {
    if ui.get_busy() {
        return;
    }
    let native = matches!(
        command,
        Command::PickFolder | Command::ExportRecovery(..) | Command::ExportDiagnostic
    );
    match commands.try_send(command) {
        Ok(()) => {
            ui.set_busy(true);
            ui.set_native_dialog(native);
            ui.set_form_error("".into());
        }
        Err(error) => {
            ui.set_notice(format!("Não foi possível executar a operação: {error}").into())
        }
    }
}

fn selected(ui: &AppWindow) -> Option<ShareRow> {
    ui.get_shares()
        .row_data(ui.get_selected_index().max(0) as usize)
}
fn recovery(ui: &AppWindow) -> Option<RecoveryRow> {
    ui.get_recoveries()
        .row_data(ui.get_recovery_index().max(0) as usize)
}
fn confirm(
    ui: &AppWindow,
    state: &RefCell<ViewState>,
    title: &str,
    body: String,
    command: Command,
) {
    state.borrow_mut().confirmation = Some(command);
    ui.set_confirm_title(title.into());
    ui.set_confirm_body(body.into());
    ui.set_confirm_open(true);
}
fn open_form(
    ui: &AppWindow,
    state: &RefCell<ViewState>,
    row: Option<ShareRow>,
    request: Option<RequestRow>,
) {
    let mut state = state.borrow_mut();
    state.edit_id = row.as_ref().map(|row| row.id.to_string());
    state.request_id = request.as_ref().map(|request| request.id.to_string());
    ui.set_editing(row.is_some());
    ui.set_from_request(request.is_some());
    let name = row
        .as_ref()
        .map(|row| row.name.clone())
        .or_else(|| request.as_ref().map(|request| request.name.clone()))
        .unwrap_or_default();
    let direction = row
        .as_ref()
        .map(|row| row.direction.clone())
        .or_else(|| request.as_ref().map(|request| request.direction.clone()))
        .unwrap_or_default();
    ui.set_form_name(name);
    ui.set_form_path(row.map(|row| row.pc_path).unwrap_or_default());
    ui.set_form_direction(
        DIRECTIONS
            .iter()
            .position(|value| *value == direction.as_str())
            .unwrap_or(0) as i32,
    );
    ui.set_form_error("".into());
    ui.set_form_open(true);
}

fn update_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    if model.iter().collect::<Vec<_>>() != rows {
        model.set_vec(rows);
    }
}

fn update_snapshot(
    ui: &AppWindow,
    state: &RefCell<ViewState>,
    snapshot: AppSnapshot,
    core: controller::CoreState,
    pending: Option<PendingPairRequest>,
    pairing: bool,
    shares: &VecModel<ShareRow>,
    requests: &VecModel<RequestRow>,
    recoveries: &VecModel<RecoveryRow>,
) {
    let selected_id = selected(ui).map(|row| row.id);
    let request_id = ui
        .get_requests()
        .row_data(ui.get_request_index().max(0) as usize)
        .map(|row| row.id);
    let recovery_id = recovery(ui).map(|row| (row.share_id, row.id));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let rows: Vec<_> = snapshot
        .shares
        .into_iter()
        .map(|status| {
            let failure = status.error.as_deref().or_else(|| {
                status
                    .last_error
                    .as_ref()
                    .filter(|error| error.resolved_at.is_none())
                    .map(|error| error.message.as_str())
            });
            let attention =
                !status.root_available || failure.is_some() || !status.conflicts.is_empty();
            let syncing = core.syncing.contains(&status.share.share_id);
            let label = if !status.root_available {
                "Pasta indisponível"
            } else if failure.is_some() {
                "Precisa de atenção"
            } else if !status.conflicts.is_empty() {
                "Conflitos preservados"
            } else if !status.share.enabled {
                "Pausada"
            } else if syncing {
                "Sincronizando"
            } else if status.last_sync.is_some() {
                "Sincronizada"
            } else {
                "Aguardando sincronização"
            };
            let summary = if let Some(error) = failure {
                error.to_string()
            } else if !status.root_available {
                "O caminho local não está disponível.".into()
            } else if !status.conflicts.is_empty() {
                format!(
                    "{} conflito(s). Consulte a recuperação.",
                    status.conflicts.len()
                )
            } else if let Some(last) = status.last_sync {
                format!(
                    "Última sincronização há {} min.",
                    now.saturating_sub(last) / 60
                )
            } else {
                "Sem sincronização concluída neste perfil.".into()
            };
            ShareRow {
                id: status.share.share_id.into(),
                name: status.share.name.into(),
                pc_path: display_path(&status.share.root).into(),
                phone_path: "A pasta correspondente é gerenciada no Rowd para Android.".into(),
                direction: DIRECTIONS[direction(status.share.mode)].into(),
                status: label.into(),
                summary: summary.into(),
                paused: !status.share.enabled,
                attention,
            }
        })
        .collect();
    let index = selected_id
        .and_then(|id| rows.iter().position(|row| row.id == id))
        .unwrap_or(0);
    ui.set_syncing(
        rows.get(index)
            .is_some_and(|row| core.syncing.contains(row.id.as_str())),
    );
    update_model(shares, rows);
    ui.set_selected_index(index as i32);
    let rows: Vec<_> = snapshot
        .requests
        .into_iter()
        .map(|request| RequestRow {
            id: request.request_id.into(),
            name: request.name.into(),
            direction: DIRECTIONS[direction(request.mode)].into(),
        })
        .collect();
    let index = request_id
        .and_then(|id| rows.iter().position(|row| row.id == id))
        .unwrap_or(0);
    ui.set_request_pending(!rows.is_empty());
    let names = rows.iter().map(|row| row.name.clone()).collect::<Vec<_>>();
    if ui.get_request_names().iter().collect::<Vec<_>>() != names {
        ui.set_request_names(Rc::new(VecModel::from(names)).into());
    }
    update_model(requests, rows);
    ui.set_request_index(index as i32);
    let rows: Vec<_> = snapshot
        .recovery
        .into_iter()
        .map(|item| RecoveryRow {
            share_id: item.share_id.into(),
            id: item.id.into(),
            name: item.share_name.into(),
            path: item.path.clone().into(),
            detail: format!(
                "{}\n{} bytes · {}",
                item.path,
                item.bytes,
                if item.backup_available {
                    "Cópia disponível"
                } else {
                    "Cópia indisponível"
                }
            )
            .into(),
            available: item.backup_available,
            finished: item.finished,
        })
        .collect();
    let index = recovery_id
        .and_then(|(share, id)| {
            rows.iter()
                .position(|row| row.share_id == share && row.id == id)
        })
        .unwrap_or(0);
    ui.set_recovery_pending(!rows.is_empty());
    let names = rows
        .iter()
        .map(|row| format!("{} / {}", row.name, row.path).into())
        .collect::<Vec<slint::SharedString>>();
    if ui.get_recovery_names().iter().collect::<Vec<_>>() != names {
        ui.set_recovery_names(Rc::new(VecModel::from(names)).into());
    }
    update_model(recoveries, rows);
    ui.set_recovery_index(index as i32);
    ui.set_all_paused(snapshot.device.sync_paused);
    ui.set_paired(snapshot.device.paired);
    ui.set_connected(core.connected && snapshot.device.paired);
    ui.set_server_running(core.running);
    ui.set_server_error(core.error.unwrap_or_default().into());
    ui.set_address(snapshot.device.address.into());
    ui.set_fingerprint(snapshot.device.fingerprint.into());
    let mut state = state.borrow_mut();
    ui.set_device_name(
        if state.preferences.peer_id == snapshot.device.identity
            && !state.preferences.peer_name.is_empty()
        {
            state.preferences.peer_name.as_str()
        } else {
            "Celular"
        }
        .into(),
    );
    ui.set_pairing_active(pairing);
    if let Some(pending) =
        pending.filter(|pending| state.decided_pair.as_deref() != Some(&pending.request_id))
    {
        if !ui.get_form_open() && !ui.get_confirm_open() && !ui.get_busy() {
            if state.pair_id.as_deref() != Some(pending.request_id.as_str()) {
                ui.set_pair_error("".into());
            }
            state.pair_id = Some(pending.request_id);
            ui.set_pair_code(pending.verification_code.into());
            ui.set_pair_name(pending.device_name.into());
            ui.set_pairing_open(true);
        }
    } else {
        state.pair_id = None;
        ui.set_pairing_open(false);
    }
}

pub fn run() -> anyhow::Result<()> {
    let ui = AppWindow::new()?;
    let mut args = std::env::args_os().skip(1);
    let home = match args.next() {
        None => {
            #[cfg(windows)]
            anyhow::ensure!(
                std::env::var_os("ROWD_HOME").is_some()
                    || std::env::var_os("LOCALAPPDATA").is_some()
                    || std::env::var_os("USERPROFILE").is_some(),
                "diretório do perfil Windows indisponível"
            );
            rowd_app::default_home()
        }
        Some(flag) if flag == "--profile" => PathBuf::from(
            args.next()
                .ok_or_else(|| anyhow::anyhow!("informe o caminho do perfil"))?,
        ),
        Some(_) => anyhow::bail!("argumento desconhecido; use rowd-app [--profile caminho]"),
    };
    anyhow::ensure!(args.next().is_none(), "argumentos inesperados");
    ui.set_home_path(display_path(&home).into());
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    ui.set_autostart_supported(cfg!(any(target_os = "linux", windows)));
    let icon = ui
        .get_app_icon()
        .to_rgba8()
        .map(|pixels| {
            let mut result = Vec::with_capacity(4096);
            if pixels.width() == 0 || pixels.height() == 0 {
                return result;
            }
            for y in 0..32 {
                for x in 0..32 {
                    let pixel = pixels.as_slice()[(y * pixels.height() as usize / 32)
                        * pixels.width() as usize
                        + x * pixels.width() as usize / 32];
                    result.extend_from_slice(&[pixel.a, pixel.r, pixel.g, pixel.b]);
                }
            }
            result
        })
        .unwrap_or_default();
    let Runtime {
        commands,
        events,
        stop,
        engine_stop,
        tray_available,
        worker,
    } = Runtime::start(App::new(home), icon);
    let state = Rc::new(RefCell::new(ViewState::default()));
    let shares = Rc::new(VecModel::<ShareRow>::default());
    let requests = Rc::new(VecModel::<RequestRow>::default());
    let recoveries = Rc::new(VecModel::<RecoveryRow>::default());
    ui.set_shares(shares.clone().into());
    ui.set_requests(requests.clone().into());
    ui.set_recoveries(recoveries.clone().into());

    // Every command captures stable identifiers before asynchronous work begins.
    macro_rules! callback {
        ($on:ident, |$window:ident, $view:ident, $sender:ident| $body:block) => {{
            let weak = ui.as_weak();
            let shared = state.clone();
            let tx = commands.clone();
            ui.$on(move || {
                if let Some($window) = weak.upgrade() {
                    let $view = &shared;
                    let $sender = &tx;
                    $body
                }
            });
        }};
    }
    callback!(on_add_share, |window, view, _tx| {
        open_form(&window, view, None, None);
    });
    callback!(on_edit_share, |window, view, _tx| {
        if let Some(row) = selected(&window) {
            open_form(&window, view, Some(row), None);
        }
    });
    callback!(on_accept_request, |window, view, _tx| {
        if let Some(request) = window
            .get_requests()
            .row_data(window.get_request_index().max(0) as usize)
        {
            open_form(&window, view, None, Some(request));
        }
    });
    callback!(on_save_share, |window, view, tx| {
        if window.get_form_name().trim().is_empty() || window.get_form_path().trim().is_empty() {
            window.set_form_error("Preencha o nome e escolha uma pasta.".into());
        } else {
            let view = view.borrow();
            submit(
                &window,
                tx,
                Command::SaveShare {
                    id: view.edit_id.clone(),
                    request: view.request_id.clone(),
                    name: window.get_form_name().trim().into(),
                    path: PathBuf::from(window.get_form_path().as_str()),
                    mode: mode(window.get_form_direction()),
                },
            );
        }
    });
    callback!(on_choose_folder, |window, _view, tx| {
        submit(&window, tx, Command::PickFolder);
    });
    callback!(on_toggle_share, |window, _view, tx| {
        if let Some(row) = selected(&window) {
            submit(
                &window,
                tx,
                Command::SetShareEnabled(row.id.to_string(), row.paused),
            );
        }
    });
    callback!(on_sync_share, |window, _view, tx| {
        if let Some(row) = selected(&window) {
            submit(&window, tx, Command::SyncShare(row.id.to_string()));
        }
    });
    callback!(on_toggle_all, |window, _view, tx| {
        submit(&window, tx, Command::SetPaused(!window.get_all_paused()));
    });
    callback!(on_reject_request, |window, _view, tx| {
        if let Some(row) = window
            .get_requests()
            .row_data(window.get_request_index().max(0) as usize)
        {
            submit(&window, tx, Command::RejectRequest(row.id.to_string()));
        }
    });
    callback!(on_remove_share, |window, view, _tx| {
        if let Some(row) = selected(&window) {
            confirm(
                &window,
                view,
                "Remover pasta?",
                format!(
                    "Remover o vínculo de {}? Os arquivos locais serão preservados.",
                    row.name
                ),
                Command::RemoveShare(row.id.to_string()),
            );
        }
    });
    callback!(on_start_pairing, |window, _view, tx| {
        submit(&window, tx, Command::StartPairing);
    });
    callback!(on_stop_pairing, |window, _view, tx| {
        submit(&window, tx, Command::StopPairing);
    });
    callback!(on_offer_peer, |window, _view, tx| {
        if let Some(row) = window
            .get_peers()
            .row_data(window.get_peer_index().max(0) as usize)
        {
            submit(&window, tx, Command::Offer(row.id.to_string()));
        }
    });
    callback!(on_check_connection, |window, _view, tx| {
        submit(&window, tx, Command::CheckConnection);
    });
    callback!(on_unlink_device, |window, view, _tx| {
        confirm(&window, view, "Desvincular celular?", "O vínculo e as credenciais serão renovados. As pastas e os arquivos locais serão preservados. Será necessário parear novamente.".into(), Command::Unlink);
    });
    callback!(on_restore_recovery, |window, view, _tx| {
        if let Some(row) = recovery(&window) {
            confirm(&window, view, "Restaurar esta versão?", format!("Restaurar {} na pasta {}? O conteúdo atual será substituído pela versão preservada.", row.path, row.name), Command::Restore(row.share_id.to_string(), row.id.to_string()));
        }
    });
    callback!(on_keep_recovery, |window, view, _tx| {
        if let Some(row) = recovery(&window) {
            confirm(&window, view, "Manter a versão atual?", format!("Marcar {} como resolvido mantendo o conteúdo atual? A cópia de recuperação será preservada.", row.path), Command::KeepRecovery(row.share_id.to_string(), row.id.to_string()));
        }
    });
    callback!(on_export_recovery, |window, _view, tx| {
        if let Some(row) = recovery(&window) {
            submit(
                &window,
                tx,
                Command::ExportRecovery(row.share_id.to_string(), row.id.to_string()),
            );
        }
    });
    callback!(on_export_diagnostic, |window, _view, tx| {
        submit(&window, tx, Command::ExportDiagnostic);
    });
    callback!(on_retry_server, |window, _view, tx| {
        submit(&window, tx, Command::RetryServer);
    });
    callback!(on_preferences_changed, |window, view, tx| {
        let mut prefs = view.borrow().preferences.clone();
        prefs.close_to_tray = window.get_close_to_tray();
        prefs.launch_on_login = window.get_launch_on_login();
        prefs.notify_errors = window.get_notify_errors();
        submit(&window, tx, Command::SavePreferences(prefs));
    });
    let weak = ui.as_weak();
    let view = state.clone();
    let tx = commands.clone();
    ui.on_confirm_action(move |accept| {
        if let Some(window) = weak.upgrade() {
            window.set_confirm_open(false);
            let command = view.borrow_mut().confirmation.take();
            if accept {
                if let Some(command) = command {
                    submit(&window, &tx, command);
                }
            }
        }
    });
    let weak = ui.as_weak();
    let view = state.clone();
    let tx = commands.clone();
    ui.on_decide_pairing(move |accept| {
        if let Some(window) = weak.upgrade() {
            if let Some(id) = view.borrow().pair_id.clone() {
                submit(&window, &tx, Command::DecidePairing(id, accept));
            }
        }
    });

    let weak = ui.as_weak();
    let close_stop = stop.clone();
    let close_engine = engine_stop.clone();
    let tray = tray_available.clone();
    ui.window().on_close_requested(move || {
        if let Some(window) = weak.upgrade() {
            if window.get_native_dialog() {
                window.set_notice("Feche o seletor de arquivos antes de sair.".into());
                return slint::CloseRequestResponse::KeepWindowShown;
            }
            if window.get_close_to_tray() && tray.load(Ordering::Acquire) {
                let _ = window.hide();
                return slint::CloseRequestResponse::KeepWindowShown;
            }
        }
        close_stop.store(true, Ordering::Release);
        close_engine.store(true, Ordering::Release);
        let _ = slint::quit_event_loop();
        slint::CloseRequestResponse::HideWindow
    });
    let timer = slint::Timer::default();
    let weak = ui.as_weak();
    let view = state.clone();
    let tx = commands.clone();
    let event_stop = stop.clone();
    let event_engine = engine_stop.clone();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            for event in events.try_iter() {
                match event {
                    Event::Snapshot(snapshot, core, pending, active) => update_snapshot(
                        &window,
                        &view,
                        snapshot,
                        core,
                        pending,
                        active,
                        &shares,
                        &requests,
                        &recoveries,
                    ),
                    Event::Finished(kind, result) => {
                        window.set_busy(false);
                        window.set_native_dialog(false);
                        match result {
                            Ok(message) => {
                                if kind == Completion::SaveShare {
                                    window.set_form_open(false);
                                }
                                if kind == Completion::Pairing {
                                    let mut state = view.borrow_mut();
                                    state.decided_pair = state.pair_id.take();
                                    window.set_pairing_open(false);
                                }
                                if !message.is_empty() {
                                    window.set_notice(message.into());
                                }
                            }
                            Err(error) => {
                                if kind == Completion::SaveShare
                                    || (kind == Completion::NativeDialog && window.get_form_open())
                                {
                                    window.set_form_error(error.clone().into());
                                }
                                if kind == Completion::Preferences {
                                    set_preferences(&window, &view.borrow().preferences);
                                }
                                if kind == Completion::Pairing {
                                    window.set_pair_error(error.clone().into());
                                }
                                window.set_notice(
                                    format!("Não foi possível concluir: {error}").into(),
                                );
                            }
                        }
                    }
                    Event::FolderSelected(path) => {
                        if window.get_form_open() {
                            if let Some(path) = path {
                                window.set_form_path(path.to_string_lossy().into_owned().into());
                            }
                        }
                    }
                    Event::Peers(peers) => {
                        let selected_id = window
                            .get_peers()
                            .row_data(window.get_peer_index().max(0) as usize)
                            .map(|row| row.id);
                        let index = selected_id
                            .and_then(|id| {
                                peers.iter().position(|peer| peer.session_id == id.as_str())
                            })
                            .unwrap_or(0);
                        window.set_peer_names(
                            Rc::new(VecModel::from(
                                peers
                                    .iter()
                                    .map(|peer| peer.device_name.clone().into())
                                    .collect::<Vec<slint::SharedString>>(),
                            ))
                            .into(),
                        );
                        window.set_peers(
                            Rc::new(VecModel::from(
                                peers
                                    .into_iter()
                                    .map(|peer| PeerRow {
                                        id: peer.session_id.into(),
                                        name: peer.device_name.into(),
                                    })
                                    .collect::<Vec<_>>(),
                            ))
                            .into(),
                        );
                        window.set_peer_index(index as i32);
                    }
                    Event::Preferences(prefs) => {
                        set_preferences(&window, &prefs);
                        view.borrow_mut().preferences = prefs;
                    }
                    Event::Fatal(error) => {
                        window.set_server_error(error.clone().into());
                        window.set_server_running(false);
                        window.set_busy(false);
                        window.set_notice(error.into());
                    }
                    Event::TrayAvailable(available) => {
                        window.set_tray_available(available);
                        if !available {
                            let _ = window.show();
                        }
                    }
                    Event::ShowWindow => {
                        window.window().set_minimized(false);
                        let _ = window.show();
                    }
                    Event::TogglePause => {
                        if window.get_server_running() {
                            submit(&window, &tx, Command::SetPaused(!window.get_all_paused()));
                        }
                    }
                    Event::Quit => {
                        if window.get_native_dialog() {
                            let _ = window.show();
                            window.set_notice("Feche o seletor de arquivos antes de sair.".into());
                        } else {
                            event_stop.store(true, Ordering::Release);
                            event_engine.store(true, Ordering::Release);
                            let _ = slint::quit_event_loop();
                        }
                    }
                }
            }
        },
    );
    let result = ui.show().and_then(|_| slint::run_event_loop_until_quit());
    stop.store(true, Ordering::Release);
    engine_stop.store(true, Ordering::Release);
    drop(timer);
    drop(commands);
    let _ = worker.join();
    result?;
    Ok(())
}

fn set_preferences(ui: &AppWindow, prefs: &Preferences) {
    ui.set_close_to_tray(prefs.close_to_tray);
    ui.set_launch_on_login(prefs.launch_on_login);
    ui.set_notify_errors(prefs.notify_errors);
}

#[cfg(windows)]
pub fn startup_error(message: &str) {
    windows::startup_error(message);
}
