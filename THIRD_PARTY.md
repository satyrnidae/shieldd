# Third-party notices

This project is licensed under GPL-2.0-only (see LICENSE). It ports, embeds or follows the
work below. Each entry says what was taken and under which license.

## Ported or derived code

### rtw88, Linux Realtek 802.11ac driver
- **Used for:** `rtlusb`. Register access, power-on sequences, firmware download, efuse decode,
  PHY/RF setup, TX/RX descriptors and security CAM follow rtw88's `usb.c`, `mac.c`, `fw.c`,
  `efuse.c`, `rtw88xxa.c` and `rtw8821a*.c`. `rtlusb/src/tables.rs` is generated from
  `rtw8821a_table.c` by `tools/gen_rtw_tables.py`.
- **License:** GPL-2.0 OR BSD-3-Clause, taken here under GPL-2.0.
- **Copyright:** (c) 2018-2024 Realtek Corporation.
- **Source:** https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git,
  `drivers/net/wireless/realtek/rtw88/`

### Realtek RTL8812AU/8821AU vendor driver, incl. halmac
- **Used for:** cross-checking rtw88. The per-frame retry limit bits in the TX descriptor come
  from halmac's 88xx layout.
- **License:** GPL-2.0.
- **Copyright:** (c) Realtek Corporation.

### ozwpan, Linux Ozmo USB-over-WPAN driver
- **Used for:** `ozproto`. Wire format, element types and session behavior follow ozwpan's
  `ozprotocol.h` and host code, where the SHIELD Tablet's observed behavior doesn't override it.
- **License:** GPL-2.0.
- **Copyright:** (c) 2011 Ozmo Inc.
- **Source:** Linux `drivers/staging/ozwpan/`, removed from mainline in 4.x.

## Behavior references, no code taken

- **hostapd / wpa_supplicant**, BSD-3-Clause, (c) Jouni Malinen and contributors.
  `wpa_auth.c` and `wps_registrar.c` were the behavior reference for the 4-way handshake and
  the WPS registrar; the WMM parameter defaults match hostapd's. The saved group file uses
  wpa_supplicant's config format.
- **Specifications:** IEEE 802.11-2020, Wi-Fi Simple Configuration 2.0, RFC 3526 (DH group 5
  prime), RFC 3394 (AES key wrap).

## Embedded firmware

### rtw8821a_fw.bin
- **Used for:** `rtlusb/firmware/rtw8821a_fw.bin`, embedded in `rtlusb` and downloaded to the
  adapter's 8051 at startup. Unmodified; SHA-256
  `4693ece655044d96f81bd3f3e8db14dc352b493219c4fd0a75788a6fbd9eac3c`, matching linux-firmware's
  `rtw88/rtw8821a_fw.bin`.
- **License:** redistributable binary, `LICENCE.rtlwifi_firmware.txt`
  (SPDX `LicenseRef-LICENCE.rtlwifi_firmware.txt`). Not covered by this project's GPL; it is
  a separate work that runs on the adapter, not on the host.
- **Source:** https://git.kernel.org/pub/scm/linux/kernel/git/firmware/linux-firmware.git

Full license text, as required for redistribution:

```
Copyright (c) 2010, Realtek Semiconductor Corporation
All rights reserved.

Redistribution.  Redistribution and use in binary form, without
modification, are permitted provided that the following conditions are
met:

* Redistributions must reproduce the above copyright notice and the
  following disclaimer in the documentation and/or other materials
  provided with the distribution.
* Neither the name of Realtek Semiconductor Corporation nor the names of its
  suppliers may be used to endorse or promote products derived from this
  software without specific prior written permission.
* No reverse engineering, decompilation, or disassembly of this software
  is permitted.

Limited patent license. Realtek Semiconductor Corporation grants a world-wide,
royalty-free, non-exclusive license under patents it now or hereafter
owns or controls to make, have made, use, import, offer to sell and
sell ("Utilize") this software, but solely to the extent that any
such patent is necessary to Utilize the software alone, or in
combination with an operating system licensed under an approved Open
Source license as listed by the Open Source Initiative at
http://opensource.org/licenses.  The patent license shall not apply to
any other combinations which include this software.  No hardware per
se is licensed hereunder.

DISCLAIMER.  THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND
CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING,
BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND
FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE
COPYRIGHT OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT,
INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING,
BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS
OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND
ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR
TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE
USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH
DAMAGE.
```

## Runtime dependencies, not distributed

Installed separately by the user; nothing from them ships with this project.

- **ViGEmBus**, BSD-3-Clause, (c) Nefarius Software Solutions e.U. Virtual Xbox 360 pad.
- **Npcap**, Npcap License, (c) Insecure.Com LLC. Only `--wifi-direct` and `--receiver` load
  it; the build links its import library from the Npcap SDK.
- **WinUSB**, Microsoft, part of Windows.

## Icons

### Ikono Bold Line Icons
- **Used for:** the tray icons, `res/battery-*-svgrepo-com.svg` and
  `res/loading-spinner-svgrepo-com.svg`, embedded in `shieldd` and recolored per theme at
  runtime. `battery-empty` is the outline of the set's battery icons.
- **License:** MIT.
- **Author:** Will Kelly. Retrieved via SVG Repo, https://www.svgrepo.com

```
MIT License

Copyright (c) Will Kelly

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

### Clarity Project Icons
- **Used for:** the tray icons while no controller is connected, `res/disconnected-svgrepo-com.svg`,
  and while pairing, `res/wifi-svgrepo-com.svg`, embedded in `shieldd` and recolored per theme
  at runtime.
- **License:** MIT.
- **Author:** VMware. Retrieved via SVG Repo, https://www.svgrepo.com

```
MIT License

Copyright (c) VMware, Inc.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Rust crates

