//! Power on/off and MAC init, ported from rtw88xxa_power_on/off and the 8821a power sequences.

use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

use crate::efuse::{self, Efuse};
use crate::phy::{self, Band, PhyCond};
use crate::regs::*;
use crate::usb::Rtl;
use crate::{fw, tables};

//region Power sequences

enum Pwr {
    /// Read-modify-write: (addr, mask, value)
    W(u16, u8, u8),
    /// Poll until (addr & mask) == value
    Poll(u16, u8, u8),
    DelayMs(u64),
}

use Pwr::*;

/// card_enable_flow_8821a, USB entries only
const CARD_ENABLE: &[Pwr] = &[
    // carddis -> cardemu
    W(0x0005, 0x88, 0x00),
    W(0x004a, 0x01, 0x00),
    W(0x0005, 0x18, 0x00),
    // cardemu -> act
    W(0x0020, 0x01, 0x01),
    W(0x0067, 0x10, 0x00),
    DelayMs(1),
    W(0x0000, 0x20, 0x00),
    W(0x0005, 0x1c, 0x00),
    Poll(0x0006, 0x02, 0x02),
    W(0x0006, 0x01, 0x01),
    W(0x0005, 0x80, 0x00),
    W(0x0005, 0x18, 0x00),
    W(0x0005, 0x01, 0x01),
    Poll(0x0005, 0x01, 0x00),
    W(0x004f, 0x01, 0x01),
    W(0x0067, 0x30, 0x30),
    W(0x0025, 0x40, 0x00),
    W(0x0049, 0x02, 0x02),
    W(0x0063, 0x02, 0x02),
    W(0x0062, 0x02, 0x00),
    W(0x0058, 0x01, 0x01),
    W(0x005a, 0x02, 0x02),
    W(0x002e, 0xff, 0x82),
    W(0x0010, 0x40, 0x40),
];

/// enter_lps_flow_8821a, USB entries only
const ENTER_LPS: &[Pwr] = &[
    W(0x0522, 0xff, 0xff),
    Poll(0x05f8, 0xff, 0x00),
    Poll(0x05f9, 0xff, 0x00),
    Poll(0x05fa, 0xff, 0x00),
    Poll(0x05fb, 0xff, 0x00),
    W(0x0002, 0x01, 0x00),
    W(0x0002, 0x02, 0x00),
    W(0x0100, 0xff, 0x03),
    W(0x0101, 0x02, 0x00),
    W(0x0553, 0x20, 0x20),
];

/// card_disable_flow_8821a, USB entries only
const CARD_DISABLE: &[Pwr] = &[
    // act -> cardemu
    W(0x001f, 0xff, 0x00),
    W(0x004f, 0x01, 0x00),
    W(0x0049, 0x02, 0x00),
    W(0x0006, 0x01, 0x01),
    W(0x0005, 0x02, 0x02),
    Poll(0x0005, 0x02, 0x00),
    W(0x0000, 0x20, 0x20),
    W(0x0020, 0x01, 0x00),
    // cardemu -> carddis
    W(0x0005, 0x18, 0x08),
    W(0x004a, 0x01, 0x01),
];

fn run_pwr_seq(rtl: &Rtl, seq: &[Pwr]) -> Result<()> {
    for cmd in seq {
        match *cmd {
            W(addr, mask, value) => {
                let v = rtl.r8(addr)?;
                rtl.w8(addr, (v & !mask) | (value & mask))?;
            }
            Poll(addr, mask, value) => rtl.poll8(addr, mask, value, Duration::from_secs(1))?,
            DelayMs(ms) => sleep(Duration::from_millis(ms)),
        }
    }
    Ok(())
}

//endregion

/// Page layout: 64 KiB TX FIFO in 256-byte pages, 8 reserved for the driver
const TXFF_PAGES: u32 = 256;
pub const RSVD_BOUNDARY: u32 = TXFF_PAGES - 8;
const RXFF_SIZE: u16 = 16128;
const REPORT_BUF: u16 = 128;
/// Units of 8 bytes
const PHY_STATUS_SIZE: u8 = 4;

