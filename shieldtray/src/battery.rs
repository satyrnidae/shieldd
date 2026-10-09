//! The battery as a tray shows it: a percent from the controller's voltage reading, steadied
//! against load and the charger, and whether it's done charging. No platform code; a tray for
//! any platform can use it.

use std::time::{Duration, Instant};

/// The battery, from the controller's status answer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    pub charging: bool,
    /// The controller's own level band: 100 (on the charger), 75, 50, 25, 10, 0 seen so far
    pub band: u8,
    /// Mailbox 01 bytes 2-3, a voltage reading
    pub reading: u16,
    /// Below the rumble cutoff
    pub critical: bool,
    /// Kicked until it charges
    pub must_charge: bool,
}

/// Reading -> percent on battery, straight between points. The reading is the cell's voltage at
/// 4.5 V full scale. A Li-ion cell at rest: flat through the middle, steep at both ends. The
/// controller's own bands agree: 25 starts at 0xD47F (3.74 V), 10 at 0xD081 (3.67 V), and 75 was
/// still showing at 0xEB29 (4.13 V). 0% is where it gave out on 2026-10-08 (~0xCCFD). On the
/// charger, the same curve after CHARGE_LIFT.
pub(crate) const CURVE: [(u16, u8); 7] = [
    (EMPTY, 0),
    (0xD081, 10),
    (0xD47F, 25),
    // 3.84 V
    (0xDA74, 50),
    // 3.98 V
    (0xE26B, 75),
    // 4.11 V
    (0xE9D0, 90),
    // 4.20 V
    (0xEEEF, 100),
];

pub(crate) const EMPTY: u16 = 0xCD00;
/// A reading this much above what's shown means it was charged while away
pub(crate) const RECHARGED_BY: u8 = 10;

/// How far the charger lifts the reading the moment it's plugged in: 0xDCB2 -> 0xE34E and
/// 0xDD36 -> 0xE374 on 2026-10-08. A straight line from EMPTY to 0xE800 used to stand in for
/// the curve on the charger and read 100 at 0xF1D3, which was 91% off it on 2026-10-09.
pub(crate) const CHARGE_LIFT: u16 = 0x0660;

/// The controller never says it's done charging; band 100 and the charging bit stay up. Near
/// full the charger holds the reading flat while it tops off, then it falls this far when the
/// charger stops: 0xF43D -> 0xF23B as the light went green on 2026-10-09.
pub(crate) const CHARGED_DROP: u16 = 0x0180;
/// Only from this high; lower down, a dip under load isn't the charger stopping
pub(crate) const CHARGED_PEAK: u16 = 0xF200;
/// And only once it's stayed down this long; a rumble or audio burst comes back up
pub(crate) const CHARGED_DROP_FOR: Duration = Duration::from_secs(10);
/// Plugged in already full, the charger may not top it off at all: 0xF0A8 at rest -> 0xF2C0 and
/// flat, light green, on 2026-10-09. A top-up starts ~10 s in and lifts it ~0x410 over rest;
/// one that isn't full, ~0x660. So full at rest and still under this lift this long after
/// plugging in is charged.
pub(crate) const IDLE_LIFT: u16 = 0x0300;
pub(crate) const IDLE_AFTER: Duration = Duration::from_secs(30);

/// On battery, see CURVE
pub(crate) fn percent(reading: u16) -> u8 {
    let Some(i) = CURVE.iter().position(|&(r, _)| reading < r) else { return 100 };
    let Some(&(r0, p0)) = i.checked_sub(1).map(|i| &CURVE[i]) else { return 0 };
    let (r1, p1) = CURVE[i];
    p0 + (u32::from(reading - r0) * u32::from(p1 - p0) / u32::from(r1 - r0)) as u8
}

/// On the charger, see CURVE
pub(crate) fn charging_percent(reading: u16) -> u8 {
    percent(reading.saturating_sub(CHARGE_LIFT))
}

