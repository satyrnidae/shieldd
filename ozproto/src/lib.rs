//! Ozmo "USB over Wi-Fi" protocol, as spoken by the 2015 NVIDIA SHIELD controller.
//!
//! Pure protocol logic w/ no I/O, so it builds and tests anywhere. The wire
//! format follows Linux `ozwpan` (`ozprotocol.h`); where the SHIELD
//! Tablet's behavior differs from ozwpan, the tablet wins, since the controller
//! was built against it.

pub mod session;
pub mod usb;
pub mod wire;

pub use session::{Config, Event, Host, Mac};
