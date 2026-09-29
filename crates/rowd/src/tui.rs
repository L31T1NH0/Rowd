use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Span,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
    Frame, Terminal,
};
use rowd_app::{App, AppSnapshot, ConnectionTest, DeviceConfig, PairingInfo, RecoveryItem, Status};
use rowd_core::trace;
use rowd_core::{
    config::{RemapPolicy, ShareRequest, SyncMode},
    pairing,
};
use std::{
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const ACCENT: Color = Color::Rgb(75, 207, 210);
const ACCENT_DARK: Color = Color::Rgb(24, 67, 73);
const MUTED: Color = Color::Rgb(148, 163, 170);
const SUCCESS: Color = Color::Rgb(92, 205, 138);
const WARNING: Color = Color::Rgb(239, 184, 85);

struct Screen;

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Shares,
    Device,
    Advanced,
}

impl Tab {
    const ALL: [Self; 3] = [Self::Shares, Self::Device, Self::Advanced];

    fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }

    fn title(self) -> &'static str {
        match self {
            Self::Shares => "Shares",
            Self::Device => "Dispositivo",
            Self::Advanced => "Avançado",
        }
    }

    fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    fn previous(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UiAction {
    AddShare,
    EditShare,
    ToggleShare,
    SyncShare,
    ReindexShare,
    RemapShare,
    EditIgnore,
    RemoveShare,
    AcceptRequest,
    RejectRequest,
    RestoreRecovery,
    KeepRecovery,
    ExportRecovery,
    Pair,
    TestConnection,
    ToggleTrace,
    ToggleGlobalPause,
    Unlink,
    ExportProfile,
    ImportProfile,
    ExportBackup,
    ImportBackup,
    ExportDiagnostic,
    ResetInitial,
    ResetAll,
}

impl UiAction {
    fn mutates(self) -> bool {
        !matches!(self, Self::TestConnection | Self::ToggleTrace)
    }
}

#[derive(Clone, Copy)]
struct BindingDef {
    action: UiAction,
    tab: Tab,
    label: &'static str,
    default: &'static str,
}

const BINDINGS: &[BindingDef] = &[
    BindingDef {
        action: UiAction::AddShare,
        tab: Tab::Shares,
        label: "Adicionar Share",
        default: "a",
    },
    BindingDef {
        action: UiAction::EditShare,
        tab: Tab::Shares,
        label: "Editar",
        default: "e",
    },
    BindingDef {
        action: UiAction::ToggleShare,
        tab: Tab::Shares,
        label: "Pausar / Retomar",
        default: "space",
    },
    BindingDef {
        action: UiAction::SyncShare,
        tab: Tab::Shares,
        label: "Sincronizar agora",
        default: "s",
    },
    BindingDef {
        action: UiAction::ReindexShare,
        tab: Tab::Shares,
        label: "Reparar Share",
        default: "x",
    },
    BindingDef {
        action: UiAction::RemapShare,
        tab: Tab::Shares,
        label: "Trocar pasta no celular",
        default: "m",
    },
    BindingDef {
        action: UiAction::EditIgnore,
        tab: Tab::Shares,
        label: "Arquivos ignorados",
        default: "i",
    },
    BindingDef {
        action: UiAction::RemoveShare,
        tab: Tab::Shares,
        label: "Remover Share",
        default: "d",
    },
    BindingDef {
        action: UiAction::AcceptRequest,
        tab: Tab::Shares,
        label: "Aceitar",
        default: "enter",
    },
    BindingDef {
        action: UiAction::RejectRequest,
        tab: Tab::Shares,
        label: "Recusar",
        default: "r",
    },
    BindingDef {
        action: UiAction::RestoreRecovery,
        tab: Tab::Shares,
        label: "Usar versão anterior",
        default: "v",
    },
    BindingDef {
        action: UiAction::KeepRecovery,
        tab: Tab::Shares,
        label: "Manter versão atual",
        default: "k",
    },
    BindingDef {
        action: UiAction::ExportRecovery,
        tab: Tab::Shares,
        label: "Exportar versão",
        default: "E",
    },
    BindingDef {
        action: UiAction::Pair,
        tab: Tab::Device,
        label: "Conectar celular",
        default: "p",
    },
    BindingDef {
        action: UiAction::TestConnection,
        tab: Tab::Advanced,
        label: "Testar conexão",
        default: "T",
    },
    BindingDef {
        action: UiAction::ToggleTrace,
        tab: Tab::Advanced,
        label: "Trace de desempenho",
        default: "t",
    },
    BindingDef {
        action: UiAction::ToggleGlobalPause,
        tab: Tab::Device,
        label: "Pausar tudo / Retomar tudo",
        default: "space",
    },
    BindingDef {
        action: UiAction::Unlink,
        tab: Tab::Device,
        label: "Desvincular celular",
        default: "u",
    },
    BindingDef {
        action: UiAction::ExportProfile,
        tab: Tab::Advanced,
        label: "Exportar configuração",
        default: "x",
    },
    BindingDef {
        action: UiAction::ImportProfile,
        tab: Tab::Advanced,
        label: "Importar configuração",
        default: "i",
    },
    BindingDef {
        action: UiAction::ExportBackup,
        tab: Tab::Advanced,
        label: "Exportar backup completo",
        default: "b",
    },
    BindingDef {
        action: UiAction::ImportBackup,
        tab: Tab::Advanced,
        label: "Restaurar backup completo",
        default: "n",
    },
    BindingDef {
        action: UiAction::ExportDiagnostic,
        tab: Tab::Advanced,
        label: "Exportar diagnóstico",
        default: "d",
    },
    BindingDef {
        action: UiAction::ResetInitial,
        tab: Tab::Advanced,
        label: "Redefinir Rowd",
        default: "f",
    },
    BindingDef {
        action: UiAction::ResetAll,
        tab: Tab::Advanced,
        label: "Apagar dados internos",
        default: "X",
    },
];

const ADVANCED_ACTIONS: [UiAction; 9] = [
    UiAction::ExportProfile,
    UiAction::ImportProfile,
    UiAction::ExportBackup,
    UiAction::ImportBackup,
    UiAction::TestConnection,
    UiAction::ExportDiagnostic,
    UiAction::ToggleTrace,
    UiAction::ResetInitial,
    UiAction::ResetAll,
];

fn binding(action: UiAction) -> &'static BindingDef {
    BINDINGS
        .iter()
        .find(|item| item.action == action)
        .expect("ação cadastrada")
}

struct KeyMap;

impl KeyMap {
    fn resolve(key: KeyEvent, tab: Tab) -> Option<KeyCommand> {
        let fixed = match key.code {
            KeyCode::Char('q') => Some(KeyCommand::Quit),
            KeyCode::Char('?') => Some(KeyCommand::Help),
            KeyCode::Char('1'..='3') if tab == Tab::Shares => {
                let KeyCode::Char(digit) = key.code else {
                    unreachable!()
                };
                Some(KeyCommand::ShareSection(digit as usize - '1' as usize))
            }
            KeyCode::Tab => Some(KeyCommand::NextTab),
            KeyCode::BackTab => Some(KeyCommand::PreviousTab),
            KeyCode::Up => Some(KeyCommand::MoveUp),
            KeyCode::Down => Some(KeyCommand::MoveDown),
            KeyCode::Left | KeyCode::Right if tab == Tab::Shares => {
                Some(KeyCommand::MoveRecovery(key.code == KeyCode::Right))
            }
            KeyCode::Enter if tab == Tab::Advanced => Some(KeyCommand::AdvancedAction),
            KeyCode::Esc => Some(KeyCommand::Close),
            _ => None,
        };
        fixed.or_else(|| {
            BINDINGS
                .iter()
                .filter(|binding| binding.tab == tab)
                .find(|binding| key_matches(key.code, binding.default))
                .map(|binding| KeyCommand::Action(binding.action))
        })
    }
}

fn key_matches(code: KeyCode, configured: &str) -> bool {
    match configured {
        "space" => code == KeyCode::Char(' '),
        "enter" => code == KeyCode::Enter,
        value => {
            let mut chars = value.chars();
            matches!((chars.next(), chars.next(), code), (Some(expected), None, KeyCode::Char(actual)) if expected == actual)
        }
    }
}

#[derive(Clone, Copy)]
enum KeyCommand {
    Quit,
    Help,
    NextTab,
    PreviousTab,
    MoveUp,
    MoveDown,
    MoveRecovery(bool),
    ShareSection(usize),
    AdvancedAction,
    Close,
    Action(UiAction),
}

enum Submit {
    AddShare,
    EditShare(String),
    RemoveShare(String),
    AcceptRequest(String),
    RejectRequest(String),
    ReindexShare(String),
    RemapShare(String),
    Ignore(String),
    RestoreRecovery(String, String),
    KeepRecovery(String, String),
    ExportRecovery(String, String),
    Unlink,
    ExportProfile,
    ImportProfile,
    ExportBackupPath,
    ExportBackupPassphrase(PathBuf),
    ImportBackupPath,
    ImportBackupPassphrase(PathBuf),
    ExportDiagnostic,
    ResetInitial,
    ResetAll,
}

