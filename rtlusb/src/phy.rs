//! BB/RF setup: conditional table loading, RF register access, band and channel switch.
//! Ported from rtw88 phy.c and rtw88xxa.c, 8821a paths only.

use std::thread::sleep;
use std::time::Duration;

use anyhow::Result;

use crate::efuse::Efuse;
use crate::regs::*;
use crate::usb::Rtl;

//region Table conditions

const INTF_USB: u32 = 1 << 1;
const BRANCH_ELSE: u32 = 2;
const BRANCH_ENDIF: u32 = 3;

/// rtw_phy_cond packed as in the tables: rfe 0-7, intf 8-11, pkg 12-15, cut 24-27,
/// branch 28-29, neg 30, pos 31
#[derive(Clone, Copy, Default)]
struct Cond {
    rfe: u32,
    intf: u32,
    pkg: u32,
    cut: u32,
}

impl Cond {
    fn from_word(w: u32) -> Self {
        Self { rfe: w & 0xff, intf: (w >> 8) & 0xf, pkg: (w >> 12) & 0xf, cut: (w >> 24) & 0xf }
    }
}

/// What the tables get matched against, rtw_phy_setup_phy_cond for 8821a
pub struct PhyCond {
    drv: Cond,
}

impl PhyCond {
    pub fn new(cut: u8, efuse: &Efuse, btcoex: bool) -> Self {
        let rfe = efuse.ext_lna_2g() as u32
            | (efuse.ext_pa_2g() as u32) << 1
            | (efuse.ext_lna_5g() as u32) << 2
            | (efuse.ext_pa_5g() as u32) << 3
            | (btcoex as u32) << 4;
        Self { drv: Cond { rfe, intf: INTF_USB, pkg: 15, cut: if cut == 0 { 15 } else { cut as u32 } } }
    }

    /// check_positive, 8821a branch. cond2 amplifier types are only read on 8812a, so all zero here.
    fn matches(&self, c: Cond, c2: u32) -> bool {
        if c.cut != 0 && c.cut != self.drv.cut {
            return false;
        }
        if c.pkg != 0 && c.pkg != self.drv.pkg {
            return false;
        }
        if c.intf != 0 && c.intf != self.drv.intf {
            return false;
        }
        if c.rfe & 0x0f == 0 {
            return true;
        }
        if c.rfe & self.drv.rfe != c.rfe {
            return false;
        }
        let [glna, gpa, alna, apa] = c2.to_le_bytes();
        let typed = [(0, glna), (1, gpa), (2, alna), (3, apa)];
        typed.iter().all(|&(bit, t)| c.rfe & (1 << bit) == 0 || t == 0)
    }

    /// rtw_parse_tbl_phy_cond: yields the (addr, data) pairs that apply to this chip
    pub fn select(&self, tbl: &[(u32, u32)]) -> Vec<(u32, u32)> {
        let mut out = Vec::with_capacity(tbl.len());
        let (mut matched, mut skipped) = (true, false);
        let (mut pos, mut pos2) = (Cond::default(), 0);
        for &(a, d) in tbl {
            if a & (1 << 31) != 0 {
                match (a >> 28) & 0x3 {
                    BRANCH_ENDIF => {
                        matched = true;
                        skipped = false;
                    }
                    BRANCH_ELSE => matched = !skipped,
                    _ => {
                        pos = Cond::from_word(a);
                        pos2 = d;
                    }
                }
            } else if a & (1 << 30) != 0 {
                if !skipped {
                    matched = self.matches(pos, pos2);
                    skipped = matched;
                } else {
                    matched = false;
                }
            } else if matched {
                out.push((a, d));
            }
        }
        out
    }
}

//endregion

//region Table loaders

pub fn load_mac(rtl: &Rtl, cond: &PhyCond, tbl: &[(u32, u32)]) -> Result<()> {
    for (a, d) in cond.select(tbl) {
        rtl.w8(a as u16, d as u8)?;
    }
    Ok(())
}

pub fn load_bb(rtl: &Rtl, cond: &PhyCond, tbl: &[(u32, u32)]) -> Result<()> {
    for (a, d) in cond.select(tbl) {
        match a {
            0xfe => sleep(Duration::from_millis(50)),
            0xfd => sleep(Duration::from_millis(5)),
            0xfc => sleep(Duration::from_millis(1)),
            // Sub-50 us delays: a USB control transfer already takes longer
            0xf9..=0xfb => {}
            _ => rtl.w32(a as u16, d)?,
        }
    }
    Ok(())
}