#[derive(Clone, Copy)]
enum Dma {
    Extra = 0,
    Low = 1,
    Normal = 2,
    High = 3,
}

pub struct Chip {
    pub rtl: Rtl,
    pub efuse: Efuse,
    #[allow(dead_code)] // For IQK and TX power, M2
    pub cut: u8,
    pub btcoex: bool,
    cond: PhyCond,
}

impl Chip {
    /// Open the adapter and read what init needs, without powering anything up
    pub fn open() -> Result<Self> {
        let rtl = Rtl::open()?;
        let sys_cfg = rtl.r32(SYS_CFG1)?;
        let cut = ((sys_cfg >> 12) & 0xf) as u8;
        if sys_cfg & (1 << 27) != 0 {
            warn!("SYS_CFG says 2T2R; this port only drives path A");
        }
        let log = efuse::logical(&efuse::read_phys(&rtl)?);
        let efuse = Efuse::parse(&log);
        let btcoex = rtl.r32(WL_BT_PWR_CTRL)? & BT_FUNC_EN != 0;
        let cond = PhyCond::new(cut, &efuse, btcoex);
        info!(
            mac = efuse::fmt_mac(&efuse.mac),
            cut,
            btcoex,
            speed = ?rtl.speed,
            bulk_in = format!("{:#04x}", rtl.bulk_in),
            bulk_out = ?rtl.bulk_out,
            "adapter"
        );
        Ok(Self { rtl, efuse, cut, btcoex, cond })
    }

    //region Power on

    /// rtw88xxau_hw_reset: undo a previous run that left firmware loaded
    fn hw_reset(&self) -> Result<()> {
        let rtl = &self.rtl;
        if rtl.r8(MCUFW_CTRL)? & RAM_DL_SEL == 0 {
            return Ok(());
        }
        debug!("firmware still loaded, resetting");
        self.reset_8051()?;
        rtl.w8(MCUFW_CTRL, 0)?;
        rtl.set32(FPGA0_XCD_RF_PARA, 1 << 6)?;
        rtl.clr8(SYS_FUNC_EN, FEN_BB_RSTB | FEN_BB_GLB_RST)?;
        rtl.w8(RF_CTRL, 0)?;
        rtl.w16(CR, 0)?;
        rtl.set8(APS_FSMCO + 1, (APS_FSMCO_MAC_OFF >> 8) as u8)?;
        if rtl.poll8(APS_FSMCO + 1, (APS_FSMCO_MAC_OFF >> 8) as u8, 0, Duration::from_millis(5)).is_err() {
            warn!("hw reset: 0x5[1] never cleared");
        }
        rtl.set8(APS_FSMCO + 1, (APS_FSMCO_MAC_ENABLE >> 8) as u8)?;
        rtl.clr8(SYS_FUNC_EN + 1, (1 << 4) | (1 << 7))?;
        rtl.set8(SYS_FUNC_EN + 1, (1 << 4) | (1 << 7))
    }

    fn reset_8051(&self) -> Result<()> {
        let rtl = &self.rtl;
        rtl.clr8(RSV_CTRL, 1 << 1)?;
        rtl.clr8(RSV_CTRL + 1, 1 << 0)?;
        let v = rtl.r8(SYS_FUNC_EN + 1)?;
        rtl.w8(SYS_FUNC_EN + 1, v & !(1 << 2))?;
        rtl.clr8(RSV_CTRL, 1 << 1)?;
        rtl.set8(RSV_CTRL + 1, 1 << 0)?;
        rtl.w8(SYS_FUNC_EN + 1, v | (1 << 2))
    }

