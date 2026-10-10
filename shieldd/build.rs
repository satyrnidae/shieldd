use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // The About box's build date, UTC. Cargo reruns this when anything that goes into shieldd
    // changes, so it's the last build that changed anything. SOURCE_DATE_EPOCH pins it for a
    // reproducible build.
    for path in
        ["src", "Cargo.toml", "../Cargo.toml", "../rtlusb", "../ozproto", "../shdlink", "../shieldtray", "../shieldutl"]
    {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    let secs = env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()));
    println!("cargo:rustc-env=SHIELDD_BUILD_DATE={}", shieldutl::civil::date(secs / 86_400));
    common_controls();
}

/// A manifest asking for Common Controls 6, so the tray's dialogs get the themed buttons and
/// edit box rather than Windows 95's. shieldw's build.rs does the same; keep the two in step.
fn common_controls() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return;
    }
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' \
         version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
    );
}