/// The percent to show, from readings that bounce under load and jump w/ the charger. On
/// battery it only goes down, on the charger only up, and each switch starts over.
#[derive(Debug, Default)]
pub(crate) struct Gauge {
    pct: Option<u8>,
    charging: Option<bool>,
    /// Highest reading on the charger since it was plugged in, 0 off it
    peak: u16,
    /// Last reading off the charger this connection, 0 if none
    rest: u16,
    /// When it was plugged in this connection
    plugged: Option<Instant>,
    /// When the reading fell CHARGED_DROP below the peak, if it's still down
    dropped: Option<Instant>,
    /// When to look again w/o a new reading, since none may come: IDLE_AFTER or CHARGED_DROP_FOR in
    look_at: Option<Instant>,
}

impl Gauge {
    /// Returns the percent to show, and whether a discharge just began (re-arms the critical alert)
    pub(crate) fn feed(&mut self, b: &Battery, now: Instant) -> (u8, bool) {
        let switched = self.charging != Some(b.charging);
        if !b.charging {
            (self.peak, self.rest, self.plugged) = (0, b.reading, None);
        } else if switched {
            (self.peak, self.plugged) = (b.reading, Some(now));
        } else {
            self.peak = self.peak.max(b.reading);
        }
        let down = b.charging && self.peak >= CHARGED_PEAK && self.peak - b.reading >= CHARGED_DROP;
        self.dropped = if down { self.dropped.or(Some(now)) } else { None };
        let let_go = self.dropped.is_some_and(|t| now >= t + CHARGED_DROP_FOR);
        let idle = self.rest >= CURVE[CURVE.len() - 1].0
            && self.plugged.is_some_and(|t| now >= t + IDLE_AFTER)
            && self.peak < self.rest.saturating_add(IDLE_LIFT);
        let due = [self.plugged.map(|t| t + IDLE_AFTER), self.dropped.map(|t| t + CHARGED_DROP_FOR)];
        self.look_at = due.into_iter().flatten().filter(|&t| t > now).min();
        let p = match b.charging {
            true if let_go || idle => 100,
            true => charging_percent(b.reading),
            false => percent(b.reading),
        };
        let (pct, fresh) = match self.pct {
            None => (p, true),
            Some(_) if switched => (p, !b.charging),
            Some(shown) if b.charging => (shown.max(p), false),
            // Charged while disconnected
            Some(shown) if p >= shown.saturating_add(RECHARGED_BY) => (p, true),
            Some(shown) => (shown.min(p), false),
        };
        self.pct = Some(pct);
        self.charging = Some(b.charging);
        (pct, fresh)
    }

    /// Time for another look, once per look_at
    pub(crate) fn look_due(&mut self, now: Instant) -> bool {
        let due = self.look_at.is_some_and(|t| now >= t);
        if due {
            self.look_at = None;
        }
        due
    }

