//! The tray on its own, for eyeballing: walks through disconnected, pairing, joining, then up to
//! three controllers charging and running down, and prints what's picked from the menu. Exit from
//! the menu, or ctrl+c, ends it.
//!
//! `cargo run -p shdtray --example tray --features tray`

use std::time::{Duration, Instant};

use shdtray::{About, Battery, Command, Connected, Signal, Status, Tray};

fn pad(n: u8, name: &str, band: u8, charging: bool) -> Connected {
    // A voltage reading at the bottom of its band, so the percent agrees with it, from battery.rs's
    // curve. The charger lifts it by CHARGE_LIFT.
    let lift = if charging { 0x0660 } else { 0 };
    let reading = lift
        + match band {
            0..=10 => 0xD081,
            11..=25 => 0xD47F,
            26..=50 => 0xDA74,
            51..=75 => 0xE26B,
            _ => 0xEEEF,
        };
    let battery = Battery { charging, band, reading, critical: band <= 10, must_charge: false };
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
    let about = About { version: env!("CARGO_PKG_VERSION"), authors: env!("CARGO_PKG_AUTHORS"), built: "example", source: env!("CARGO_PKG_REPOSITORY") };
    let mut tray = Tray::start(about);
    let network = Status { ssid: Some("DIRECT-xy-example_net".into()), channel: Some(6), ..Default::default() };
    let steps = [
        Status { ..network.clone() },
        Status { pairing: true, ..network.clone() },
        Status { joining: true, ..network.clone() },
        Status { controllers: vec![pad(1, "CASSINI", 75, false)], ..network.clone() },
        Status { controllers: vec![pad(1, "CASSINI", 50, false), pad(2, "albiorix", 25, true)], ..network.clone() },
        Status {
            controllers: vec![
                pad(1, "CASSINI", 10, false),
                pad(2, "albiorix", 100, true),
                pad(3, "Chrysalis", 50, false),
            ],
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