pub fn load_rf(rtl: &Rtl, cond: &PhyCond, tbl: &[(u32, u32)]) -> Result<()> {
    for (a, d) in cond.select(tbl) {
        match a {
            0xffe => sleep(Duration::from_millis(50)),
            0xfe => sleep(Duration::from_micros(100)),
            _ => write_rf(rtl, a as u8, RFREG_MASK, d)?,
        }
    }
    Ok(())
}

//endregion

//region RF access, path A only

/// rtw88xxa_phy_read_rf
pub fn read_rf(rtl: &Rtl, addr: u8, mask: u32) -> Result<u32> {
    let pi_mode = rtl.r32_mask(THREE_WIRE_SWA, 0x4)? != 0;
    rtl.w32_mask(HSSI_READ, 0xff, addr as u32)?;
    let reg = if pi_mode { PI_READ_A } else { SI_READ_A };
    rtl.r32_mask(reg, mask)
}

/// rtw_phy_write_rf_reg_sipi
pub fn write_rf(rtl: &Rtl, addr: u8, mask: u32, data: u32) -> Result<()> {
    let mask = mask & RFREG_MASK;
    let data = if mask != RFREG_MASK {
        let old = read_rf(rtl, addr, RFREG_MASK)?;
        (old & !mask) | ((data << mask.trailing_zeros()) & mask)
    } else {
        data
    };
    rtl.w32(LSSI_WRITE_A, (((addr as u32) << 20) | (data & 0xfffff)) & 0x0fff_ffff)
}

//endregion

//region Band and channel

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Band {
    G2,
    G5,
}

const SWING_SETTING: [u32; 4] = [0x200, 0x16a, 0x101, 0x0b6];

fn set_bb_swing(rtl: &Rtl, efuse: &Efuse, band: Band) -> Result<()> {
    let swing = match band {
        Band::G2 => efuse.tx_bb_swing_2g,
        Band::G5 => efuse.tx_bb_swing_5g,
    };
    rtl.w32_mask(TXSCALE_A, BB_SWING_MASK, SWING_SETTING[(swing & 0x3) as usize])?;
    rtl.w32_mask(TXSCALE_B, BB_SWING_MASK, SWING_SETTING[((swing >> 2) & 0x3) as usize])
}

/// rtw8821a_set_ext_band_switch: the 8811AU has one antenna for both bands
fn ext_band_switch(rtl: &Rtl, band: Band) -> Result<()> {
    rtl.w32_mask(LED_CFG, DPDT_SEL_EN, 0)?;
    rtl.w32_mask(LED_CFG, DPDT_WL_SEL, 1)?;
    rtl.w32_mask(RFE_INV_A, 0xf, 7)?;
    rtl.w32_mask(RFE_INV_A, 0xf0, 7)?;
    rtl.w32_mask(RFE_INV_A, (1 << 29) | (1 << 28), if band == Band::G2 { 1 } else { 2 })
}

fn rfe_24g(rtl: &Rtl, efuse: &Efuse) -> Result<()> {
    rtl.w32_mask(RFE_PINMUX_A, 0xf000, 0x7)?;
    rtl.w32_mask(RFE_PINMUX_A, 0xf0, 0x7)?;
    let ext = efuse.ext_lna_2g();
    rtl.w32_mask(RFE_INV_A, 1 << 20, ext as u32)?;
    rtl.w32_mask(RFE_INV_A, 1 << 22, 0)?;
    let pin = if ext { 0x2 } else { 0x7 };
    rtl.w32_mask(RFE_PINMUX_A, 0x7, pin)?;
    rtl.w32_mask(RFE_PINMUX_A, 0x700, pin)
}

fn rfe_5g(rtl: &Rtl) -> Result<()> {
    rtl.w32_mask(RFE_PINMUX_A, 0xf000, 0x5)?;
    rtl.w32_mask(RFE_PINMUX_A, 0xf0, 0x4)?;
    rtl.w32_mask(RFE_INV_A, 1 << 20, 0)?;
    rtl.w32_mask(RFE_INV_A, 1 << 22, 0)?;
    rtl.w32_mask(RFE_PINMUX_A, 0x7, 0x7)?;
    rtl.w32_mask(RFE_PINMUX_A, 0x700, 0x7)
}

// DESC_RATE bits for RRSR
const RATE_1M: u32 = 1 << 0;
const RATE_2M: u32 = 1 << 1;
const RATE_5_5M: u32 = 1 << 2;
const RATE_11M: u32 = 1 << 3;
const RATE_6M: u32 = 1 << 4;
const RATE_12M: u32 = 1 << 6;
const RATE_24M: u32 = 1 << 8;

