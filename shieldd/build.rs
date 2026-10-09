use std::env;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "src/civil.rs"]
mod civil;

fn main() {
    // The About box's build date, UTC. Cargo reruns this when anything that goes into shieldd
    // changes, so it's the last build that changed anything. SOURCE_DATE_EPOCH pins it for a
    // reproducible build.
    for path in ["src", "Cargo.toml", "../Cargo.toml", "../res", "../rtlusb", "../ozproto", "../shdlink"] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    let secs = env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()));
    println!("cargo:rustc-env=SHIELDD_BUILD_DATE={}", civil::date(secs / 86_400));

    // Npcap is only for the Windows host; the Linux build is just --transmit
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-env-changed=NPCAP_SDK_DIR");
    let sdk = env::var_os("NPCAP_SDK_DIR")
        .expect("NPCAP_SDK_DIR not set; point it at the npcap sdk root, e.g. C:\\Programs\\npcap-sdk");
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "ARM64",
        _ => "x64",
    };
    let lib = PathBuf::from(sdk).join("Lib").join(arch);
    assert!(lib.join("wpcap.lib").exists(), "no wpcap.lib in {}", lib.display());
    println!("cargo:rustc-link-search=native={}", lib.display());

    // Npcap installs to System32\Npcap, which isn't on the dll search path
    // unless WinPcap compat mode is on. Delay-load so main() can add it first.
    println!("cargo:rustc-link-arg-bins=/DELAYLOAD:wpcap.dll");
    println!("cargo:rustc-link-lib=delayimp");
}
