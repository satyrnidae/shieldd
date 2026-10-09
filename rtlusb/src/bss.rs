//! One-BSS AP state machine: open auth, (re)association, WPA2 4-way handshake, data frames.
//!
//! Power save: the controller dozes between its own frames, and a frame sent then gets retried
//! by the hardware until it wakes, blocking its queue meanwhile. With a ps_hold, data for a
//! dozing station waits until its next frame w/ PM clear, or the hold runs out. No TIM: it
//! wakes on its own often enough. Without one, everything goes out at once, rtw88's
//! `ap_ignore_ps` behavior. On Linux, input felt the same either way; held downlink is what
//! hurts, mainly headset audio.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use anyhow::Result;
use mac::fmt_mac;
use rand::RngCore;
use tracing::{debug, info, trace, warn};

use crate::ap::{ApConfig, RSN, WMM_PARAM};
use crate::chip::Chip;
use crate::sec::FIRST_PAIRWISE_ENTRY;
use crate::tx::{RATE_6M, Tx, TxInfo};
use crate::wpa::{self, Pmk};
use crate::wps;

pub type Mac = [u8; 6];

const EAPOL_RESEND: Duration = Duration::from_millis(500);
const EAPOL_TRIES: u32 = 5;
/// Msg 1 waits this long after the assoc response. They go out on different hardware queues,
/// so sent back to back, msg 1 can beat the response to the air and get dropped.
const MSG1_DELAY: Duration = Duration::from_millis(20);
/// Downlink data. 24 Mbps w/o fallback once failed every frame at 48 retries while uplink was
/// fine: reports came 3 s late, the queue filled, EAPOL couldn't get out and the link dropped.
/// Ozmo frames are small, so the slowest OFDM rate costs little airtime.
const DATA_RATE: u8 = RATE_6M;
/// Per-frame retry cap, so one dead frame can't block its queue for long
const DATA_RETRY_LIMIT: u8 = 12;
/// More unreported data than this in the chip: drop new frames, Ozmo retransmits
const MAX_IN_FLIGHT: usize = 8;
/// Stream frames, see send_stream: late audio is worthless, so give up on it sooner
const STREAM_RETRY_LIMIT: u8 = 4;
const IN_FLIGHT_WINDOW: Duration = Duration::from_millis(500);
const GTK_IDX: u8 = 1;
/// EAP retransmission. The enrollee does its DH math between M1 and M3, so be patient.
const WPS_RESEND: Duration = Duration::from_secs(3);
const WPS_TRIES: u32 = 4;
/// Time for the EAP-Failure to get out before the deauth
const WPS_DEAUTH_DELAY: Duration = Duration::from_millis(300);
const REASON_PREV_AUTH_INVALID: u16 = 2;
const REASON_4WAY_TIMEOUT: u16 = 15;
const ETHERTYPE_EAPOL: u16 = 0x888e;

#[allow(dead_code)] // Payloads are read once ozproto runs on top, for now only logged
#[derive(Debug)]
pub enum Event {
    /// Associated for a normal join, the 4-way handshake next
    Associated(Mac),
    Connected(Mac),
    Disconnected(Mac),
    /// Ethertype and payload of a data frame from a connected station
    Data(Mac, u16, Vec<u8>),
    /// WPS handed the credential over, w/ the model name the station gave; it rejoins w/ the
    /// credential next
    Paired(Mac, String),
    PairingFailed(Mac, String),
}

enum State {
    Authenticated,
    /// Msg 1 or 3 is out, waiting for 2 or 4. Each resend bumps `replay`; an answer to any of
    /// them, `first_replay` on, counts.
    Handshake {
        anonce: [u8; 32],
        first_replay: u64,
        replay: u64,
        ptk: Option<wpa::Ptk>,
        due: Instant,
        tries: u32,
    },
    Connected,
    /// EAP-WSC under way; `due` is when the request in `eap.last` goes out again
    Wps {
        eap: Box<wps::Eap>,
        due: Instant,
        tries: u32,
    },
    /// EAP-Failure sent, deauth pending
    WpsDone {
        at: Instant,
    },
}

struct Sta {
    aid: u16,
    wmm: bool,
    state: State,
    /// CCMP packet number for frames we send
    pn: u64,
    /// Last data sequence control seen, for dropping retransmissions
    last_seq: Option<u16>,
    /// Same for management frames
    last_mgmt_seq: Option<u16>,
    /// PM bit of its last frame: it said it's going to doze
    dozing: bool,
    /// Encrypted frames held while it dozes, w/ when they were queued
    held: VecDeque<Held>,
}

