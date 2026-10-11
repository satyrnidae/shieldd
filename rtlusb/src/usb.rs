//! Register access over vendor control transfers and bulk endpoint discovery, as rtw88 usb.c.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nusb::MaybeFuture;
use nusb::descriptors::TransferType;
use nusb::transfer::{ControlIn, ControlOut, ControlType, Direction, Recipient};
use tracing::trace;

const VID: u16 = 0x0bda;
const PID: u16 = 0x0811;

/// No adapter that we can open, and what to check
#[cfg(windows)]
const NOT_FOUND: &str = "no 0bda:0811 on WinUSB; is it plugged in, and on WinUSB rather than the Realtek driver?";
#[cfg(not(windows))]
const NOT_FOUND: &str = "no 0bda:0811; is it plugged in?";
#[cfg(windows)]
const OPEN_FAILED: &str = "open device";
#[cfg(not(windows))]
const OPEN_FAILED: &str = "open device; is shieldd's udev rule installed (`sudo shieldd --install-udev-rule`)?";

const REQ: u8 = 0x05;
const TIMEOUT: Duration = Duration::from_millis(500);

/// rtw88 writes firmware in 196-byte control transfers
const FW_BLOCK: usize = 196;

pub struct Rtl {
    iface: nusb::Interface,
    pub speed: Option<nusb::Speed>,
    pub bulk_in: u8,
    pub bulk_out: Vec<u8>,
}

/// The adapter is plugged in, on whatever driver
pub fn present() -> bool {
    nusb::list_devices().wait().is_ok_and(|mut d| d.any(|d| d.vendor_id() == VID && d.product_id() == PID))
}

impl Rtl {
    pub fn open() -> Result<Self> {
        let info =
            nusb::list_devices().wait()?.find(|d| d.vendor_id() == VID && d.product_id() == PID).context(NOT_FOUND)?;
        let dev = info.open().wait().context(OPEN_FAILED)?;
        let speed = dev.speed();
        // On Linux the kernel's own driver, rtw88 or an out-of-tree 8821au, has it first. It's
        // not rebound when we let go; replugging the adapter gives it back.
        let iface = dev.detach_and_claim_interface(0).wait().context("claim interface 0")?;

        let mut bulk_in = None;
        let mut bulk_out = Vec::new();
        let desc = iface.descriptor().context("no interface descriptor")?;
        for ep in desc.endpoints() {
            if ep.transfer_type() != TransferType::Bulk {
                continue;
            }
            match ep.direction() {
                Direction::In => bulk_in = Some(ep.address()),
                Direction::Out => bulk_out.push(ep.address()),
            }
        }
        let bulk_in = bulk_in.context("no bulk IN endpoint")?;
        if !(2..=4).contains(&bulk_out.len()) {
            bail!("{} bulk OUT endpoints, expected 2-4", bulk_out.len());
        }
        Ok(Self { iface, speed, bulk_in, bulk_out })
    }

    pub fn interface(&self) -> &nusb::Interface {
        &self.iface
    }

    //region Raw access

    pub fn read(&self, addr: u16, len: u16) -> Result<Vec<u8>> {
        let ctl = ControlIn {
            control_type: ControlType::Vendor,
            recipient: Recipient::Device,
            request: REQ,
            value: addr,
            index: 0,
            length: len,
        };
        trace!("read {addr:#06x} len {len}");
        let data = self.iface.control_in(ctl, TIMEOUT).wait().with_context(|| format!("read {addr:#06x}"))?;
        trace!("  = {data:02x?}");
        if data.len() != len as usize {
            bail!("read {addr:#06x}: short, {} of {len}", data.len());
        }
        Ok(data)
    }

    pub fn write(&self, addr: u16, data: &[u8]) -> Result<()> {
        if data.len() <= 4 {
            trace!("write {addr:#06x} {data:02x?}");
        } else {
            trace!("write {addr:#06x} {} bytes", data.len());
        }
        let ctl = ControlOut {
            control_type: ControlType::Vendor,
            recipient: Recipient::Device,
            request: REQ,
            value: addr,
            index: 0,
            data,
        };
        self.iface.control_out(ctl, TIMEOUT).wait().with_context(|| format!("write {addr:#06x}"))
    }

