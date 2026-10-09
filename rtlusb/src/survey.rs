//! Channel survey: how much 2.4 GHz traffic each channel carries, to host on the quietest.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{info, warn};

use crate::chip::Chip;
use crate::radio::rx_loop;

/// US 2.4 GHz channels
pub const CHANNELS: std::ops::RangeInclusive<u8> = 1..=11;
/// The non-overlapping set; anything between them overlaps two
pub const CANDIDATES: [u8; 3] = [1, 6, 11];
const DWELL: Duration = Duration::from_millis(400);
/// Frames still in flight from the last channel after a switch
const SETTLE: Duration = Duration::from_millis(20);
/// What each channel's counted for, after SETTLE
const LISTEN: Duration = DWELL.saturating_sub(SETTLE);

/// Frames this quiet or quieter don't count; at this level and up they all do in full
const FLOOR_DBM: f64 = -95.0;
const FULL_DBM: f64 = -50.0;

/// What one channel sounded like
#[derive(Debug, Clone, Copy, Default)]
pub struct Load {
    pub frames: u32,
    /// Weighted airtime, us: each frame's time on air times its weight(). Busy matters as much
    /// as loud, since our own signal is modest and every frame is air we wait out or collide
    /// with; summing mW instead let one loud network drown out everything else. Scaled to a full
    /// LISTEN, so a channel that got less time to itself isn't any quieter for it.
    pub busy_us: f64,
    pub max_dbm: Option<i8>,
}

/// 0 at FLOOR_DBM, 1 at FULL_DBM and up, linear in dB between
fn weight(dbm: i8) -> f64 {
    ((f64::from(dbm) - FLOOR_DBM) / (FULL_DBM - FLOOR_DBM)).clamp(0.0, 1.0)
}

/// Time on air for `bytes` at a descriptor rate index, preamble included
fn airtime_us(rate: u8, bytes: usize) -> f64 {
    const LEGACY_MBPS: [f64; 12] = [1.0, 2.0, 5.5, 11.0, 6.0, 9.0, 12.0, 18.0, 24.0, 36.0, 48.0, 54.0];
    // HT/VHT: 1SS 20 MHz long GI, MCS0-7 then 8-9; multi-stream rates scale up
    const MCS_MBPS: [f64; 10] = [6.5, 13.0, 19.5, 26.0, 39.0, 52.0, 58.5, 65.0, 78.0, 86.7];
    let (mbps, preamble) = match rate {
        0..=3 => (LEGACY_MBPS[rate as usize], 192.0),
        4..=11 => (LEGACY_MBPS[rate as usize], 20.0),
        12..=43 => (MCS_MBPS[((rate - 12) % 8) as usize] * f64::from((rate - 12) / 8 + 1), 36.0),
        44.. => (MCS_MBPS[((rate - 44) % 10) as usize] * f64::from((rate - 44) / 10 + 1), 36.0),
    };
    preamble + bytes as f64 * 8.0 / mbps
}

/// Listens on every channel in CHANNELS for DWELL each, timed from when the switch to it is
/// done. The chip should accept all frames (promiscuous) for this to see other networks' data.
/// A channel it never got to is left out rather than passed off as idle.
pub fn survey(chip: &Chip) -> Result<Vec<(u8, Load)>> {
    let mut loads: Vec<(u8, Load)> = CHANNELS.map(|c| (c, Load::default())).collect();
    let mut listened = vec![Duration::ZERO; loads.len()];
    let mut i = 0;
    chip.set_channel(loads[0].0)?;
    let mut since = Instant::now();
    let quit = AtomicBool::new(false);
    // Only a backstop; the last channel's dwell ends it
    let limit = DWELL * loads.len() as u32 * 2;
    rx_loop(chip, Some(limit), Duration::from_millis(5), &quit, &mut |p| {
        if quit.load(Ordering::Relaxed) {
            return Ok(());
        }
        if since.elapsed() >= DWELL {
            listened[i] = since.elapsed().saturating_sub(SETTLE);
            if i + 1 == loads.len() {
                quit.store(true, Ordering::Relaxed);
                return Ok(());
            }
            i += 1;
            chip.set_channel(loads[i].0)?;
            since = Instant::now();
        }
        let Some(p) = p else { return Ok(()) };
        if p.c2h || since.elapsed() < SETTLE {
            return Ok(());
        }
        let l = &mut loads[i].1;
        l.frames += 1;
        if let Some(dbm) = p.dbm {
            l.busy_us += airtime_us(p.rate, p.frame.len()) * weight(dbm);
            l.max_dbm = Some(l.max_dbm.map_or(dbm, |m| m.max(dbm)));
        }
        Ok(())
    })?;
    if !quit.load(Ordering::Relaxed) {
        listened[i] = since.elapsed().saturating_sub(SETTLE);
    }
    let mut heard = Vec::with_capacity(loads.len());
    for ((c, mut l), t) in loads.into_iter().zip(listened) {
        if t.is_zero() {
            warn!(channel = c, "survey never got to it");
            continue;
        }
        l.busy_us *= LISTEN.as_secs_f64() / t.as_secs_f64();
        let busy_pct = format!("{:.1}", l.busy_us / LISTEN.as_micros() as f64 * 100.0);
        let listened_ms = t.as_millis() as u64;
        info!(channel = c, frames = l.frames, max_dbm = l.max_dbm, busy_pct, listened_ms, "survey");
        heard.push((c, l));
    }
    Ok(heard)
}