Statically linked from crates.io. All are permissive and GPL-2.0 compatible; Apache-2.0 crates
are all dual-licensed w/ MIT. Their license texts ship in each crate's source. Regenerate this
list w/ `cargo metadata` after changing dependencies.

**MIT OR Apache-2.0**: aes 0.8.4, aes-kw 0.2.1, anstream 1.0.0, anstyle 1.0.14, anstyle-parse 1.0.0, anstyle-query 1.1.5, anstyle-wincon 3.0.11, anyhow 1.0.104, arrayvec 0.7.8, autocfg 1.5.1, bitflags 1.3.2, bitflags 2.13.2, block-buffer 0.10.4, block-padding 0.3.3, bumpalo 3.20.3, cbc 0.1.2, cc 1.6.0, cfg-if 1.0.5, cipher 0.4.4, clap 4.6.7, clap_builder 4.6.7, clap_derive 4.6.7, clap_lex 1.1.1, colorchoice 1.0.5, core-foundation 0.10.1, core-foundation-sys 0.8.7, cpufeatures 0.2.17, crc32fast 1.5.2, crypto-common 0.1.7, ctrlc 3.5.2, data-url 0.3.2, digest 0.10.7, errno 0.2.8, errno 0.3.14, euclid 0.22.14, fdeflate 0.3.7, find-msvc-tools 0.1.14, flate2 1.1.10, futures-core 0.3.34, futures-task 0.3.34, futures-util 0.3.34, getrandom 0.2.17, heck 0.5.0, hmac 0.12.1, inout 0.1.4, io-kit-sys 0.5.0, is_terminal_polyfill 1.70.2, js-sys 0.3.106, kurbo 0.13.1, lazy_static 1.5.1, libc 0.2.190, log 0.4.34, num-bigint 0.4.8, num-integer 0.1.47, num-traits 0.2.19, nusb 0.2.7, once_cell 1.21.4, once_cell_polyfill 1.70.2, pbkdf2 0.12.2, pcap 2.5.0, pin-project-lite 0.2.17, pkg-config 0.3.34, png 0.18.1, polycool 0.4.0, ppv-lite86 0.2.21, proc-macro2 1.0.107, quote 1.0.47, rand 0.8.8, rand_chacha 0.3.1, rand_core 0.6.4, regex 1.13.1, regex-automata 0.4.18, regex-syntax 0.8.11, resvg 0.48.1, roxmltree 0.21.1, rustversion 1.0.23, sha1 0.10.7, sha2 0.10.9, shlex 2.0.1, simplecss 0.2.2, siphasher 1.0.4, smallvec 1.16.2, svgtypes 0.16.1, syn 2.0.119, syn 3.0.6, thread_local 1.1.10, typenum 1.20.1, usvg 0.48.1, utf8parse 0.2.2, version_check 0.9.5, wasm-bindgen 0.2.129, wasm-bindgen-futures 0.4.79, wasm-bindgen-macro 0.2.129, wasm-bindgen-macro-support 0.2.129, wasm-bindgen-shared 0.2.129, web-sys 0.3.106, winapi 0.3.9, winapi-i686-pc-windows-gnu 0.4.0, winapi-x86_64-pc-windows-gnu 0.4.0, windows 0.62.2, windows-collections 0.3.2, windows-core 0.62.2, windows-future 0.3.2, windows-implement 0.60.2, windows-interface 0.59.3, windows-link 0.2.1, windows-numerics 0.3.1, windows-result 0.4.1, windows-strings 0.5.1, windows-sys 0.36.1, windows-sys 0.61.2, windows-threading 0.2.1, windows_aarch64_msvc 0.36.1, windows_i686_gnu 0.36.1, windows_i686_msvc 0.36.1, windows_x86_64_gnu 0.36.1, windows_x86_64_msvc 0.36.1

**MIT**: block2 0.6.2, cfg_aliases 0.2.2, errno-dragonfly 0.1.2, float-cmp 0.9.0, generic-array 0.14.7, imagesize 0.15.0, matchers 0.2.0, names 0.14.0, nix 0.31.3, nu-ansi-term 0.50.3, objc2 0.6.5, objc2-encode 4.1.0, pico-args 0.5.0, rgb 0.8.53, sharded-slab 0.1.7, simd-adler32 0.3.10, slab 0.4.12, strict-num 0.1.1, strsim 0.11.1, tokio 1.53.2, tracing 0.1.44, tracing-attributes 0.1.31, tracing-core 0.1.36, tracing-log 0.2.0, tracing-subscriber 0.3.23, valuable 0.1.1, vigem-client 0.1.4

**BSD-3-Clause**: subtle 2.6.1, tiny-skia 0.12.0, tiny-skia-path 0.12.0

**Apache-2.0 WITH LLVM-exception OR MIT OR Apache-2.0**: linux-raw-sys 0.12.1, rustix 1.1.5, wasi 0.11.1+wasi-snapshot-preview1

**BSD-2-Clause OR MIT OR Apache-2.0**: mach2 0.5.0, zerocopy 0.8.61, zerocopy-derive 0.8.61

**MIT OR Zlib OR Apache-2.0**: miniz_oxide 0.8.9, miniz_oxide 0.9.1

**Unlicense OR MIT**: aho-corasick 1.1.5, memchr 2.8.3

**Zlib OR MIT OR Apache-2.0**: bytemuck 1.25.2, dispatch2 0.3.1

**(MIT OR Apache-2.0) AND Unicode-3.0**: unicode-ident 1.0.26

**ISC**: libloading 0.8.9

**BSD-2-Clause**: arrayref 0.3.9

**Zlib**: zlib-rs 0.6.8

**0BSD OR MIT OR Apache-2.0**: adler2 2.0.1
