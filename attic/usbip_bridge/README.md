# USB/IP bridge (retired 2026-10-07)

Not compiled. `shieldd` once exported the controller over USB/IP so usbip-win2 could present it
to Windows as a real USB device, plus a `shieldd pair` probe for WPS pairing via WinRT.

Retired because usbip-win2 breaks WSL2 on this machine, and the pairing probe proved Windows'
GO can't pair a plain WPS enrollee. Why and what was learned: HANDOFF.md §9c.

- `bridge.rs`: URB <-> Ozmo mapping, EP0 queue, descriptor cache, HID init injection
- `usbip.rs`: USB/IP v1.1.1 server-side codec, w/ tests
- `net.rs`: TCP accept/reader threads
- `pair.rs`: non-legacy WinRT GO pairing probe