struct InputDialog {
    title: String,
    hint: String,
    value: String,
    submit: Submit,
    secret: bool,
    share: Option<ShareDraft>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShareStep {
    Name,
    Folder,
    Direction,
}

struct ShareDraft {
    step: ShareStep,
    name: String,
    folder: String,
    direction: usize,
    error: Option<String>,
}

const SHARE_DIRECTIONS: [(&str, SyncMode); 3] = [
    ("Computador ↔ celular", SyncMode::Bidirectional),
    ("Computador → celular", SyncMode::ToAndroid),
    ("Celular → computador", SyncMode::ToPc),
];

enum Modal {
    Help,
    Qr(PairingInfo),
    PairMenu(usize),
    PairDiscovery(Vec<pairing::Peer>, usize),
    PairApproval(rowd_app::PendingPairRequest),
    Input(InputDialog),
}

enum UiEvent {
    Notice(String),
    JobDone(String),
    PairPeers(Vec<pairing::Peer>),
}

struct Ui {
    tab: Tab,
    shares_index: usize,
    share_section: usize,
    recovery_index: usize,
    advanced_index: usize,
    snapshot: AppSnapshot,
    modal: Option<Modal>,
    notice: String,
    connection: Option<ConnectionTest>,
    job: Option<String>,
    dirty: bool,
    last_refresh: Instant,
    last_pair_search: Instant,
}

impl Ui {
    fn new(app: &App) -> Self {
        let mut ui = Self {
            tab: Tab::Shares,
            shares_index: 0,
            share_section: 0,
            recovery_index: 0,
            advanced_index: 0,
            snapshot: AppSnapshot::default(),
            modal: None,
            notice: "Pronto. Use ? para consultar todas as ações.".into(),
            connection: None,
            job: None,
            dirty: true,
            last_refresh: Instant::now() - Duration::from_secs(2),
            last_pair_search: Instant::now() - Duration::from_secs(2),
        };
        ui.refresh(app);
        if !ui.snapshot.device.paired {
            ui.tab = Tab::Device;
        }
        ui
    }

    fn refresh(&mut self, app: &App) {
        if !self.dirty && self.last_refresh.elapsed() < Duration::from_secs(1) {
            return;
        }
        if let Ok(snapshot) = app.snapshot() {
            self.snapshot = snapshot;
        }
        self.shares_index = clamp_index(
            self.shares_index,
            self.snapshot.requests.len() + self.snapshot.shares.len(),
        );
        self.recovery_index = clamp_index(self.recovery_index, self.recovery_len());
        self.last_refresh = Instant::now();
        self.dirty = false;
    }

    fn selected_share(&self) -> Option<&Status> {
        self.snapshot.shares.get(
            self.shares_index
                .checked_sub(self.snapshot.requests.len())?,
        )
    }

    fn selected_request(&self) -> Option<&ShareRequest> {
        self.snapshot.requests.get(self.shares_index)
    }

    fn selected_recovery(&self) -> Option<&RecoveryItem> {
        let share_id = &self.selected_share()?.share.share_id;
        self.snapshot
            .recovery
            .iter()
            .filter(|item| &item.share_id == share_id && !item.finished)
            .nth(self.recovery_index)
    }

    fn recovery_len(&self) -> usize {
        let Some(share) = self.selected_share() else {
            return 0;
        };
        self.snapshot
            .recovery
            .iter()
            .filter(|item| item.share_id == share.share.share_id && !item.finished)
            .count()
    }

    fn move_selection(&mut self, down: bool) {
        let (index, len) = match self.tab {
            Tab::Shares => (
                &mut self.shares_index,
                self.snapshot.requests.len() + self.snapshot.shares.len(),
            ),
            Tab::Device => return,
            Tab::Advanced => (&mut self.advanced_index, ADVANCED_ACTIONS.len()),
        };
        *index = if down {
            (*index + 1).min(len.saturating_sub(1))
        } else {
            index.saturating_sub(1)
        };
        if self.tab == Tab::Shares {
            self.recovery_index = 0;
        }
    }

    fn open_input(
        &mut self,
        title: impl Into<String>,
        hint: impl Into<String>,
        value: impl Into<String>,
        submit: Submit,
    ) {
        self.modal = Some(Modal::Input(InputDialog {
            title: title.into(),
            hint: hint.into(),
            value: value.into(),
            submit,
            secret: false,
            share: None,
        }));
    }

    fn open_secret(&mut self, title: impl Into<String>, hint: impl Into<String>, submit: Submit) {
        self.modal = Some(Modal::Input(InputDialog {
            title: title.into(),
            hint: hint.into(),
            value: String::new(),
            submit,
            secret: true,
            share: None,
        }));
    }

    fn handle_key(&mut self, key: KeyEvent, app: &App, tx: &Sender<UiEvent>) -> Result<bool> {
        if self.modal.is_some() {
            return self.handle_modal_key(key, app, tx).map(|()| false);
        }
        let Some(command) = KeyMap::resolve(key, self.tab) else {
            return Ok(false);
        };
        let blocked_by_job = match command {
            KeyCommand::Action(action) => action.mutates(),
            KeyCommand::AdvancedAction => ADVANCED_ACTIONS[self.advanced_index].mutates(),
            _ => false,
        };
        if self.job.is_some() && blocked_by_job {
            self.notice =
                "Aguarde a operação em andamento; navegação e ajuda continuam disponíveis.".into();
            return Ok(false);
        }
        match command {
            KeyCommand::Quit => return Ok(true),
            KeyCommand::Help => self.modal = Some(Modal::Help),
            KeyCommand::NextTab => self.tab = self.tab.next(),
            KeyCommand::PreviousTab => self.tab = self.tab.previous(),
            KeyCommand::MoveUp => self.move_selection(false),
            KeyCommand::MoveDown => self.move_selection(true),
            KeyCommand::MoveRecovery(down) => {
                self.recovery_index = if down {
                    (self.recovery_index + 1).min(self.recovery_len().saturating_sub(1))
                } else {
                    self.recovery_index.saturating_sub(1)
                };
            }
            KeyCommand::ShareSection(index) => self.share_section = index,
            KeyCommand::AdvancedAction => {
                self.begin_action(ADVANCED_ACTIONS[self.advanced_index], app)?
            }
            KeyCommand::Close => {}
            KeyCommand::Action(action) => {
                if self.action_available(action) {
                    if self.tab == Tab::Advanced {
                        self.advanced_index = ADVANCED_ACTIONS
                            .iter()
                            .position(|item| *item == action)
                            .unwrap_or(self.advanced_index);
                    }
                    self.begin_action(action, app)?;
                }
            }
        }
        Ok(false)
    }

    fn action_available(&self, action: UiAction) -> bool {
        match self.tab {
            Tab::Shares => match action {
                UiAction::AddShare => true,
                UiAction::AcceptRequest | UiAction::RejectRequest => {
                    self.selected_request().is_some()
                }
                UiAction::RestoreRecovery | UiAction::KeepRecovery | UiAction::ExportRecovery => {
                    self.selected_recovery().is_some()
                }
                UiAction::SyncShare | UiAction::ToggleShare => {
                    self.selected_share().is_some() && self.share_section == 0
                }
                UiAction::EditShare | UiAction::RemapShare | UiAction::EditIgnore => {
                    self.selected_share().is_some() && self.share_section == 1
                }
                UiAction::ReindexShare | UiAction::RemoveShare => {
                    self.selected_share().is_some() && self.share_section == 2
                }
                _ => false,
            },
            Tab::Device => match action {
                UiAction::Pair => !self.snapshot.device.paired,
                UiAction::Unlink => self.snapshot.device.paired,
                UiAction::ToggleGlobalPause => self.snapshot.device.configured,
                _ => false,
            },
            Tab::Advanced => ADVANCED_ACTIONS.contains(&action),
        }
    }

