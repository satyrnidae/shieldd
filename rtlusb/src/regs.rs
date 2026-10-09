//! RTL8821A register map, from rtw88 reg.h, rtw88xxa.h and sec.h.

#![allow(dead_code)] // Full map kept for later milestones

//region System

pub const SYS_FUNC_EN: u16 = 0x0002;
pub const APS_FSMCO: u16 = 0x0004;
pub const SYS_CLKR: u16 = 0x0008;
pub const SYS_EEPROM_CTRL: u16 = 0x000a;
pub const RSV_CTRL: u16 = 0x001c;
pub const RF_CTRL: u16 = 0x001f;
pub const AFE_CTRL3: u16 = 0x002c;
pub const EFUSE_CTRL: u16 = 0x0030;
pub const ACLK_MON: u16 = 0x003e;
pub const GPIO_MUXCFG: u16 = 0x0040;
pub const LED_CFG: u16 = 0x004c;
/// GPIO 8-15: input, output, output enable, mode, a byte each
pub const GPIO_PIN_CTRL_2: u16 = 0x0060;
pub const WL_BT_PWR_CTRL: u16 = 0x0068;
pub const SYS_SDIO_CTRL: u16 = 0x0070;
pub const RF_B_CTRL: u16 = 0x0076;
pub const LDO_SWR_CTRL: u16 = 0x007c;
pub const MCUFW_CTRL: u16 = 0x0080;
pub const HIMR0: u16 = 0x00b0;
pub const HISR0: u16 = 0x00b4;
pub const HIMR1: u16 = 0x00b8;
pub const HISR1: u16 = 0x00bc;
pub const EFUSE_ACCESS: u16 = 0x00cf;
pub const SYS_CFG1: u16 = 0x00f0;

pub const FEN_BB_GLB_RST: u8 = 1 << 1;
pub const FEN_BB_RSTB: u8 = 1 << 0;
pub const FEN_USBA: u8 = 1 << 2;
pub const FEN_ELDR: u16 = 1 << 12;
/// In SYS_FUNC_EN + 1
pub const FEN_CPUEN: u8 = 1 << 2;
pub const ANA8M: u16 = 1 << 1;
pub const LOADER_CLK_EN: u16 = 1 << 5;
pub const EEPROM_AUTOLOAD_OK: u8 = 1 << 5;
pub const EFUSE_ACCESS_ON: u8 = 0x69;
pub const EFUSE_ACCESS_OFF: u8 = 0x00;
pub const EF_FLAG: u32 = 1 << 31;
pub const APS_FSMCO_MAC_ENABLE: u16 = 1 << 8;
pub const APS_FSMCO_MAC_OFF: u16 = 1 << 9;
pub const APS_FSMCO_HW_POWERDOWN: u16 = 1 << 15;
/// In RSV_CTRL + 1
pub const WLMCU_IOIF: u8 = 1 << 0;
pub const RF_EN: u8 = 1 << 0;
pub const RF_RSTB: u8 = 1 << 1;
pub const RF_SDM_RSTB: u8 = 1 << 2;
pub const BT_PTA_EN: u8 = 1 << 5;
pub const BT_FUNC_EN: u32 = 1 << 18;
pub const DPDT_SEL_EN: u32 = 1 << 23;
pub const DPDT_WL_SEL: u32 = 1 << 24;
pub const EN_SIC: u16 = 1 << 12;

pub const MCUFWDL_EN: u8 = 1 << 0;
pub const MCUFWDL_RDY: u32 = 1 << 1;
pub const FWDL_CHK_RPT: u8 = 1 << 2;
pub const WINTINI_RDY: u32 = 1 << 6;
pub const RAM_DL_SEL: u8 = 1 << 7;
pub const ROM_PGE: u32 = 0x7 << 16;
pub const ROM_DLEN: u32 = 1 << 19;
pub const FW_READY: u32 = MCUFWDL_RDY | FWDL_CHK_RPT as u32 | WINTINI_RDY | RAM_DL_SEL as u32;
pub const FW_START: u16 = 0x1000;

//endregion

//region MAC

pub const CR: u16 = 0x0100;
pub const MSR: u16 = 0x0102;
pub const TXDMA_PQ_MAP: u16 = 0x010c;
pub const TRXFF_BNDY: u16 = 0x0114;
pub const HMETFR: u16 = 0x01cc;
pub const LLT_INIT: u16 = 0x01e0;
pub const RQPN: u16 = 0x0200;
pub const FIFOPAGE_CTRL_2: u16 = 0x0204;
pub const DWBCN0_CTRL: u16 = 0x0208;
pub const TXDMA_OFFSET_CHK: u16 = 0x020c;
pub const RQPN_NPQ: u16 = 0x0214;
pub const DWBCN1_CTRL: u16 = 0x0228;
pub const RXDMA_AGG_PG_TH: u16 = 0x0280;
pub const RXDMA_STATUS: u16 = 0x0288;
pub const RXDMA_MODE: u16 = 0x0290;
pub const EARLY_MODE_CONTROL: u16 = 0x02bc;

