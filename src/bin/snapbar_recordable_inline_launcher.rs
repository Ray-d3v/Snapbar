#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

#[cfg(target_os = "windows")]
// The launcher must exit while the resident Snapbar child keeps running. Windows
// closes the process handle when `Child` is dropped and has no Unix-style zombies.
#[allow(clippy::zombie_processes)]
fn main() {
    let launcher = std::env::current_exe().expect("launcher path is unavailable");
    let snapbar = launcher.with_file_name("snapbar.exe");
    std::process::Command::new(snapbar)
        .args(["--inline-titlebar", "--recordable-overlay"])
        .spawn()
        .expect("recordable Snapbar could not be started");
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("Snapbar development launchers support Windows only.");
}