    fn handle_modal_key(&mut self, key: KeyEvent, app: &App, tx: &Sender<UiEvent>) -> Result<()> {
        if key.code == KeyCode::Esc {
            if let Some(Modal::PairApproval(request)) = &self.modal {
                app.decide_pairing(&request.request_id, false)?;
            }
            if matches!(
                self.modal,
                Some(
                    Modal::PairMenu(_)
                        | Modal::PairDiscovery(_, _)
                        | Modal::PairApproval(_)
                        | Modal::Qr(_)
                )
            ) {
                app.stop_pairing_mode();
            }
            self.modal = None;
            return Ok(());
        }
        match self.modal.as_mut() {
            Some(Modal::PairMenu(index)) => {
                match key.code {
                    KeyCode::Up => *index = index.saturating_sub(1),
                    KeyCode::Down => *index = (*index + 1).min(1),
                    KeyCode::Enter if *index == 0 => {
                        app.start_pairing_mode()?;
                        self.modal = Some(Modal::PairDiscovery(Vec::new(), 0));
                        self.last_pair_search = Instant::now() - Duration::from_secs(2);
                    }
                    KeyCode::Enter => self.modal = Some(Modal::Qr(app.pairing_info()?)),
                    _ => {}
                }
                return Ok(());
            }
            Some(Modal::PairDiscovery(peers, index)) => {
                match key.code {
                    KeyCode::Up => *index = index.saturating_sub(1),
                    KeyCode::Down => *index = (*index + 1).min(peers.len().saturating_sub(1)),
                    KeyCode::Enter => {
                        if let Some(peer) = peers.get(*index) {
                            app.offer_pairing(peer)?;
                            self.notice = format!(
                                "Convite enviado para {}. Confirme no celular.",
                                peer.device_name
                            );
                        }
                    }
                    _ => {}
                }
                return Ok(());
            }
            Some(Modal::PairApproval(request)) => {
                match key.code {
                    KeyCode::Enter => {
                        app.decide_pairing(&request.request_id, true)?;
                        self.modal = Some(Modal::PairDiscovery(Vec::new(), 0));
                    }
                    KeyCode::Char('r') => {
                        app.decide_pairing(&request.request_id, false)?;
                        self.modal = Some(Modal::PairDiscovery(Vec::new(), 0));
                    }
                    _ => {}
                }
                return Ok(());
            }
            _ => {}
        }
        if let Some(Modal::Input(input)) = self.modal.as_mut() {
            if input.share.is_some() {
                if let Some((name, folder, mode)) = handle_share_form_key(input, key, app) {
                    match app.add_share(name, folder, mode) {
                        Ok(_) => {
                            self.modal = None;
                            self.notice = "Share adicionado.".into();
                            self.dirty = true;
                        }
                        Err(error) => {
                            input.share.as_mut().unwrap().error =
                                Some(format!("Não foi possível criar o Share: {error:#}"));
                        }
                    }
                }
                return Ok(());
            }
        }
        match self.modal.as_mut() {
            Some(Modal::Input(input)) => match key.code {
                KeyCode::Backspace => {
                    input.value.pop();
                }
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    input.value.push('\n');
                }
                KeyCode::Enter => {
                    let Modal::Input(input) = self.modal.take().unwrap() else {
                        unreachable!()
                    };
                    if let Err(error) = self.submit(input.submit, input.value, app, tx) {
                        self.notice = format!("Ação não concluída: {error:#}");
                    }
                    self.dirty = true;
                }
                KeyCode::Char(character) => input.value.push(character),
                _ => {}
            },
            Some(
                Modal::Help
                | Modal::Qr(_)
                | Modal::PairMenu(_)
                | Modal::PairDiscovery(_, _)
                | Modal::PairApproval(_),
            )
            | None => {}
        }
        Ok(())
    }

    fn begin_action(&mut self, action: UiAction, app: &App) -> Result<()> {
        match action {
            UiAction::AddShare => self.modal = Some(Modal::Input(InputDialog {
                title: "Novo Share".into(),
                hint: String::new(),
                value: String::new(),
                submit: Submit::AddShare,
                secret: false,
                share: Some(ShareDraft {
                    step: ShareStep::Name,
                    name: String::new(),
                    folder: String::new(),
                    direction: 0,
                    error: None,
                }),
            })),
            UiAction::EditShare => {
                if let Some(share) = self.selected_share().map(|status| status.share.clone()) {
                    self.open_input(
                        format!("Editar · {}", share.name),
                        "nome | pasta absoluta no computador | ambos/para_celular/para_computador",
                        format!(
                            "{} | {} | {}",
                            share.name,
                            share.root.display(),
                            mode_value(share.mode)
                        ),
                        Submit::EditShare(share.share_id.clone()),
                    );
                }
            }
            UiAction::ToggleShare => {
                if let Some(share) = self.selected_share().map(|status| status.share.clone()) {
                    app.set_share_enabled(&share.share_id, !share.enabled)?;
                    self.notice = if share.enabled {
                        "Share pausado; arquivos e estado foram preservados.".into()
                    } else {
                        "Share retomado; a próxima rodada usará a configuração atual.".into()
                    };
                    self.dirty = true;
                }
            }
            UiAction::SyncShare => {
                if let Some(share) = self.selected_share().map(|status| status.share.clone()) {
                    app.request_share_sync(&share.share_id)?;
                    self.notice = format!("{} foi priorizado para a próxima conexão.", share.name);
                }
            }
            UiAction::ReindexShare => {
                if let Some(share) = self.selected_share().map(|status| status.share.clone()) {
                    self.open_input(
                        format!("Reparar Share · {}", share.name),
                        "Reconstrói o estado de sincronização sem apagar arquivos. Versões preservadas continuam disponíveis. Digite REINDEXAR.",
                        "",
                        Submit::ReindexShare(share.share_id),
                    );
                }
            }
            UiAction::RemapShare => {
                if let Some(share) = self.selected_share().map(|status| status.share.clone()) {
                    self.open_input(
                        format!("Trocar pasta no celular · {}", share.name),
                        "pc/celular/comparar; a nova pasta será escolhida no celular",
                        "comparar",
                        Submit::RemapShare(share.share_id),
                    );
                }
            }
            UiAction::EditIgnore => {
                if let Some(share) = self.selected_share().map(|status| status.share.clone()) {
                    let id = share.share_id;
                    let text = app.ignore_text(&id)?;
                    self.open_input(
                        format!("Arquivos ignorados · {}", share.name),
                        "Enter aplica; Shift+Enter cria linha. Regras inválidas são recusadas.",
                        text,
                        Submit::Ignore(id),
                    );
                }
            }
            UiAction::RemoveShare => {
                if let Some(share) = self.selected_share().map(|status| status.share.clone()) {
                    self.open_input(
                        format!("Remover · {}", share.name),
                        "Remove o Share do Rowd e preserva os arquivos locais. Digite REMOVER.",
                        "",
                        Submit::RemoveShare(share.share_id),
                    );
                }
            }
            UiAction::AcceptRequest => {
                if let Some(request) = self.selected_request().cloned() {
                    self.open_input(
                        format!("Aceitar · {}", request.name),
                        "Pasta absoluta no PC que receberá este Share.",
                        "",
                        Submit::AcceptRequest(request.request_id.clone()),
                    );
                }
            }
            UiAction::RejectRequest => {
                if let Some(request) = self.selected_request().cloned() {
                    self.open_input(
                        format!("Rejeitar · {}", request.name),
                        "A decisão será enviada ao celular. Digite REJEITAR.",
                        "",
                        Submit::RejectRequest(request.request_id.clone()),
                    );
                }
            }
            UiAction::RestoreRecovery => self.begin_recovery("RESTAURAR", Submit::RestoreRecovery),
            UiAction::KeepRecovery => self.begin_recovery("MANTER", Submit::KeepRecovery),
            UiAction::ExportRecovery => {
                if let Some(item) = self.selected_recovery().cloned() {
                    self.open_input(
                        format!("Exportar versão · {}", item.path),
                        "Destino novo para a versão recuperada.",
                        "",
                        Submit::ExportRecovery(item.share_id.clone(), item.id.clone()),
                    );
                }
            }
            UiAction::Pair => {
                app.pair("")?;
                self.modal = Some(Modal::PairMenu(0));
            },
            UiAction::TestConnection => {
                self.connection = Some(app.connection_test()?);
                self.notice =
                    "Teste concluído; detalhes atualizados em Avançado.".into();
            }
            UiAction::ToggleTrace => {
                if trace::enabled() {
                    trace::disable()?;
                    self.notice = "Trace de desempenho desligado.".into();
                } else {
                    std::fs::create_dir_all(app.home().join(".rowd"))?;
                    trace::enable(&app.home().join(".rowd/performance-trace-pc.jsonl"), "pc")?;
                    self.notice = "Trace de desempenho ligado.".into();
                }
            }
            UiAction::ToggleGlobalPause => {
                let paused = self.snapshot.device.sync_paused;
                app.set_sync_paused(!paused)?;
                self.notice = if paused {
                    "Sincronização global retomada.".into()
                } else {
                    "Sincronização global pausada sem apagar estado.".into()
                };
                self.dirty = true;
            }
            UiAction::Unlink => self.open_input(
                "Desvincular celular",
                "Revoga o celular imediatamente. Digite DESVINCULAR.",
                "",
                Submit::Unlink,
            ),
            UiAction::ExportProfile => self.open_input(
                "Exportar configuração",
                "Caminho de destino novo, por exemplo /tmp/rowd-profile.json",
                "",
                Submit::ExportProfile,
            ),
            UiAction::ImportProfile => self.open_input(
                "Importar configuração",
                "Caminho do perfil. A configuração atual terá backup automático.",
                "",
                Submit::ImportProfile,
            ),
            UiAction::ExportBackup => self.open_input(
                "Exportar backup completo criptografado",
                "Caminho de destino novo.",
                "",
                Submit::ExportBackupPath,
            ),
            UiAction::ImportBackup => self.open_input(
                "Restaurar backup completo",
                "Caminho do backup. A configuração atual será preservada antes da troca.",
                "",
                Submit::ImportBackupPath,
            ),
            UiAction::ExportDiagnostic => self.open_input(
                "Exportar diagnóstico sanitizado",
                "Caminho de destino novo; segredos e chave privada não serão incluídos.",
                "",
                Submit::ExportDiagnostic,
            ),
            UiAction::ResetInitial => self.open_input(
                "Redefinir Rowd",
                "Nova identidade de pareamento; remove celular e Shares ativos, arquiva o estado administrativo e preserva arquivos pessoais. Digite INICIAL.",
                "",
                Submit::ResetInitial,
            ),
            UiAction::ResetAll => self.open_input(
                "Apagar dados internos do Rowd",
                "Arquiva .rowd; Rowd inicia sem configuração ativa. Arquivos pessoais nas pastas sincronizadas são preservados. Digite APAGAR TUDO.",
                "",
                Submit::ResetAll,
            ),
        }
        Ok(())
    }

