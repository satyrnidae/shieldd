use std::env;

/// `gui` where there's a tray to show: always on Windows, and on Linux with the `tray` feature.
/// Elsewhere only the headless one is built.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(gui)");
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os == "windows" || (os == "linux" && env::var_os("CARGO_FEATURE_TRAY").is_some()) {
        println!("cargo::rustc-cfg=gui");
    }
}
