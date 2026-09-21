use crate::service;
use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
    Terminal,
};
use rowd_core::{config::DeviceConfig, storage::LocalStore};
use std::{
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};

struct Screen;
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

struct ShareWizard {
    step: u8,
    name: String,
    folder: String,
    mode: String,
}

impl ShareWizard {
    fn prompt(&self) -> &'static str {
        match self.step {
            0 => "Qual o nome desse Share?",
            1 => "Qual o caminho da pasta? Use pwd para copiar o caminho.",
            _ => "Qual modo você deseja? 1 bidirecional · 2 PC para Android · 3 Android para PC",
        }
    }
}

fn pairing_view(home: &Path, cfg: &DeviceConfig) -> Result<(String, PathBuf, String)> {
    use sha2::{Digest, Sha256};

    let image = service::export_qr(home, cfg)?;
    let fingerprint = Sha256::digest(hex::decode(&cfg.cert)?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":");
    Ok((service::qr(cfg)?, image, fingerprint))
}

pub fn run(home: &Path) -> Result<()> {
    anyhow::ensure!(
        io::stdin().is_terminal(),
        "A TUI requer terminal interativo; use --help para a CLI."
    );
    enable_raw_mode()?;
    let _screen = Screen;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let mut worker: Option<std::thread::JoinHandle<()>> = None;
    let mut selected: usize = 0;
    let mut request_index: usize = 0;
    let mut status = "Pareie o Android para começar. Depois adicione seus Shares.".to_string();
    let mut input: Option<(char, String)> = None;
    let mut share_wizard: Option<ShareWizard> = None;
    let mut qr: Option<String> = None;
    let mut qr_image = None;
    let mut fingerprint = String::new();
    let mut table_state = TableState::default();
    let mut recovery: Option<Vec<rowd_core::storage::RecoveryEntry>> = None;
    let mut recovery_index: usize = 0;
    let mut recovery_table = TableState::default();
    loop {
        if worker.as_ref().is_some_and(|w| w.is_finished()) {
            worker.take().unwrap().join().ok();
        }
        if worker.is_none() && home.join(".rowd/device.json").exists() {
            let home = home.to_path_buf();
            let tx = tx.clone();
            let stop = stop.clone();
            worker = Some(std::thread::spawn(move || {
                if let Err(e) = service::serve(&home, None, false, stop, |s| {
                    let _ = tx.send(s);
                }) {
                    let _ = tx.send(format!("Servidor: {e:#}"));
                }
            }));
        }
        for message in rx.try_iter() {
            status = message;
        }
        let entries = service::status(home).unwrap_or_default();
        let requests = service::pending_share_requests(home).unwrap_or_default();
        selected = selected.min(entries.len().saturating_sub(1));
        request_index = request_index.min(requests.len().saturating_sub(1));
        terminal.draw(|frame| {
            let area = frame.area();
            if let Some(code) = &qr {
                let width = code.lines().map(|l| l.chars().count()).max().unwrap_or(0);
                let height = code.lines().count() + 5;
                let hint = format!("Parear Android · Esc volta · o abre imagem\nQR privado: {}\nSHA-256: {fingerprint}",qr_image.as_ref().map(|p: &PathBuf| p.display().to_string()).unwrap_or_default());
                let text = if area.width as usize >= width && area.height as usize >= height { format!("{hint}\n{code}") } else { format!("{hint}\n\nAmplie o terminal para {width} × {height} ou pressione o para abrir o QR.\nJSON: rowd pair --address IP:43821 --invite arquivo.json") };
                frame.render_widget(Paragraph::new(text).wrap(Wrap{trim:false}),area);
                return;
            }
            if let Some(records) = &recovery {
                let regions = Layout::vertical([Constraint::Min(4),Constraint::Length(3)]).split(area);
                let rows = records.iter().map(|r| Row::new([r.path.clone(),r.id.clone(),if r.finished {"Preservado".into()} else {"Pendente".into()}]));
                recovery_table.select(Some(recovery_index));
                frame.render_stateful_widget(Table::new(rows,[Constraint::Percentage(40),Constraint::Percentage(45),Constraint::Percentage(15)]).header(Row::new(["Caminho","Versão (ID)","Estado"])).row_highlight_style(Style::default().bg(Color::DarkGray)).block(Block::default().title("Versões recuperáveis").borders(Borders::ALL)),regions[0],&mut recovery_table);
                frame.render_widget(Paragraph::new("↑↓ escolher versão · Enter escolher ação · Esc voltar\nRestaurar conserva a versão atual em outro backup."),regions[1]);
                return;
            }
            let layout = Layout::vertical([Constraint::Length(3),Constraint::Min(5),Constraint::Length(4),Constraint::Length(3),Constraint::Length(4)]).split(area);
            let paired = DeviceConfig::load(home).ok().and_then(|c| c.peer_device).is_some();
            frame.render_widget(Paragraph::new(format!("ROWD  /  Seus arquivos, entre seus dispositivos\nAndroid: {}  ·  {} Shares",if paired {"pareado"} else {"aguardando pareamento"},entries.len())).style(Style::default().fg(Color::Green)),layout[0]);
            let rows = entries.iter().enumerate().map(|(i,s)| Row::new(vec![Cell::from(s.share.name.clone()),Cell::from(format!("{:?}",s.share.mode)),Cell::from(s.pending.to_string()),Cell::from(s.conflicts.len().to_string()),Cell::from(s.last_sync.map(|t| format!("há {}s",std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs().saturating_sub(t))).unwrap_or_else(|| "Ainda não".into()))]).style(if i == selected { Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD) } else {Style::default()}));
            table_state.select(Some(selected));
            frame.render_stateful_widget(Table::new(rows,[Constraint::Percentage(30),Constraint::Percentage(25),Constraint::Percentage(15),Constraint::Percentage(15),Constraint::Percentage(15)]).header(Row::new(["Share","Direção","Pendentes","Conflitos","Última rodada"]).style(Style::default().add_modifier(Modifier::BOLD))).block(Block::default().borders(Borders::TOP|Borders::BOTTOM)),layout[1],&mut table_state);
            let request_hint = if requests.is_empty() {
                String::new()
            } else {
                format!(
                    "\nSolicitações Android (n para alternar): {}",
                    requests
                        .iter()
                        .enumerate()
                        .map(|(index, request)| format!(
                            "{}{} ({:?})",
                            if index == request_index { "→ " } else { "" },
                            request.name,
                            request.mode
                        ))
                        .collect::<Vec<_>>()
                        .join(" · ")
                )
            };
            let detail = entries.get(selected).map(|s| format!("{} ↔ pasta escolhida no Android\nPendentes: {}\nConflitos: {}\n{}{}",s.share.root.display(),s.pending_paths.iter().take(3).cloned().collect::<Vec<_>>().join(", "),s.conflicts.iter().take(3).cloned().collect::<Vec<_>>().join(", "),s.error.as_deref().unwrap_or(""),request_hint)).unwrap_or_else(|| format!("Nenhum Share cadastrado. Pressione a para adicionar uma pasta.\n{status}{request_hint}"));
            frame.render_widget(Paragraph::new(detail).wrap(Wrap{trim:false}),layout[2]);
            frame.render_widget(Paragraph::new(status.clone()).wrap(Wrap{trim:false}),layout[3]);
            let footer = if let Some(wizard) = &share_wizard {
            format!("{}\n> {}\nEnter confirma · Esc cancela", wizard.prompt(), match wizard.step { 0 => &wizard.name, 1 => &wizard.folder, _ => &wizard.mode })
            } else if let Some((action,text)) = &input {
                let label = match action {
                    'p' => "Endereço do PC (IP:43821)",
                    'a' => "Nome | pasta absoluta | modo (bidirectional/to_android/to_pc)",
                    'e' => "Nome | pasta absoluta | modo",
                    'd' => "Digite REMOVER para desvincular; arquivos e recovery serão mantidos",
                    'r' => "Recovery: ID | keep/restore/export | destino de exportação",
                    'c' => "Pasta local para aceitar a solicitação Android",
                    _ => "Entrada",
                };
                format!("{label}\n> {text}\nEnter confirma · Esc cancela")
            } else { "↑↓ escolher  p parear  a adicionar  e editar  d remover\nn alternar solicitação Android  c aceitar pasta\ns sincronizar agora  v verificar tudo  r recovery  q sair".into() };
            frame.render_widget(Paragraph::new(footer).wrap(Wrap{trim:false}),layout[4]);
        })?;
        if !event::poll(Duration::from_millis(150))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if qr.is_some() {
            if key.code == KeyCode::Esc {
                qr = None;
            }
            if key.code == KeyCode::Char('o') {
                if let Some(path) = &qr_image {
                    if let Err(e) = std::process::Command::new("xdg-open")
                        .arg(path)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                    {
                        status = format!("Não foi possível abrir QR: {e}");
                        qr = None;
                    }
                }
            }
            continue;
        }
        if let Some(records) = &recovery {
            match key.code {
                KeyCode::Esc => recovery = None,
                KeyCode::Down => {
                    recovery_index = (recovery_index + 1).min(records.len().saturating_sub(1))
                }
                KeyCode::Up => recovery_index = recovery_index.saturating_sub(1),
                KeyCode::Enter => {
                    if let Some(record) = records.get(recovery_index) {
                        input = Some(('r', format!("{} | keep", record.id)));
                        status = format!(
                            "Versão selecionada: {} · {}. Escolha keep, restore ou export.",
                            record.path, record.id
                        );
                    }
                    recovery = None;
                }
                _ => {}
            }
            continue;
        }
        if let Some(mut wizard) = share_wizard.take() {
            let mut keep_wizard = true;
            match key.code {
                KeyCode::Esc => keep_wizard = false,
                KeyCode::Backspace => match wizard.step {
                    0 => {
                        wizard.name.pop();
                    }
                    1 => {
                        wizard.folder.pop();
                    }
                    _ => {
                        wizard.mode.pop();
                    }
                },
                KeyCode::Char(c) => match wizard.step {
                    0 => wizard.name.push(c),
                    1 => wizard.folder.push(c),
                    _ => wizard.mode.push(c),
                },
                KeyCode::Enter => match wizard.step {
                    0 if wizard.name.trim().is_empty() => {
                        status = "Informe um nome para o Share.".into()
                    }
                    0 => wizard.step = 1,
                    1 if wizard.folder.trim().is_empty() => {
                        status = "Informe o caminho da pasta.".into()
                    }
                    1 => wizard.step = 2,
                    _ => {
                        let mode = match wizard.mode.trim() {
                            "1" => Ok(rowd_core::config::SyncMode::Bidirectional),
                            "2" => Ok(rowd_core::config::SyncMode::ToAndroid),
                            "3" => Ok(rowd_core::config::SyncMode::ToPc),
                            _ => Err(anyhow::anyhow!("Escolha 1, 2 ou 3.")),
                        };
                        match mode {
                            Ok(mode) => {
                                let result = service::update(home, |cfg| {
                                    cfg.add_share(
                                        home,
                                        wizard.name.trim().into(),
                                        wizard.folder.trim().into(),
                                        None,
                                        mode,
                                    )?;
                                    Ok(())
                                });
                                status = match result {
                                    Ok(()) => "Share cadastrado.".into(),
                                    Err(error) => format!("{error:#}"),
                                };
                                keep_wizard = false;
                            }
                            Err(error) => status = error.to_string(),
                        }
                    }
                },
                _ => {}
            }
            if keep_wizard {
                share_wizard = Some(wizard);
            }
            continue;
        }
        if let Some((action, text)) = &mut input {
            match key.code {
                KeyCode::Esc => input = None,
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) => text.push(c),
                KeyCode::Enter => {
                    let action = *action;
                    let text = text.clone();
                    let request_id = if action == 'c' {
                        requests
                            .get(request_index)
                            .map(|request| request.request_id.clone())
                    } else {
                        None
                    };
                    input = None;
                    let result = (|| -> Result<()> {
                        match action {
                            'p' => {
                                let cfg = service::pair(home, text.trim())?;
                                let (code, image, hash) = pairing_view(home, &cfg)?;
                                qr = Some(code);
                                qr_image = Some(image);
                                fingerprint = hash;
                            }
                            'a' | 'e' => {
                                let parts: Vec<_> = text.split('|').map(str::trim).collect();
                                anyhow::ensure!(parts.len() == 3, "Use nome | pasta | modo");
                                let mode = crate::parse_mode(parts[2])?;
                                service::update(home, |cfg| {
                                    if action == 'a' {
                                        cfg.add_share(
                                            home,
                                            parts[0].into(),
                                            PathBuf::from(parts[1]),
                                            None,
                                            mode,
                                        )?;
                                    } else {
                                        let mut s = entries
                                            .get(selected)
                                            .context("Selecione um Share")?
                                            .share
                                            .clone();
                                        s.name = parts[0].into();
                                        s.root = parts[1].into();
                                        s.mode = mode;
                                        cfg.put_share(home, s)?;
                                    }
                                    Ok(())
                                })?;
                            }
                            'd' => {
                                anyhow::ensure!(text == "REMOVER", "Remoção cancelada");
                                let s = &entries.get(selected).context("Selecione um Share")?.share;
                                service::update(home, |cfg| cfg.remove_share(home, &s.share_id))?;
                            }
                            'r' => {
                                let s = &entries.get(selected).context("Selecione um Share")?.share;
                                let mut store = LocalStore::open_recovery(&s.root)?;
                                let parts: Vec<_> = text.split('|').map(str::trim).collect();
                                anyhow::ensure!(
                                    parts.len() >= 2,
                                    "Informe ID | ação | destino opcional"
                                );
                                store.resolve_recovery(
                                    parts[0],
                                    parts[1],
                                    parts.get(2).filter(|p| !p.is_empty()).map(Path::new),
                                )?;
                            }
                            'c' => {
                                let request_id = request_id.context("Selecione uma solicitação")?;
                                anyhow::ensure!(
                                    !text.trim().is_empty(),
                                    "Informe o caminho da pasta"
                                );
                                service::accept_share_request(
                                    home,
                                    &request_id,
                                    Path::new(text.trim()),
                                )?;
                            }
                            _ => {}
                        }
                        Ok(())
                    })();
                    status = match result {
                        Ok(()) => "Configuração salva".into(),
                        Err(e) => format!("{e:#}"),
                    };
                }
                _ => {}
            }
            continue;
        }
        match key.code {
            KeyCode::Char('q') => break,
            KeyCode::Down => selected = (selected + 1).min(entries.len().saturating_sub(1)),
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Char('p') => match DeviceConfig::load(home) {
                Ok(cfg) => {
                    let port = cfg
                        .address
                        .parse::<std::net::SocketAddr>()
                        .map(|address| address.port())
                        .unwrap_or(43821);
                    match service::pair(home, &format!("0.0.0.0:{port}"))
                        .and_then(|cfg| pairing_view(home, &cfg))
                    {
                        Ok((code, image, hash)) => {
                            qr = Some(code);
                            qr_image = Some(image);
                            fingerprint = hash;
                            status =
                                "QR de pareamento pronto. Pressione o para abrir a imagem.".into();
                        }
                        Err(error) => status = format!("Não foi possível gerar o QR: {error:#}"),
                    }
                }
                Err(_) => input = Some(('p', String::new())),
            },
            KeyCode::Char('a') => {
                share_wizard = Some(ShareWizard {
                    step: 0,
                    name: String::new(),
                    folder: String::new(),
                    mode: String::new(),
                })
            }
            KeyCode::Char('e') => {
                if let Some(s) = entries.get(selected) {
                    input = Some((
                        'e',
                        format!(
                            "{} | {} | {}",
                            s.share.name,
                            s.share.root.display(),
                            match s.share.mode {
                                rowd_core::config::SyncMode::Bidirectional => "bidirectional",
                                rowd_core::config::SyncMode::ToAndroid => "to_android",
                                rowd_core::config::SyncMode::ToPc => "to_pc",
                            }
                        ),
                    ));
                }
            }
            KeyCode::Char('d') => input = Some(('d', String::new())),
            KeyCode::Char('n') if !requests.is_empty() => {
                request_index = (request_index + 1) % requests.len();
                status = format!("Solicitação selecionada: {}", requests[request_index].name);
            }
            KeyCode::Char('c') if !requests.is_empty() => {
                status = format!(
                    "Aceitando {}. Informe a pasta do PC e use pwd para copiar o caminho.",
                    requests[request_index].name
                );
                input = Some(('c', String::new()));
            }
            KeyCode::Char('r') => {
                if let Some(s) = entries.get(selected) {
                    match LocalStore::open_recovery(&s.share.root)
                        .and_then(|s| s.recovery_entries())
                    {
                        Ok(list) => {
                            if list.is_empty() {
                                status = "Nenhuma versão recuperável neste Share".into();
                            } else {
                                recovery_index = 0;
                                recovery = Some(list);
                            }
                        }
                        Err(e) => status = format!("{e:#}"),
                    }
                }
            }
            KeyCode::Char(c @ ('s' | 'v')) => {
                let home = home.to_path_buf();
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let message=match service::scan(&home,c=='v') {Ok(())=>"Manifestos atualizados. Aguardando a próxima conexão do Android (ative o modo automático).".into(),Err(e)=>format!("{e:#}")};
                    let _ = tx.send(message);
                });
            }
            _ => {}
        }
    }
    stop.store(true, Ordering::Relaxed);
    Ok(())
}
