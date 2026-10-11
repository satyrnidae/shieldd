use std::env;

/// `gui` where there's a tray to show: always on Windows, and on Linux with the `tray` feature.
/// Elsewhere only the headless one is built.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(gui)");
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os == "windows" || (os == "linux" && env::var_os("CARGO_FEATURE_TRAY").is_some()) {
        println!("cargo::rustc-cfg=gui");
    }
    common_controls(&os);
}

/// As shieldd's build.rs, for the tray example: a manifest asking for Common Controls 6, so its
/// boxes are the ones shieldd shows, the About box's link and themed buttons included. Keep it in
/// step with shieldd's and shieldw's.
fn common_controls(os: &str) {
    if os != "windows" || env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    println!("cargo:rustc-link-arg-examples=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-examples=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' \
         version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
    );
}
