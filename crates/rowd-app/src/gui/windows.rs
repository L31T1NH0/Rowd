//! Windows desktop integration. A hidden window owns the notification-area icon.
use super::controller::Event;
use anyhow::{ensure, Context, Result};
use std::{
    mem::size_of,
    path::Path,
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
        Arc,
    },
    thread::{self, JoinHandle},
};
use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HWND, LPARAM, LRESULT, POINT, WPARAM},
    Graphics::Gdi::*,
    System::{LibraryLoader::GetModuleHandleW, Registry::*},
    UI::{Shell::*, WindowsAndMessaging::*},
};
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const TRAY_EVENT: u32 = WM_APP + 1;
// Shell headers define this as a macro rather than an exported constant.
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
const ATTENTION: u32 = WM_APP + 2;
static NOTIFICATION_WINDOW: AtomicUsize = AtomicUsize::new(0);
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn quoted(value: &str) -> Result<String> {
    ensure!(
        !value.chars().any(char::is_control) && !value.contains('"'),
        "caminho inválido para inicialização automática"
    );
    let trailing = value.chars().rev().take_while(|c| *c == '\\').count();
    Ok(format!("\"{}{}\"", value, "\\".repeat(trailing)))
}
fn run_value(home: &Path) -> Vec<u16> {
    wide(&format!("Rowd-{}", super::desktop::profile_id(home)))
}
fn run_command(home: &Path) -> Result<String> {
    Ok(format!(
        "{} --profile {}",
        quoted(
            std::env::current_exe()?
                .to_str()
                .context("executável não é UTF-8")?
        )?,
        quoted(home.to_str().context("perfil não é UTF-8")?)?
    ))
}
pub fn autostart_enabled(home: &Path) -> Result<bool> {
    let key = wide(RUN_KEY);
    let value = run_value(home);
    let mut size = 0;
    // RegGetValue copies into an owned UTF-16 buffer; no pointer outlives it.
    unsafe {
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            null_mut(),
            &mut size,
        );
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(false);
        }
        ensure!(
            status == ERROR_SUCCESS,
            "não foi possível ler a inicialização automática: {}",
            std::io::Error::from_raw_os_error(status as i32)
        );
        ensure!(
            size <= 64 * 1024 && size % 2 == 0,
            "entrada de inicialização automática inválida"
        );
        let mut data = vec![0u16; size as usize / 2];
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            data.as_mut_ptr().cast(),
            &mut size,
        );
        ensure!(
            status == ERROR_SUCCESS,
            "não foi possível ler a inicialização automática: {}",
            std::io::Error::from_raw_os_error(status as i32)
        );
        let end = data
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(data.len());
        Ok(String::from_utf16(&data[..end])? == run_command(home)?)
    }
}
pub fn set_autostart(home: &Path, enabled: bool) -> Result<()> {
    let key = wide(RUN_KEY);
    let value = run_value(home);
    let command = if enabled {
        Some(wide(&run_command(home)?))
    } else {
        None
    };
    // Modify only this user's Run value for this profile, without administrator access.
    unsafe {
        let mut handle = null_mut();
        let status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            0,
            null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            null(),
            &mut handle,
            null_mut(),
        );
        ensure!(
            status == ERROR_SUCCESS,
            "não foi possível abrir a inicialização automática: {}",
            std::io::Error::from_raw_os_error(status as i32)
        );
        let status = if let Some(command) = command {
            RegSetValueExW(
                handle,
                value.as_ptr(),
                0,
                REG_SZ,
                command.as_ptr().cast(),
                (command.len() * 2) as u32,
            )
        } else {
            RegDeleteValueW(handle, value.as_ptr())
        };
        RegCloseKey(handle);
        ensure!(
            status == ERROR_SUCCESS || (!enabled && status == ERROR_FILE_NOT_FOUND),
            "não foi possível alterar a inicialização automática: {}",
            std::io::Error::from_raw_os_error(status as i32)
        );
    }
    Ok(())
}
pub fn notify_attention() {
    let window = NOTIFICATION_WINDOW.load(Ordering::Acquire);
    if window != 0 {
        unsafe {
            PostMessageW(window as HWND, ATTENTION, 0, 0);
        }
    }
}
pub fn startup_error(message: &str) {
    unsafe {
        MessageBoxW(
            null_mut(),
            wide(message).as_ptr(),
            wide("Rowd — não foi possível iniciar").as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}
pub struct Tray {
    window: usize,
    thread: Option<JoinHandle<()>>,
}
impl Tray {
    pub fn start(
        events: SyncSender<Event>,
        available: Arc<AtomicBool>,
        pixels: Vec<u8>,
    ) -> Result<Self> {
        let (ready, opened) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            if let Err(error) = tray_loop(events, available, pixels, &ready) {
                let _ = ready.send(Err(format!("{error:#}")));
            }
        });
        let window = opened
            .recv()
            .context("bandeja não iniciou")?
            .map_err(anyhow::Error::msg)?;
        Ok(Self {
            window,
            thread: Some(thread),
        })
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            PostMessageW(self.window as HWND, WM_CLOSE, 0, 0);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
struct Icon(HICON);
impl Drop for Icon {
    fn drop(&mut self) {
        unsafe {
            DestroyIcon(self.0);
        }
    }
}
fn icon(pixels: &[u8]) -> Result<Icon> {
    ensure!(pixels.len() == 4096, "ícone da bandeja inválido");
    let bitmap = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: 32,
            biHeight: -32,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = null_mut();
    // CreateIconIndirect copies these owned DIB/mask buffers before they are released.
    unsafe {
        let color = CreateDIBSection(
            null_mut(),
            &bitmap,
            DIB_RGB_COLORS,
            &mut bits,
            null_mut(),
            0,
        );
        ensure!(
            !color.is_null() && !bits.is_null(),
            "não foi possível criar o bitmap da bandeja"
        );
        let output = std::slice::from_raw_parts_mut(bits.cast::<u8>(), 4096);
        for (source, target) in pixels.chunks_exact(4).zip(output.chunks_exact_mut(4)) {
            let alpha = source[0] as u16;
            target.copy_from_slice(&[
                (source[3] as u16 * alpha / 255) as u8,
                (source[2] as u16 * alpha / 255) as u8,
                (source[1] as u16 * alpha / 255) as u8,
                source[0],
            ]);
        }
        let mask_bits = [0u8; 128];
        let mask = CreateBitmap(32, 32, 1, 1, mask_bits.as_ptr().cast());
        if mask.is_null() {
            DeleteObject(color);
            anyhow::bail!("não foi possível criar a máscara da bandeja");
        }
        let handle = CreateIconIndirect(&ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        });
        DeleteObject(mask);
        DeleteObject(color);
        ensure!(
            !handle.is_null(),
            "não foi possível criar o ícone da bandeja"
        );
        Ok(Icon(handle))
    }
}
struct State {
    events: SyncSender<Event>,
    available: Arc<AtomicBool>,
    data: NOTIFYICONDATAW,
    taskbar_created: u32,
}
impl State {
    fn set_available(&self, available: bool) {
        if self.available.swap(available, Ordering::AcqRel) != available {
            let _ = self.events.try_send(Event::TrayAvailable(available));
        }
    }
    fn add(&self) {
        unsafe {
            let mut data = self.data;
            let success = Shell_NotifyIconW(NIM_ADD, &data) != 0;
            if success {
                data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                Shell_NotifyIconW(NIM_SETVERSION, &data);
            }
            self.set_available(success);
        }
    }
    fn attention(&self) {
        let mut data = self.data;
        data.uFlags = NIF_INFO;
        copy_text(&mut data.szInfoTitle, "Rowd");
        copy_text(
            &mut data.szInfo,
            "Uma pasta precisa de atenção. Abra o Rowd para consultar os detalhes.",
        );
        data.dwInfoFlags = NIIF_WARNING;
        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &data);
        }
    }
    fn menu(&self) {
        unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            AppendMenuW(menu, MF_STRING, 1, wide("Abrir Rowd").as_ptr());
            AppendMenuW(
                menu,
                MF_STRING,
                2,
                wide("Pausar / retomar sincronização").as_ptr(),
            );
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            AppendMenuW(menu, MF_STRING, 3, wide("Sair").as_ptr());
            let mut point = POINT::default();
            GetCursorPos(&mut point);
            SetForegroundWindow(self.data.hWnd);
            let choice = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                0,
                self.data.hWnd,
                null(),
            );
            DestroyMenu(menu);
            let event = match choice {
                1 => Some(Event::ShowWindow),
                2 => Some(Event::TogglePause),
                3 => Some(Event::Quit),
                _ => None,
            };
            if let Some(event) = event {
                let _ = self.events.try_send(event);
            }
            PostMessageW(self.data.hWnd, WM_NULL, 0, 0);
            Shell_NotifyIconW(NIM_SETFOCUS, &self.data);
        }
    }
}
fn copy_text<const N: usize>(target: &mut [u16; N], value: &str) {
    for (output, input) in target.iter_mut().take(N - 1).zip(value.encode_utf16()) {
        *output = input;
    }
}
unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(lparam as *const CREATESTRUCTW);
        SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let state = GetWindowLongPtrW(window, GWLP_USERDATA) as *const State;
    // Boxed State stays alive on this thread until the window is destroyed.
    if let Some(state) = state.as_ref() {
        match message {
            TRAY_EVENT => match lparam as u32 & 0xffff {
                NIN_SELECT | NIN_KEYSELECT | NIN_BALLOONUSERCLICK | WM_LBUTTONUP
                | WM_LBUTTONDBLCLK => {
                    let _ = state.events.try_send(Event::ShowWindow);
                }
                WM_CONTEXTMENU | WM_RBUTTONUP => state.menu(),
                _ => {}
            },
            ATTENTION => state.attention(),
            WM_DESTROY => {
                PostQuitMessage(0);
                return 0;
            }
            _ if message == state.taskbar_created => state.add(),
            _ => {}
        }
    }
    DefWindowProcW(window, message, wparam, lparam)
}
fn tray_loop(
    events: SyncSender<Event>,
    available: Arc<AtomicBool>,
    pixels: Vec<u8>,
    ready: &SyncSender<Result<usize, String>>,
) -> Result<()> {
    let icon = icon(&pixels)?;
    let class_name = wide("Rowd.BackgroundTray");
    // All HWND operations and callbacks run on this thread, separate from Slint's loop.
    unsafe {
        let instance = GetModuleHandleW(null());
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class_name.as_ptr(),
            ..Default::default()
        };
        ensure!(
            RegisterClassW(&class) != 0,
            "não foi possível registrar a janela da bandeja"
        );
        let mut state = Box::new(State {
            events,
            available,
            data: NOTIFYICONDATAW::default(),
            taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
        });
        state.data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        state.data.uID = 1;
        state.data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
        state.data.uCallbackMessage = TRAY_EVENT;
        state.data.hIcon = icon.0;
        copy_text(&mut state.data.szTip, "Rowd");
        // Top-level, hidden: it receives Explorer's TaskbarCreated broadcast.
        let window = CreateWindowExW(
            0,
            class_name.as_ptr(),
            wide("Rowd").as_ptr(),
            0,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            (&*state as *const State).cast(),
        );
        if window.is_null() {
            UnregisterClassW(class_name.as_ptr(), instance);
            anyhow::bail!("não foi possível criar a janela da bandeja");
        }
        state.data.hWnd = window;
        state.add();
        NOTIFICATION_WINDOW.store(window as usize, Ordering::Release);
        if ready.send(Ok(window as usize)).is_ok() {
            let mut message = MSG::default();
            while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        NOTIFICATION_WINDOW.store(0, Ordering::Release);
        Shell_NotifyIconW(NIM_DELETE, &state.data);
        state.set_available(false);
        if IsWindow(window) != 0 {
            DestroyWindow(window);
        }
        UnregisterClassW(class_name.as_ptr(), instance);
    }
    Ok(())
}
