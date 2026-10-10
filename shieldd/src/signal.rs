//! A controller's link quality, from rtlusb's running averages, as the tray's signal bars.
//!
//! The thresholds are a first guess, to tune on hardware: the signal is at our antenna, so it
//! speaks for the controller-to-PC direction, and our failed sends are the only word on the other.
//! Downlink data gets 12 retries before it fails, so even a few failures mean a struggling link.
//! The controller's resends alone top out at OK: past half of its frames resent, at a strong
//! signal and with nothing failing, input still felt the same.

use std::time::{Duration, Instant};

use rtlusb::LinkQuality;
use shdtray::Signal;

/// Bars by signal, dBm: at or below each, at most that many
const DROPPING_DBM: f32 = -88.0;
const LOW_DBM: f32 = -80.0;
const FAIR_DBM: f32 = -70.0;
/// Bars by our sends that ran out of retries, as a share: at or past each, at most that many
const DROPPING_TX_FAIL: f32 = 0.5;
const LOW_TX_FAIL: f32 = 0.1;
const FAIR_TX_FAIL: f32 = 0.02;
/// Bars by its frames that were resends, as a share; a busy channel makes a few
const FAIR_RX_RETRY: f32 = 0.1;
/// Too few reports or frames to go on yet; the shares only count past these
const MIN_TX_REPORTS: u32 = 8;
const MIN_RX_FRAMES: u32 = 16;
/// Nothing from a controller for this long and it's dropping. Longer than radio.rs's STALE: an
/// idle controller that isn't streaming goes quiet for up to ~2 s at a time.
pub const HEARD_WITHIN: Duration = Duration::from_secs(5);
/// A new level shows once it's held this long, so the bars and an open menu don't flicker
/// between two. Dropping shows at once.
const SETTLE: Duration = Duration::from_secs(2);

/// The bars for `q`. `heard` is whether a frame came from it lately; without, it's dropping.
pub fn of(q: Option<&LinkQuality>, heard: bool) -> Signal {
    let Some(q) = q.filter(|_| heard) else { return Signal::Dropping };
    let dbm = q.dbm.unwrap_or(0.0);
    let tx_fail = if q.tx_reports >= MIN_TX_REPORTS { q.tx_fail } else { 0.0 };
    let rx_retry = if q.rx_frames >= MIN_RX_FRAMES { q.rx_retry } else { 0.0 };
    if dbm <= DROPPING_DBM || tx_fail >= DROPPING_TX_FAIL {
        Signal::Dropping
    } else if dbm <= LOW_DBM || tx_fail >= LOW_TX_FAIL {
        Signal::Low
    } else if dbm <= FAIR_DBM || tx_fail >= FAIR_TX_FAIL || rx_retry >= FAIR_RX_RETRY {
        Signal::Fair
    } else {
        Signal::Good
    }
}

/// The level shown for one controller, held back by SETTLE
#[derive(Debug, Default)]
pub struct Shown {
    shown: Option<Signal>,
    /// A different level, and since when it's been the answer
    next: Option<(Signal, Instant)>,
}

impl Shown {
    /// The level to show, given what `of` says now. The first answer shows at once.
    pub fn update(&mut self, now: Instant, want: Signal) -> Signal {
        let Some(shown) = self.shown.filter(|&s| s != want && want != Signal::Dropping) else {
            self.shown = Some(want);
            self.next = None;
            return want;
        };
        match self.next {
            Some((n, since)) if n == want && now.duration_since(since) >= SETTLE => {
                self.shown = Some(want);
                self.next = None;
                want
            }
            Some((n, _)) if n == want => shown,
            _ => {
                self.next = Some((want, now));
                shown
            }
        }
    }

    pub fn shown(&self) -> Option<Signal> {
        self.shown
    }

    /// Starts over, for a new session
    pub fn reset(&mut self) {
        *self = Shown::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(dbm: f32, tx_fail: f32, rx_retry: f32) -> LinkQuality {
        LinkQuality { dbm: Some(dbm), rx_retry, rx_dup: 0.0, rx_frames: 100, tx_fail, tx_reports: 100 }
    }

    #[test]
    fn levels() {
        assert_eq!(of(Some(&q(-50.0, 0.0, 0.0)), true), Signal::Good);
        assert_eq!(of(Some(&q(-50.0, 0.0, 0.0)), false), Signal::Dropping, "nothing heard lately");
        assert_eq!(of(None, true), Signal::Dropping);
        assert_eq!(of(Some(&q(-72.0, 0.0, 0.0)), true), Signal::Fair);
        assert_eq!(of(Some(&q(-82.0, 0.0, 0.0)), true), Signal::Low);
        assert_eq!(of(Some(&q(-90.0, 0.0, 0.0)), true), Signal::Dropping);
        // A strong signal doesn't make up for sends that don't get through
        assert_eq!(of(Some(&q(-40.0, 0.05, 0.0)), true), Signal::Fair);
        assert_eq!(of(Some(&q(-40.0, 0.2, 0.0)), true), Signal::Low);
        assert_eq!(of(Some(&q(-40.0, 0.6, 0.0)), true), Signal::Dropping);
        assert_eq!(of(Some(&q(-40.0, 0.0, 0.15)), true), Signal::Fair);
        assert_eq!(of(Some(&q(-40.0, 0.0, 0.6)), true), Signal::Fair, "resends alone are OK at worst");
        // Too few to go on: one failed report in two isn't a verdict
        let early = LinkQuality { tx_reports: 2, rx_frames: 2, ..q(-50.0, 0.5, 0.5) };
        assert_eq!(of(Some(&early), true), Signal::Good);
        // No PHY status yet: the rates alone
        assert_eq!(of(Some(&LinkQuality { dbm: None, ..q(0.0, 0.0, 0.0) }), true), Signal::Good);
    }

    #[test]
    fn settles_before_changing_but_drops_at_once() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_millis(s);
        let mut s = Shown::default();
        assert_eq!(s.update(at(0), Signal::Good), Signal::Good);
        assert_eq!(s.update(at(100), Signal::Fair), Signal::Good);
        assert_eq!(s.update(at(1_000), Signal::Good), Signal::Good, "back before it settled");
        assert_eq!(s.update(at(1_100), Signal::Fair), Signal::Good);
        assert_eq!(s.update(at(3_000), Signal::Fair), Signal::Good, "its own two seconds, from 1.1");
        assert_eq!(s.update(at(3_100), Signal::Fair), Signal::Fair);
        assert_eq!(s.update(at(3_200), Signal::Dropping), Signal::Dropping);
        assert_eq!(s.update(at(3_300), Signal::Good), Signal::Dropping);
        assert_eq!(s.update(at(5_300), Signal::Good), Signal::Good);
        s.reset();
        assert_eq!(s.update(at(5_400), Signal::Low), Signal::Low, "a new session shows at once");
    }
}