/// How much of channel `a`'s 20 MHz lands on channel `b`: channels are 5 MHz apart, so
/// 4 apart is clear
fn overlap(a: u8, b: u8) -> f64 {
    (1.0 - f64::from(a.abs_diff(b)) / 4.0).max(0.0)
}

/// Interference each candidate would see: every channel's load, weighted by its overlap
pub fn score(loads: &[(u8, Load)], channel: u8) -> f64 {
    loads.iter().map(|(c, l)| l.busy_us * overlap(*c, channel)).sum()
}

/// The quietest of CANDIDATES the survey heard, or of all of them if it heard none. Ties go to
/// the higher channel, since 11 is the default.
pub fn pick(loads: &[(u8, Load)]) -> u8 {
    let heard: Vec<u8> = CANDIDATES.into_iter().filter(|c| loads.iter().any(|(h, _)| h == c)).collect();
    let options = if heard.is_empty() { &CANDIDATES[..] } else { &heard[..] };
    let best = options
        .iter()
        .copied()
        .min_by(|a, b| score(loads, *a).total_cmp(&score(loads, *b)).then(b.cmp(a)))
        .expect("non-empty");
    let scores: Vec<String> = CANDIDATES.iter().map(|c| format!("{c}: {:.0}", score(loads, *c))).collect();
    info!(channel = best, ?scores, "quietest channel");
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loads(busy: &[(u8, i8, u32)]) -> Vec<(u8, Load)> {
        CHANNELS
            .map(|c| {
                let mut l = Load::default();
                // Beacon-sized frames at 1 Mbps
                for &(_, dbm, n) in busy.iter().filter(|b| b.0 == c) {
                    l.frames += n;
                    l.busy_us += f64::from(n) * airtime_us(0, 300) * weight(dbm);
                }
                (c, l)
            })
            .collect()
    }

    #[test]
    fn quiet_air_picks_11() {
        assert_eq!(pick(&loads(&[])), 11);
    }

    #[test]
    fn bleed_from_4_rules_out_1_and_6() {
        // The setup this was written for: ch1 busy, a loud network on ch4
        let l = loads(&[(1, -75, 200), (4, -48, 50), (3, -60, 20), (5, -60, 20), (11, -85, 30)]);
        assert_eq!(pick(&l), 11);
    }

    #[test]
    fn busy_11_moves_elsewhere() {
        // 10 doesn't reach 6, so 6 hears nothing at all
        let l = loads(&[(11, -40, 300), (10, -50, 100), (1, -85, 10)]);
        assert_eq!(pick(&l), 6);
        let l = loads(&[(11, -40, 300), (8, -50, 100), (1, -85, 10)]);
        assert_eq!(pick(&l), 1);
    }

    #[test]
    fn busy_weak_channel_outweighs_one_loud_beacon() {
        // 400 frames at -78 on 6 vs 10 at -45 on 1: mW summing would have picked 6
        let l = loads(&[(1, -45, 10), (6, -78, 400), (11, -60, 300)]);
        assert_eq!(pick(&l), 1);
    }

    #[test]
    fn unheard_candidate_isnt_picked() {
        // 11 was never reached; quiet as it'd score, it's unknown
        let mut l = loads(&[(1, -60, 50), (6, -60, 100)]);
        l.retain(|(c, _)| *c < 9);
        assert_eq!(pick(&l), 1);
        assert_eq!(pick(&[]), 11);
    }

    #[test]
    fn weights_and_airtime() {
        assert_eq!(weight(-100), 0.0);
        assert_eq!(weight(-40), 1.0);
        assert!((weight(-72) - 0.511).abs() < 0.01);
        assert_eq!(airtime_us(4, 75), 120.0);
        assert_eq!(airtime_us(0, 100), 992.0);
    }

    #[test]
    fn overlap_falls_off() {
        assert_eq!(overlap(6, 6), 1.0);
        assert_eq!(overlap(4, 6), 0.5);
        assert_eq!(overlap(1, 6), 0.0);
    }
}
