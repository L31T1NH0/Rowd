// THESIS: Rowd is a route desk: every Share has a visible route, state, and next action.
// OWN-WORLD: charcoal operations console, cyan route markers, amber maintenance signals.
// STORY: global health -> category -> selected route -> safe contextual action.
// FIRST VIEWPORT: device trust and sync health stay visible above five focused work areas.
// FORM: responsive master/detail desk; concept candidate 3, seed a0e162b2.
// FINISH: keyboard, empty/error/loading states, compact QR, and narrow layouts ship together.

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
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
    Frame, Terminal,
};
use rowd_app::{App, AppSnapshot, ConnectionTest, PairingInfo, RecoveryItem, Status};
use rowd_core::config::{RemapPolicy, ShareRequest, SyncMode};
use rowd_core::trace;
use std::{
    collections::BTreeMap,
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
    Requests,
    Recovery,
    Device,
}

impl Tab {
    const ALL: [Self; 4] = [Self::Shares, Self::Requests, Self::Recovery, Self::Device];

    fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }

    fn title(self) -> &'static str {
        match self {
            Self::Shares => "Shares",
            Self::Requests => "Solicitações",
            Self::Recovery => "Recovery",
            Self::Device => "Dispositivo",
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
    CleanupRecovery,
    FilterRecovery,
    Pair,
    ShowQr,
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
        !matches!(
            self,
            Self::FilterRecovery | Self::ShowQr | Self::TestConnection | Self::ToggleTrace
        )
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
        label: "Editar Share",
        default: "e",
    },
    BindingDef {
        action: UiAction::ToggleShare,
        tab: Tab::Shares,
        label: "Pausar/retomar Share",
        default: "space",
    },
    BindingDef {
        action: UiAction::SyncShare,
        tab: Tab::Shares,
        label: "Sincronizar Share",
        default: "s",
    },
    BindingDef {
        action: UiAction::ReindexShare,
        tab: Tab::Shares,
        label: "Reindexar Share",
        default: "x",
    },
    BindingDef {
        action: UiAction::RemapShare,
        tab: Tab::Shares,
        label: "Remapear Android",
        default: "m",
    },
    BindingDef {
        action: UiAction::EditIgnore,
        tab: Tab::Shares,
        label: "Editar .rowdignore",
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
        tab: Tab::Requests,
        label: "Aceitar solicitação",
        default: "a",
    },
    BindingDef {
        action: UiAction::RejectRequest,
        tab: Tab::Requests,
        label: "Rejeitar solicitação",
        default: "r",
    },
    BindingDef {
        action: UiAction::RestoreRecovery,
        tab: Tab::Recovery,
        label: "Restaurar versão",
        default: "enter",
    },
    BindingDef {
        action: UiAction::KeepRecovery,
        tab: Tab::Recovery,
        label: "Manter versão atual",
        default: "k",
    },
    BindingDef {
        action: UiAction::ExportRecovery,
        tab: Tab::Recovery,
        label: "Exportar versão",
        default: "e",
    },
    BindingDef {
        action: UiAction::CleanupRecovery,
        tab: Tab::Recovery,
        label: "Limpar registro resolvido",
        default: "d",
    },
    BindingDef {
        action: UiAction::FilterRecovery,
        tab: Tab::Recovery,
        label: "Filtrar por Share",
        default: "f",
    },
    BindingDef {
        action: UiAction::Pair,
        tab: Tab::Device,
        label: "Configurar endereço",
        default: "p",
    },
    BindingDef {
        action: UiAction::ShowQr,
        tab: Tab::Device,
        label: "Exibir QR",
        default: "o",
    },
    BindingDef {
        action: UiAction::TestConnection,
        tab: Tab::Device,
        label: "Testar conexão",
        default: "T",
    },
    BindingDef {
        action: UiAction::ToggleTrace,
        tab: Tab::Device,
        label: "Trace de desempenho",
        default: "t",
    },
    BindingDef {
        action: UiAction::ToggleGlobalPause,
        tab: Tab::Device,
        label: "Pausar/retomar tudo",
        default: "space",
    },
    BindingDef {
        action: UiAction::Unlink,
        tab: Tab::Device,
        label: "Desvincular",
        default: "u",
    },
    BindingDef {
        action: UiAction::ExportProfile,
        tab: Tab::Device,
        label: "Exportar perfil",
        default: "x",
    },
    BindingDef {
        action: UiAction::ImportProfile,
        tab: Tab::Device,
        label: "Importar perfil",
        default: "i",
    },
    BindingDef {
        action: UiAction::ExportBackup,
        tab: Tab::Device,
        label: "Exportar backup",
        default: "b",
    },
    BindingDef {
        action: UiAction::ImportBackup,
        tab: Tab::Device,
        label: "Importar backup",
        default: "n",
    },
    BindingDef {
        action: UiAction::ExportDiagnostic,
        tab: Tab::Device,
        label: "Exportar diagnóstico",
        default: "d",
    },
    BindingDef {
        action: UiAction::ResetInitial,
        tab: Tab::Device,
        label: "Configuração inicial",
        default: "f",
    },
    BindingDef {
        action: UiAction::ResetAll,
        tab: Tab::Device,
        label: "Apagar dados internos",
        default: "X",
    },
];

