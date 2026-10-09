use std::env;
use std::path::PathBuf;

fn main() {
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