    /// The controller's gone: what the charger did belongs to that connection. The percent
    /// stays, to tell a charge elsewhere.
    pub(crate) fn forget_charger(&mut self) {
        (self.peak, self.rest, self.plugged, self.dropped, self.look_at) = (0, 0, None, None, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_scale() {
        assert_eq!(percent(0xCCFD), 0);
        assert_eq!(percent(0xCD00), 0);
        assert_eq!(percent(0xEEEF), 100);
        assert_eq!(percent(0xF000), 100);
        // Where the controller's bands start
        assert_eq!(percent(0xD47F), 25);
        assert_eq!(percent(0xD081), 10);
        // Band 75, fresh off the charger; the old straight line said 100
        assert_eq!(percent(0xEB29), 92);
        // On the charger
        assert_eq!(charging_percent(EMPTY + CHARGE_LIFT), 0);
        // Band 100 on the charger, 91% off it; the old straight line said 100 and "charged"
        assert_eq!(charging_percent(0xF1D3), 93);
    }

    #[test]
    fn gauge_follows_the_charger() {
        let t0 = Instant::now();
        let b = |charging, reading| Battery { charging, band: 10, reading, critical: false, must_charge: false };
        let mut g = Gauge::default();
        let (p, fresh) = g.feed(&b(false, 0xD0FD), t0);
        assert!(fresh && p == 11);
        // Plugging in lifts the reading ~0x660; the gauge doesn't jump w/ it
        let (p, _) = g.feed(&b(true, 0xD0FD + 0x660), t0);
        assert_eq!(p, 11);
        assert_eq!(g.feed(&b(true, 0xD600 + 0x660), t0).0, 31);
        // A dip while charging doesn't take it back down
        assert_eq!(g.feed(&b(true, 0xD500 + 0x660), t0).0, 31);
        // Unplugged: starts over from the resting reading, then only goes down
        let (p, fresh) = g.feed(&b(false, 0xD580), t0);
        assert!(fresh && p == 29);
        assert_eq!(g.feed(&b(false, 0xD700), t0).0, 29);
        assert_eq!(g.feed(&b(false, 0xD400), t0).0, 23);
        // Back after charging elsewhere
        let (p, fresh) = g.feed(&b(false, 0xE000), t0);
        assert!(fresh && p == 67);
    }

    #[test]
    fn charged_when_the_charger_lets_go() {
        let t0 = Instant::now();
        let b = |charging, reading| Battery { charging, band: 100, reading, critical: false, must_charge: false };
        let mut g = Gauge::default();
        // 2026-10-09: plugged in full, topped off, then the light went green
        g.feed(&b(false, 0xF042), t0);
        assert_eq!(g.feed(&b(true, 0xF248), t0).0, 94);
        assert_eq!(g.feed(&b(true, 0xF43D), t0).0, 97);
        // Down, but not for long enough yet
        assert_eq!(g.feed(&b(true, 0xF23B), t0).0, 97);
        let later = t0 + CHARGED_DROP_FOR;
        assert!(g.look_due(later));
        assert_eq!(g.feed(&b(true, 0xF23B), later).0, 100);
        // And stays there
        assert_eq!(g.feed(&b(true, 0xF2A0), later).0, 100);
        // Up again before CHARGED_DROP_FOR: a dip under load
        let mut g = Gauge::default();
        g.feed(&b(true, 0xF43D), t0);
        g.feed(&b(true, 0xF23B), t0);
        g.feed(&b(true, 0xF430), t0 + Duration::from_secs(3));
        assert!(g.feed(&b(true, 0xF23B), t0 + Duration::from_secs(11)).0 < 100);
        // A dip under load lower down is just a dip
        let mut g = Gauge::default();
        g.feed(&b(true, 0xF100), t0);
        assert!(g.feed(&b(true, 0xEF00), t0).0 < 100);
        // Unplugged and back: starts over
        let mut g = Gauge::default();
        g.feed(&b(true, 0xF43D), t0);
        g.feed(&b(true, 0xF23B), t0);
        g.feed(&b(false, 0xF042), t0);
        assert!(g.feed(&b(true, 0xF248), t0).0 < 100);
    }

    #[test]
    fn charged_when_the_charger_never_starts() {
        let t0 = Instant::now();
        let later = t0 + IDLE_AFTER;
        let b = |charging, reading| Battery { charging, band: 100, reading, critical: false, must_charge: false };
        // 2026-10-09: plugged in full, light straight to green, reading flat
        let mut g = Gauge::default();
        g.feed(&b(false, 0xF0A8), t0);
        assert!(g.feed(&b(true, 0xF2C0), t0).0 < 100);
        assert!(!g.look_due(t0));
        assert!(g.look_due(later));
        assert!(!g.look_due(later));
        assert_eq!(g.feed(&b(true, 0xF2C0), later).0, 100);
        // Topping off instead: lifted well past rest
        let mut g = Gauge::default();
        g.feed(&b(false, 0xF042), t0);
        g.feed(&b(true, 0xF248), t0);
        assert!(g.feed(&b(true, 0xF43D), later).0 < 100);
        // Not full at rest: charging, however long
        let mut g = Gauge::default();
        g.feed(&b(false, 0xE000), t0);
        g.feed(&b(true, 0xE200), t0);
        assert!(g.feed(&b(true, 0xE200), later).0 < 100);
        // On the charger at connect, no resting reading to go by
        let mut g = Gauge::default();
        g.feed(&b(true, 0xF2C0), t0);
        assert!(g.feed(&b(true, 0xF2C0), later).0 < 100);
        // Full at rest, gone, drained elsewhere, back on the charger: that rest was another time
        let mut g = Gauge::default();
        g.feed(&b(false, 0xF0A8), t0);
        g.forget_charger();
        g.feed(&b(true, 0xDE00), t0);
        assert!(g.feed(&b(true, 0xDE00), later).0 < 100);
    }
}