/// Power save counters since the last take_ps_stats
#[derive(Debug, Default, Clone, Copy)]
pub struct PsStats {
    /// Frames sent right away, the station awake
    pub direct: u32,
    /// Held frames released when it woke up
    pub on_wake: u32,
    /// Held frames sent anyway after the hold limit
    pub timed_out: u32,
    pub max_wait_ms: u32,
    /// Dropped because the chip's queue was backed up
    pub dropped: u32,
    /// Stream frames handed to the chip, see Bss::send_stream
    pub stream: u32,
}

pub struct Bss {
    pub cfg: ApConfig,
    pmk: Pmk,
    gtk: [u8; 16],
    stas: HashMap<Mac, Sta>,
    next_aid: u16,
    /// Hold frames for a dozing station up to this long; None sends right away like ap_ignore_ps
    ps_hold: Option<Duration>,
    ps_stats: PsStats,
    /// Ignored until then, see shun
    shunned: Option<(Mac, Instant)>,
}

fn fc_of(f: &[u8]) -> u16 {
    u16::from_le_bytes([f[0], f[1]])
}

fn addr(f: &[u8], at: usize) -> Mac {
    f[at..at + 6].try_into().unwrap()
}

impl Bss {
    /// Some station got through the 4-way handshake
    pub fn any_connected(&self) -> bool {
        self.stas.values().any(|s| matches!(s.state, State::Connected))
    }

    /// Past the 4-way handshake, so data to it can go out
    pub fn is_connected(&self, sta: &Mac) -> bool {
        self.stas.get(sta).is_some_and(|s| matches!(s.state, State::Connected))
    }

    pub fn set_ps_hold(&mut self, hold: Option<Duration>) {
        self.ps_hold = hold;
    }

    pub fn take_ps_stats(&mut self) -> PsStats {
        std::mem::take(&mut self.ps_stats)
    }

