//! Userspace RTL8811AU driver over WinUSB (plan B, HANDOFF §9f).
//!
//! Register access, init and tables follow rtw88 (usb.c, mac.c, rtw88xxa.c, rtw8821a*.c)
//! and the Realtek 8812/8821 vendor driver. [`radio::Radio`] is the entry point for hosts.

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
