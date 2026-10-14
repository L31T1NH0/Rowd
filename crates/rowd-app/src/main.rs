#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
mod gui;
fn main() {
    if let Err(error) = gui::run() {
        #[cfg(windows)]
        gui::startup_error(&format!("{error:#}"));
        #[cfg(not(windows))]
        eprintln!("Rowd: {error:#}");
        std::process::exit(1);
    }
}