    /// Firmware page body to FW_START, rtw_usb_write_firmware_page
    pub fn write_fw_block(&self, data: &[u8]) -> Result<()> {
        let mut addr = crate::regs::FW_START;
        let mut rest = data;
        while !rest.is_empty() {
            let n = if rest.len() >= FW_BLOCK {
                FW_BLOCK
            } else if rest.len() >= 8 {
                8
            } else {
                1
            };
            self.write(addr, &rest[..n])?;
            addr += n as u16;
            rest = &rest[n..];
        }
        Ok(())
    }

    //endregion

    //region Typed access

    pub fn r8(&self, addr: u16) -> Result<u8> {
        Ok(self.read(addr, 1)?[0])
    }

    pub fn r16(&self, addr: u16) -> Result<u16> {
        Ok(u16::from_le_bytes(self.read(addr, 2)?.try_into().unwrap()))
    }

    pub fn r32(&self, addr: u16) -> Result<u32> {
        Ok(u32::from_le_bytes(self.read(addr, 4)?.try_into().unwrap()))
    }

    pub fn w8(&self, addr: u16, v: u8) -> Result<()> {
        self.write(addr, &[v])
    }

    pub fn w16(&self, addr: u16, v: u16) -> Result<()> {
        self.write(addr, &v.to_le_bytes())
    }

    pub fn w32(&self, addr: u16, v: u32) -> Result<()> {
        self.write(addr, &v.to_le_bytes())
    }

    pub fn set8(&self, addr: u16, bits: u8) -> Result<()> {
        let v = self.r8(addr)?;
        self.w8(addr, v | bits)
    }

    pub fn clr8(&self, addr: u16, bits: u8) -> Result<()> {
        let v = self.r8(addr)?;
        self.w8(addr, v & !bits)
    }

    pub fn set16(&self, addr: u16, bits: u16) -> Result<()> {
        let v = self.r16(addr)?;
        self.w16(addr, v | bits)
    }

    pub fn clr16(&self, addr: u16, bits: u16) -> Result<()> {
        let v = self.r16(addr)?;
        self.w16(addr, v & !bits)
    }

    pub fn set32(&self, addr: u16, bits: u32) -> Result<()> {
        let v = self.r32(addr)?;
        self.w32(addr, v | bits)
    }

    pub fn clr32(&self, addr: u16, bits: u32) -> Result<()> {
        let v = self.r32(addr)?;
        self.w32(addr, v & !bits)
    }

    /// rtw_read32_mask: the field under `mask`, shifted down
    pub fn r32_mask(&self, addr: u16, mask: u32) -> Result<u32> {
        Ok((self.r32(addr)? & mask) >> mask.trailing_zeros())
    }

    /// rtw_write32_mask: replace the field under `mask` with `v`
    pub fn w32_mask(&self, addr: u16, mask: u32, v: u32) -> Result<()> {
        let old = self.r32(addr)?;
        self.w32(addr, (old & !mask) | ((v << mask.trailing_zeros()) & mask))
    }

    /// Poll until `(r8(addr) & mask) == target`
    pub fn poll8(&self, addr: u16, mask: u8, target: u8, timeout: Duration) -> Result<()> {
        let start = Instant::now();
        loop {
            if self.r8(addr)? & mask == target & mask {
                return Ok(());
            }
            if start.elapsed() > timeout {
                bail!("poll {addr:#06x} mask {mask:#04x} for {target:#04x} timed out");
            }
        }
    }

    /// Poll until `r32_mask(addr, mask) == target`, rtw88 check_hw_ready
    pub fn poll32(&self, addr: u16, mask: u32, target: u32, timeout: Duration) -> Result<bool> {
        let start = Instant::now();
        loop {
            if self.r32_mask(addr, mask)? == target {
                return Ok(true);
            }
            if start.elapsed() > timeout {
                return Ok(false);
            }
        }
    }

    //endregion
}
