//! Hardware CCMP: security engine and key CAM, as rtw88 sec.c and rtw_ops_set_key.
//!
//! Default key search is on, like rtw88: CAM entries 0-3 hold group keys by key index,
//! pairwise keys go in entry 4 and up, one per station, matched by station address.

use anyhow::Result;

use crate::chip::Chip;
use crate::regs::*;

const SEC_WRITE: u16 = 0x0674;
const SEC_CONFIG: u16 = 0x0680;

const CMD_WRITE_ENABLE: u32 = 1 << 16;
const CMD_POLLING: u32 = 1 << 31;

const TX_UNI_USE_DK: u16 = 1 << 0;
const RX_UNI_USE_DK: u16 = 1 << 1;
const TX_DEC_EN: u16 = 1 << 2;
const RX_DEC_EN: u16 = 1 << 3;
const TX_BC_USE_DK: u16 = 1 << 6;
const RX_BC_USE_DK: u16 = 1 << 7;
const ENGINE_EN: u16 = 1 << 9;

/// RTW_CAM_AES
const CAM_AES: u32 = 4;

pub const FIRST_PAIRWISE_ENTRY: u8 = 4;
/// The key CAM's size, as rtw88's RTW_MAX_SEC_CAM_NUM
pub const CAM_ENTRIES: u8 = 32;

impl Chip {
    /// rtw_sec_enable_sec_engine
    pub fn enable_sec_engine(&self) -> Result<()> {
        let rtl = &self.rtl;
        rtl.set16(CR, ENGINE_EN)?;
        rtl.set16(SEC_CONFIG, TX_DEC_EN | RX_DEC_EN | TX_UNI_USE_DK | RX_UNI_USE_DK | TX_BC_USE_DK | RX_BC_USE_DK)
    }

    /// rtw_sec_write_cam for a CCMP key. `addr` is the station for a pairwise key, broadcast for a group key.
    pub fn write_cam(&self, entry: u8, key_idx: u8, group: bool, addr: &[u8; 6], key: &[u8; 16]) -> Result<()> {
        let base = (entry as u32) << 3;
        for i in (0..8u32).rev() {
            let content = match i {
                0 => {
                    (key_idx as u32 & 0x3)
                        | CAM_AES << 2
                        | (group as u32) << 6
                        | 1 << 15
                        | (addr[0] as u32) << 16
                        | (addr[1] as u32) << 24
                }
                1 => u32::from_le_bytes([addr[2], addr[3], addr[4], addr[5]]),
                6 | 7 => 0,
                _ => {
                    let j = ((i - 2) * 4) as usize;
                    u32::from_le_bytes(key[j..j + 4].try_into().unwrap())
                }
            };
            self.rtl.w32(SEC_WRITE, content)?;
            self.rtl.w32(SEC_CMD, CMD_WRITE_ENABLE | CMD_POLLING | (base + i))?;
        }
        Ok(())
    }

    /// rtw_sec_clear_cam
    pub fn clear_cam(&self, entry: u8) -> Result<()> {
        self.rtl.w32(SEC_WRITE, 0)?;
        self.rtl.w32(SEC_CMD, CMD_WRITE_ENABLE | CMD_POLLING | (entry as u32) << 3)
    }
}
