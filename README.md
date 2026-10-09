# shieldd

Native Windows host for the 2015 NVIDIA SHIELD controller. The controller talks Ozmo
"USB over Wi-Fi" over Wi-Fi Direct; shieldd runs the access point itself on a Realtek
RTL8811AU USB adapter, through its own userspace driver over WinUSB, and turns the
controller into a virtual Xbox 360 pad, a mouse and media keys.

No Bluetooth, no vendor app, no Linux VM. Input latency is under ~20 ms.

## AI disclosure

Most of this project's code, docs and comments were written by an AI model, Anthropic's
Claude via Claude Code, under the direction of a human maintainer. The maintainer set the
goals and constraints, tested on real hardware, and judged the results; the protocol work
leaned on captures and testing rather than on the model's say-so. Even so, assume any given
line was machine-written.

If you'd rather not run or build on AI-generated code, this isn't the project for you. Other
ways to use the controller on a PC:
- **[Syniurge/ShieldControllerWinDriver](https://github.com/Syniurge/ShieldControllerWinDriver)**,
  continued in **[nefarius' fork](https://github.com/nefarius/ShieldControllerWinDriver)**:
  Windows HID filter driver. Wired only. Written 2016-2017, before AI coding tools.
- **NVIDIA's own drivers:** wired only, and only on PCs w/ NVIDIA graphics. Closed source, so
  there's no checking, but NVIDIA has said publicly that its engineers use AI coding tools;
  assume current releases are at least partly AI-written.
- **[devmapal/nvidia-shield-controller-driver](https://github.com/devmapal/nvidia-shield-controller-driver)**:
  Linux, wireless, via ozwpan plus Shield patches and wpa_supplicant scripts. Written in 2016,
  before AI coding tools. Targets Linux 4.x kernels; newer ones need porting.
- **[keyboardspecialist/nvshield2015-driver](https://github.com/keyboardspecialist/nvshield2015-driver)**:
  Linux, wireless. Repackages the devmapal patches and chunkeey's standalone ozwpan, both
  human-written, w/ new scripts and docs. Those scripts and docs show signs of AI generation,
  so the devmapal original is the safer pick if that matters to you.

## Status

Works:
- Gamepad -> virtual Xbox 360 pad (ViGEmBus)
- Trackpad -> mouse, volume buttons -> media keys (SendInput)
- Rumble: games' rumble on the virtual pad drives the controller's left and right motors
- WPS push-button pairing to a fresh random group
- Reconnect on a tap of the NVIDIA button, hidden SSID, WPA2-CCMP
- Adapter LED blinks while waiting, solid while connected
- Battery in the tray, colored for the taskbar theme, w/ a notification when it goes critical;
  a short rumble when charging starts. Rumble cuts out near empty, and a controller that
  connects flat is sent away until it's plugged in.
- Tray menu: pair, identify, disconnect, VBAN audio on/off
- Runs at login, windowless, and picks the adapter back up after a replug

Not yet, or untested:
- Audio over VBAN: the mic and headset paths are written but not yet tried on the controller.
  No native Windows audio device; that would need a kernel driver, and test-signed drivers
  don't get along w/ anti-cheat.
- 5 GHz, TX power calibration and IQK on the adapter. 2.4 GHz at fixed power is enough at
  couch distance.

## Supported hardware

**Controller:** NVIDIA SHIELD controller, 2015 Wi-Fi model. Once connected it shows up as USB
device `0955:7210`, "NVIDIA Controller v01.03". The later Bluetooth controllers aren't Ozmo
devices and don't apply.

**Adapter:** Realtek RTL8811AU / RTL8821AU, 1T1R 802.11ac USB, at VID:PID `0bda:0811`.
- Tested: ALFA Network AWUS036ACS, AC600 dual-band w/ an external antenna. It reports Realtek's
  stock `0bda:0811` and efuse strings "Realtek" / "802.11ac WLAN Adapter", so Zadig lists it
  under that name rather than ALFA's.
- Other 8811AU/8821AU adapters w/ vendor-specific IDs should work once their ID is added to
  `rtlusb/src/usb.rs`. Untested.
- Not supported: RTL8812AU (2T2R, different tables), RTL8814AU, and anything on rtw89 or
  older rtl8xxxu chips.
- The adapter is dedicated to shieldd while it's on WinUSB; Windows can't use it for Wi-Fi.

## Requirements

Run:
- Windows 10 or 11, x64. Developed on Windows 11.
- [ViGEmBus](https://github.com/nefarius/ViGEmBus/releases) for the virtual pad.
- [Zadig](https://zadig.akeo.ie/), once, to put the adapter on WinUSB.
- [Voicemeeter](https://vb-audio.com/Voicemeeter/) or another VBAN app, only for controller
  audio; see [Audio](#audio).
- [Npcap](https://npcap.com/), only for the legacy `--wifi-direct` and `--receiver` modes.

Build:
- Rust 1.88 or newer, `x86_64-pc-windows-msvc` toolchain (edition 2024, let chains).
- Visual Studio Build Tools w/ the "Desktop development with C++" workload, for the MSVC linker.
- [Npcap SDK](https://npcap.com/#download), unpacked anywhere, w/ `NPCAP_SDK_DIR` pointing at its
  root. The build links its `wpcap.lib` import library even though the default mode never loads
  Npcap.

## Build

```powershell
$env:NPCAP_SDK_DIR = "C:\path\to\npcap-sdk"
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
in Device Manager w/ "Attempt to remove the driver for this device" ticked, then replug.

The USB serial on these adapters is a generic `00E04C000001`, so two of them can't be told apart
by serial. shieldd opens the first `0bda:0811` it finds.

## Pairing

```powershell
.\target\release\shieldd.exe --pair
```

Hold the controller's NVIDIA button until its light flashes. shieldd makes a new group,
`DIRECT-<2 chars>-<adjective>-<noun>` w/ a random passphrase, and hands it over
by WPS push-button. The controller then rejoins on its own and shieldd carries on hosting.
The pairing window is 120 s.

The group is saved to `--conf`, which defaults to the user's settings folder:
- Windows: `%LOCALAPPDATA%\shieldd\p2p.conf`
- Linux: `$XDG_CONFIG_HOME/shieldd/p2p.conf`, or `~/.config/shieldd/p2p.conf`

A previous file is kept next to it as `p2p.conf.<unix time>.bak`. The file is in
wpa_supplicant's P2P format and holds the passphrase in plain text; the passphrase is never
logged. The model name each controller gives while pairing goes in `p2p.models` alongside,
for the tray.

Before pairing, shieldd spends ~4.5 s listening to channels 1-11 and hosts the new group on
the quietest of 1, 6 and 11, counting both how loud and how busy each channel and its
neighbors are. It only hears Wi-Fi, so microwaves and Bluetooth don't count. The controller
rejoins on whatever channel it was paired on, read back from the file's `frequency=` line;
re-pair to move.

## Use

```powershell
.\target\release\shieldd.exe
```

Tap the NVIDIA button. The adapter's LED goes solid once the controller is connected. Ctrl+C
to stop; the adapter powers down and the virtual pad unplugs.

Options worth knowing, the rest in `shieldd --help`:
- `--mouse-scale N`, `--mouse-scale-x`, `--mouse-scale-y`: trackpad speed.
- `--mic`: keep the controller's mic stream open even w/o VBAN, which keeps the controller from
  dozing between inputs.
- `RUST_LOG`: log level, e.g. `shieldd=debug,rtlusb=debug`. `trace` dumps every frame.
- `SHIELDD_PS_HOLD_MS`: how long replies wait for a dozing controller, default 30, 0 to send
  at once.

### Tray

The icon shows the controller's battery: a flashing bolt while it charges, an all-green full
battery once it's charged, an all-red one w/ a flashing warning mark when it's critical. A
crossed-out signal flashes red for 5 s at startup and when a controller drops off on its own
rather than sent away. A spinner turns while one is joining, and a Wi-Fi signal lights up blue,
a bar at a time, while pairing. A controller joining or leaving Wi-Fi plays Windows' Device
Connect or Device Disconnect sound, as a USB device would.

Click for a menu. Its title shows the version, how many controllers are connected, and the
group's name and channel. The controller line shows its model, XInput port and battery, and its
MAC on hover; clicking it buzzes the controller twice, to tell which one it is. The rest:
- **Pair controller...**, **Re-pair controller...** while one is connected: drops the current
  controller and pairs a new one on a new group, as `--pair` does, after a confirmation, since
  every controller paired to the old group has to pair again. Back to the saved group if nothing
  pairs within 2 minutes.
- **Enable / Disable VBAN**: controller audio, see below. Remembered between runs.
- **Disconnect**: says goodbye to the controller over Ozmo, drops it from Wi-Fi, then ignores
  it for 30 s so it gives up and goes to sleep. A controller that connects flat gets the same.
  Meanwhile it's listed, grayed out, as "(shunned)" when nothing else is connected.
- **Exit**

### Audio

With VBAN on, the controller's audio goes through
[Voicemeeter](https://vb-audio.com/Voicemeeter/) over VBAN on localhost:
- Mic: stream `ShieldMic`, 16 kHz, sent to port 6980. Add it as an incoming stream.
- Headset jack: stream `ShieldPad`, PCM 16-bit at any rate, received on port 6981. Point an
  outgoing stream at `127.0.0.1:6981`. Played ~12 dB down, since the jack has no volume of its
  own.

The headset side is new on this host and untested; on the old Linux path it stuttered.

### At login

```powershell
.\target\release\shieldd.exe --install
```

Starts `shieldw.exe` at login for this user, w/ any other options given alongside `--install`.
It waits for the adapter, picks it back up after it's unplugged, and logs to
`%LOCALAPPDATA%\shieldd\shieldd.log`, the previous run's in `shieldd.old.log`. It shows under
Task Manager's startup apps, where it can be turned off too.
- `shieldd --stop`: stop the running one, e.g. to pair or rebuild. Only one runs at a time.
- `shieldd --uninstall`: don't start at login anymore.
- `shieldd --background`: the same keep-running behavior in a console.

## Other modes

Older paths, kept because they still work:
- `--wifi-direct`: Windows Wi-Fi Direct group owner on the stock Realtek driver, w/ Npcap for
  the Ozmo frames. Needs the adapter on its normal driver and a group paired elsewhere.
  Carries ~330 ms of input lag from the Windows driver's TX queue.
- `--receiver` (Windows) + `--transmit` (Linux): the WSL bridge. The adapter is attached to WSL,
  Linux's ozwpan driver enumerates the controller, and `--transmit` forwards its input to
  `--receiver` over UDP.

## Layout

| Path | What |
| --- | --- |
| `ozproto/` | Ozmo protocol: wire format and host session state machine, no I/O |
| `rtlusb/` | Userspace RTL8811AU driver over WinUSB: init, firmware, PHY, AP, WPA2, WPS |
| `shieldd/` | The host: radio, controller setup, outputs, pairing, tray, login start, legacy modes |
| `shdlink/` | Packet format shared by `--transmit` and `--receiver` |
| `tools/` | Python one-offs: rtw88 table generator, Wi-Fi Direct probe |
| `attic/` | Retired USB/IP bridge, not compiled |

`rtlusb.exe efuse | rx | ap` are bring-up tools for the adapter: dump efuse, print received
frames, or run a bare AP.

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
