//! 8051 firmware download, as rtw88 mac.c __rtw_download_firmware_legacy.

use std::thread::sleep;
use std::time::Duration;

use anyhow::{Result, bail};
use tracing::{debug, info, warn};

use crate::regs::*;
use crate::usb::Rtl;

pub static FIRMWARE: &[u8] = include_bytes!("../firmware/rtw8821a_fw.bin");

const HDR_SIZE: usize = 32;
const PAGE_SIZE: usize = 0x1000;

pub struct FwInfo {
    pub signature: u16,
    pub version: u16,
    pub subversion: u8,
}

pub fn info(fw: &[u8]) -> FwInfo {
    FwInfo {
        signature: u16::from_le_bytes([fw[0], fw[1]]),
        version: u16::from_le_bytes([fw[4], fw[5]]),
        subversion: fw[6],
    }
}

fn wlan_cpu_enable(rtl: &Rtl, enable: bool) -> Result<()> {
    if enable {
        rtl.set8(RSV_CTRL + 1, WLMCU_IOIF)?;
        rtl.set8(SYS_FUNC_EN + 1, FEN_CPUEN)
    } else {
        rtl.clr8(SYS_FUNC_EN + 1, FEN_CPUEN)?;
        rtl.clr8(RSV_CTRL + 1, WLMCU_IOIF)
    }
}

fn enable_download(rtl: &Rtl, en: bool) -> Result<()> {
    if !en {
        return rtl.clr8(MCUFW_CTRL, MCUFWDL_EN);
    }
    wlan_cpu_enable(rtl, false)?;
    wlan_cpu_enable(rtl, true)?;
    rtl.set8(MCUFW_CTRL, MCUFWDL_EN)?;
    let mut ready = false;
    for _ in 0..10 {
        if rtl.r8(MCUFW_CTRL)? & MCUFWDL_EN != 0 {
            ready = true;
            break;
        }
        rtl.set8(MCUFW_CTRL, MCUFWDL_EN)?;
        sleep(Duration::from_millis(20));
    }
    if !ready {
        warn!("firmware download enable never latched");
    }
    rtl.clr32(MCUFW_CTRL, ROM_DLEN)
}

fn write_pages(rtl: &Rtl, body: &[u8]) -> Result<()> {
    for attempt in 0..5 {
        rtl.set8(MCUFW_CTRL, FWDL_CHK_RPT)?;
        for (page, chunk) in body.chunks(PAGE_SIZE).enumerate() {
            let t = std::time::Instant::now();
            rtl.w32_mask(MCUFW_CTRL, ROM_PGE, page as u32)?;
            rtl.write_fw_block(chunk)?;
            debug!(page, ms = t.elapsed().as_millis() as u64, "firmware page");
        }
        if rtl.poll32(MCUFW_CTRL, FWDL_CHK_RPT as u32, 1, Duration::from_millis(50))? {
            return Ok(());
        }
        debug!(attempt, "firmware checksum report not set, retrying");
    }
    bail!("firmware download: checksum report never set")
}

fn validate(rtl: &Rtl) -> Result<()> {
    let v = rtl.r32(MCUFW_CTRL)?;
    rtl.w32(MCUFW_CTRL, (v | MCUFWDL_RDY) & !WINTINI_RDY)?;
    wlan_cpu_enable(rtl, false)?;
    wlan_cpu_enable(rtl, true)?;
    for _ in 0..10 {
        if rtl.r32(MCUFW_CTRL)? & FW_READY == FW_READY {
            return Ok(());
        }
        sleep(Duration::from_millis(20));
    }
    bail!("firmware didn't come up, MCUFW_CTRL {:#010x}", rtl.r32(MCUFW_CTRL)?)
}

pub fn download(rtl: &Rtl, fw: &[u8]) -> Result<()> {
    let i = info(fw);
    info!(
        signature = format!("{:#06x}", i.signature),
        version = i.version,
        sub = i.subversion,
        size = fw.len(),
        "downloading firmware"
    );
    enable_download(rtl, true)?;
    let res = write_pages(rtl, &fw[HDR_SIZE..]);
    enable_download(rtl, false)?;
    res?;
    validate(rtl)
}
