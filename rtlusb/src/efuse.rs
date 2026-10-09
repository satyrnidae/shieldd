//! Efuse read and logical map decode, as rtw88 efuse.c and rtw88xxa_read_efuse.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::regs::*;
use crate::usb::Rtl;

pub const PHYS_SIZE: usize = 512;
pub const LOGICAL_SIZE: usize = 512;

/// Fields the init path needs, from the 88xxa logical map layout
#[allow(dead_code)] // Channel plan, thermal and BT fields are for M2+
#[derive(Debug, Clone)]
pub struct Efuse {
    pub mac: [u8; 6],
    pub vid: u16,
    pub pid: u16,
    pub channel_plan: u8,
    pub crystal_cap: u8,
    pub thermal_meter: u8,
    pub pa_type: u8,
    pub lna_type_2g: u8,
    pub lna_type_5g: u8,
    pub rf_board_option: u8,
    pub bt_setting: u8,
    pub tx_bb_swing_2g: u8,
    pub tx_bb_swing_5g: u8,
    /// Path A 2.4 GHz power: cck_base[6], bw40_base[5], ht_1s_diff
    pub txpwr_2g_a: [u8; 12],
}

impl Efuse {
    pub fn parse(log: &[u8]) -> Self {
        let ff0 = |v: u8| if v == 0xff { 0 } else { v };
        let crystal_cap = match log[0xb9] {
            0xff => 0x20,
            v => v,
        };
        Self {
            mac: log[0x107..0x10d].try_into().unwrap(),
            vid: u16::from_le_bytes([log[0x100], log[0x101]]),
            pid: u16::from_le_bytes([log[0x102], log[0x103]]),
            channel_plan: log[0xb8],
            crystal_cap,
            thermal_meter: log[0xba],
            pa_type: ff0(log[0xbc]),
            lna_type_2g: ff0(log[0xbd]),
            lna_type_5g: ff0(log[0xbf]),
            rf_board_option: ff0(log[0xc1]),
            bt_setting: log[0xc3],
            tx_bb_swing_2g: ff0(log[0xc6]),
            tx_bb_swing_5g: ff0(log[0xc7]),
            txpwr_2g_a: log[0x10..0x1c].try_into().unwrap(),
        }
    }

    pub fn ext_pa_2g(&self) -> bool {
        self.pa_type & (1 << 4) != 0
    }

    pub fn ext_lna_2g(&self) -> bool {
        self.lna_type_2g & (1 << 3) != 0
    }

    pub fn ext_pa_5g(&self) -> bool {
        self.pa_type & (1 << 0) != 0
    }

    pub fn ext_lna_5g(&self) -> bool {
        self.lna_type_5g & (1 << 3) != 0
    }
}

/// Vendor Hal_EfusePowerSwitch, on side
fn power_on(rtl: &Rtl) -> Result<()> {
    rtl.w8(EFUSE_ACCESS, EFUSE_ACCESS_ON)?;
    let func = rtl.r16(SYS_FUNC_EN)?;
    if func & FEN_ELDR == 0 {
        rtl.w16(SYS_FUNC_EN, func | FEN_ELDR)?;
    }
    let clk = rtl.r16(SYS_CLKR)?;
    if clk & (LOADER_CLK_EN | ANA8M) != LOADER_CLK_EN | ANA8M {
        rtl.w16(SYS_CLKR, clk | LOADER_CLK_EN | ANA8M)?;
    }
    Ok(())
}

/// rtw_dump_physical_efuse_map
pub fn read_phys(rtl: &Rtl) -> Result<Vec<u8>> {
    power_on(rtl)?;
    let mut map = vec![0xff; PHYS_SIZE];
    let base = rtl.r32(EFUSE_CTRL)? & !(EF_FLAG | 0x3ff << 8 | 0xff);
    for (addr, out) in map.iter_mut().enumerate() {
        rtl.w32(EFUSE_CTRL, base | (addr as u32) << 8)?;
        let start = Instant::now();
        let ctl = loop {
            let ctl = rtl.r32(EFUSE_CTRL)?;
            if ctl & EF_FLAG != 0 {
                break ctl;
            }
            if start.elapsed() > Duration::from_millis(500) {
                bail!("efuse byte {addr:#05x}: EF_FLAG never set");
            }
        };
        *out = ctl as u8;
    }
    rtl.w8(EFUSE_ACCESS, EFUSE_ACCESS_OFF)?;
    Ok(map)
}

/// rtw_dump_logical_efuse_map: 8-byte blocks, 4 words each, word_en bit 0 = present
pub fn logical(phys: &[u8]) -> Vec<u8> {
    let mut log = vec![0xff; LOGICAL_SIZE];
    let mut p = 0;
    while p < phys.len() {
        let hdr1 = phys[p];
        if hdr1 == 0xff {
            break;
        }
        let (blk, word_en) = if hdr1 & 0x1f == 0x0f {
            let Some(&hdr2) = phys.get(p + 1) else { break };
            if hdr2 == 0xff {
                break;
            }
            p += 2;
            ((((hdr2 & 0xf0) >> 1) | (hdr1 >> 5)) as usize, hdr2 & 0x0f)
        } else {
            p += 1;
            ((hdr1 >> 4) as usize, hdr1 & 0x0f)
        };
        for i in 0..4 {
            if word_en & (1 << i) != 0 {
                continue;
            }
            let at = blk * 8 + i * 2;
            if p + 2 > phys.len() || at + 2 > log.len() {
                return log;
            }
            log[at..at + 2].copy_from_slice(&phys[p..p + 2]);
            p += 2;
        }
    }
    log
}

pub fn hexdump(map: &[u8]) {
    for (row, chunk) in map.chunks(16).enumerate() {
        if chunk.iter().all(|&b| b == 0xff) {
            continue;
        }
        let bytes: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        println!("  {:03x}: {}", row * 16, bytes.join(" "));
    }
}

pub fn fmt_mac(mac: &[u8]) -> String {
    mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}