    fn begin_recovery(&mut self, token: &'static str, constructor: fn(String, String) -> Submit) {
        if let Some(item) = self.selected_recovery().cloned() {
            self.open_input(
                format!("Versão preservada · {}", item.path),
                format!("Digite {token} para confirmar."),
                "",
                constructor(item.share_id.clone(), item.id.clone()),
            );
        }
    }

    fn submit(
        &mut self,
        submit: Submit,
        value: String,
        app: &App,
        tx: &Sender<UiEvent>,
    ) -> Result<()> {
        match submit {
            Submit::AddShare => unreachable!("novo Share usa o formulário em etapas"),
            Submit::EditShare(id) => {
                let (name, root, mode) = share_form(&value)?;
                app.edit_share(&id, name, root, mode)?;
                self.notice =
                    "Share atualizado; a próxima rodada usará a nova configuração.".into();
            }
            Submit::RemoveShare(id) => {
                require_token(&value, "REMOVER")?;
                app.remove_share(&id)?;
                self.notice = "Share removido; arquivos locais foram preservados.".into();
            }
            Submit::AcceptRequest(id) => {
                anyhow::ensure!(!value.trim().is_empty(), "informe a pasta local");
                let folder = PathBuf::from(value.trim());
                let app = app.clone();
                start_job(self, tx, "Aceitando solicitação", move || {
                    app.accept_share_request(&id, &folder)?;
                    Ok("Solicitação aceita.".into())
                })?;
            }
            Submit::RejectRequest(id) => {
                require_token(&value, "REJEITAR")?;
                let app = app.clone();
                start_job(self, tx, "Rejeitando solicitação", move || {
                    app.reject_share_request(&id)?;
                    Ok(
                        "Solicitação rejeitada; o celular receberá a decisão na próxima conexão."
                            .into(),
                    )
                })?;
            }
            Submit::ReindexShare(id) => {
                require_token(&value, "REINDEXAR")?;
                let app = app.clone();
                start_job(self, tx, "Reindexando Share", move || {
                    app.reindex_share(&id)?;
                    Ok("Reindexação concluída; recovery foi preservado.".into())
                })?;
            }
            Submit::RemapShare(id) => {
                let policy = match value.trim() {
                    "pc" => RemapPolicy::Pc,
                    "celular" | "android" => RemapPolicy::Android,
                    "comparar" | "compare" => RemapPolicy::Compare,
                    _ => anyhow::bail!("política deve ser pc, celular ou comparar"),
                };
                let app = app.clone();
                start_job(self, tx, "Remapeando Share", move || {
                    app.remap_share(&id, policy)?;
                    Ok("Pasta anterior desvinculada; escolha a nova pasta no celular.".into())
                })?;
            }
            Submit::Ignore(id) => {
                let app = app.clone();
                start_job(self, tx, "Aplicando .rowdignore", move || {
                    app.set_ignore_text(&id, &value)?;
                    Ok(".rowdignore validado e aplicado; Share reindexado.".into())
                })?;
            }
            Submit::RestoreRecovery(share, id) => {
                require_token(&value, "RESTAURAR")?;
                let app = app.clone();
                start_job(self, tx, "Restaurando versão", move || {
                    app.restore_recovery(&share, &id)?;
                    Ok("Versão restaurada; a versão substituída foi preservada.".into())
                })?;
            }
            Submit::KeepRecovery(share, id) => {
                require_token(&value, "MANTER")?;
                let app = app.clone();
                start_job(self, tx, "Mantendo versão atual", move || {
                    app.keep_recovery(&share, &id)?;
                    Ok("Versão atual mantida e recovery marcado como resolvido.".into())
                })?;
            }
            Submit::ExportRecovery(share, id) => {
                anyhow::ensure!(!value.trim().is_empty(), "informe o destino");
                let output = PathBuf::from(value.trim());
                let app = app.clone();
                start_job(self, tx, "Exportando versão", move || {
                    app.export_recovery(&share, &id, &output)?;
                    Ok("Versão exportada.".into())
                })?;
            }
            Submit::Unlink => {
                require_token(&value, "DESVINCULAR")?;
                let app = app.clone();
                start_job(self, tx, "Desvinculando dispositivo", move || {
                    app.unlink_device()?;
                    Ok("Celular revogado; Shares, arquivos e recovery preservados.".into())
                })?;
            }
            Submit::ExportProfile => {
                app.export_profile(required_path(&value)?)?;
                self.notice = "Perfil sem segredos exportado.".into();
            }
            Submit::ImportProfile => {
                let input = required_path(&value)?.to_path_buf();
                let app = app.clone();
                start_job(self, tx, "Importando perfil", move || {
                    app.import_profile(&input)?;
                    Ok("Perfil importado.".into())
                })?;
            }
            Submit::ExportBackupPath => {
                let path = required_path(&value)?.to_path_buf();
                self.open_secret(
                    "Senha do backup",
                    "Mínimo de 8 caracteres. A senha não será armazenada.",
                    Submit::ExportBackupPassphrase(path),
                );
            }
            Submit::ExportBackupPassphrase(path) => {
                let app = app.clone();
                start_job(self, tx, "Exportando backup", move || {
                    app.export_backup(&path, &value)?;
                    Ok("Backup completo criptografado exportado com permissão privada.".into())
                })?;
            }
            Submit::ImportBackupPath => {
                let path = required_path(&value)?.to_path_buf();
                self.open_secret(
                    "Senha do backup",
                    "A autenticação do arquivo ocorre antes de qualquer alteração.",
                    Submit::ImportBackupPassphrase(path),
                );
            }
            Submit::ImportBackupPassphrase(path) => {
                let app = app.clone();
                start_job(self, tx, "Importando backup", move || {
                    app.import_backup(&path, &value)?;
                    Ok("Backup completo autenticado e importado.".into())
                })?;
            }
            Submit::ExportDiagnostic => {
                app.export_diagnostic(required_path(&value)?)?;
                self.notice = "Diagnóstico sanitizado exportado.".into();
            }
            Submit::ResetInitial => {
                require_token(&value, "INICIAL")?;
                let app = app.clone();
                start_job(self, tx, "Restaurando configuração inicial", move || {
                    app.reset_device_configuration()?;
                    Ok("Configuração inicial restaurada; estado anterior arquivado.".into())
                })?;
            }
            Submit::ResetAll => {
                require_token(&value, "APAGAR TUDO")?;
                let app = app.clone();
                start_job(self, tx, "Arquivando dados internos", move || {
                    app.archive_all_application_data()?;
                    Ok("Dados internos movidos para um arquivo recuperável.".into())
                })?;
            }
        }
        self.dirty = true;
        Ok(())
    }

    fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        frame.render_widget(Clear, area);
        let regions = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(3),
        ])
        .split(area);
        self.render_header(frame, regions[0]);
        self.render_tabs(frame, regions[1]);
        let work = self.job.as_deref().unwrap_or(&self.notice);
        frame.render_widget(
            Paragraph::new(truncate(work, regions[2].width as usize))
                .style(Style::default().fg(if self.job.is_some() { WARNING } else { MUTED })),
            regions[2],
        );
        self.render_body(frame, regions[3]);
        self.render_footer(frame, regions[4]);
        if let Some(modal) = &self.modal {
            render_modal(frame, area, modal);
        }
    }

    fn render_header(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(
            Paragraph::new(Span::styled(
                " ROWD ",
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            )),
            area,
        );
        let connected = self.cell_connected();
        let label = if connected {
            "● Celular conectado"
        } else {
            "○ Celular desconectado"
        };
        let width = label.chars().count() as u16;
        if area.width > width + 7 {
            frame.render_widget(
                Paragraph::new(label).style(Style::default().fg(if connected {
                    SUCCESS
                } else {
                    MUTED
                })),
                Rect::new(area.right() - width, area.y, width, 1),
            );
        }
    }

    fn cell_connected(&self) -> bool {
        self.snapshot.device.paired
            && self.snapshot.device.last_connection.is_some_and(|at| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    .saturating_sub(at)
                    < 60
            })
    }

    fn render_tabs(&self, frame: &mut Frame<'_>, area: Rect) {
        let titles = Tab::ALL
            .iter()
            .map(|tab| format!(" {} ", tab.title()))
            .collect::<Vec<_>>();
        frame.render_widget(
            Tabs::new(titles)
                .select(self.tab.index())
                .block(Block::default().borders(Borders::BOTTOM))
                .style(Style::default().fg(MUTED))
                .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
                .divider(" | "),
            area,
        );
    }

    fn render_body(&self, frame: &mut Frame<'_>, area: Rect) {
        let panels = body_panels(area);
        match self.tab {
            Tab::Shares => self.render_shares(frame, panels),
            Tab::Device => self.render_device(frame, panels),
            Tab::Advanced => self.render_advanced(frame, panels),
        }
    }

    fn render_shares(&self, frame: &mut Frame<'_>, panels: [Rect; 2]) {
        let request_count = self.snapshot.requests.len();
        let shares_area = if request_count > 0 {
            let height = (request_count as u16 + 2).min(panels[0].height.saturating_sub(3));
            let areas =
                Layout::vertical([Constraint::Length(height), Constraint::Min(3)]).split(panels[0]);
            let requests = self
                .snapshot
                .requests
                .iter()
                .map(|request| ListItem::new(format!("{}    Solicitação do celular", request.name)))
                .collect();
            render_list(
                frame,
                areas[0],
                "Solicitações",
                requests,
                (self.shares_index < request_count).then_some(self.shares_index),
            );
            areas[1]
        } else {
            panels[0]
        };
        let items = if self.snapshot.shares.is_empty() {
            vec![ListItem::new("Nenhum Share. Use a para adicionar.")
                .style(Style::default().fg(MUTED))]
        } else {
            self.snapshot
                .shares
                .iter()
                .map(|status| {
                    let pending = self
                        .snapshot
                        .recovery
                        .iter()
                        .any(|item| item.share_id == status.share.share_id && !item.finished);
                    let state = if pending {
                        "Ação necessária"
                    } else if !status.root_available
                        || status.error.is_some()
                        || status
                            .last_error
                            .as_ref()
                            .is_some_and(|e| e.resolved_at.is_none())
                    {
                        "Erro"
                    } else if !status.share.enabled {
                        "Pausado"
                    } else if status.share.remap_policy.is_some() {
                        "Ação necessária"
                    } else {
                        ""
                    };
                    ListItem::new(format!("{}    {}", status.share.name, state))
                })
                .collect()
        };
        render_list(
            frame,
            shares_area,
            "Shares · a Adicionar Share",
            items,
            self.shares_index.checked_sub(request_count),
        );
        if let Some(request) = self.selected_request() {
            render_detail(frame, panels[1], "Solicitação selecionada", format!(
                "Nome          {}\nDireção       {}\n\nO celular solicitou este Share. Ao aceitar, defina a pasta do computador.\n\nEnter Aceitar     r Recusar",
                request.name, mode_label(request.mode)));
        } else if let Some(status) = self.selected_share() {
            render_detail(
                frame,
                panels[1],
                "Share selecionado",
                share_detail(
                    status,
                    self.share_section,
                    self.selected_recovery(),
                    self.recovery_index,
                    self.recovery_len(),
                ),
            );
        } else {
            render_detail(
                frame,
                panels[1],
                "Share selecionado",
                "Use a para adicionar um Share ou aceite uma solicitação do celular.".into(),
            );
        }
    }

    fn render_device(&self, frame: &mut Frame<'_>, panels: [Rect; 2]) {
        let device = &self.snapshot.device;
        let status = if self.cell_connected() {
            "Conectado"
        } else {
            "Desconectado"
        };
        let details = format!(
            "CELULAR\n\nNome            {}\nStatus          {}\nÚltima conexão  {}\nEndereço        {}\nIdentidade      {}\nFingerprint     {}\n\nSincronização   {}",
            if device.paired { "Não informado" } else { "Nenhum celular vinculado" },
            status, timestamp_label(device.last_connection),
            if device.configured { device.address.as_str() } else { "não configurado" },
            if device.configured { short_id(&device.identity) } else { "—".into() },
            if device.configured { device.fingerprint.as_str() } else { "—" },
            if device.sync_paused { "Pausada" } else { "Ativa" });
        render_detail(frame, panels[0], "Dispositivo", details);
        let actions = format!(
            "{}{}",
            if device.paired {
                "u Desvincular celular"
            } else {
                "p Conectar celular"
            },
            if !device.configured {
                ""
            } else if device.sync_paused {
                "\n\nEspaço Retomar tudo"
            } else {
                "\n\nEspaço Pausar tudo"
            }
        );
        render_detail(frame, panels[1], "Ações do dispositivo", actions);
    }

    fn render_advanced(&self, frame: &mut Frame<'_>, panels: [Rect; 2]) {
        let mut items = Vec::new();
        for (index, action) in ADVANCED_ACTIONS.iter().enumerate() {
            if index == 0 {
                items.push(
                    ListItem::new("DADOS")
                        .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                );
            }
            if index == 4 {
                items.push(ListItem::new(""));
                items.push(
                    ListItem::new("DIAGNÓSTICO")
                        .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                );
            }
            if index == 7 {
                items.push(ListItem::new(""));
                items.push(
                    ListItem::new("SISTEMA")
                        .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                );
            }
            let entry = binding(*action);
            let state = if *action == UiAction::ToggleTrace {
                if trace::enabled() {
                    "    Ligado"
                } else {
                    "    Desligado"
                }
            } else {
                ""
            };
            items.push(ListItem::new(format!("{}{}", entry.label, state)));
        }
        let selected = self.advanced_index
            + if self.advanced_index < 4 {
                1
            } else if self.advanced_index < 7 {
                3
            } else {
                5
            };
        render_list(frame, panels[0], "Avançado", items, Some(selected));
        let action = ADVANCED_ACTIONS[self.advanced_index];
        let mut detail = format!(
            "{}\n\n{}\n\nEnter executar",
            binding(action).label,
            advanced_description(action)
        );
        if action == UiAction::TestConnection {
            if let Some(test) = &self.connection {
                detail.push_str("\n\nResultado do teste\n");
                for check in &test.checks {
                    detail.push_str(&format!(
                        "[{}] {}: {}\n",
                        if check.ok { "OK" } else { "FALHA" },
                        check.label,
                        check.detail
                    ));
                }
                detail.push_str(&format!(
                    "Shares disponíveis: {}/{}",
                    test.available_shares, test.total_shares
                ));
            }
        }
        render_detail(frame, panels[1], "Opção selecionada", detail);
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        let text = match self.tab {
            Tab::Shares => "a Adicionar Share  ·  ↑↓ selecionar  ·  1/2/3 seções",
            Tab::Device => "",
            Tab::Advanced => "↑↓ selecionar  ·  Enter executar",
        };
        let version = format!("v{}", env!("CARGO_PKG_VERSION"));
        let navigation = truncate(
            "Tab/Shift+Tab abas  ·  ? ajuda  ·  q sair",
            area.width.saturating_sub(version.len() as u16 + 1) as usize,
        );
        let footer = format!("{text}\n{navigation}");
        frame.render_widget(
            Paragraph::new(footer)
                .style(Style::default().fg(MUTED))
                .block(Block::default().borders(Borders::TOP)),
            area,
        );
        let width = version.len() as u16;
        if area.width > width {
            frame.render_widget(
                Paragraph::new(version).style(Style::default().fg(MUTED)),
                Rect::new(area.right() - width, area.bottom() - 1, width, 1),
            );
        }
    }
}

fn handle_share_form_key(
    input: &mut InputDialog,
    key: KeyEvent,
    app: &App,
) -> Option<(String, PathBuf, SyncMode)> {
    let draft = input.share.as_mut().expect("formulário de Share");
    match (draft.step, key.code) {
        (ShareStep::Name | ShareStep::Folder, KeyCode::Char(character)) => {
            input.value.push(character);
            draft.error = None;
        }
        (ShareStep::Name | ShareStep::Folder, KeyCode::Backspace) => {
            input.value.pop();
            draft.error = None;
        }
        (ShareStep::Name, KeyCode::Enter) => {
            let name = input.value.trim();
            if name.is_empty() || name.len() > 120 || name.chars().any(char::is_control) {
                draft.error = Some("Informe um nome válido e curto, sem quebras de linha.".into());
            } else {
                draft.name = name.into();
                draft.step = ShareStep::Folder;
                draft.error = None;
                input.value.clear();
            }
        }
        (ShareStep::Folder, KeyCode::Enter) => {
            let folder = input.value.trim();
            match validate_share_folder(app, &draft.name, folder) {
                Ok(()) => {
                    draft.folder = folder.into();
                    draft.step = ShareStep::Direction;
                    draft.error = None;
                    input.value.clear();
                }
                Err(error) => draft.error = Some(folder_error(&error)),
            }
        }
        (ShareStep::Direction, KeyCode::Up) => {
            draft.direction =
                (draft.direction + SHARE_DIRECTIONS.len() - 1) % SHARE_DIRECTIONS.len();
            draft.error = None;
        }
        (ShareStep::Direction, KeyCode::Down) => {
            draft.direction = (draft.direction + 1) % SHARE_DIRECTIONS.len();
            draft.error = None;
        }
        (ShareStep::Direction, KeyCode::Enter) => {
            return Some((
                draft.name.clone(),
                PathBuf::from(&draft.folder),
                SHARE_DIRECTIONS[draft.direction].1,
            ));
        }
        _ => {}
    }
    None
}

fn validate_share_folder(app: &App, name: &str, folder: &str) -> Result<()> {
    let path = Path::new(folder);
    anyhow::ensure!(
        path.is_absolute(),
        "informe o caminho absoluto da pasta no computador"
    );
    let mut config = DeviceConfig::load(app.home())?;
    config.add_share(
        app.home(),
        name.into(),
        path.to_path_buf(),
        SyncMode::Bidirectional,
    )?;
    Ok(())
}

