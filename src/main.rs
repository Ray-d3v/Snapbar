#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

#[cfg(target_os = "windows")]
mod app;
#[cfg(target_os = "windows")]
mod assets;
#[cfg(target_os = "windows")]
mod capture;
#[cfg(target_os = "windows")]
mod overlay;
#[cfg(target_os = "windows")]
mod settings;

#[cfg(target_os = "windows")]
fn main() {
    app::run();
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("Snapbar supports Windows 11 only.");
}
