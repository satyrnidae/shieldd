//! shieldd without a console window, for running at login. Logs to the settings folder.

#![windows_subsystem = "windows"]

fn main() -> std::process::ExitCode {
    match shieldd::run(true) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => std::process::ExitCode::FAILURE,
    }
}
