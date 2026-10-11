//! shieldd without a console window, for running at login. Logs to the settings folder.
//!
//! Windows only: a console program always gets a window there, which only the subsystem in the
//! exe's header avoids. Elsewhere there's nothing to hide, and the login start runs
//! `shieldd --background`; this is a stub that says so, without shieldd linked in.

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(target_os = "windows")]
fn main() -> std::process::ExitCode {
    match shieldd::run(true) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => std::process::ExitCode::FAILURE,
    }
}

#[cfg(not(target_os = "windows"))]
fn main() -> std::process::ExitCode {
    eprintln!("shieldw is for Windows; here, run `shieldd --background`");
    std::process::ExitCode::FAILURE
}
