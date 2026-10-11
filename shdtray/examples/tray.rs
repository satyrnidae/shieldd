//! The tray on its own, for eyeballing: walks through disconnected, pairing, joining, then up to
//! three controllers charging and running down, and prints what's picked from the menu. Exit from
//! the menu, or ctrl+c, ends it.
//!
//! `cargo run -p shdtray --example tray --features tray`

use std::time::{Duration, Instant};

use shdtray::{About, Battery, Command, Connected, Signal, Status, Tray};

fn pad(n: u8, name: &str, band: u8, charging: bool) -> Connected {
    // The reading only matters on the charger, where it's near enough full
    let battery = Battery { charging, band, reading: 0xD500, critical: band <= 10, must_charge: false };
    Connected {
        mac: [0x02, 0, 0, 0, 0, n],
        name: Some(name.into()),
        port: Some(n),
        battery: Some(battery),
        audio: false,
        signal: Some([Signal::Good, Signal::Fair, Signal::Low][usize::from(n - 1) % 3]),
        firmware: Some("1.96/99/3.71/0.32".into()),
    }
}

fn main() {
    tracing_subscriber_init();
    let about = About { version: env!("CARGO_PKG_VERSION"), authors: env!("CARGO_PKG_AUTHORS"), built: "example" };
    let mut tray = Tray::start(about);
    let network = Status { ssid: Some("DIRECT-xy-example_net".into()), channel: Some(6), ..Default::default() };
    let steps = [
        Status { ..network.clone() },
        Status { pairing: true, ..network.clone() },
        Status { joining: true, ..network.clone() },
        Status { controllers: vec![pad(1, "Pad & Co", 75, false)], ..network.clone() },
        Status { controllers: vec![pad(1, "Pad & Co", 50, false), pad(2, "Second", 25, true)], ..network.clone() },
        Status {
            controllers: vec![pad(1, "Pad & Co", 10, false), pad(2, "Second", 100, true), pad(3, "Third", 50, false)],
            ..network.clone()
        },
    ];
    let mut status = steps[0].clone();
    let mut next = 0;
    let mut at = Instant::now();
    loop {
        if next < steps.len() && at.elapsed() >= Duration::from_secs(if next == 0 { 0 } else { 8 }) {
            println!("step {next}");
            status = steps[next].clone();
            tray.update(status.clone());
            next += 1;
            at = Instant::now();
        }
        for cmd in tray.commands() {
            println!("picked {cmd:?}");
            match cmd {
                Command::Exit => return,
                Command::Rename(mac, name) => {
                    for c in status.controllers.iter_mut().filter(|c| c.mac == mac) {
                        c.name = Some(name.clone());
                    }
                }
                Command::Audio(mac, on) => {
                    for c in status.controllers.iter_mut().filter(|c| c.mac == mac) {
                        c.audio = on;
                    }
                }
                Command::Disconnect(mac) | Command::Forget(mac) => {
                    status.controllers.retain(|c| c.mac != mac);
                    status.sent_away = true;
                }
                Command::Identify(_) => tray.problem("Identify", "Nothing to identify in the example."),
                _ => {}
            }
            tray.update(status.clone());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The tray's own logs on stderr, so what it warns about shows; RUST_LOG for more
fn tracing_subscriber_init() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "shdtray=info".into());
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
}
