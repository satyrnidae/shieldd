# USB/IP bridge (retired 2026-10-07)

Not compiled. `shieldd` once exported the controller over USB/IP so usbip-win2 could present it
to Windows as a real USB device, plus a `shieldd pair` probe for WPS pairing via WinRT.

Retired because usbip-win2 breaks WSL2 on this machine, and the pairing probe proved Windows'
GO can't pair a plain WPS enrollee: the controller in pairing mode never shows up as a Wi-Fi
Direct peer, and Windows' GO runs no WPS registrar for a plain enrollee.

Lessons:
- With usbip-win2's drivers active, Hyper-V can't attach WSL's system.vhd
  (`Wsl/Service/CreateInstance/CreateVm/HCS/ERROR_NOT_SUPPORTED`). Removing it fixed WSL.
- usbip-win2 keeps the device node alive across reconnects and doesn't resend
  SET_CONFIGURATION, so the bridge configures the controller and runs the HID init itself.
- The session layer must answer every trigger. One frame in flight made each request take
  ~1.5 s, since the controller dozes when a trigger goes unanswered.

- `bridge.rs`: URB <-> Ozmo mapping, EP0 queue, descriptor cache, HID init injection
- `usbip.rs`: USB/IP v1.1.1 server-side codec, w/ tests
- `net.rs`: TCP accept/reader threads
- `pair.rs`: non-legacy WinRT GO pairing probe
