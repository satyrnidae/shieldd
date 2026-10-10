# shieldd

Native Windows host for the 2015 NVIDIA SHIELD controller. The controller talks Ozmo
"USB over Wi-Fi" over Wi-Fi Direct; shieldd runs the access point itself on a Realtek
RTL8811AU USB adapter, through its own userspace driver over WinUSB, and turns the
controller into a virtual Xbox 360 pad, a mouse and media keys.

No Bluetooth, no vendor app, no Linux VM. Input latency is under ~20 ms.

## AI disclosure

Most of this project's code, docs and comments were written by an AI model, Anthropic's
Claude via Claude Code, under the direction of a human maintainer. If you'd rather not run
or build on AI-generated code, this isn't the project for you. Other ways to use the
controller on a PC:
- **[Syniurge/ShieldControllerWinDriver](https://github.com/Syniurge/ShieldControllerWinDriver)**:
  Windows HID filter driver. Wired only. Written 2016-2017, before AI coding tools.
- **NVIDIA's own drivers:** wired only, and only on PCs with NVIDIA graphics. Closed source, so
  there's no checking, but NVIDIA has said publicly that its engineers use AI coding tools;
  assume current releases are at least partly AI-written.
- **[devmapal/nvidia-shield-controller-driver](https://github.com/devmapal/nvidia-shield-controller-driver)**:
  Linux, wireless, via ozwpan plus Shield patches and wpa_supplicant scripts. Written in 2016,
  before AI coding tools. Targets Linux 4.x kernels; newer ones need porting.
- **[keyboardspecialist/nvshield2015-driver](https://github.com/keyboardspecialist/nvshield2015-driver)**:
  Linux, wireless. Repackages the devmapal patches and chunkeey's standalone ozwpan, both
  human-written, with new scripts and docs. Those scripts and docs show signs of AI generation,
  so the devmapal original is the safer pick if that matters to you.

## Status

Works:
- Up to four controllers at once, each its own virtual Xbox 360 pad (ViGEmBus)
- Trackpad -> mouse, volume buttons -> media keys (SendInput), shared by all of them
- Rumble: games' rumble on the virtual pad drives the controller's left and right motors
- WPS push-button pairing, one controller at a time, to a random group made once
- Reconnect on a tap of the NVIDIA button, hidden SSID, WPA2-CCMP
- Adapter LED blinks while waiting, solid while connected
- Battery in the tray, colored for the taskbar theme, with a notification when it goes critical;
  a short rumble when charging starts. Rumble cuts out near empty, and a controller that
  connects flat is sent away until it's plugged in.
- Tray menu: pair, reconfigure the network, and per controller identify, rename, audio on/off
  and disconnect; about
- Runs at login, windowless, and picks the adapter back up after a replug
- Mic and headset audio over VBAN, tested with Voicemeeter Potato

Planned:
- Linux native utility
- PulseAudio / ALSA audio backend

Unplanned:
- A native Windows audio device. That would need a kernel driver, and test-signed drivers don't
  get along with anti-cheat.
- 5 GHz, TX power calibration and IQK on the adapter. 2.4 GHz at fixed power is enough at
  couch distance.

## Supported hardware

**Controller:** NVIDIA SHIELD controller, 2015 Wi-Fi model. Once connected it shows up as USB
device `0955:7210`, "NVIDIA Controller v01.03". The later Bluetooth controllers aren't Ozmo
devices and don't apply.

**Adapter:** Realtek RTL8811AU / RTL8821AU, 1T1R 802.11ac USB, at VID:PID `0bda:0811`.
- Tested: ALFA Network AWUS036ACS, AC600 dual-band with an external antenna. It reports Realtek's
  stock `0bda:0811` and efuse strings "Realtek" / "802.11ac WLAN Adapter", so Zadig lists it
  under that name rather than ALFA's.
- Other 8811AU/8821AU adapters with vendor-specific IDs should work once their ID is added to
  `rtlusb/src/usb.rs`. Untested.
- Not supported: RTL8812AU (2T2R, different tables), RTL8814AU, and anything on rtw89 or
  older rtl8xxxu chips.
- The adapter is dedicated to shieldd while it's on WinUSB; Windows can't use it for Wi-Fi.

## Requirements

Run:
- Windows 10 or 11, x64. Developed on Windows 11.
- [ViGEmBus](https://github.com/nefarius/ViGEmBus/releases) for the virtual pad.
  ViGEmBus was retired in 2023 over a trademark conflict with ViGEM GmbH and its repo archived; it
  won't get any more updates, security fixes included. The last release still installs and works.
  Its updater phoned home to a domain the project no longer holds, so leave it off or install
  the final version 1.22.0; see the [End of Life notice](https://docs.nefarius.at/projects/ViGEm/End-of-Life/).
- [Zadig](https://zadig.akeo.ie/), once, to put the adapter on WinUSB.
- [Voicemeeter](https://vb-audio.com/Voicemeeter/) or another VBAN app, only for controller
  audio; see [Audio](#audio).

Build:
- Rust 1.88 or newer, `x86_64-pc-windows-msvc` toolchain (edition 2024, let chains).
- Visual Studio Build Tools with the "Desktop development with C++" workload, for the MSVC linker.

## Build

```powershell
cargo build --release
cargo test --release
```

Binaries land in `target\release`: `shieldd.exe`, its windowless twin `shieldw.exe` for running
at login, and the `rtlusb.exe` bring-up tool.

On Linux, `cargo build --release -p shieldd` builds only the `--transmit` side of the WSL
bridge; see [Other modes](#other-modes).

## Adapter setup

One time per adapter:
1. Plug in the adapter. If it's attached to WSL through usbipd, detach it first.
2. Open Zadig, Options -> List All Devices.
3. Pick the adapter, USB ID `0BDA 0811`. Its name is usually "802.11ac WLAN Adapter".
4. Target driver: WinUSB. Replace Driver.

The adapter disappears from Windows' Wi-Fi list; that's expected. To undo, uninstall the device
in Device Manager with "Attempt to remove the driver for this device" ticked, then replug.

The USB serial on these adapters is a generic `00E04C000001`, so two of them can't be told apart
by serial. shieldd opens the first `0bda:0811` it finds.

## Pairing

```powershell
.\target\release\shieldd.exe --pair
```

Hold the controller's NVIDIA button until its light flashes. shieldd hands the saved group over
by WPS push-button; the controller then rejoins on its own and shieldd carries on hosting.
Pairing stays open until one controller pairs, or 120 s; run `--pair` again, or pick
**Pair controller...** in the tray, for each more. Controllers already connected stay connected.

The group is made once: `DIRECT-<2 chars>-<adjective>-<noun>` with a random passphrase, saved
right away. `--pair` makes one if there's none yet.

```powershell
.\target\release\shieldd.exe --reconfigure-network
```

makes a new group in place of the old, then pairs as `--pair`. Every controller paired to the
old group has to pair again.

The group is saved to `--conf`, which defaults to the user's settings folder:
- Windows: `%LOCALAPPDATA%\shieldd\p2p.conf`
- Linux: `$XDG_CONFIG_HOME/shieldd/p2p.conf`, or `~/.config/shieldd/p2p.conf`

A previous file is kept, gzipped, in `backup\` next to it, named for it and when it was last
written, like the logs: `p2p.2026-10-09T14_01_04.conf.gz`. The file is in wpa_supplicant's P2P format, except that
the key is sealed with Windows DPAPI as `psk=dpapi:<base64>`: only the Windows user who saved it
can read it, and only on that machine. A plain `psk=`, a quoted passphrase or 64 hex digits, is
sealed in place the first time the default file is read, and in its backup when a new group
replaces it. A file given with `--conf` is read, backed up and written with a plain key, since
wpa_supplicant or something else may read it too. Moving to another machine or user means
pairing again. `rtlusb.exe ap` reads sealed files as well. The key is never logged.

Controller names go in `known-clients` alongside, for the tray: one a line, the MAC,
whitespace, then the name, like a hosts file, with `#` lines as comments. A controller starts out
named for the model it gives while pairing, `nVidiaBlake` for this one. An older `p2p.models` is
moved over on first run.

Before making a new group, shieldd spends ~4.5 s listening to channels 1-11 and hosts it on
the quietest of 1, 6 and 11, counting both how loud and how busy each channel and its
neighbors are. It only hears Wi-Fi, so microwaves and Bluetooth don't count. Controllers
rejoin on whatever channel the group was made on, read back from the file's `frequency=` line;
reconfigure the network to move.

## Use

```powershell
.\target\release\shieldd.exe
```

Tap the NVIDIA button. The adapter's LED goes solid once a controller is connected. Each one
gets its own virtual pad and XInput port, up to four; a fifth is sent away, unless one of the
four is gone, whose pad it takes over. A controller that drops out comes back on the same port,
as long as the adapter stays up: a replug or a failed run starts the pads over, and ports may
shuffle.
The trackpads share the mouse: a button stays down while any controller holds it. Ctrl+C to
stop; the adapter powers down and the virtual pads unplug.

Options worth knowing, the rest in `shieldd --help`:
- `--mouse-scale N`, `--mouse-scale-x`, `--mouse-scale-y`: trackpad speed.
- `--mic`: keep the controller's mic stream open even without VBAN, which keeps the controller from
  dozing between inputs.
- `RUST_LOG`: log level, e.g. `shieldd=debug,rtlusb=debug`. `trace` dumps every frame.
- `SHIELDD_PS_HOLD_MS`: how long replies wait for a dozing controller, default 30, 0 to send
  at once.

### Tray

The icon shows the battery of the controller lowest on charge, and its tooltip each one's: a
bolt while it charges, the bolt flashing green for
5 s when it's plugged in; an all-green full battery once it's charged, like the controller's
light turning green, which it reads from the charger letting go after a top-off, or from one
plugged in full getting no top-off within 30 s; and an all-red one with a flashing warning mark
when it's critical. A crossed-out signal flashes red for 5 s at startup and when the
last controller drops off on its own rather than sent away. A spinner turns while one is
joining with none up yet, and a Wi-Fi signal lights up blue, a bar at a time, while pairing.
Notifications, naming the controller, say when it goes on the charger, is fully charged, gets
low in the 25% band, turns critical and is empty. With more than one controller connected, a
blue badge in the icon's lower right corner says how many. A controller joining or leaving
Wi-Fi plays Windows' Device Connect or Device Disconnect sound, as a USB device would.

Click for a menu. Its title shows the version, how many controllers are connected, and the
group's name and channel; clicking the version opens an About box with the build date, license and
notices. Clicking the tray icon again closes the menu.

Each connected controller has a line with its name, XInput port and battery, opening a submenu
headed by its MAC and firmware version, as the SHIELD app shows it. The bars beside it are its
link: three green is good, two yellow OK, one red poor, and none means it's about to drop or
hasn't been heard from in 5 seconds. They go by its signal at the adapter and by how many frames
need resending either way. A change shows once it's held for 2 seconds, a drop at once, and each
one is logged; with the menu open, the bars catch up the next time it opens. Its shortcut is the
first letter of its name the menu hasn't used yet:
- **Identify**: buzzes the controller twice, to tell which one it is.
- **Rename...**: up to 15 characters, saved to `known-clients`.
- **Enable / Disable audio**: its mic and headset over VBAN, see below. Remembered between runs.
- **Disconnect**: says goodbye to the controller over Ozmo, drops it from Wi-Fi, then ignores
  it for 30 s so it gives up and goes to sleep. A controller that connects flat gets the same.
  Meanwhile it's listed, grayed out, as "(shunned)".

The rest:
- **Pair controller...**: pairs one more controller to the network as `--pair` does. Nobody's
  dropped. Grayed out while pairing's open, and with four connected, as many as there are
  XInput ports.
- **Reconfigure network**: a new network as `--reconfigure-network` makes, after a warning,
  since every paired controller is disconnected and has to pair again. Pairing opens right
  after.
- **Exit**

### Audio

With VBAN on, a controller's audio goes through
[Voicemeeter](https://vb-audio.com/Voicemeeter/) over VBAN on localhost. The streams are up
while any controller has VBAN on, and shared: their mics are mixed into one, and every headset
plays the same audio.
- Mic: stream `ShieldMic`, 16 kHz, sent to port 6980. Add it as an incoming stream.
- Headset jack: stream `ShieldPad`, PCM 16-bit at any rate, received on port 6981. Point an
  outgoing stream at `127.0.0.1:6981`. Played ~12 dB down, since the jack has no volume of its
  own.

### At login

```powershell
.\target\release\shieldd.exe --install
```

Starts `shieldw.exe` at login for this user, with any other options given alongside `--install`;
a `--conf` among them has to be an absolute path. It waits for the adapter, picks it back up after it's unplugged, and logs to
`%LOCALAPPDATA%\shieldd\logs\latest.log`, as does a hosting `shieldd`. Earlier runs' logs are
gzipped next to it, named for when they started, UTC: `2026-10-09T14_01_04.log.gz`. It shows under
Task Manager's startup apps, where it can be turned off too.
- `shieldd --stop`: stop the running one, e.g. to pair or rebuild. Only one runs at a time.
- `shieldd --uninstall`: don't start at login anymore.
- `shieldd --background`: the same keep-running behavior in a console.

## Other modes

An older path, kept because it still works:
- `--receiver` (Windows) + `--transmit` (Linux): the WSL bridge. The adapter is attached to WSL,
  Linux's ozwpan driver enumerates the controller, and `--transmit` forwards its input to
  `--receiver` over UDP.

## Layout

| Path | What |
| --- | --- |
| `shdtray/` | The tray: what it shows and sends back, the battery gauge, the Windows tray, its icon SVGs in `res/` and its text in `res/locales/` |
| `shieldw/` | shieldd without a console window, for running at login |
| `shdlib/` | Shared by the other crates: the firmware version, base64, `psk=` values and their DPAPI seal |
| `shdnet/` | MAC addresses and how they're shown; no dependencies, so ozproto can use it |
| `ozproto/` | Ozmo protocol: wire format and host session state machine, no I/O |
| `rtlusb/` | Userspace RTL8811AU driver over WinUSB: init, firmware, PHY, AP, WPA2, WPS |
| `shieldd/` | The host: radio, controller setup, outputs, pairing, tray, login start, WSL bridge receiver |
| `shdlink/` | Packet format shared by `--transmit` and `--receiver` |
| `tools/` | rtw88 table generator, Python |

The tray's text, its menu, tooltips, dialogs and notices, is in Fluent files,
`shdtray/res/locales/<language>/shdtray.ftl`, picked by Windows' display language with US
English as the fallback. To add a language, copy `en-US`, translate it, and add it to `LOCALES`
in `shdtray/src/l10n.rs`; anything left out stays English. CLI help, errors and logs are
English only.

`rtlusb.exe efuse | rx | ap` are bring-up tools for the adapter: dump efuse, print received
frames, or run a bare AP. Each run logs to `%LOCALAPPDATA%\shieldd\logs\rtlusb\latest.log` too,
earlier runs gzipped next to it as for shieldd.

## Prior art

shieldd stands on:
- **rtw88**, the Linux Realtek 802.11ac driver. `rtlusb` is a port of its 8821a USB path, and
  its tables are generated from rtw88's.
- **ozwpan**, Linux's former Ozmo driver. `ozproto` follows its wire format and session
  handling, corrected against captures of a SHIELD Tablet.
- **The Realtek 8812AU/8821AU vendor driver** and its halmac layer, for a few TX descriptor
  fields rtw88 doesn't use.
- **hostapd / wpa_supplicant**, as the behavior reference for the 4-way handshake and the WPS
  registrar.
- **linux-firmware**, for `rtw8821a_fw.bin`, embedded unmodified.

Details, licenses and the full Rust crate list are in [THIRD_PARTY.md](THIRD_PARTY.md).

## License

GPL-2.0-only, see [LICENSE](LICENSE). rtlusb and ozproto derive from GPL-2.0 sources: rtw88,
taken under its GPL-2.0 option, the Realtek vendor driver and ozwpan. The embedded firmware is
a separate, redistributable binary under Realtek's own license; see THIRD_PARTY.md.