    fn llt_write(&self, addr: u32, data: u32) -> Result<()> {
        self.rtl.w32(LLT_INIT, LLT_WRITE_ACCESS | (addr << 8) | data)?;
        for _ in 0..=20 {
            if self.rtl.r32_mask(LLT_INIT, (1 << 31) | (1 << 30))? == 0 {
                return Ok(());
            }
        }
        bail!("LLT write {addr} never finished")
    }

    /// rtw88xxa_llt_init: two linked lists, TX pages up to the boundary and a ring for the rest
    fn llt_init(&self, boundary: u32) -> Result<()> {
        for i in 0..boundary - 1 {
            self.llt_write(i, i + 1)?;
        }
        self.llt_write(boundary - 1, 0xff)?;
        for i in boundary..255 {
            self.llt_write(i, i + 1)?;
        }
        self.llt_write(255, boundary)
    }

    /// Queue page split and queue -> endpoint mapping, by bulk OUT count
    fn init_queues(&self) -> Result<()> {
        let rtl = &self.rtl;
        let n = rtl.bulk_out.len();
        // hq, nq, lq, exq, gapq from page_table_8821a
        let (hq, nq, lq, exq, gapq) = match n {
            2 => (8, 0, 0, 0, 1),
            3 => (8, 0, 8, 0, 1),
            _ => (8, 0, 8, 4, 1),
        };
        let pubq = RSVD_BOUNDARY - hq - lq - nq - exq - gapq;
        rtl.w32(RQPN_NPQ, nq | exq << 16)?;
        rtl.w32(RQPN, LD_RQPN | hq | lq << 8 | pubq << 16)?;

        let b = RSVD_BOUNDARY as u8;
        rtl.w8(BCNQ_BDNY, b)?;
        rtl.w8(MGQ_BDNY, b)?;
        rtl.w8(WMAC_LBK_BF_HD, b)?;
        rtl.w8(TRXFF_BNDY, b)?;
        rtl.w8(DWBCN0_CTRL + 1, b)?;

        // vo, vi, be, bk, mg, hi from rqpn_table_8821a
        use Dma::*;
        let [vo, vi, be, bk, mg, hi] = match n {
            2 => [High, High, Normal, Normal, High, High],
            3 => [High, Normal, Low, Low, High, High],
            _ => [Normal, Normal, Low, Low, Extra, High],
        };
        let map = (rtl.r16(TXDMA_PQ_MAP)? & 0x7)
            | (hi as u16) << 14
            | (mg as u16) << 12
            | (bk as u16) << 10
            | (be as u16) << 8
            | (vi as u16) << 6
            | (vo as u16) << 4;
        rtl.w16(TXDMA_PQ_MAP, map)?;
        if n == 4 {
            rtl.w8(HIQ_NO_LMT_EN, 0xff)?;
        }
        Ok(())
    }