/// rtw88xxa_switch_band
pub fn switch_band(rtl: &Rtl, efuse: &Efuse, btcoex: bool, band: Band) -> Result<()> {
    if !btcoex {
        ext_band_switch(rtl, band)?;
    }
    match band {
        Band::G2 => {
            rtl.set32(RXPSEL, RX_PSEL_RST)?;
            rfe_24g(rtl, efuse)?;
            rtl.w32_mask(TXSCALE_A, 0xf00, 0)?;
            rtl.w32_mask(TXPSEL, 0xf0, 0x1)?;
            rtl.w32_mask(CCK_RX, 0x0f00_0000, 0x1)?;
            let basic = RATE_1M | RATE_2M | RATE_5_5M | RATE_11M | RATE_6M | RATE_12M | RATE_24M;
            rtl.w32_mask(RRSR, 0xfffff, basic)?;
            rtl.clr8(CCK_CHECK, CHECK_CCK_EN)?;
        }
        Band::G5 => {
            rfe_5g(rtl)?;
            rtl.set8(CCK_CHECK, CHECK_CCK_EN)?;
            // Wait for the TX queues to drain, rtw88 gives up after 2.5 ms
            let _ = rtl.poll8(TXPKT_EMPTY, 0x30, 0x30, Duration::from_millis(3));
            rtl.set32(RXPSEL, RX_PSEL_RST)?;
            rtl.w32_mask(TXSCALE_A, 0xf00, 1)?;
            rtl.w32_mask(TXPSEL, 0xf0, 0)?;
            rtl.w32_mask(CCK_RX, 0x0f00_0000, 0xf)?;
            rtl.w32_mask(RRSR, 0xfffff, RATE_6M | RATE_12M | RATE_24M)?;
        }
    }
    set_bb_swing(rtl, efuse, band)
}

fn current_band(rtl: &Rtl) -> Result<Band> {
    Ok(if rtl.r8(CCK_CHECK)? & CHECK_CCK_EN != 0 { Band::G5 } else { Band::G2 })
}

/// rtw88xxa_set_channel at 20 MHz. Wider channels aren't needed for the controller.
pub fn set_channel_20(rtl: &Rtl, efuse: &Efuse, btcoex: bool, channel: u8) -> Result<()> {
    let band = if channel > 14 { Band::G5 } else { Band::G2 };
    if current_band(rtl)? != band {
        switch_band(rtl, efuse, btcoex, band)?;
    }

    // rtw88xxa_switch_channel
    let fc_area = match channel {
        36..=48 => 0x494,
        50..=64 => 0x453,
        100..=116 => 0x452,
        118.. => 0x412,
        _ => 0x96a,
    };
    rtl.w32_mask(CLKTRK, 0x1ffe_0000, fc_area)?;
    let rf_mod_ag = match channel {
        36..=64 => 0x101,
        100..=140 => 0x301,
        141.. => 0x501,
        _ => 0x000,
    };
    write_rf(rtl, RF_CFGCH, RF18_RFSI_MASK | RF18_BAND_MASK, rf_mod_ag)?;
    write_rf(rtl, RF_CFGCH, RF18_CHANNEL_MASK, channel as u32)?;

    // rtw88xxa_post_set_bw_mode, 20 MHz
    let trx = rtl.r16(WMAC_TRXPTCL_CTL)?;
    rtl.w16(WMAC_TRXPTCL_CTL, trx & !RFMOD)?;
    rtl.w8(DATA_SC, 0)?;
    rtl.w32_mask(ADCCLK, 0x003003c3, 0x00300200)?;
    rtl.w32_mask(ADC160, 1 << 30, 0)?;
    rtl.w32_mask(L1PKTH, 0x03c00000, 8)?;

    // rtw88xxa_set_channel_rf
    write_rf(rtl, RF_CFGCH, RF18_BW_MASK, 3)
}

//endregion

//region TX power

const MAX_POWER_INDEX: u8 = 0x3f;

fn channel_group_2g(channel: u8) -> usize {
    match channel {
        1..=2 => 0,
        3..=5 => 1,
        6..=8 => 2,
        9..=11 => 3,
        _ => 4,
    }
}

/// Sign-extend a 4-bit efuse power difference
fn nibble(v: u8) -> i8 {
    ((v << 4) as i8) >> 4
}

/// Power indices for path A at 2.4 GHz, 20 MHz: (cck, ofdm, ht/vht 1ss).
/// rtw_phy_get_2g_tx_power_index from efuse bases only. By-rate offsets and regulatory
/// limits aren't applied yet; they only ever lower these.
pub fn tx_power_2g(efuse: &Efuse, channel: u8) -> (u8, u8, u8) {
    let p = &efuse.txpwr_2g_a;
    let group = channel_group_2g(channel);
    let cck = p[if channel == 14 { 5 } else { group }];
    let bw40 = p[6 + group] as i16;
    let diff = p[11];
    let ofdm = bw40 + nibble(diff) as i16;
    let ht20 = bw40 + nibble(diff >> 4) as i16;
    let clamp = |v: i16| v.clamp(0, MAX_POWER_INDEX as i16) as u8;
    (clamp(cck as i16), clamp(ofdm), clamp(ht20))
}