fn folder_error(error: &anyhow::Error) -> String {
    let detail = format!("{error:#}");
    if detail.contains("Share directory does not exist") {
        "A pasta não existe ou não pode ser acessada.".into()
    } else if detail.contains("Share root must be a directory") {
        "O caminho precisa apontar para uma pasta.".into()
    } else if detail.contains("internal directory cannot be shared") {
        "A pasta interna .rowd não pode ser compartilhada.".into()
    } else if detail.contains("Share overlaps Rowd configuration") {
        "A pasta não pode conter a configuração do Rowd nem estar dentro dela.".into()
    } else if detail.contains("overlapping Share roots") {
        "A pasta se sobrepõe à de outro Share.".into()
    } else if detail.contains("too many Shares") {
        "O limite de 256 Shares foi atingido.".into()
    } else {
        format!("Pasta inválida: {detail}")
    }
}

fn start_job(
    ui: &mut Ui,
    tx: &Sender<UiEvent>,
    label: &str,
    job: impl FnOnce() -> Result<String> + Send + 'static,
) -> Result<()> {
    anyhow::ensure!(ui.job.is_none(), "aguarde a operação em andamento");
    ui.job = Some(label.into());
    let tx = tx.clone();
    std::thread::spawn(move || {
        let message = match job() {
            Ok(message) => message,
            Err(error) => format!("Ação não concluída: {error:#}"),
        };
        let _ = tx.send(UiEvent::JobDone(message));
    });
    Ok(())
}

pub fn run(home: &Path) -> Result<()> {
    anyhow::ensure!(
        io::stdin().is_terminal(),
        "A TUI requer terminal interativo; use --help para a CLI."
    );
    let app = App::new(home);
    let mut ui = Ui::new(&app);
    enable_raw_mode()?;
    let _screen = Screen;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let mut worker: Option<std::thread::JoinHandle<()>> = None;

    loop {
        if worker.as_ref().is_some_and(|worker| worker.is_finished()) {
            if let Some(worker) = worker.take() {
                let _ = worker.join();
            }
        }
        if worker.is_none() && ui.snapshot.device.configured {
            let worker_home = home.to_path_buf();
            let worker_stop = stop.clone();
            let worker_tx = tx.clone();
            worker = Some(std::thread::spawn(move || {
                if let Err(error) =
                    App::new(&worker_home).serve(None, false, worker_stop, |message| {
                        let _ = worker_tx.send(UiEvent::Notice(message));
                    })
                {
                    let _ = worker_tx.send(UiEvent::Notice(format!("Servidor: {error:#}")));
                }
            }));
        }
        for message in rx.try_iter() {
            match message {
                UiEvent::Notice(message) => ui.notice = message,
                UiEvent::PairPeers(peers) => {
                    if let Some(Modal::PairDiscovery(found, index)) = &mut ui.modal {
                        *found = peers;
                        *index = (*index).min(found.len().saturating_sub(1));
                    }
                }
                UiEvent::JobDone(message) => {
                    ui.job = None;
                    ui.notice = message;
                    ui.dirty = true;
                }
            }
        }
        ui.refresh(&app);
        if matches!(
            ui.modal,
            Some(Modal::PairMenu(_) | Modal::PairDiscovery(_, _))
        ) && !app.pairing_mode_active()
        {
            app.stop_pairing_mode();
            ui.modal = None;
            ui.notice =
                "A busca de pareamento expirou. Abra Conectar celular para tentar novamente."
                    .into();
        }
        if ui.snapshot.device.paired
            && matches!(
                ui.modal,
                Some(
                    Modal::PairMenu(_)
                        | Modal::PairDiscovery(_, _)
                        | Modal::PairApproval(_)
                        | Modal::Qr(_)
                )
            )
        {
            app.stop_pairing_mode();
            ui.modal = None;
            ui.notice = "Celular conectado.".into();
        }
        if let Some(request) = app.pending_pairing() {
            if !matches!(&ui.modal, Some(Modal::PairApproval(current)) if current.request_id == request.request_id)
            {
                ui.modal = Some(Modal::PairApproval(request));
            }
        }
        if matches!(ui.modal, Some(Modal::PairDiscovery(_, _)))
            && ui.last_pair_search.elapsed() >= Duration::from_secs(2)
        {
            ui.last_pair_search = Instant::now();
            let app = app.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                if let Ok(peers) = app.discover_phones() {
                    let _ = tx.send(UiEvent::PairPeers(peers));
                }
            });
        }
        terminal.draw(|frame| ui.render(frame))?;
        if !event::poll(Duration::from_millis(150))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if ui.handle_key(key, &app, &tx)? {
            break;
        }
    }

    stop.store(true, Ordering::Relaxed);
    if let Some(worker) = worker {
        let _ = worker.join();
    }
    trace::disable()?;
    Ok(())
}

fn body_panels(area: Rect) -> [Rect; 2] {
    let (direction, constraints) = if area.width >= 110 {
        (
            Direction::Horizontal,
            [Constraint::Percentage(38), Constraint::Percentage(62)],
        )
    } else if area.width >= 80 {
        (
            Direction::Horizontal,
            [Constraint::Percentage(44), Constraint::Percentage(56)],
        )
    } else {
        (
            Direction::Vertical,
            [Constraint::Percentage(43), Constraint::Percentage(57)],
        )
    };
    let regions = Layout::default()
        .direction(direction)
        .constraints(constraints)
        .split(area);
    [regions[0], regions[1]]
}

fn render_list(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    items: Vec<ListItem<'_>>,
    selected: Option<usize>,
) {
    let mut state = ListState::default();
    state.select(selected);
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(format!(" {title} "))
                    .borders(Borders::ALL),
            )
            .highlight_style(
                Style::default()
                    .bg(ACCENT_DARK)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("› "),
        area,
        &mut state,
    );
}