    /// rtw88xxa_power_on, 8821a USB path
    pub fn power_on(&self) -> Result<()> {
        let rtl = &self.rtl;
        self.hw_reset().context("hw reset")?;

        run_pwr_seq(rtl, CARD_ENABLE).context("card enable sequence")?;
        rtl.w16(CR, 0)?;
        rtl.set16(
            CR,
            HCI_TXDMA_EN | HCI_RXDMA_EN | TXDMA_EN | RXDMA_EN | PROTOCOL_EN | SCHEDULE_EN | MAC_SEC_EN | CAL_32K_TMR_EN,
        )?;
        if rtl.r8(SYS_CFG1 + 3)? & 1 != 0 {
            rtl.set8(LDO_SWR_CTRL, 1 << 6)?;
        }

        self.llt_init(RSVD_BOUNDARY).context("LLT init")?;
        rtl.set32(TXDMA_OFFSET_CHK, DROP_DATA_EN)?;

        fw::download(rtl, fw::FIRMWARE).context("firmware")?;
        rtl.w8(HMETFR, 0xf)?;

        phy::load_mac(rtl, &self.cond, tables::MAC)?;
        self.init_queues()?;
        rtl.w16(TRXFF_BNDY + 2, RXFF_SIZE - REPORT_BUF - 1)?;
        rtl.w8(RX_DRVINFO_SZ, PHY_STATUS_SIZE)?;
        rtl.w32(HIMR0, 0)?;
        rtl.w32(HIMR1, 0)?;
        rtl.w32_mask(CR, 0x30000, 0x2)?;

        // rtw88xxa_init_wmac_setting
        rtl.w16(RXFLTMAP0, 0xffff)?;
        rtl.w16(RXFLTMAP1, 0x0400)?;
        rtl.w16(RXFLTMAP2, 0xffff)?;
        rtl.w32(MAR, 0xffff_ffff)?;
        rtl.w32(MAR + 4, 0xffff_ffff)?;

        // rtw88xxa_init_adaptive_ctrl
        rtl.w32_mask(RRSR, 0xfffff, 0xffff1)?;
        rtl.w16(RETRY_LIMIT, 0x3030)?;

        // rtw88xxa_init_edca
        rtl.w16(SPEC_SIFS, 0x100a)?;
        rtl.w16(MAC_SPEC_SIFS, 0x100a)?;
        rtl.w16(SIFS, 0x100a)?;
        rtl.w16(SIFS + 2, 0x100a)?;
        rtl.w32(EDCA_BE_PARAM, 0x005e_a42b)?;
        rtl.w32(EDCA_BK_PARAM, 0x0000_a44f)?;
        rtl.w32(EDCA_VI_PARAM, 0x005e_a324)?;
        rtl.w32(EDCA_VO_PARAM, 0x002f_a226)?;
        rtl.w8(USTIME_TSF, 0x50)?;
        rtl.w8(USTIME_EDCA, 0x50)?;

        rtl.set8(FWHW_TXQ_CTRL, 1 << 7)?;
        rtl.w8(ACKTO, 0x80)?;

        // rtw88xxau_tx_aggregation, usb_tx_agg_desc_num 6
        rtl.w32_mask(DWBCN0_CTRL, 0xf0, 6)?;
        rtl.w8(DWBCN1_CTRL, 6 << 1)?;

        // rtw88xxa_init_beacon_parameters
        let mut bcn = (DIS_TSF_UDT << 8) | DIS_TSF_UDT;
        if self.btcoex {
            bcn |= EN_BCN_FUNCTION;
        }
        rtl.w16(BCN_CTRL, bcn)?;
        rtl.w32_mask(TBTT_PROHIBIT, 0xfffff, 0x04 | 0x64 << 8)?;
        rtl.w8(DRVERLYINT, 0x05)?;
        rtl.w8(BCNDMATIM, 0x02)?;
        rtl.w16(BCNTCFG, 0x4413)?;
        rtl.w8(BCN_MAX_ERR, 0xff)?;

        // rtw_usb_init_burst_pkt_len
        let burst = match rtl.speed {
            Some(nusb::Speed::Super | nusb::Speed::SuperPlus) => DMA_BURST_SIZE_1024,
            Some(nusb::Speed::High) => DMA_BURST_SIZE_512,
            _ => DMA_BURST_SIZE_64,
        };
        rtl.w8(RXDMA_MODE, DMA_BURST_CNT | DMA_MODE | burst << DMA_BURST_SIZE_SHIFT)?;
        rtl.set16(TXDMA_OFFSET_CHK, DROP_DATA_EN as u16)?;

        rtl.w8(USB3_RXITV, 0x01)?;
        rtl.w16(RXDMA_STATUS, 0x7400)?;
        rtl.w8(RXDMA_STATUS + 1, 0xf5)?;
        rtl.w8(AMPDU_MAX_TIME, 0x5e)?;
        rtl.w32(AMPDU_MAX_LENGTH, 0xffff_ffff)?;
        rtl.w8(USTIME_TSF, 0x50)?;
        rtl.w8(USTIME_EDCA, 0x50)?;
        rtl.set8(SINGLE_AMPDU_CTRL, EN_SINGLE_AMPDU)?;
        rtl.w8(RX_PKT_LIMIT, 0x18)?;
        rtl.w8(PIFS, 0x00)?;
        rtl.w16(MAX_AGGR_NUM, 0x1f1f)?;
        rtl.w8(FWHW_TXQ_CTRL, 0x80)?;
        rtl.w32(FAST_EDCA_CTRL, 0x0308_7777)?;

        // Keep the MAC from being reset by the bus
        rtl.set8(RSV_CTRL, (1 << 5) | (1 << 6))?;

        // ARFB tables 9-12
        rtl.w32(ARFR0, 0x0000_0010)?;
        rtl.w32(ARFRH0, 0xffff_f000)?;
        rtl.w32(ARFR1_V1, 0x0000_0010)?;
        rtl.w32(ARFRH1_V1, 0x003f_f000)?;
        rtl.w32(ARFR2_V1, 0x0000_0015)?;
        rtl.w32(ARFRH2_V1, 0x003f_f000)?;
        rtl.w32(ARFR3_V1, 0x0000_0015)?;
        rtl.w32(ARFRH3_V1, 0xffcf_f000)?;

        rtl.set8(CR, MACTXEN | MACRXEN)?;

        self.bb_config()?;
        phy::load_rf(rtl, &self.cond, tables::RF_A).context("RF table")?;
        phy::switch_band(rtl, &self.efuse, self.btcoex, Band::G2)?;

        rtl.w32(SEC_CMD, (1 << 31) | (1 << 30))?;
        rtl.w8(HWSEQ_CTRL, 0xff)?;
        rtl.w32(BAR_MODE_CTRL, 0x0201_ffff)?;
        rtl.w8(NAV_CTRL + 2, 0)?;
        rtl.clr8(GPIO_MUXCFG, BT_PTA_EN)?;

        // RTS bandwidth follows CCA
        rtl.clr8(QUEUE_CTRL, 1 << 3)?;
        // TX report on, pretx for WEP/TKIP
        rtl.w8(FWHW_TXQ_CTRL + 1, 0x0f)?;
        rtl.w8(EARLY_MODE_CONTROL + 3, 0x01)?;
        rtl.w16(TX_RPT_TIME, 0x3df0)?;

        // Reset USB mode switch setting
        rtl.w8(SYS_SDIO_CTRL, 0)?;
        rtl.w8(ACLK_MON, 0)?;
        rtl.w8(USB_HRPWM, 0)?;

        // ACK for transmitted management frames
        rtl.set32(FWHW_TXQ_CTRL, 1 << 12)?;

        if self.btcoex {
            warn!("efuse says BT coex; coex init isn't ported");
        }
        Ok(())
    }