pub const HCI_TXDMA_EN: u16 = 1 << 0;
pub const HCI_RXDMA_EN: u16 = 1 << 1;
pub const TXDMA_EN: u16 = 1 << 2;
pub const RXDMA_EN: u16 = 1 << 3;
pub const PROTOCOL_EN: u16 = 1 << 4;
pub const SCHEDULE_EN: u16 = 1 << 5;
pub const MACTXEN: u8 = 1 << 6;
pub const MACRXEN: u8 = 1 << 7;
pub const MAC_SEC_EN: u16 = 1 << 9;
pub const CAL_32K_TMR_EN: u16 = 1 << 10;
pub const LLT_WRITE_ACCESS: u32 = 1 << 30;
pub const LD_RQPN: u32 = 1 << 31;
pub const DROP_DATA_EN: u32 = 1 << 9;
pub const RXDMA_AGG_EN: u8 = 1 << 2;
pub const DMA_MODE: u8 = 1 << 1;
pub const DMA_BURST_CNT: u8 = 0x3 << 2;
pub const DMA_BURST_SIZE_SHIFT: u8 = 4;
pub const DMA_BURST_SIZE_1024: u8 = 0;
pub const DMA_BURST_SIZE_512: u8 = 1;
pub const DMA_BURST_SIZE_64: u8 = 2;

pub const FWHW_TXQ_CTRL: u16 = 0x0420;
pub const HWSEQ_CTRL: u16 = 0x0423;
pub const BCNQ_BDNY: u16 = 0x0424;
pub const MGQ_BDNY: u16 = 0x0425;
pub const SPEC_SIFS: u16 = 0x0428;
pub const RETRY_LIMIT: u16 = 0x042a;
pub const RRSR: u16 = 0x0440;
pub const ARFR0: u16 = 0x0444;
pub const ARFRH0: u16 = 0x0448;
pub const ARFR1_V1: u16 = 0x044c;
pub const ARFRH1_V1: u16 = 0x0450;
pub const CCK_CHECK: u16 = 0x0454;
pub const AMPDU_MAX_TIME: u16 = 0x0456;
pub const AMPDU_MAX_LENGTH: u16 = 0x0458;
pub const WMAC_LBK_BF_HD: u16 = 0x045d;
pub const FAST_EDCA_CTRL: u16 = 0x0460;
pub const DATA_SC: u16 = 0x0483;
pub const ARFR2_V1: u16 = 0x048c;
pub const ARFRH2_V1: u16 = 0x0490;
pub const ARFR3_V1: u16 = 0x0494;
pub const ARFRH3_V1: u16 = 0x0498;
pub const TXPKT_EMPTY: u16 = 0x041a;
pub const QUEUE_CTRL: u16 = 0x04c6;
pub const SINGLE_AMPDU_CTRL: u16 = 0x04c7;
pub const MAX_AGGR_NUM: u16 = 0x04ca;
pub const BAR_MODE_CTRL: u16 = 0x04cc;
pub const TX_RPT_CTRL: u16 = 0x04ec;
pub const TX_RPT_TIME: u16 = 0x04f0;
pub const EDCA_VO_PARAM: u16 = 0x0500;
pub const EDCA_VI_PARAM: u16 = 0x0504;
pub const EDCA_BE_PARAM: u16 = 0x0508;
pub const EDCA_BK_PARAM: u16 = 0x050c;
pub const BCNTCFG: u16 = 0x0510;
pub const PIFS: u16 = 0x0512;
pub const SIFS: u16 = 0x0514;
pub const TBTT_PROHIBIT: u16 = 0x0540;
pub const BCN_CTRL: u16 = 0x0550;
pub const DRVERLYINT: u16 = 0x0558;
pub const BCNDMATIM: u16 = 0x0559;
pub const USTIME_TSF: u16 = 0x055c;
pub const BCN_MAX_ERR: u16 = 0x055d;
pub const HIQ_NO_LMT_EN: u16 = 0x05a7;
pub const RCR: u16 = 0x0608;
pub const RX_PKT_LIMIT: u16 = 0x060c;
pub const RX_DRVINFO_SZ: u16 = 0x060f;
pub const MACID: u16 = 0x0610;
pub const BSSID: u16 = 0x0618;
pub const MAR: u16 = 0x0620;
pub const USTIME_EDCA: u16 = 0x0638;
pub const MAC_SPEC_SIFS: u16 = 0x063a;
pub const ACKTO: u16 = 0x0640;
pub const NAV_CTRL: u16 = 0x0650;
pub const WMAC_TRXPTCL_CTL: u16 = 0x0668;
pub const SEC_CMD: u16 = 0x0670;
pub const RXFLTMAP0: u16 = 0x06a0;
pub const RXFLTMAP1: u16 = 0x06a2;
pub const RXFLTMAP2: u16 = 0x06a4;