fn render_detail(frame: &mut Frame<'_>, area: Rect, title: &str, text: String) {
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(format!(" {title} "))
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn render_modal(frame: &mut Frame<'_>, area: Rect, modal: &Modal) {
    match modal {
        Modal::PairMenu(index) => {
            let popup = centered(area, 54, 10);
            frame.render_widget(Clear, popup);
            frame.render_widget(Paragraph::new(format!("Conectar celular\n\n{} Encontrar na rede\n{} Mostrar QR\n\n↑↓ seleciona · Enter abre · Esc fecha", if *index == 0 { ">" } else { " " }, if *index == 1 { ">" } else { " " }))
                .block(Block::default().title(" Pareamento ").borders(Borders::ALL).border_style(Style::default().fg(ACCENT))), popup);
        }
        Modal::PairDiscovery(peers, index) => {
            let popup = centered(area, 64, 16);
            frame.render_widget(Clear, popup);
            let mut lines = vec!["Celulares Rowd na rede".to_string(), String::new()];
            if peers.is_empty() {
                lines.push("Procurando celulares...".into());
            }
            for (i, peer) in peers.iter().enumerate() {
                lines.push(format!(
                    "{} {} · {}",
                    if i == *index { ">" } else { " " },
                    peer.device_name,
                    peer.endpoint.ip()
                ));
            }
            lines.push(String::new());
            lines.push("↑↓ seleciona · Enter envia convite · Esc fecha".into());
            frame.render_widget(
                Paragraph::new(lines.join("\n")).block(
                    Block::default()
                        .title(" Encontrar na rede ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(ACCENT)),
                ),
                popup,
            );
        }
        Modal::PairApproval(request) => {
            let popup = centered(area, 58, 11);
            frame.render_widget(Clear, popup);
            frame.render_widget(Paragraph::new(format!("{} quer se conectar\n\nCódigo: {}\n\nCompare com o celular.\n\nEnter Aceitar · r Recusar", request.device_name, request.verification_code))
                .block(Block::default().title(" Confirmar celular ").borders(Borders::ALL).border_style(Style::default().fg(WARNING))), popup);
        }
        Modal::Help => {
            let popup = centered(area, 92, 32);
            frame.render_widget(Clear, popup);
            let mut lines = vec![
                "Navegação global".to_string(),
                "Tab/Shift+Tab alterna abas · ↑↓ seleciona · Esc fecha · q sai".into(),
                String::new(),
                "SHARES".into(),
                "a Adicionar Share · 1 Geral · 2 Configuração · 3 Manutenção".into(),
                "Solicitação: Enter Aceitar · r Recusar".into(),
                "Versão pendente: ←/→ escolher · v Usar anterior · k Manter atual · E Exportar"
                    .into(),
                "Geral: s Sincronizar agora · Espaço Pausar/Retomar".into(),
                "Configuração: e Editar · m Trocar pasta no celular · i Arquivos ignorados".into(),
                "Manutenção: x Reparar Share · d Remover Share".into(),
                String::new(),
                "DISPOSITIVO".into(),
                "p Conectar celular · u Desvincular celular · Espaço Pausar/Retomar tudo".into(),
                String::new(),
                "AVANÇADO".into(),
                "↑↓ escolhe opção · Enter executa; atalhos existentes também funcionam".into(),
            ];
            for action in ADVANCED_ACTIONS {
                let entry = binding(action);
                lines.push(format!("{} {}", display_key(entry.default), entry.label));
            }
            frame.render_widget(
                Paragraph::new(lines.join("\n"))
                    .wrap(Wrap { trim: false })
                    .block(
                        Block::default()
                            .title(" Ajuda · Esc fecha ")
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(ACCENT)),
                    ),
                popup,
            );
        }
        Modal::Qr(info) => {
            let qr_width = info
                .qr
                .lines()
                .map(|line| line.chars().count())
                .max()
                .unwrap_or(0) as u16;
            let qr_height = info.qr.lines().count() as u16;
            let desired_width = qr_width.saturating_add(6).max(54);
            let desired_height = qr_height.saturating_add(9).max(16);
            let popup = centered(area, desired_width, desired_height);
            frame.render_widget(Clear, popup);
            let fits = popup.width >= qr_width.saturating_add(4)
                && popup.height >= qr_height.saturating_add(7);
            let body = if fits {
                format!(
                    "Leia no celular\n{}\nFingerprint SHA-256\n{}\nSVG privado: {}",
                    info.qr,
                    info.device.fingerprint,
                    info.qr_image.display()
                )
            } else {
                format!(
                    "O terminal está pequeno para este QR.\nAmplie para cerca de {} × {}.\n\nFingerprint SHA-256\n{}\n\nFallback SVG\n{}",
                    desired_width,
                    desired_height,
                    info.device.fingerprint,
                    info.qr_image.display()
                )
            };
            frame.render_widget(
                Paragraph::new(body).wrap(Wrap { trim: false }).block(
                    Block::default()
                        .title(" Pareamento · Esc fecha ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(ACCENT)),
                ),
                popup,
            );
        }
        Modal::Input(input) => {
            if let Some(draft) = &input.share {
                let popup = centered(area, 76, 14);
                frame.render_widget(Clear, popup);
                let error = draft.error.as_deref().unwrap_or("");
                let body = match draft.step {
                    ShareStep::Name => format!("Nome do Share\n\n> {}\n\nEnter avança · Esc cancela\n\n{error}", input.value),
                    ShareStep::Folder => format!("Nome: {}\n\nPasta no computador (caminho absoluto)\n\n> {}\n\nEnter avança · Esc cancela\n\n{error}", draft.name, input.value),
                    ShareStep::Direction => format!("Nome: {}\nPasta no computador: {}\n\nDireção\n\n\n\n\n\n{error}\n\n↑↓ seleciona · Enter cria · Esc cancela", draft.name, draft.folder),
                };
                frame.render_widget(
                    Paragraph::new(body).wrap(Wrap { trim: false }).block(
                        Block::default()
                            .title(format!(" {} ", input.title))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(WARNING)),
                    ),
                    popup,
                );
                if draft.step == ShareStep::Direction && popup.height >= 12 && popup.width >= 5 {
                    let options = SHARE_DIRECTIONS
                        .iter()
                        .map(|(label, _)| ListItem::new(*label))
                        .collect::<Vec<_>>();
                    let mut selected = ListState::default();
                    selected.select(Some(draft.direction));
                    frame.render_stateful_widget(
                        List::new(options)
                            .highlight_style(
                                Style::default()
                                    .bg(ACCENT_DARK)
                                    .fg(Color::White)
                                    .add_modifier(Modifier::BOLD),
                            )
                            .highlight_symbol("› "),
                        Rect::new(popup.x + 2, popup.y + 6, popup.width.saturating_sub(4), 3),
                        &mut selected,
                    );
                }
                return;
            }
            let height = if input.value.contains('\n') { 16 } else { 9 };
            let popup = centered(area, 76, height);
            frame.render_widget(Clear, popup);
            let shown = if input.secret {
                "•".repeat(input.value.chars().count())
            } else {
                input.value.clone()
            };
            let body = format!(
                "{}\n\n> {}\n\nEnter confirma · Esc cancela",
                input.hint, shown
            );
            frame.render_widget(
                Paragraph::new(body).wrap(Wrap { trim: false }).block(
                    Block::default()
                        .title(format!(" {} ", input.title))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(WARNING)),
                ),
                popup,
            );
        }
    }
}

fn centered(area: Rect, desired_width: u16, desired_height: u16) -> Rect {
    let width = desired_width.min(area.width.saturating_sub(2)).max(1);
    let height = desired_height.min(area.height.saturating_sub(2)).max(1);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn share_detail(
    status: &Status,
    section: usize,
    recovery: Option<&RecoveryItem>,
    recovery_index: usize,
    recovery_len: usize,
) -> String {
    let share = &status.share;
    let mut text = format!(
        "{}\n\n{}1 Geral   {}2 Configuração   {}3 Manutenção\n\n",
        share.name.to_uppercase(),
        if section == 0 { "› " } else { "" },
        if section == 1 { "› " } else { "" },
        if section == 2 { "› " } else { "" }
    );
    if let Some(item) = recovery {
        text.push_str(&format!("AÇÃO NECESSÁRIA  ({}/{})\n{}\n\nO Rowd preservou uma versão anterior deste arquivo e precisa saber qual versão deve permanecer.\n\nv Usar versão anterior   k Manter versão atual   E Exportar versão\n←/→ outro arquivo\n\n", recovery_index + 1, recovery_len, item.path));
    }
    match section {
        0 => {
            let state = if recovery.is_some() { "Ação necessária" } else if !status.root_available || status.error.is_some() || status.last_error.as_ref().is_some_and(|error| error.resolved_at.is_none()) { "Erro" } else if !share.enabled { "Pausado" } else if share.remap_policy.is_some() { "Ação necessária" } else if status.last_sync.is_none() { "Aguardando sincronização" } else { "Sincronizado" };
            text.push_str(&format!("Estado       {}\nDireção      {}\nComputador   {}\nCelular      {}\nÚltimo sync  {}\n\ns Sincronizar agora     Espaço {}",
                state, mode_label(share.mode), share.root.display(),
                if share.remap_policy.is_some() { "Aguardando escolha no celular" } else { "Escolhida no celular" },
                timestamp_label(status.last_sync), if share.enabled { "Pausar" } else { "Retomar" }));
            if let Some(error) = &status.last_error {
                if error.resolved_at.is_none() { text.push_str(&format!("\n\nÚltimo erro  {}", error.message)); }
            }
        }
        1 => text.push_str(&format!("Nome         {}\nDireção      {}\nComputador   {}\nCelular      {}\n\ne Editar   m Trocar pasta no celular   i Arquivos ignorados",
            share.name, mode_label(share.mode), share.root.display(),
            if share.remap_policy.is_some() { "Aguardando escolha no celular" } else { "Escolhida no celular" })),
        _ => text.push_str("x Reparar Share\nReconstrói o estado de sincronização deste Share sem apagar seus arquivos. Versões preservadas continuam disponíveis.\n\nd Remover Share\nRemove o Share do Rowd e preserva os arquivos locais."),
    }
    text
}

fn advanced_description(action: UiAction) -> &'static str {
    match action {
        UiAction::ExportProfile => "Salva Shares, nomes, caminhos, direções, estado habilitado ou pausado, regras de arquivos ignorados e demais configurações públicas do perfil. Não inclui credenciais internas: não é um backup completo.",
        UiAction::ImportProfile => "Restaura uma configuração anteriormente exportada. A configuração atual recebe um backup automático.",
        UiAction::ExportBackup => "Cria um backup administrativo completo e criptografado do estado interno do Rowd. Não inclui os arquivos pessoais sincronizados.",
        UiAction::ImportBackup => "Restaura um backup administrativo completo e criptografado. A configuração atual é preservada antes da troca.",
        UiAction::TestConnection => "Testa endereço, TCP, TLS, autenticação, reconhecimento do celular e Shares disponíveis. O resultado aparece aqui após o teste.",
        UiAction::ExportDiagnostic => "Gera um relatório sanitizado para investigar problemas, sem exportar credenciais privadas.",
        UiAction::ToggleTrace => "Liga ou desliga o registro de desempenho da sincronização.",
        UiAction::ResetInitial => "Gera nova identidade de pareamento, remove o celular vinculado e os Shares da configuração ativa e arquiva o estado administrativo anterior. Preserva os arquivos pessoais nas pastas.",
        UiAction::ResetAll => "Arquiva os dados internos atuais (.rowd) e inicia sem configuração ativa. Não apaga os arquivos pessoais contidos nas pastas sincronizadas.",
        _ => "",
    }
}

fn share_form(value: &str) -> Result<(String, PathBuf, SyncMode)> {
    let parts = split_fields(value, 3, "nome | pasta | modo")?;
    anyhow::ensure!(!parts[0].is_empty(), "informe o nome");
    anyhow::ensure!(!parts[1].is_empty(), "informe a pasta");
    Ok((
        parts[0].into(),
        PathBuf::from(parts[1]),
        match parts[2] {
            "ambos" => SyncMode::Bidirectional,
            "para_celular" => SyncMode::ToAndroid,
            "para_computador" => SyncMode::ToPc,
            other => crate::parse_mode(other)?,
        },
    ))
}

fn split_fields<'a>(value: &'a str, expected: usize, format: &str) -> Result<Vec<&'a str>> {
    let parts = value.split('|').map(str::trim).collect::<Vec<_>>();
    anyhow::ensure!(parts.len() == expected, "use {format}");
    Ok(parts)
}