    pub fn new(cfg: ApConfig, pmk: Pmk) -> Self {
        let mut gtk = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut gtk);
        Self {
            cfg,
            pmk,
            gtk,
            stas: HashMap::new(),
            next_aid: 1,
            ps_hold: None,
            ps_stats: PsStats::default(),
            shunned: None,
        }
    }

    /// Deauths `sta`. It's free to come right back.
    pub fn kick(&mut self, chip: &Chip, tx: &mut Tx, sta: &Mac) -> Result<Option<Event>> {
        // 3: deauthenticated because the sender is leaving the BSS
        self.send_deauth(chip, tx, sta, 3)?;
        info!(sta = fmt_mac(sta), "kicked");
        self.on_leave(chip, *sta)
    }

    /// Ignores `sta`'s probes, auth and association for `d`, so to it the AP is gone. One
    /// station at a time; a new shun replaces the old.
    pub fn shun(&mut self, sta: &Mac, d: Duration) {
        self.shunned = Some((*sta, Instant::now() + d));
        info!(sta = fmt_mac(sta), ?d, "shunned");
    }

    /// Who's being ignored right now, if anyone
    pub fn shunned(&self) -> Option<Mac> {
        self.shunned.filter(|&(_, until)| Instant::now() < until).map(|(m, _)| m)
    }

    /// `sa` is being ignored. Lifts an expired shun.
    pub fn is_shunned(&mut self, sa: &Mac) -> bool {
        match self.shunned {
            Some((m, until)) if Instant::now() >= until => {
                self.shunned = None;
                info!(sta = fmt_mac(&m), "shun over");
                false
            }
            Some((m, _)) => m == *sa,
            None => false,
        }
    }

    /// Install the group key. Call once after the security engine is on.
    pub fn install_gtk(&self, chip: &Chip) -> Result<()> {
        chip.write_cam(GTK_IDX, GTK_IDX, true, &[0xff; 6], &self.gtk)
    }

    //region Frame builders

    fn mgmt_header(&self, fc: u16, da: &Mac) -> Vec<u8> {
        let mut f = Vec::with_capacity(128);
        f.extend_from_slice(&fc.to_le_bytes());
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(da);
        f.extend_from_slice(&self.cfg.bssid);
        f.extend_from_slice(&self.cfg.bssid);
        f.extend_from_slice(&[0, 0]);
        f
    }

    fn data_frame(&self, sta: &Mac, s: &Sta, ccmp: bool, ethertype: u16, payload: &[u8]) -> Vec<u8> {
        let mut flags = 0x02u8; // FromDS
        if ccmp {
            flags |= 0x40;
        }
        let subtype = if s.wmm { 0x88 } else { 0x08 };
        let mut f = vec![subtype, flags, 0, 0];
        f.extend_from_slice(sta);
        f.extend_from_slice(&self.cfg.bssid);
        f.extend_from_slice(&self.cfg.bssid);
        f.extend_from_slice(&[0, 0]);
        if s.wmm {
            f.extend_from_slice(&[0, 0]); // QoS control, TID 0
        }
        if ccmp {
            let pn = s.pn.to_le_bytes();
            f.extend_from_slice(&[pn[0], pn[1], 0, 0x20, pn[2], pn[3], pn[4], pn[5]]);
        }
        f.extend_from_slice(&[0xaa, 0xaa, 0x03, 0x00, 0x00, 0x00]);
        f.extend_from_slice(&ethertype.to_be_bytes());
        f.extend_from_slice(payload);
        f
    }

    //endregion

    //region Management

    fn on_auth(&mut self, chip: &Chip, tx: &mut Tx, sa: Mac, body: &[u8]) -> Result<()> {
        if body.len() < 6 {
            return Ok(());
        }
        let alg = u16::from_le_bytes([body[0], body[1]]);
        let seq = u16::from_le_bytes([body[2], body[3]]);
        if seq != 1 {
            return Ok(());
        }
        let status: u16 = if alg == 0 { 0 } else { 13 }; // 13: unsupported algorithm
        let mut f = self.mgmt_header(0x00b0, &sa);
        f.extend_from_slice(&alg.to_le_bytes());
        f.extend_from_slice(&2u16.to_le_bytes());
        f.extend_from_slice(&status.to_le_bytes());
        tx.send(&chip.rtl, TxInfo::mgmt(), &f)?;
        if status == 0 {
            let old = self.stas.remove(&sa);
            if old.as_ref().is_some_and(|o| matches!(o.state, State::Connected)) {
                chip.clear_cam(FIRST_PAIRWISE_ENTRY)?;
            }
            // A station coming back keeps its AID and PN. Frames from the old session may still
            // sit in the chip and go out under the new key; restarting the PN under them would
            // make every new frame look like a replay.
            let (aid, pn) = match old {
                Some(o) => (o.aid, o.pn + 64),
                None => {
                    let aid = self.next_aid;
                    self.next_aid = self.next_aid % 2007 + 1;
                    (aid, 1)
                }
            };
            let sta = Sta {
                aid,
                wmm: false,
                state: State::Authenticated,
                pn,
                last_seq: None,
                last_mgmt_seq: None,
                dozing: false,
                held: VecDeque::new(),
            };
            self.stas.insert(sa, sta);
            info!(sta = fmt_mac(&sa), "authenticated");
        }
        Ok(())
    }

    fn on_assoc(&mut self, chip: &Chip, tx: &mut Tx, sa: Mac, reassoc: bool, body: &[u8]) -> Result<Option<Event>> {
        let ies_at = if reassoc { 10 } else { 4 };
        let Some(ies) = body.get(ies_at..) else {
            return Ok(None);
        };
        let Some(sta) = self.stas.get_mut(&sa) else {
            debug!(sta = fmt_mac(&sa), "association without authentication");
            return Ok(None);
        };
        sta.wmm = has_wmm(ies);
        let aid = sta.aid;
        let wmm = sta.wmm;
        // An enrollee joins open, w/ a WPS IE and no RSN IE
        let wps_join = self.cfg.wps.is_some() && wps::has_ie(ies) && !has_ie(ies, 48);

        let mut f = self.mgmt_header(if reassoc { 0x0030 } else { 0x0010 }, &sa);
        f.extend_from_slice(&(0x0431u16).to_le_bytes()); // ESS, privacy, short preamble, short slot
        f.extend_from_slice(&0u16.to_le_bytes());
        f.extend_from_slice(&(aid | 0xc000).to_le_bytes());
        f.extend_from_slice(&[1, 8, 0x8c, 0x12, 0x98, 0x24, 0xb0, 0x48, 0x60, 0x6c]);
        if wmm {
            f.push(221);
            f.push(WMM_PARAM.len() as u8);
            f.extend_from_slice(&WMM_PARAM);
        }
        if wps_join {
            f.extend(wps::assoc_resp_ie());
        }
        tx.send(&chip.rtl, TxInfo::mgmt(), &f)?;
        info!(sta = fmt_mac(&sa), aid, wmm, reassoc, wps = wps_join, "associated");

        if let Some(w) = self.cfg.wps.as_ref().filter(|_| wps_join) {
            let reg = wps::Registrar::new(w.device.clone(), w.credential.clone(), sa);
            let eap = Box::new(wps::Eap::new(reg));
            // tick() sends the identity request, after the assoc response is out
            self.stas.get_mut(&sa).expect("station exists").state =
                State::Wps { eap, due: Instant::now() + MSG1_DELAY, tries: 0 };
            return Ok(None);
        }

        let mut anonce = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut anonce);
        // tick() sends it
        let due = Instant::now() + MSG1_DELAY;
        self.stas.get_mut(&sa).expect("station exists").state =
            State::Handshake { anonce, first_replay: 1, replay: 0, ptk: None, due, tries: 0 };
        Ok(Some(Event::Associated(sa)))
    }

    fn on_leave(&mut self, chip: &Chip, sa: Mac) -> Result<Option<Event>> {
        let Some(old) = self.stas.remove(&sa) else {
            return Ok(None);
        };
        if matches!(old.state, State::Connected) {
            chip.clear_cam(FIRST_PAIRWISE_ENTRY)?;
        }
        info!(sta = fmt_mac(&sa), "left");
        if matches!(old.state, State::Wps { .. } | State::WpsDone { .. }) {
            return Ok(None);
        }
        Ok(Some(Event::Disconnected(sa)))
    }

    //endregion

    //region 4-way handshake

    /// Msg 1 (no ptk) or msg 3 (ptk known), unencrypted, at 6 Mbps
    fn send_eapol(
        &self,
        chip: &Chip,
        tx: &mut Tx,
        sa: &Mac,
        anonce: &[u8; 32],
        replay: u64,
        ptk: Option<&wpa::Ptk>,
    ) -> Result<()> {
        let key = match ptk {
            None => wpa::build(wpa::KI_VERSION_AES | wpa::KI_PAIRWISE | wpa::KI_ACK, replay, anonce, &[], None),
            Some(p) => {
                let mut rsn = vec![48, RSN.len() as u8];
                rsn.extend_from_slice(&RSN);
                let data = wpa::msg3_key_data(&p.kek, &rsn, &self.gtk, GTK_IDX)?;
                let ki = wpa::KI_VERSION_AES
                    | wpa::KI_PAIRWISE
                    | wpa::KI_INSTALL
                    | wpa::KI_ACK
                    | wpa::KI_MIC
                    | wpa::KI_SECURE
                    | wpa::KI_ENC_DATA;
                wpa::build(ki, replay, anonce, &data, Some(&p.kck))
            }
        };
        let sta = self.stas.get(sa).expect("station exists");
        let f = self.data_frame(sa, sta, false, ETHERTYPE_EAPOL, &key);
        tx.send(&chip.rtl, TxInfo::data(7, RATE_6M, false), &f)?;
        debug!(sta = fmt_mac(sa), msg = if ptk.is_some() { 3 } else { 1 }, replay, "EAPOL sent");
        Ok(())
    }

    fn on_eapol(&mut self, chip: &Chip, tx: &mut Tx, sa: Mac, eapol: &[u8]) -> Result<Option<Event>> {
        let Some(key) = wpa::parse(eapol) else {
            return Ok(None);
        };
        let Some(sta) = self.stas.get_mut(&sa) else {
            return Ok(None);
        };
        let State::Handshake { anonce, first_replay, replay, ptk, .. } = &sta.state else {
            return Ok(None);
        };
        if !(*first_replay..=*replay).contains(&key.replay) || key.key_info & wpa::KI_MIC == 0 {
            debug!(got = key.replay, want = replay, "EAPOL replay mismatch");
            return Ok(None);
        }
        let (anonce, replay) = (*anonce, *replay);

        match ptk {
            None => {
                // Msg 2: derive the PTK from its SNonce, which also checks the passphrase
                let p = wpa::ptk(&self.pmk, &self.cfg.bssid, &sa, &anonce, &key.nonce);
                if !wpa::verify_mic(&p.kck, eapol) {
                    warn!(sta = fmt_mac(&sa), "msg 2 MIC mismatch; wrong passphrase?");
                    return Ok(None);
                }
                debug!(sta = fmt_mac(&sa), "msg 2 ok");
                let replay = replay + 1;
                self.send_eapol(chip, tx, &sa, &anonce, replay, Some(&p))?;
                let due = Instant::now() + EAPOL_RESEND;
                self.stas.get_mut(&sa).expect("station exists").state =
                    State::Handshake { anonce, first_replay: replay, replay, ptk: Some(p), due, tries: 1 };
                Ok(None)
            }
            Some(p) => {
                let (kck, tk) = (p.kck, p.tk);
                if key.key_info & wpa::KI_SECURE == 0 || !wpa::verify_mic(&kck, eapol) {
                    warn!(sta = fmt_mac(&sa), "bad msg 4");
                    return Ok(None);
                }
                chip.write_cam(FIRST_PAIRWISE_ENTRY, 0, false, &sa, &tk)?;
                sta.state = State::Connected;
                info!(sta = fmt_mac(&sa), "4-way handshake done, keys installed");
                Ok(Some(Event::Connected(sa)))
            }
        }
    }

    /// Sends the delayed msg 1 and resends msg 1 or 3 while the station hasn't answered, each
    /// time w/ a fresh replay counter. Call every few ms.
    pub fn tick(&mut self, chip: &Chip, tx: &mut Tx) -> Result<Vec<Event>> {
        // Just to log the end of a shun on time
        self.is_shunned(&[0; 6]);
        self.release_overdue(chip, tx)?;
        let mut out = Vec::new();
        self.wps_tick(chip, tx, &mut out)?;
        let mut drop = Vec::new();
        let now = Instant::now();
        let due: Vec<Mac> = self
            .stas
            .iter()
            .filter(|(_, s)| matches!(s.state, State::Handshake { due, .. } if due <= now))
            .map(|(m, _)| *m)
            .collect();
        for mac in due {
            let Some(sta) = self.stas.get_mut(&mac) else {
                continue;
            };
            let State::Handshake { anonce, replay, ptk, due, tries, .. } = &mut sta.state else {
                continue;
            };
            if *tries >= EAPOL_TRIES {
                drop.push(mac);
                continue;
            }
            *tries += 1;
            *replay += 1;
            *due = now + EAPOL_RESEND;
            let (anonce, replay, ptk) = (*anonce, *replay, ptk.clone());
            self.send_eapol(chip, tx, &mac, &anonce, replay, ptk.as_ref())?;
        }
        for mac in drop {
            warn!(sta = fmt_mac(&mac), "handshake timed out");
            self.send_deauth(chip, tx, &mac, REASON_4WAY_TIMEOUT)?;
            if let Some(e) = self.on_leave(chip, mac)? {
                out.push(e);
            }
        }
        Ok(out)
    }

    fn send_deauth(&self, chip: &Chip, tx: &mut Tx, sta: &Mac, reason: u16) -> Result<()> {
        let mut f = self.mgmt_header(0x00c0, sta);
        f.extend_from_slice(&reason.to_le_bytes());
        tx.send(&chip.rtl, TxInfo::mgmt(), &f)
    }

    //endregion

    //region WPS

    /// EAPOL frame around an EAP packet, unencrypted at 6 Mbps like the 4-way handshake
    fn send_eap(&self, chip: &Chip, tx: &mut Tx, sa: &Mac, eap: &[u8]) -> Result<()> {
        let mut eapol = vec![2, 0];
        eapol.extend_from_slice(&(eap.len() as u16).to_be_bytes());
        eapol.extend_from_slice(eap);
        let sta = self.stas.get(sa).expect("station exists");
        let f = self.data_frame(sa, sta, false, ETHERTYPE_EAPOL, &eapol);
        tx.send(&chip.rtl, TxInfo::data(7, RATE_6M, false), &f)
    }

    /// EAPOL from a station doing WPS: EAP packets and EAPOL-Start
    fn on_wps_eapol(&mut self, chip: &Chip, tx: &mut Tx, sa: Mac, eapol: &[u8]) -> Result<Option<Event>> {
        if eapol.len() < 4 {
            return Ok(None);
        }
        let len = u16::from_be_bytes([eapol[2], eapol[3]]) as usize;
        let body = eapol.get(4..4 + len).unwrap_or(&eapol[4..]);
        let Some(Sta { state: State::Wps { eap, .. }, .. }) = self.stas.get_mut(&sa) else {
            return Ok(None);
        };
        let action = match eapol[1] {
            0 => eap.on_packet(body),
            1 => wps::Action::Send(eap.restart()),
            _ => wps::Action::Ignore,
        };
        let id = eap.id();
        let enrollee = eap.reg.enrollee.clone();
        match action {
            wps::Action::Ignore => Ok(None),
            wps::Action::Send(p) => {
                self.send_eap(chip, tx, &sa, &p)?;
                if let Some(Sta { state: State::Wps { due, tries, .. }, .. }) = self.stas.get_mut(&sa) {
                    *due = Instant::now() + WPS_RESEND;
                    *tries = 1;
                }
                Ok(None)
            }
            wps::Action::Delivered => {
                self.wps_finish(chip, tx, sa, id)?;
                info!(
                    sta = fmt_mac(&sa),
                    manufacturer = enrollee.manufacturer,
                    model = enrollee.model,
                    name = enrollee.device_name,
                    "WPS: credential delivered"
                );
                Ok(Some(Event::Paired(sa, enrollee.model.trim().to_owned())))
            }
            wps::Action::Fail(why) => {
                self.wps_finish(chip, tx, sa, id)?;
                warn!(sta = fmt_mac(&sa), "WPS failed: {why}");
                Ok(Some(Event::PairingFailed(sa, why)))
            }
        }
    }

    /// EAP-Failure ends EAP-WSC either way; the deauth follows from tick()
    fn wps_finish(&mut self, chip: &Chip, tx: &mut Tx, sa: Mac, id: u8) -> Result<()> {
        self.send_eap(chip, tx, &sa, &wps::eap_failure(id))?;
        if let Some(s) = self.stas.get_mut(&sa) {
            s.state = State::WpsDone { at: Instant::now() };
        }
        Ok(())
    }

    /// The first EAP request, retransmissions, and deauth once WPS is over
    fn wps_tick(&mut self, chip: &Chip, tx: &mut Tx, out: &mut Vec<Event>) -> Result<()> {
        let now = Instant::now();
        let mut resend = Vec::new();
        let mut give_up = Vec::new();
        let mut finished = Vec::new();
        for (mac, s) in self.stas.iter_mut() {
            match &mut s.state {
                State::Wps { eap, due, tries } if *due <= now => {
                    if *tries >= WPS_TRIES {
                        give_up.push((*mac, eap.id()));
                    } else {
                        *tries += 1;
                        *due = now + WPS_RESEND;
                        resend.push((*mac, eap.last.clone()));
                    }
                }
                State::WpsDone { at } if now.duration_since(*at) >= WPS_DEAUTH_DELAY => finished.push(*mac),
                _ => {}
            }
        }
        for (mac, p) in resend {
            debug!(sta = fmt_mac(&mac), id = p[1], "EAP request sent");
            self.send_eap(chip, tx, &mac, &p)?;
        }
        for (mac, id) in give_up {
            self.wps_finish(chip, tx, mac, id)?;
            warn!(sta = fmt_mac(&mac), "WPS timed out");
            out.push(Event::PairingFailed(mac, "timed out".into()));
        }
        for mac in finished {
            self.send_deauth(chip, tx, &mac, REASON_PREV_AUTH_INVALID)?;
            self.stas.remove(&mac);
        }
        Ok(())
    }

    //endregion

    /// Handles one received frame, FCS included. `decrypted` is the RX descriptor's flag.
    pub fn handle(&mut self, chip: &Chip, tx: &mut Tx, f: &[u8], decrypted: bool) -> Result<Option<Event>> {
        if f.len() < 24 + 4 {
            return Ok(None);
        }
        let fc = fc_of(f);
        let (ty, sub) = ((fc >> 2) & 3, (fc >> 4) & 0xf);
        let to_us = addr(f, 4) == self.cfg.bssid;
        let sa = addr(f, 10);
        let body = &f[24..f.len() - 4];

        if to_us && let Some(s) = self.stas.get_mut(&sa) {
            let pm = fc & 0x1000 != 0;
            if pm != s.dozing {
                s.dozing = pm;
                trace!(sta = fmt_mac(&sa), dozing = pm, "power management");
            }
            // Its frame w/ PM clear: awake right now, so out w/ anything held
            if !pm && !s.held.is_empty() {
                let held = std::mem::take(&mut s.held);
                self.release(chip, tx, held, false)?;
            }
        }

        if ty == 0 && to_us && self.is_shunned(&sa) {
            trace!(sta = fmt_mac(&sa), sub, "shunned, ignored");
            return Ok(None);
        }
        if ty == 0 && to_us {
            // A retry of something we already answered: only our ACK got lost
            let seq = u16::from_le_bytes([f[22], f[23]]);
            if fc & 0x0800 != 0 && self.stas.get(&sa).is_some_and(|s| s.last_mgmt_seq == Some(seq)) {
                debug!(sta = fmt_mac(&sa), sub, "management retry dropped");
                return Ok(None);
            }
            let res = match sub {
                11 => self.on_auth(chip, tx, sa, body).map(|()| None),
                0 => self.on_assoc(chip, tx, sa, false, body),
                2 => self.on_assoc(chip, tx, sa, true, body),
                10 | 12 => self.on_leave(chip, sa),
                _ => Ok(None),
            };
            if let Some(s) = self.stas.get_mut(&sa) {
                s.last_mgmt_seq = Some(seq);
            }
            return res;
        }
        if ty == 2 && to_us && fc & 0x0300 == 0x0100 {
            return self.on_data(chip, tx, sa, f, decrypted);
        }
        Ok(None)
    }

    fn on_data(&mut self, chip: &Chip, tx: &mut Tx, sa: Mac, f: &[u8], decrypted: bool) -> Result<Option<Event>> {
        let fc = fc_of(f);
        let sub = (fc >> 4) & 0xf;
        if sub & 0x4 != 0 {
            return Ok(None); // Null / QoS null: power save signaling only
        }
        let Some(sta) = self.stas.get_mut(&sa) else {
            return Ok(None);
        };
        let seq = u16::from_le_bytes([f[22], f[23]]);
        if fc & 0x0800 != 0 && sta.last_seq == Some(seq) {
            return Ok(None); // Retransmission we already have
        }
        sta.last_seq = Some(seq);

        let mut at = 24 + if sub & 0x8 != 0 { 2 } else { 0 };
        let mut end = f.len() - 4;
        if fc & 0x4000 != 0 {
            if !decrypted {
                debug!(sta = fmt_mac(&sa), "protected frame the hardware didn't decrypt");
                return Ok(None);
            }
            at += 8; // CCMP header
            end = end.saturating_sub(8); // MIC
        }
        let Some(p) = f.get(at..end) else {
            return Ok(None);
        };
        if p.len() < 8 || p[..6] != [0xaa, 0xaa, 0x03, 0x00, 0x00, 0x00] {
            return Ok(None);
        }
        let ethertype = u16::from_be_bytes([p[6], p[7]]);
        let payload = &p[8..];
        if ethertype == ETHERTYPE_EAPOL {
            if matches!(sta.state, State::Wps { .. }) {
                return self.on_wps_eapol(chip, tx, sa, payload);
            }
            return self.on_eapol(chip, tx, sa, payload);
        }
        if !matches!(sta.state, State::Connected) {
            return Ok(None);
        }
        Ok(Some(Event::Data(sa, ethertype, payload.to_vec())))
    }

    /// Encrypted data to a connected station. Held while it dozes, if ps_hold is set.
    pub fn send(&mut self, chip: &Chip, tx: &mut Tx, sta_addr: &Mac, ethertype: u16, payload: &[u8]) -> Result<()> {
        self.send_data(chip, tx, sta_addr, ethertype, payload, false)
    }

    /// Like send, for a steady stream that would rather lose a frame than hold up the rest:
    /// no TX report, so it never counts toward MAX_IN_FLIGHT, and fewer retries. Same queue and
    /// power-save order as send, since the CCMP packet numbers have to arrive in order.
    pub fn send_stream(&mut self, chip: &Chip, tx: &mut Tx, sta: &Mac, ethertype: u16, payload: &[u8]) -> Result<()> {
        self.send_data(chip, tx, sta, ethertype, payload, true)
    }

    fn send_data(
        &mut self,
        chip: &Chip,
        tx: &mut Tx,
        sta_addr: &Mac,
        ethertype: u16,
        payload: &[u8],
        stream: bool,
    ) -> Result<()> {
        let Some(sta) = self.stas.get(sta_addr) else { anyhow::bail!("no station {}", fmt_mac(sta_addr)) };
        if !matches!(sta.state, State::Connected) {
            anyhow::bail!("station {} isn't connected", fmt_mac(sta_addr));
        }
        if !stream && tx.data_in_flight(IN_FLIGHT_WINDOW) >= MAX_IN_FLIGHT {
            self.ps_stats.dropped += 1;
            return Ok(());
        }
        let f = self.data_frame(sta_addr, sta, true, ethertype, payload);
        let info = if stream { stream_info() } else { data_info() };
        if stream {
            self.ps_stats.stream += 1;
        }
        let sta = self.stas.get_mut(sta_addr).expect("station exists");
        sta.pn += 1;
        // Behind anything already held, to keep the order
        if self.ps_hold.is_some() && (sta.dozing || !sta.held.is_empty()) {
            sta.held.push_back((Instant::now(), f, info));
            return Ok(());
        }
        self.ps_stats.direct += 1;
        tx.send(&chip.rtl, info, &f)
    }

    fn release(&mut self, chip: &Chip, tx: &mut Tx, held: VecDeque<Held>, timed_out: bool) -> Result<()> {
        for (queued, f, info) in held {
            let waited = queued.elapsed().as_millis() as u32;
            self.ps_stats.max_wait_ms = self.ps_stats.max_wait_ms.max(waited);
            if timed_out {
                self.ps_stats.timed_out += 1;
            } else {
                self.ps_stats.on_wake += 1;
            }
            tx.send(&chip.rtl, info, &f)?;
        }
        Ok(())
    }

    /// Held frames past ps_hold go out anyway; the hardware retries until the station wakes
    fn release_overdue(&mut self, chip: &Chip, tx: &mut Tx) -> Result<()> {
        let Some(hold) = self.ps_hold else {
            return Ok(());
        };
        let overdue: Vec<Mac> = self
            .stas
            .iter()
            .filter(|(_, s)| s.held.front().is_some_and(|(t, ..)| t.elapsed() >= hold))
            .map(|(m, _)| *m)
            .collect();
        for mac in overdue {
            let held = std::mem::take(&mut self.stas.get_mut(&mac).expect("station exists").held);
            self.release(chip, tx, held, true)?;
        }
        Ok(())
    }
}