pub const CHECK_CCK_EN: u8 = 1 << 7;
pub const EN_SINGLE_AMPDU: u8 = 1 << 7;
pub const DIS_TSF_UDT: u16 = 1 << 4;
pub const EN_BCN_FUNCTION: u16 = 1 << 3;
pub const RFMOD: u16 = (1 << 7) | (1 << 8);
pub const NET_TYPE_AP: u32 = 3;
pub const BCN_INTERVAL: u16 = 0x0554;
pub const TCR: u16 = 0x0604;
pub const TCR_UPDATE_TIMIE: u32 = 1 << 5;
pub const DTIM_COUNTER_ROOT: u16 = 0x05a8;
/// In DWBCN0_CTRL, 8051 chips
pub const BCN_VALID: u32 = 1 << 16;
/// In FIFOPAGE_CTRL_2
pub const BCN_VALID_V1: u16 = 1 << 15;
/// In CR
pub const ENSWBCN: u16 = 1 << 8;
/// In FWHW_TXQ_CTRL
pub const EN_BCNQ_DL: u32 = 1 << 22;

pub const RCR_AAP: u32 = 1 << 0;
pub const RCR_APM: u32 = 1 << 1;
pub const RCR_AM: u32 = 1 << 2;
pub const RCR_AB: u32 = 1 << 3;
pub const RCR_ACRC32: u32 = 1 << 8;
pub const RCR_HTC_LOC_CTRL: u32 = 1 << 14;
pub const RCR_PKTCTL_DLEN: u32 = 1 << 20;
pub const RCR_VHT_DACK: u32 = 1 << 26;
pub const RCR_APP_PHYSTS: u32 = 1 << 28;
pub const RCR_APP_ICV: u32 = 1 << 29;
pub const RCR_APP_MIC: u32 = 1 << 30;
pub const RCR_APP_FCS: u32 = 1 << 31;

//endregion

//region USB

pub const USB3_RXITV: u16 = 0xf050;
pub const USB_HRPWM: u16 = 0xfe58;

//endregion

//region BB

pub const CCK_RPT_FORMAT: u16 = 0x0804;
pub const RXPSEL: u16 = 0x0808;
pub const TXPSEL: u16 = 0x080c;
pub const BWINDICATION: u16 = 0x0834;
pub const CCA2ND: u16 = 0x0838;
pub const L1PKTH: u16 = 0x0848;
pub const CLKTRK: u16 = 0x0860;
pub const ADCCLK: u16 = 0x08ac;
pub const HSSI_READ: u16 = 0x08b0;
pub const FPGA0_XCD_RF_PARA: u16 = 0x08b4;
pub const ADC160: u16 = 0x08c4;
pub const RXSB: u16 = 0x0a00;
pub const CCK_RX: u16 = 0x0a04;
pub const THREE_WIRE_SWA: u16 = 0x0c00;
pub const TXSCALE_A: u16 = 0x0c1c;
pub const TX_AGC_A: u16 = 0x0c20;
pub const TX_PWR_TRAINING_A: u16 = 0x0c54;
pub const RXIGI_A: u16 = 0x0c50;
pub const LSSI_WRITE_A: u16 = 0x0c90;
pub const RFE_PINMUX_A: u16 = 0x0cb0;
pub const RFE_INV_A: u16 = 0x0cb4;
pub const PI_READ_A: u16 = 0x0d04;
pub const SI_READ_A: u16 = 0x0d08;
pub const TXSCALE_B: u16 = 0x0e1c;

pub const RX_PSEL_RST: u32 = (1 << 28) | (1 << 29);
pub const BB_SWING_MASK: u32 = 0xffe0_0000;

//endregion

//region RF

pub const RF_CFGCH: u8 = 0x18;
pub const RFREG_MASK: u32 = 0xfffff;
pub const RF18_CHANNEL_MASK: u32 = 0xff;
pub const RF18_BAND_MASK: u32 = (1 << 16) | (1 << 9) | (1 << 8);
pub const RF18_RFSI_MASK: u32 = (1 << 18) | (1 << 17);
pub const RF18_BW_MASK: u32 = (1 << 11) | (1 << 10);

//endregion