fn require_token(value: &str, expected: &str) -> Result<()> {
    anyhow::ensure!(
        value.trim() == expected,
        "confirmação não corresponde a {expected}"
    );
    Ok(())
}

fn required_path(value: &str) -> Result<&Path> {
    anyhow::ensure!(!value.trim().is_empty(), "informe o caminho");
    Ok(Path::new(value.trim()))
}

fn clamp_index(index: usize, len: usize) -> usize {
    index.min(len.saturating_sub(1))
}

fn mode_label(mode: SyncMode) -> &'static str {
    match mode {
        SyncMode::Bidirectional => "Computador ↔ celular",
        SyncMode::ToAndroid => "Computador → celular",
        SyncMode::ToPc => "Celular → computador",
    }
}

fn mode_value(mode: SyncMode) -> &'static str {
    match mode {
        SyncMode::Bidirectional => "ambos",
        SyncMode::ToAndroid => "para_celular",
        SyncMode::ToPc => "para_computador",
    }
}

fn timestamp_label(timestamp: Option<u64>) -> String {
    let Some(timestamp) = timestamp else {
        return "ainda não".into();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let elapsed = now.saturating_sub(timestamp);
    if elapsed < 60 {
        format!("há {elapsed}s")
    } else if elapsed < 3600 {
        format!("há {}min", elapsed / 60)
    } else if elapsed < 86_400 {
        format!("há {}h", elapsed / 3600)
    } else {
        format!("há {}d", elapsed / 86_400)
    }
}

fn short_id(value: &str) -> String {
    let characters = value.chars().collect::<Vec<_>>();
    if characters.len() <= 18 {
        value.into()
    } else {
        format!(
            "{}…{}",
            characters[..10].iter().collect::<String>(),
            characters[characters.len() - 6..]
                .iter()
                .collect::<String>()
        )
    }
}

fn truncate(value: &str, width: usize) -> String {
    if width < 4 {
        return String::new();
    }
    if value.chars().count() <= width {
        value.into()
    } else {
        value.chars().take(width - 1).collect::<String>() + "…"
    }
}

fn display_key(value: &str) -> String {
    match value {
        "space" => "Espaço".into(),
        "enter" => "Enter".into(),
        value => value.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn share_fixture() -> (TempDir, App, Ui, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let folder = temp.path().join("Fotos");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&folder).unwrap();
        let app = App::new(home);
        app.pair("127.0.0.1:43821").unwrap();
        let mut ui = Ui::new(&app);
        ui.begin_action(UiAction::AddShare, &app).unwrap();
        (temp, app, ui, folder)
    }

    fn share_input(ui: &mut Ui) -> &mut InputDialog {
        let Some(Modal::Input(input)) = &mut ui.modal else {
            panic!("formulário ausente")
        };
        input
    }

    fn press(ui: &mut Ui, app: &App, code: KeyCode) {
        let (tx, _) = mpsc::channel();
        ui.handle_modal_key(KeyEvent::new(code, KeyModifiers::NONE), app, &tx)
            .unwrap();
    }

    fn reach_direction(ui: &mut Ui, app: &App, folder: &Path) {
        share_input(ui).value = "Fotos".into();
        press(ui, app, KeyCode::Enter);
        share_input(ui).value = folder.display().to_string();
        press(ui, app, KeyCode::Enter);
        assert_eq!(
            share_input(ui).share.as_ref().unwrap().step,
            ShareStep::Direction
        );
    }

    #[test]
    fn layout_changes_at_the_two_breakpoints() {
        let wide = body_panels(Rect::new(0, 0, 120, 30));
        let medium = body_panels(Rect::new(0, 0, 90, 30));
        let narrow = body_panels(Rect::new(0, 0, 70, 30));
        assert_eq!(wide[0].y, wide[1].y);
        assert_eq!(medium[0].y, medium[1].y);
        assert!(narrow[1].y > narrow[0].y);
    }

    #[test]
    fn unpaired_device_starts_on_pairing_menu_and_keeps_qr() {
        let directory = tempfile::tempdir().unwrap();
        let app = App::new(directory.path());
        let mut ui = Ui::new(&app);
        assert_eq!(ui.tab, Tab::Device);
        ui.begin_action(UiAction::Pair, &app).unwrap();
        assert!(matches!(ui.modal, Some(Modal::PairMenu(0))));
        assert!(!app.pairing_mode_active());
        press(&mut ui, &app, KeyCode::Enter);
        assert!(matches!(ui.modal, Some(Modal::PairDiscovery(_, _))));
        assert!(app.pairing_mode_active());
        press(&mut ui, &app, KeyCode::Esc);
        assert!(!app.pairing_mode_active());
        ui.begin_action(UiAction::Pair, &app).unwrap();
        press(&mut ui, &app, KeyCode::Down);
        press(&mut ui, &app, KeyCode::Enter);
        assert!(matches!(ui.modal, Some(Modal::Qr(_))));
        assert!(!app.pairing_mode_active());
        press(&mut ui, &app, KeyCode::Esc);
    }

    #[test]
    fn keymap_uses_fixed_contextual_bindings() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(matches!(
            KeyMap::resolve(key, Tab::Shares),
            Some(KeyCommand::Action(UiAction::AddShare))
        ));
        assert!(KeyMap::resolve(key, Tab::Device).is_none());
    }

    #[test]
    fn tabs_and_share_sections_have_separate_navigation() {
        assert_eq!(Tab::ALL, [Tab::Shares, Tab::Device, Tab::Advanced]);
        assert_eq!(Tab::Advanced.next(), Tab::Shares);
        assert!(matches!(
            KeyMap::resolve(
                KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE),
                Tab::Shares
            ),
            Some(KeyCommand::ShareSection(1))
        ));
        assert!(KeyMap::resolve(
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE),
            Tab::Device
        )
        .is_none());
        assert!(KeyMap::resolve(
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE),
            Tab::Advanced
        )
        .is_none());
    }

    #[test]
    fn share_form_accepts_cellular_direction() {
        let (_, _, mode) = share_form("Fotos | /tmp/fotos | para_celular").unwrap();
        assert_eq!(mode, SyncMode::ToAndroid);
    }

    #[test]
    fn add_share_creates_each_direction() {
        for (steps, mode) in [
            (0, SyncMode::Bidirectional),
            (1, SyncMode::ToAndroid),
            (2, SyncMode::ToPc),
        ] {
            let (_temp, app, mut ui, folder) = share_fixture();
            reach_direction(&mut ui, &app, &folder);
            for _ in 0..steps {
                press(&mut ui, &app, KeyCode::Down);
            }
            press(&mut ui, &app, KeyCode::Enter);
            assert!(ui.modal.is_none());
            let shares = app.snapshot().unwrap().shares;
            assert_eq!(shares.len(), 1);
            assert_eq!(shares[0].share.name, "Fotos");
            assert_eq!(shares[0].share.mode, mode);
            assert_eq!(shares[0].share.root, folder);
        }
    }

    #[test]
    fn add_share_keeps_name_and_folder_on_validation_errors() {
        let (_temp, app, mut ui, folder) = share_fixture();
        press(&mut ui, &app, KeyCode::Enter);
        assert_eq!(
            share_input(&mut ui).share.as_ref().unwrap().step,
            ShareStep::Name
        );
        assert!(share_input(&mut ui).share.as_ref().unwrap().error.is_some());
        share_input(&mut ui).value = "Fotos".into();
        press(&mut ui, &app, KeyCode::Enter);
        let invalid = folder.join("inexistente").display().to_string();
        share_input(&mut ui).value = invalid.clone();
        press(&mut ui, &app, KeyCode::Enter);
        let input = share_input(&mut ui);
        assert_eq!(input.share.as_ref().unwrap().step, ShareStep::Folder);
        assert_eq!(input.share.as_ref().unwrap().name, "Fotos");
        assert_eq!(input.value, invalid);
        assert!(input.share.as_ref().unwrap().error.is_some());
        assert!(app.snapshot().unwrap().shares.is_empty());
        share_input(&mut ui).value = folder.display().to_string();
        press(&mut ui, &app, KeyCode::Enter);
        press(&mut ui, &app, KeyCode::Enter);
        assert_eq!(app.snapshot().unwrap().shares[0].share.name, "Fotos");
    }

    #[test]
    fn add_share_direction_arrows_and_escape() {
        let (_temp, app, mut ui, folder) = share_fixture();
        reach_direction(&mut ui, &app, &folder);
        press(&mut ui, &app, KeyCode::Down);
        press(&mut ui, &app, KeyCode::Down);
        assert_eq!(share_input(&mut ui).share.as_ref().unwrap().direction, 2);
        press(&mut ui, &app, KeyCode::Up);
        assert_eq!(share_input(&mut ui).share.as_ref().unwrap().direction, 1);
        press(&mut ui, &app, KeyCode::Char('3'));
        assert_eq!(share_input(&mut ui).share.as_ref().unwrap().direction, 1);
        press(&mut ui, &app, KeyCode::Esc);
        assert!(ui.modal.is_none());
        assert!(app.snapshot().unwrap().shares.is_empty());
    }
}
