//! Userspace RTL8811AU driver over WinUSB. The Windows Wi-Fi Direct stack holds frames for the
//! dozing controller, ~330 ms of input lag; here a reply goes out as soon as it's made.
//!
//! Register access, init and tables follow rtw88 (usb.c, mac.c, fw.c, efuse.c, phy.c, rx.c,
//! tx.c, sec.c, mac80211.c, rtw88xxa.c, rtw8821a*.c and their headers) and the Realtek
//! 8812/8821 vendor driver. See THIRD_PARTY.md. [`radio::Radio`] is the entry point for hosts.

pub mod ap;
pub mod bss;
pub mod chip;
pub mod efuse;
pub mod fw;
pub mod phy;
pub mod radio;
pub mod regs;
pub mod rx;
pub mod sec;
pub mod survey;
pub mod tables;
pub mod tx;
pub mod usb;
pub mod wpa;
pub mod wps;

pub use radio::{ApOptions, Link, Radio, RadioEvent};