struct KeyMap;

impl KeyMap {
    fn resolve(key: KeyEvent, tab: Tab) -> Option<KeyCommand> {
        let fixed = match key.code {
            KeyCode::Char('q') => Some(KeyCommand::Quit),
            KeyCode::Char('?') => Some(KeyCommand::Help),
            KeyCode::Char('1') => Some(KeyCommand::OpenTab(Tab::Shares)),
            KeyCode::Char('2') => Some(KeyCommand::OpenTab(Tab::Requests)),
            KeyCode::Char('3') => Some(KeyCommand::OpenTab(Tab::Recovery)),
            KeyCode::Char('4') => Some(KeyCommand::OpenTab(Tab::Device)),
            KeyCode::Tab => Some(KeyCommand::NextTab),
            KeyCode::BackTab => Some(KeyCommand::PreviousTab),
            KeyCode::Up => Some(KeyCommand::MoveUp),
            KeyCode::Down => Some(KeyCommand::MoveDown),
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
    OpenTab(Tab),
    NextTab,
    PreviousTab,
    MoveUp,
    MoveDown,
    Close,
    Action(UiAction),
}

enum Submit {
    Pair,
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
    CleanupRecovery(String, String),
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
}

enum Modal {
    Help,
    Qr(PairingInfo),
    Input(InputDialog),
}

enum UiEvent {
    Notice(String),
    JobDone(String),
}

struct Ui {
    tab: Tab,
    shares_index: usize,
    requests_index: usize,
    recovery_index: usize,
    recovery_filter: Option<String>,
    snapshot: AppSnapshot,
    modal: Option<Modal>,
    notice: String,
    connection: Option<ConnectionTest>,
    job: Option<String>,
    dirty: bool,
    last_refresh: Instant,
}

impl Ui {
    fn new(app: &App) -> Self {
        let mut ui = Self {
            tab: Tab::Shares,
            shares_index: 0,
            requests_index: 0,
            recovery_index: 0,
            recovery_filter: None,
            snapshot: AppSnapshot::default(),
            modal: None,
            notice: "Pronto. Use ? para consultar todas as ações.".into(),
            connection: None,
            job: None,
            dirty: true,
            last_refresh: Instant::now() - Duration::from_secs(2),
        };
        ui.refresh(app);
        ui
    }

    fn refresh(&mut self, app: &App) {
        if !self.dirty && self.last_refresh.elapsed() < Duration::from_secs(1) {
            return;
        }
        if let Ok(snapshot) = app.snapshot() {
            self.snapshot = snapshot;
        }
        self.shares_index = clamp_index(self.shares_index, self.snapshot.shares.len());
        self.requests_index = clamp_index(self.requests_index, self.snapshot.requests.len());
        if self.recovery_filter.as_ref().is_some_and(|share_id| {
            !self
                .snapshot
                .recovery
                .iter()
                .any(|item| &item.share_id == share_id)
        }) {
            self.recovery_filter = None;
        }
        self.recovery_index = clamp_index(self.recovery_index, self.recovery_len());
        self.last_refresh = Instant::now();
        self.dirty = false;
    }

    fn selected_share(&self) -> Option<&Status> {
        self.snapshot.shares.get(self.shares_index)
    }

    fn selected_request(&self) -> Option<&ShareRequest> {
        self.snapshot.requests.get(self.requests_index)
    }

    fn selected_recovery(&self) -> Option<&RecoveryItem> {
        self.snapshot
            .recovery
            .iter()
            .filter(|item| {
                self.recovery_filter
                    .as_ref()
                    .is_none_or(|share_id| &item.share_id == share_id)
            })
            .nth(self.recovery_index)
    }

    fn recovery_len(&self) -> usize {
        self.snapshot
            .recovery
            .iter()
            .filter(|item| {
                self.recovery_filter
                    .as_ref()
                    .is_none_or(|share_id| &item.share_id == share_id)
            })
            .count()
    }

    fn move_selection(&mut self, down: bool) {
        let recovery_len = self.recovery_len();
        let (index, len) = match self.tab {
            Tab::Shares => (&mut self.shares_index, self.snapshot.shares.len()),
            Tab::Requests => (&mut self.requests_index, self.snapshot.requests.len()),
            Tab::Recovery => (&mut self.recovery_index, recovery_len),
            Tab::Device => return,
        };
        *index = if down {
            (*index + 1).min(len.saturating_sub(1))
        } else {
            index.saturating_sub(1)
        };
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
        }));
    }

    fn open_secret(&mut self, title: impl Into<String>, hint: impl Into<String>, submit: Submit) {
        self.modal = Some(Modal::Input(InputDialog {
            title: title.into(),
            hint: hint.into(),
            value: String::new(),
            submit,
            secret: true,
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
            KeyCommand::OpenTab(tab) => self.tab = tab,
            KeyCommand::NextTab => self.tab = self.tab.next(),
            KeyCommand::PreviousTab => self.tab = self.tab.previous(),
            KeyCommand::MoveUp => self.move_selection(false),
            KeyCommand::MoveDown => self.move_selection(true),
            KeyCommand::Close => {}
            KeyCommand::Action(action) => self.begin_action(action, app)?,
        }
        Ok(false)
    }

    fn handle_modal_key(&mut self, key: KeyEvent, app: &App, tx: &Sender<UiEvent>) -> Result<()> {
        if key.code == KeyCode::Esc {
            self.modal = None;
            return Ok(());
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
            Some(Modal::Help | Modal::Qr(_)) | None => {}
        }
        Ok(())
    }

    fn open_pair_input(&mut self) {
        let current = if self.snapshot.device.configured {
            self.snapshot.device.address.clone()
        } else {
            "0.0.0.0:43821".into()
        };
        self.open_input(
            "Endereço publicado do PC",
            "Use IP:porta alcançável pelo Android; 0.0.0.0 descobre o IP local.",
            current,
            Submit::Pair,
        );
    }

    fn begin_action(&mut self, action: UiAction, app: &App) -> Result<()> {
        match action {
            UiAction::AddShare => self.open_input(
                "Novo Share",
                "nome | pasta absoluta no PC | bidirectional/to_android/to_pc",
                "",
                Submit::AddShare,
            ),
            UiAction::EditShare => {
                if let Some(share) = self.selected_share().map(|status| status.share.clone()) {
                    self.open_input(
                        format!("Editar · {}", share.name),
                        "nome | pasta absoluta no PC | bidirectional/to_android/to_pc",
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
                        format!("Reindexar · {}", share.name),
                        "Reconstrói estado derivado e preserva arquivos/recovery. Digite REINDEXAR.",
                        "",
                        Submit::ReindexShare(share.share_id),
                    );
                }
            }
            UiAction::RemapShare => {
                if let Some(share) = self.selected_share().map(|status| status.share.clone()) {
                    self.open_input(
                        format!("Remapear Android · {}", share.name),
                        "pc/android/compare; a pasta SAF será escolhida no Android",
                        "compare",
                        Submit::RemapShare(share.share_id),
                    );
                }
            }
            UiAction::EditIgnore => {
                if let Some(share) = self.selected_share().map(|status| status.share.clone()) {
                    let id = share.share_id;
                    let text = app.ignore_text(&id)?;
                    self.open_input(
                        format!(".rowdignore · {}", share.name),
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
                        "Remove a rota, preservando os arquivos. Digite REMOVER.",
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
                        "A decisão será enviada ao Android. Digite REJEITAR.",
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
                        format!("Exportar recovery · {}", item.path),
                        "Destino novo para a versão recuperada.",
                        "",
                        Submit::ExportRecovery(item.share_id.clone(), item.id.clone()),
                    );
                }
            }
            UiAction::CleanupRecovery => self.begin_recovery("LIMPAR", Submit::CleanupRecovery),
            UiAction::FilterRecovery => {
                let shares = self
                    .snapshot
                    .recovery
                    .iter()
                    .map(|item| (item.share_id.clone(), item.share_name.clone()))
                    .collect::<BTreeMap<_, _>>();
                let ids = shares.keys().cloned().collect::<Vec<_>>();
                self.recovery_filter = match &self.recovery_filter {
                    None => ids.first().cloned(),
                    Some(current) => ids
                        .iter()
                        .position(|id| id == current)
                        .and_then(|index| ids.get(index + 1))
                        .cloned(),
                };
                self.recovery_index = 0;
                self.notice = match &self.recovery_filter {
                    Some(id) => format!(
                        "Recovery filtrado por {}.",
                        shares.get(id).map(String::as_str).unwrap_or("Share")
                    ),
                    None => "Recovery exibindo todos os Shares.".into(),
                };
            }
            UiAction::Pair => self.open_pair_input(),
            UiAction::ShowQr => match app.pairing_info() {
                Ok(info) => self.modal = Some(Modal::Qr(info)),
                Err(_) => self.open_pair_input(),
            },
            UiAction::TestConnection => {
                self.connection = Some(app.connection_test()?);
                self.notice =
                    "Teste concluído; detalhes atualizados no painel do dispositivo.".into();
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
                "Desvincular Android",
                "Aguarda confirmação do Android antes de revogar. Digite DESVINCULAR.",
                "",
                Submit::Unlink,
            ),
            UiAction::ExportProfile => self.open_input(
                "Exportar perfil sem segredos",
                "Caminho de destino novo, por exemplo /tmp/rowd-profile.json",
                "",
                Submit::ExportProfile,
            ),
            UiAction::ImportProfile => self.open_input(
                "Importar perfil sem segredos",
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
                "Importar backup completo criptografado",
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
                "Restaurar configuração inicial",
                "Arquiva o estado administrativo e preserva arquivos. Digite INICIAL.",
                "",
                Submit::ResetInitial,
            ),
            UiAction::ResetAll => self.open_input(
                "Apagar dados internos do Rowd",
                "Move .rowd para um arquivo recuperável. Digite APAGAR TUDO.",
                "",
                Submit::ResetAll,
            ),
        }
        Ok(())
    }

    fn begin_recovery(&mut self, token: &'static str, constructor: fn(String, String) -> Submit) {
        if let Some(item) = self.selected_recovery().cloned() {
            self.open_input(
                format!("Recovery · {}", item.path),
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
            Submit::Pair => {
                app.pair(value.trim())?;
                self.modal = Some(Modal::Qr(app.pairing_info()?));
                self.notice = "QR pronto para leitura pelo Android.".into();
            }
            Submit::AddShare => {
                let (name, root, mode) = share_form(&value)?;
                app.add_share(name, root, mode)?;
                self.notice = "Share adicionado.".into();
            }
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
                        "Solicitação rejeitada; o Android receberá a decisão na próxima conexão."
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
                    "android" => RemapPolicy::Android,
                    "compare" => RemapPolicy::Compare,
                    _ => anyhow::bail!("política deve ser pc, android ou compare"),
                };
                let app = app.clone();
                start_job(self, tx, "Remapeando Share", move || {
                    app.remap_share(&id, policy)?;
                    Ok("Vínculo Android invalidado; escolha a nova pasta SAF no aparelho.".into())
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
            Submit::CleanupRecovery(share, id) => {
                require_token(&value, "LIMPAR")?;
                let app = app.clone();
                start_job(self, tx, "Limpando recovery", move || {
                    app.cleanup_recovery(&share, &id)?;
                    Ok("Registro resolvido removido manualmente.".into())
                })?;
            }
            Submit::Unlink => {
                require_token(&value, "DESVINCULAR")?;
                let app = app.clone();
                start_job(self, tx, "Desvinculando dispositivo", move || {
                    app.unlink_device()?;
                    Ok("Desvinculação pendente até o Android confirmar; arquivos e recovery preservados.".into())
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
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(3),
        ])
        .split(area);
        self.render_header(frame, regions[0]);
        self.render_tabs(frame, regions[1]);
        self.render_body(frame, regions[2]);
        self.render_footer(frame, regions[3]);
        if let Some(modal) = &self.modal {
            render_modal(frame, area, modal);
        }
    }

    fn render_header(&self, frame: &mut Frame<'_>, area: Rect) {
        let paused = self.snapshot.device.sync_paused;
        let paired = self.snapshot.device.paired;
        let conflicts: usize = self
            .snapshot
            .shares
            .iter()
            .map(|status| status.conflicts.len())
            .sum();
        let line_one = Line::from(vec![
            Span::styled(
                " ROWD ",
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" v{}  ", env!("CARGO_PKG_VERSION")),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                if paused { "SYNC PAUSADO" } else { "SYNC ATIVO" },
                Style::default()
                    .fg(if paused { WARNING } else { SUCCESS })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                if paired {
                    "ANDROID VINCULADO"
                } else {
                    "ANDROID AGUARDANDO"
                },
                Style::default().fg(if paired { SUCCESS } else { MUTED }),
            ),
        ]);
        let work = self.job.as_deref().unwrap_or(&self.notice);
        let line_two = Line::from(vec![
            Span::styled(
                format!(
                    " {} Shares  ·  {conflicts} conflitos  ",
                    self.snapshot.shares.len()
                ),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                truncate(work, area.width.saturating_sub(45) as usize),
                Style::default().fg(if self.job.is_some() {
                    WARNING
                } else {
                    Color::White
                }),
            ),
        ]);
        frame.render_widget(Paragraph::new(Text::from(vec![line_one, line_two])), area);
    }

    fn render_tabs(&self, frame: &mut Frame<'_>, area: Rect) {
        let titles = Tab::ALL
            .iter()
            .enumerate()
            .map(|(index, tab)| format!(" {} {} ", index + 1, tab.title()))
            .collect::<Vec<_>>();
        frame.render_widget(
            Tabs::new(titles)
                .select(self.tab.index())
                .block(Block::default().borders(Borders::BOTTOM))
                .style(Style::default().fg(MUTED))
                .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
                .divider(" "),
            area,
        );
    }

    fn render_body(&self, frame: &mut Frame<'_>, area: Rect) {
        let panels = body_panels(area);
        match self.tab {
            Tab::Shares => self.render_shares(frame, panels),
            Tab::Requests => self.render_requests(frame, panels),
            Tab::Recovery => self.render_recovery(frame, panels),
            Tab::Device => self.render_device(frame, panels),
        }
    }

    fn render_shares(&self, frame: &mut Frame<'_>, panels: [Rect; 2]) {
        let items = if self.snapshot.shares.is_empty() {
            vec![
                ListItem::new("Nenhum Share\nUse a para criar a primeira rota.")
                    .style(Style::default().fg(MUTED)),
            ]
        } else {
            self.snapshot
                .shares
                .iter()
                .map(|status| {
                    let state = if !status.root_available {
                        "INDISPONÍVEL"
                    } else if status.share.enabled {
                        "ATIVO"
                    } else {
                        "PAUSADO"
                    };
                    ListItem::new(format!(
                        "{}\n{} · {} · {} conf.",
                        status.share.name,
                        state,
                        mode_label(status.share.mode),
                        status.conflicts.len()
                    ))
                })
                .collect()
        };
        render_list(
            frame,
            panels[0],
            "Rotas de sincronização",
            items,
            self.shares_index,
        );
        let detail = self.selected_share().map(share_detail).unwrap_or_else(|| {
            "Cada Share liga uma pasta do PC a uma pasta escolhida no Android.\n\nAdicione uma rota para começar; as validações impedem raízes sobrepostas e caminhos inseguros.".into()
        });
        render_detail(frame, panels[1], "Rota selecionada", detail);
    }

    fn render_requests(&self, frame: &mut Frame<'_>, panels: [Rect; 2]) {
        let items = if self.snapshot.requests.is_empty() {
            vec![ListItem::new("Nenhuma solicitação recebida").style(Style::default().fg(MUTED))]
        } else {
            self.snapshot
                .requests
                .iter()
                .map(|request| {
                    ListItem::new(format!(
                        "{}\n{} · {}",
                        request.name,
                        "PENDENTE",
                        mode_label(request.mode)
                    ))
                })
                .collect()
        };
        render_list(
            frame,
            panels[0],
            "Recebidas do Android",
            items,
            self.requests_index,
        );
        let detail = self.selected_request().map(|request| {
            format!(
                "Nome\n{}\n\nEstado\n{}\n\nModo\n{}\n\nIdentificador\n{}\n\nSolicitações enviadas pelo Android podem ser canceladas no próprio aparelho. Rejeições ficam registradas até os dois lados convergirem.",
                request.name,
                "PENDENTE",
                mode_label(request.mode),
                short_id(&request.request_id)
            )
        }).unwrap_or_else(|| "Aguardando solicitações.\n\nEnviadas: administradas no Android, onde podem ser canceladas antes da aceitação.".into());
        render_detail(frame, panels[1], "Ciclo da solicitação", detail);
    }

    fn render_recovery(&self, frame: &mut Frame<'_>, panels: [Rect; 2]) {
        let visible = self
            .snapshot
            .recovery
            .iter()
            .filter(|item| {
                self.recovery_filter
                    .as_ref()
                    .is_none_or(|share_id| &item.share_id == share_id)
            })
            .collect::<Vec<_>>();
        let items = if visible.is_empty() {
            vec![ListItem::new("Nenhuma versão neste filtro").style(Style::default().fg(MUTED))]
        } else {
            visible
                .iter()
                .map(|item| {
                    ListItem::new(format!(
                        "{}\n{} · {}",
                        item.path,
                        item.share_name,
                        if item.finished {
                            "RESOLVIDO"
                        } else {
                            "PENDENTE"
                        }
                    ))
                })
                .collect()
        };
        render_list(
            frame,
            panels[0],
            "Versões preservadas",
            items,
            self.recovery_index,
        );
        let total: u64 = self.snapshot.recovery.iter().map(|item| item.bytes).sum();
        let filtered_total: u64 = visible.iter().map(|item| item.bytes).sum();
        let mut by_share = BTreeMap::<&str, (usize, u64)>::new();
        for item in &self.snapshot.recovery {
            let aggregate = by_share.entry(&item.share_name).or_default();
            aggregate.0 += 1;
            aggregate.1 += item.bytes;
        }
        let aggregate = by_share
            .iter()
            .map(|(name, (count, bytes))| {
                format!("{name}: {count} versões · {}", human_bytes(*bytes))
            })
            .collect::<Vec<_>>()
            .join("\n");
        let filter = self
            .recovery_filter
            .as_ref()
            .and_then(|id| {
                self.snapshot
                    .recovery
                    .iter()
                    .find(|item| &item.share_id == id)
            })
            .map(|item| item.share_name.as_str())
            .unwrap_or("todos os Shares");
        let detail = self.selected_recovery().map(|item| {
            format!(
                "Filtro\n{} · {} em {} versões\n\nShare\n{}\n\nCaminho\n{}\n\nEstado\n{}\n\nBackup disponível\n{}\n\nTamanho\n{}\n\nID\n{}\n\nUso total do recovery\n{} em {} versões\n\nPor Share\n{}",
                filter,
                human_bytes(filtered_total),
                visible.len(),
                item.share_name,
                item.path,
                if item.finished { "resolvido" } else { "aguardando decisão" },
                yes_no(item.backup_available),
                human_bytes(item.bytes),
                short_id(&item.id),
                human_bytes(total),
                self.snapshot.recovery.len(),
                if aggregate.is_empty() { "nenhum" } else { &aggregate }
            )
        }).unwrap_or_else(|| format!("Filtro\n{filter} · {} em {} versões\n\nO Rowd preserva versões quando uma reconciliação exige decisão. A limpeza é sempre manual e só aceita registros resolvidos.\n\nPor Share\n{}", human_bytes(filtered_total), visible.len(), if aggregate.is_empty() { "nenhum" } else { &aggregate }));
        render_detail(frame, panels[1], "Versão selecionada", detail);
    }

    fn render_device(&self, frame: &mut Frame<'_>, panels: [Rect; 2]) {
        let device = &self.snapshot.device;
        let left = if device.configured {
            format!(
                "Vínculo\n{}\n\nEndereço publicado\n{}\n\nÚltima conexão\n{}\n\nSincronização\n{}\n\nTrace de desempenho\n{}\n\nIdentidade do Android\n{}\n\nFingerprint SHA-256\n{}",
                if device.paired { "Android vinculado" } else { "aguardando primeiro vínculo" },
                device.address,
                timestamp_label(device.last_connection),
                if device.sync_paused { "pausada" } else { "ativa" },
                if trace::enabled() { "Ligado" } else { "Desligado" },
                short_id(&device.identity),
                device.fingerprint
            )
        } else {
            format!("Dispositivo ainda não configurado.\n\nTrace de desempenho    {}\n\nPressione p, confirme o endereço alcançável na rede local e leia o QR no Android.", if trace::enabled() { "Ligado" } else { "Desligado" })
        };
        render_detail(frame, panels[0], "Confiança PC ↔ Android", left);
        let right = if let Some(test) = &self.connection {
            let checks = test
                .checks
                .iter()
                .map(|check| {
                    format!(
                        "[{}] {}\n    {}",
                        if check.ok { "OK" } else { "FALHA" },
                        check.label,
                        check.detail
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "{checks}\n\nShares disponíveis\n{}/{}",
                test.available_shares, test.total_shares
            )
        } else {
            "Teste por camadas\n\nPC alcançável\nTCP\nTLS\nAutenticação\nDispositivo reconhecido\nShares disponíveis\n\nUse T para testar, t para alternar o trace e o para abrir o QR compacto no terminal.".into()
        };
        render_detail(frame, panels[1], "Diagnóstico e pareamento", right);
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        let actions = BINDINGS
            .iter()
            .filter(|binding| binding.tab == self.tab)
            .map(|binding| {
                format!(
                    "{} {}",
                    display_key(binding.default),
                    short_action(binding.label)
                )
            })
            .collect::<Vec<_>>()
            .join("  ");
        let text =
            format!("{actions}\n1..4 abas  Tab/Shift+Tab navegar  ↑↓ selecionar  ? ajuda  q sair");
        frame.render_widget(
            Paragraph::new(text)
                .style(Style::default().fg(MUTED))
                .wrap(Wrap { trim: true })
                .block(Block::default().borders(Borders::TOP)),
            area,
        );
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
                UiEvent::JobDone(message) => {
                    ui.job = None;
                    ui.notice = message;
                    ui.dirty = true;
                }
            }
        }
        ui.refresh(&app);
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
    selected: usize,
) {
    let mut state = ListState::default();
    state.select(Some(selected));
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
        Modal::Help => {
            let popup = centered(area, 92, 32);
            frame.render_widget(Clear, popup);
            let mut lines = vec![
                "Navegação global".to_string(),
                "1..4 abre aba · Tab/Shift+Tab alterna · ↑↓ seleciona · Esc fecha · q sai".into(),
                String::new(),
            ];
            for tab in Tab::ALL {
                lines.push(tab.title().to_uppercase());
                lines.push(
                    BINDINGS
                        .iter()
                        .filter(|binding| binding.tab == tab)
                        .map(|binding| {
                            format!("{} {}", display_key(binding.default), binding.label)
                        })
                        .collect::<Vec<_>>()
                        .join("  ·  "),
                );
                lines.push(String::new());
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
                    "Leia no Android\n{}\nFingerprint SHA-256\n{}\nSVG privado: {}",
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

fn share_detail(status: &Status) -> String {
    let conflicts = if status.conflicts.is_empty() {
        "nenhum".into()
    } else {
        status
            .conflicts
            .iter()
            .take(6)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n  ")
    };
    let last_error = status
        .last_error
        .as_ref()
        .map(|error| {
            format!(
                "{} · {}\n{}\n{}",
                timestamp_label(Some(error.at)),
                error.operation,
                error.message,
                error
                    .resolved_at
                    .map(|at| format!("resolvido {}", timestamp_label(Some(at))))
                    .unwrap_or_else(|| "não resolvido".into())
            )
        })
        .or_else(|| status.error.clone())
        .unwrap_or_else(|| "nenhum".into());
    format!(
        "{}\n\nEstado\n{}\n\nRaiz PC\n{} · {}\n\nPasta Android\n{}\n\nModo\n{}\n\nÚltimo sync\n{}\n\nConflitos ({})\n  {}\n\nÚltimo erro\n{}",
        status.share.name,
        if status.share.enabled { "ativo" } else { "pausado" },
        status.share.root.display(),
        if status.root_available { "disponível" } else { "indisponível" },
        if status.share.remap_policy.is_some() { "aguardando nova seleção no Android" } else { "definida no aparelho (SAF)" },
        mode_label(status.share.mode),
        timestamp_label(status.last_sync),
        status.conflicts.len(),
        conflicts,
        last_error
    )
}

fn share_form(value: &str) -> Result<(String, PathBuf, SyncMode)> {
    let parts = split_fields(value, 3, "nome | pasta | modo")?;
    anyhow::ensure!(!parts[0].is_empty(), "informe o nome");
    anyhow::ensure!(!parts[1].is_empty(), "informe a pasta");
    Ok((
        parts[0].into(),
        PathBuf::from(parts[1]),
        crate::parse_mode(parts[2])?,
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
        SyncMode::Bidirectional => "bidirecional",
        SyncMode::ToAndroid => "PC → Android",
        SyncMode::ToPc => "Android → PC",
    }
}

fn mode_value(mode: SyncMode) -> &'static str {
    match mode {
        SyncMode::Bidirectional => "bidirectional",
        SyncMode::ToAndroid => "to_android",
        SyncMode::ToPc => "to_pc",
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

fn human_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "sim"
    } else {
        "não"
    }
}

fn display_key(value: &str) -> String {
    match value {
        "space" => "Espaço".into(),
        "enter" => "Enter".into(),
        value => value.into(),
    }
}

fn short_action(value: &str) -> &str {
    value
        .strip_suffix(" Share")
        .or_else(|| value.strip_suffix(" solicitação"))
        .or_else(|| value.strip_suffix(" versão"))
        .unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn keymap_uses_fixed_contextual_bindings() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(matches!(
            KeyMap::resolve(key, Tab::Shares),
            Some(KeyCommand::Action(UiAction::AddShare))
        ));
        assert!(KeyMap::resolve(key, Tab::Device).is_none());
    }
}