    /// rtw88xxa_phy_bb_config
    fn bb_config(&self) -> Result<()> {
        let rtl = &self.rtl;
        let mut v = rtl.r8(SYS_FUNC_EN)? | FEN_USBA;
        rtl.w8(SYS_FUNC_EN, v)?;
        v |= FEN_BB_RSTB | FEN_BB_GLB_RST;
        rtl.w8(SYS_FUNC_EN, v)?;
        rtl.w8(RF_CTRL, RF_EN | RF_RSTB | RF_SDM_RSTB)?;
        rtl.w8(RF_B_CTRL, RF_EN | RF_RSTB | RF_SDM_RSTB)?;
        phy::load_bb(rtl, &self.cond, tables::BB).context("BB table")?;
        phy::load_bb(rtl, &self.cond, tables::AGC).context("AGC table")?;
        let cap = (self.efuse.crystal_cap & 0x3f) as u32;
        rtl.w32_mask(AFE_CTRL3, 0x00ff_f000, cap | cap << 6)
    }

    //endregion

    //region Runtime config

    pub fn set_mac(&self, mac: &[u8; 6]) -> Result<()> {
        self.rtl.write(MACID, mac)
    }

    /// RCR: rtw88's defaults, plus accept-all and CRC-failed frames when promiscuous
    pub fn set_rx_filter(&self, promisc: bool) -> Result<()> {
        let mut rcr = RCR_APP_FCS
            | RCR_APP_MIC
            | RCR_APP_ICV
            | RCR_PKTCTL_DLEN
            | RCR_HTC_LOC_CTRL
            | RCR_APP_PHYSTS
            | RCR_AB
            | RCR_AM
            | RCR_APM
            | RCR_VHT_DACK;
        if promisc {
            rcr |= RCR_AAP;
            self.rtl.w16(RXFLTMAP1, 0xffff)?;
        } else {
            self.rtl.w16(RXFLTMAP1, 0x0400)?;
        }
        self.rtl.w32(RCR, rcr)
    }

