//! How well a station's getting through, both ways: its signal at our antenna, how many of its
//! frames were resends, and how many of ours ran out of retries. Running averages, a step per
//! frame, so a quiet station doesn't go stale on a timer.

/// Weight of each new frame in the averages, about the last 1/alpha frames' worth
const DBM_ALPHA: f32 = 1.0 / 8.0;
const RATE_ALPHA: f32 = 1.0 / 16.0;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LinkQuality {
    /// Its signal, dBm, once a frame came with a PHY status
    pub dbm: Option<f32>,
    /// Share of its frames that were resends, 0 to 1
    pub rx_retry: f32,
    /// Share of its frames that were resends of data we already had: our ACK didn't reach it.
    /// The rest of rx_retry is first tries we never heard.
    pub rx_dup: f32,
    pub rx_frames: u32,
    /// Share of our reported frames to it that ran out of retries, 0 to 1
    pub tx_fail: f32,
    pub tx_reports: u32,
}

impl LinkQuality {
    /// A frame from it, with its signal, the frame control's retry bit, and whether it's a resend
    /// of data we already had
    pub fn on_rx(&mut self, dbm: Option<i8>, retry: bool, dup: bool) {
        if let Some(d) = dbm {
            let d = f32::from(d);
            self.dbm = Some(self.dbm.map_or(d, |avg| avg + (d - avg) * DBM_ALPHA));
        }
        self.rx_frames = self.rx_frames.saturating_add(1);
        self.rx_retry = step(self.rx_retry, retry, self.rx_frames);
        self.rx_dup = step(self.rx_dup, dup, self.rx_frames);
    }

    /// A TX report for a frame to it
    pub fn on_tx(&mut self, failed: bool) {
        self.tx_reports = self.tx_reports.saturating_add(1);
        self.tx_fail = step(self.tx_fail, failed, self.tx_reports);
    }
}

/// `avg` with one more yes or no, the `n`th. A plain mean until there are enough for the
/// running one, so the first few don't swing it.
fn step(avg: f32, yes: bool, n: u32) -> f32 {
    let alpha = RATE_ALPHA.max(1.0 / n as f32);
    avg + (f32::from(u8::from(yes)) - avg) * alpha
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_start_as_a_mean_then_run() {
        let mut q = LinkQuality::default();
        q.on_tx(true);
        assert_eq!(q.tx_fail, 1.0);
        q.on_tx(false);
        assert_eq!(q.tx_fail, 0.5);
        for _ in 0..200 {
            q.on_tx(false);
        }
        assert!(q.tx_fail < 0.001);
        q.on_tx(true);
        assert!((q.tx_fail - RATE_ALPHA).abs() < 0.001, "{}", q.tx_fail);
    }

    #[test]
    fn signal_averages_only_frames_with_one() {
        let mut q = LinkQuality::default();
        q.on_rx(None, false, false);
        assert_eq!(q.dbm, None);
        q.on_rx(Some(-60), true, true);
        assert_eq!(q.dbm, Some(-60.0));
        q.on_rx(Some(-68), false, false);
        assert_eq!(q.dbm, Some(-61.0));
        assert_eq!(q.rx_frames, 3);
        assert!((q.rx_retry - 1.0 / 3.0).abs() < 0.001, "{}", q.rx_retry);
        assert!((q.rx_dup - 1.0 / 3.0).abs() < 0.001, "{}", q.rx_dup);
    }
}
