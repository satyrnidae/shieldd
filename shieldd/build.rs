use std::env;
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
}
