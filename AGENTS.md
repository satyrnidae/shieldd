# AGENTS.md

How to work in this repository. There is no other agent doc to copy from; the tone and detail
level follow README.md and THIRD_PARTY.md, not an agent spec.

## Build

```powershell
cargo build --release
cargo test --release
```

Binaries land in an OS-specific directory under `target`: `target\windows\release` on Windows or
`target/linux/release` on Linux (the folder depends on target triple and profile): `shieldd`, its windowless twin
`shieldw` for running at login on Windows only, and the `rtlusb` bring-up tool. On Windows they are `.exe`.

## Layout

The main crates:

- `shieldd/` — the host: radio, controller setup, outputs, pairing, tray, login start, WSL bridge;
  the udev rule in `res/`.
- `shieldw/` — shieldd without a console window, for running at login on Windows; a stub elsewhere.
- `shdlib/` — shared by the other crates: firmware version, base64, `psk=` values and their DPAPI
  seal, controller audio (VBAN, PipeWire).
- `shdnet/` — MAC addresses and how they're shown; no dependencies, so ozproto can use it.
- `ozproto/` — Ozmo protocol: wire format and host session state machine, no I/O.
- `rtlusb/` — userspace RTL8811AU driver over WinUSB or usbfs: init, firmware, PHY, AP, WPA2, WPS.
- `shdlink/` — packet format shared by `--transmit` and `--receiver`.
- `shdtray/` — the tray: what it shows and sends back, the battery gauge, the Windows and Linux
  trays, its icon SVGs in `res/` and its text in `res/locales/`.
- `tools/` — rtw88 table generator, Python.

## Conventions

- Rust edition 2024 with let-chains on Windows (`x86_64-pc-windows-msvc`). On Linux, edition 2021 /
  GNU. Workspace is edition 2024; per-crate targets may differ.
- Most code is AI-assisted (Anthropic's Claude via Claude Code), under a human maintainer. See the
  "AI disclosure" section of README.md and keep contributions consistent with that stance.

## Operating rules

- Scope to this project only. Don't touch CI workflow files, secrets, or anything outside it.
- Read issues first, then write up a plan as an issue comment before editing files. Don't edit code
  prematurely.
- Write plainly and briefly. No large pasted code blocks; describe changes in prose.