    /// rtw_usb_dynamic_rx_agg_v2. Off means one frame per bulk transfer, lowest latency.
    pub fn set_rx_agg(&self, enable: bool) -> Result<()> {
        let (size, timeout): (u16, u16) = if enable { (0x5, 0x20) } else { (0x0, 0x1) };
        self.rtl.w16(RXDMA_AGG_PG_TH, size | timeout << 8)?;
        self.rtl.set8(TXDMA_PQ_MAP, RXDMA_AGG_EN)
    }

    pub fn set_channel(&self, channel: u8) -> Result<()> {
        phy::set_channel_20(&self.rtl, &self.efuse, self.btcoex, channel)
    }

    //endregion

    /// rtw8821a_led_set. The LED hangs off GPIO8, active low.
    pub fn set_led(&self, on: bool) -> Result<()> {
        let rtl = &self.rtl;
        let mut gpio = rtl.r32(GPIO_PIN_CTRL_2)?;
        if on {
            let ledcfg = rtl.r8(LED_CFG + 2)?;
            rtl.w8(LED_CFG + 2, ledcfg & 0xc0)?;
            gpio &= !(1 << 24 | 1 << 8);
            gpio |= 1 << 16;
        } else {
            gpio &= !(1 << 24);
            gpio |= 1 << 16 | 1 << 8;
        }
        rtl.w32(GPIO_PIN_CTRL_2, gpio)
    }

    /// rtw88xxa_power_off
    pub fn power_off(&self) -> Result<()> {
        let rtl = &self.rtl;
        let cr = rtl.r8(CR)?;
        if cr == 0 || cr == 0xea {
            return Ok(());
        }
        if !self.btcoex {
            rtl.clr16(GPIO_MUXCFG, EN_SIC)?;
        }
        rtl.w32(HISR0, 0xffff_ffff)?;
        rtl.w32(HISR1, 0xffff_ffff)?;
        rtl.w32(HIMR0, 0)?;
        rtl.w32(HIMR1, 0)?;
        let fsmco = rtl.r16(APS_FSMCO)?;
        rtl.w16(APS_FSMCO, fsmco & !APS_FSMCO_HW_POWERDOWN)?;
        rtl.clr8(TX_RPT_CTRL, 1 << 1)?;
        rtl.w8(CR, 0)?;
        run_pwr_seq(rtl, ENTER_LPS).context("LPS sequence")?;
        if rtl.r8(MCUFW_CTRL)? & RAM_DL_SEL != 0 {
            self.reset_8051()?;
        }
        rtl.clr8(SYS_FUNC_EN + 1, 1 << 2)?;
        rtl.w8(MCUFW_CTRL, 0)?;
        run_pwr_seq(rtl, CARD_DISABLE).context("card disable sequence")?;
        if fsmco & APS_FSMCO_HW_POWERDOWN != 0 {
            rtl.set16(APS_FSMCO, APS_FSMCO_HW_POWERDOWN)?;
        }
        Ok(())
    }
}