fn data_info() -> TxInfo {
    TxInfo { retry_limit: Some(DATA_RETRY_LIMIT), ..TxInfo::data(0, DATA_RATE, true) }
}

fn stream_info() -> TxInfo {
    TxInfo { retry_limit: Some(STREAM_RETRY_LIMIT), track: false, ..TxInfo::data(0, DATA_RATE, true) }
}

/// A frame waiting for its station to wake: since when, the frame, how to send it
type Held = (Instant, Vec<u8>, TxInfo);

/// Element `id` present
fn has_ie(ies: &[u8], id: u8) -> bool {
    let mut i = 0;
    while i + 2 <= ies.len() {
        if ies[i] == id {
            return true;
        }
        i += 2 + ies[i + 1] as usize;
    }
    false
}

/// WMM information or parameter element present
fn has_wmm(ies: &[u8]) -> bool {
    let mut i = 0;
    while i + 2 <= ies.len() {
        let (id, len) = (ies[i], ies[i + 1] as usize);
        if let Some(v) = ies.get(i + 2..i + 2 + len) {
            if id == 221 && v.len() >= 5 && v[..4] == [0x00, 0x50, 0xf2, 0x02] {
                return true;
            }
        }
        i += 2 + len;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_wmm_info_element() {
        let ies = [0, 3, b'a', b'b', b'c', 221, 7, 0x00, 0x50, 0xf2, 0x02, 0x00, 0x01, 0x00];
        assert!(has_wmm(&ies));
        assert!(!has_wmm(&ies[..5]));
    }

    #[test]
    fn data_frame_has_qos_ccmp_and_snap() {
        let cfg = ApConfig { ssid: "x".into(), bssid: [1; 6], channel: 1, beacon_int: 100, hidden: false, wps: None };
        let bss = Bss::new(cfg, [0; 32]);
        let sta = Sta {
            aid: 1,
            wmm: true,
            state: State::Connected,
            pn: 0x0102030405,
            last_seq: None,
            last_mgmt_seq: None,
            dozing: false,
            held: VecDeque::new(),
        };
        let f = bss.data_frame(&[2; 6], &sta, true, 0x892e, &[0xee]);
        assert_eq!(&f[0..2], &[0x88, 0x42]);
        assert_eq!(&f[26..34], &[0x05, 0x04, 0, 0x20, 0x03, 0x02, 0x01, 0x00]);
        assert_eq!(&f[34..42], &[0xaa, 0xaa, 0x03, 0, 0, 0, 0x89, 0x2e]);
        assert_eq!(f[42], 0xee);
    }
}
