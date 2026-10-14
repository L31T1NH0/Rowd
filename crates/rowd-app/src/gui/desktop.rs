#[cfg(target_os = "linux")]
use anyhow::ensure;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

#[cfg(target_os = "linux")]
use super::controller::Event;
#[cfg(target_os = "linux")]
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::SyncSender,
        Arc,
    },
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub close_to_tray: bool,
    pub launch_on_login: bool,
    pub notify_errors: bool,
    pub peer_id: String,
    pub peer_name: String,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            close_to_tray: true,
            launch_on_login: false,
            notify_errors: true,
            peer_id: String::new(),
            peer_name: String::new(),
        }
    }
}

impl Preferences {
    pub fn load(home: &Path) -> Result<Self> {
        match fs::read(home.join(".rowd/gui-preferences.json")) {
            Ok(bytes) => {
                Ok(serde_json::from_slice(&bytes).context("preferências da GUI inválidas")?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        rowd_core::storage::atomic_json(&home.join(".rowd/gui-preferences.json"), self)
    }
}

#[cfg(target_os = "linux")]
fn desktop_arg(value: &str) -> Result<String> {
    ensure!(
        !value.chars().any(char::is_control),
        "caminho contém caracteres de controle"
    );
    // Desktop entries unescape their strings before parsing Exec's quoted arguments.
    let escaped = value
        .replace('\\', "\\\\\\\\")
        .replace('"', "\\\\\"")
        .replace('`', "\\\\`")
        .replace('$', "\\\\$")
        .replace('%', "%%");
    Ok(format!("\"{escaped}\""))
}

pub(super) fn profile_id(home: &Path) -> String {
    hex::encode(Sha256::digest(home.as_os_str().as_encoded_bytes()))
}

#[cfg(target_os = "linux")]
fn autostart_entry(exe: &Path, home: &Path) -> Result<String> {
    let exe = desktop_arg(exe.to_str().context("caminho do executável não é UTF-8")?)?;
    let home_arg = desktop_arg(&format!(
        "ROWD_HOME={}",
        home.to_str().context("caminho do perfil não é UTF-8")?
    ))?;
    Ok(format!("[Desktop Entry]\nType=Application\nName=Rowd\nComment=Sincronização de pastas entre PC e Android\nExec=env {home_arg} {exe}\nTerminal=false\nX-GNOME-Autostart-enabled=true\nX-Rowd-Profile={}\n", profile_id(home)))
}

#[cfg(target_os = "linux")]
fn autostart_path() -> Result<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("diretório de configuração do desktop indisponível")?;
    ensure!(
        config.is_absolute(),
        "diretório de configuração deve ser absoluto"
    );
    Ok(config.join("autostart/rowd-desktop.desktop"))
}

pub fn autostart_enabled(home: &Path) -> Result<bool> {
    #[cfg(target_os = "linux")]
    {
        match fs::read_to_string(autostart_path()?) {
            Ok(entry) => Ok(entry
                .lines()
                .any(|line| line == format!("X-Rowd-Profile={}", profile_id(home)))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
    #[cfg(windows)]
    {
        super::windows::autostart_enabled(home)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = home;
        Ok(false)
    }
}

pub fn set_autostart(home: &Path, enabled: bool) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let path = autostart_path()?;
        if enabled {
            rowd_core::storage::atomic_write(
                &path,
                autostart_entry(&std::env::current_exe()?, home)?.as_bytes(),
            )?;
        } else if autostart_enabled(home)? {
            fs::remove_file(path)?;
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        super::windows::set_autostart(home, enabled)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (home, enabled);
        anyhow::bail!("inicialização automática ainda não disponível nesta plataforma")
    }
}

pub fn notify_attention() {
    #[cfg(windows)]
    super::windows::notify_attention();
    #[cfg(target_os = "linux")]
    {
        let _ = notify_rust::Notification::new()
            .summary("Rowd")
            .body("Uma pasta precisa de atenção. Abra o Rowd para consultar os detalhes.")
            .appname("Rowd")
            .show();
    }
}

#[cfg(target_os = "linux")]
pub struct Tray {
    pub events: SyncSender<Event>,
    pub available: Arc<AtomicBool>,
    pub icon: Vec<u8>,
}

#[cfg(target_os = "linux")]
impl ksni::Tray for Tray {
    fn id(&self) -> String {
        format!("rowd-{}", std::process::id())
    }
    fn title(&self) -> String {
        "Rowd".into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![ksni::Icon {
            width: 32,
            height: 32,
            data: self.icon.clone(),
        }]
    }
    fn activate(&mut self, _: i32, _: i32) {
        let _ = self.events.send(Event::ShowWindow);
    }
    fn watcher_online(&self) {
        self.available.store(true, Ordering::Release);
        let _ = self.events.send(Event::TrayAvailable(true));
    }
    fn watcher_offline(&self, _: ksni::OfflineReason) -> bool {
        self.available.store(false, Ordering::Release);
        let _ = self.events.send(Event::TrayAvailable(false));
        true
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "Abrir Rowd".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.events.send(Event::ShowWindow);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Pausar / retomar sincronização".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.events.send(Event::TogglePause);
                }),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Sair".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.events.send(Event::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}