/// rtw88xxa_set_tx_power_index for path A: one byte per rate, four rates per register
pub fn set_tx_power_2g(rtl: &Rtl, efuse: &Efuse, channel: u8) -> Result<(u8, u8, u8)> {
    let (cck, ofdm, ht) = tx_power_2g(efuse, channel);
    let x4 = |v: u8| u32::from_le_bytes([v; 4]);
    rtl.w32(TX_AGC_A, x4(cck))?; // 1, 2, 5.5, 11
    rtl.w32(TX_AGC_A + 0x04, x4(ofdm))?; // 6-18
    rtl.w32(TX_AGC_A + 0x08, x4(ofdm))?; // 24-54
    rtl.w32(TX_AGC_A + 0x0c, x4(ht))?; // MCS0-3
    rtl.w32(TX_AGC_A + 0x10, x4(ht))?; // MCS4-7
    rtl.w32(TX_AGC_A + 0x1c, x4(ht))?; // VHT1SS MCS0-3
    rtl.w32(TX_AGC_A + 0x20, x4(ht))?; // VHT1SS MCS4-7
    rtl.w32_mask(TX_AGC_A + 0x24, 0xffff, u32::from_le_bytes([ht, ht, 0, 0]))?; // VHT1SS MCS8-9

    // rtw88xxa_tx_power_training, from the MCS7 index
    let mut level = ht as i32;
    let mut data = 0u32;
    for (i, step) in [10, 8, 6].into_iter().enumerate() {
        level -= step;
        data |= (level.max(2) as u32) << (i * 8);
    }
    rtl.w32_mask(TX_PWR_TRAINING_A, 0xffffff, data)?;
    Ok((cck, ofdm, ht))
}

//endregion

#[cfg(test)]
mod tests {
    use super::*;

    fn cond() -> PhyCond {
        PhyCond { drv: Cond { rfe: 0, intf: INTF_USB, pkg: 15, cut: 2 } }
    }

    #[test]
    fn takes_else_branch_when_if_is_for_pcie() {
        // From rtw8821a_rf_a: IF rfe 0x11 intf PCIe, else 0x94838
        let tbl = [
            (0x085, 0x48000),
            (0x8000_0111, 0),
            (0x4000_0000, 0),
            (0x086, 0x9483a),
            (0xa000_0000, 0),
            (0x086, 0x94838),
            (0xb000_0000, 0),
            (0x087, 0x44980),
        ];
        assert_eq!(cond().select(&tbl), vec![(0x085, 0x48000), (0x086, 0x94838), (0x087, 0x44980)]);
    }

    #[test]
    fn takes_matching_if_and_skips_else() {
        let tbl = [(0x8000_0200, 0), (0x4000_0000, 0), (0x1, 1), (0xa000_0000, 0), (0x2, 2), (0xb000_0000, 0)];
        assert_eq!(cond().select(&tbl), vec![(0x1, 1)]);
    }

    #[test]
    fn table_addresses_are_registers() {
        use crate::tables::*;
        for (name, tbl, max) in [("MAC", MAC, 0x7ff), ("BB", BB, 0xfff), ("AGC", AGC, 0xfff), ("RF_A", RF_A, 0xfff)] {
            for &(a, _) in tbl {
                assert!(a & 0xc000_0000 != 0 || a <= max, "{name}: {a:#x}");
            }
        }
    }

    #[test]
    fn tx_power_from_adapter1_efuse() {
        // The first adapter's logical efuse: path A's 2.4 GHz power bytes at 0x10-0x1b, the rest blank
        let mut log = [0xffu8; 512];
        log[0x10..0x1c].copy_from_slice(&[0x31, 0x31, 0x31, 0x30, 0x30, 0x30, 0x29, 0x29, 0x29, 0x29, 0x29, 0x02]);
        let e = Efuse::parse(&log);
        assert_eq!(tx_power_2g(&e, 1), (0x31, 0x2b, 0x29));
        assert_eq!(tx_power_2g(&e, 11), (0x30, 0x2b, 0x29));
        assert_eq!(nibble(0x0e), -2);
    }

    #[test]
    fn real_tables_select_something() {
        let c = cond();
        for tbl in [crate::tables::MAC, crate::tables::BB, crate::tables::AGC, crate::tables::RF_A] {
            let n = c.select(tbl).len();
            assert!(n > 0 && n <= tbl.len());
        }
    }
}
