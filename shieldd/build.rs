use std::env;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Days since 1970-01-01 as YYYY-MM-DD, proleptic Gregorian (Howard Hinnant's civil_from_days)
fn civil(days: u64) -> String {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn main() {
    // The About box's build date, UTC. Cargo reruns this when the sources change, so it's the
    // last build that changed anything. SOURCE_DATE_EPOCH pins it for a reproducible build.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=../Cargo.toml");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    let secs = env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()));
    println!("cargo:rustc-env=SHIELDD_BUILD_DATE={}", civil(secs / 86_400));

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
