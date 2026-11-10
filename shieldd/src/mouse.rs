//! The trackpad's mouse options as the tray flips them: pointer acceleration and trackball. The
//! flags are where they start; a file in the settings dir, written on each change, wins from then
//! on.

use std::path::PathBuf;

use crate::outputs::{Accel, Trackball};

/// Where the choices are kept
pub fn file() -> PathBuf {
    crate::settings_dir().join("mouse")
}

/// Which options are on, and what they mean when they are
#[derive(Debug, Clone, PartialEq)]
pub struct Mouse {
    accel_on: bool,
    trackball_on: bool,
    /// What acceleration is while it's on
    curve: Accel,
    glide_ms: f32,
}

impl Mouse {
    /// From the flags, then `saved`, the text of the file. A flag that turned acceleration off
    /// leaves `Accel::DEFAULT` for the tray to turn on. Trackball is never on without
    /// acceleration, however the two got there.
    pub fn new(flag_accel: Accel, glide_ms: f32, flag_trackball: bool, saved: &str) -> Mouse {
        let (accel, trackball) = parse(saved);
        let accel_on = accel.unwrap_or_else(|| flag_accel.enabled());
        let curve = if flag_accel.enabled() { flag_accel } else { Accel::DEFAULT };
        let trackball_on = accel_on && trackball.unwrap_or(flag_trackball);
        Mouse { accel_on, trackball_on, curve, glide_ms }
    }

    pub fn accel_on(&self) -> bool {
        self.accel_on
    }

    pub fn trackball_on(&self) -> bool {
        self.trackball_on
    }

    /// What the pointer runs with now
    pub fn accel(&self) -> Accel {
        if self.accel_on { self.curve } else { Accel::OFF }
    }

    pub fn trackball(&self) -> Option<Trackball> {
        self.trackball_on.then_some(Trackball { glide_ms: self.glide_ms })
    }

    /// Turns acceleration on or off; off takes trackball with it, and on doesn't bring it back.
    /// Returns whether anything changed.
    pub fn set_accel(&mut self, on: bool) -> bool {
        let was = (self.accel_on, self.trackball_on);
        self.accel_on = on;
        self.trackball_on &= on;
        was != (self.accel_on, self.trackball_on)
    }

    /// Turns trackball on or off, which needs acceleration to be on. Returns whether anything
    /// changed.
    pub fn set_trackball(&mut self, on: bool) -> bool {
        let on = on && self.accel_on;
        let changed = on != self.trackball_on;
        self.trackball_on = on;
        changed
    }

    /// The file's text
    pub fn render(&self) -> String {
        let word = |on| if on { "on" } else { "off" };
        format!("accel={}\ntrackball={}\n", word(self.accel_on), word(self.trackball_on))
    }
}

/// What the file says of each, if anything. Lines it doesn't follow are skipped.
fn parse(text: &str) -> (Option<bool>, Option<bool>) {
    let (mut accel, mut trackball) = (None, None);
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        let on = match value.trim().to_ascii_lowercase().as_str() {
            "on" => true,
            "off" => false,
            _ => continue,
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "accel" => accel = Some(on),
            "trackball" => trackball = Some(on),
            _ => {}
        }
    }
    (accel, trackball)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURVE: Accel = Accel { min: 0.5, knee: 2.0 };

    fn mouse(flag_trackball: bool, saved: &str) -> Mouse {
        Mouse::new(CURVE, 400.0, flag_trackball, saved)
    }

    #[test]
    fn the_flags_are_the_start() {
        let m = mouse(false, "");
        assert!(m.accel_on() && !m.trackball_on());
        assert_eq!((m.accel(), m.trackball()), (CURVE, None));
        let m = mouse(true, "");
        assert_eq!(m.trackball(), Some(Trackball { glide_ms: 400.0 }));
        // Acceleration off by flag: off, with the stock curve ready for the tray
        let m = Mouse::new(Accel::OFF, 400.0, false, "");
        assert!(!m.accel_on());
        assert_eq!(m.accel(), Accel::OFF);
        let mut m = m;
        assert!(m.set_accel(true));
        assert_eq!(m.accel(), Accel::DEFAULT);
    }

    #[test]
    fn the_file_wins_over_the_flags() {
        let m = mouse(true, "accel=on\ntrackball=off\n");
        assert!(m.accel_on() && !m.trackball_on());
        let m = mouse(false, "accel=on\ntrackball=on\n");
        assert!(m.trackball_on());
        let m = mouse(false, "accel=off\n");
        assert!(!m.accel_on());
        // A key it leaves out stays what the flag said
        let m = mouse(true, "accel=on\n");
        assert!(m.trackball_on());
    }

    #[test]
    fn what_the_file_gets_wrong_is_skipped() {
        let m = mouse(false, "garbage\naccel=maybe\n=on\ntrackball\nACCEL = OFF \nfoo=on\n");
        assert!(!m.accel_on(), "the one good line still counts");
        let m = mouse(true, "\u{0}\u{1}\n=\n==\n");
        assert!(m.accel_on() && m.trackball_on(), "nothing usable is the flags");
    }

    #[test]
    fn trackball_never_runs_without_acceleration() {
        assert!(!mouse(true, "accel=off\n").trackball_on());
        assert!(!mouse(false, "accel=off\ntrackball=on\n").trackball_on());
        let mut m = mouse(false, "accel=off\n");
        assert!(!m.set_trackball(true), "nothing to change");
        assert!(!m.trackball_on());
    }

    #[test]
    fn acceleration_off_takes_trackball_with_it() {
        let mut m = mouse(true, "");
        assert!(m.set_accel(false));
        assert!(!m.trackball_on() && m.trackball().is_none());
        assert!(m.set_accel(true));
        assert!(!m.trackball_on(), "it doesn't come back on its own");
        assert!(!m.set_accel(true), "already on");
        assert!(m.set_trackball(true));
        assert!(!m.set_trackball(true));
    }

    #[test]
    fn the_file_round_trips() {
        for (accel, trackball) in [(true, true), (true, false), (false, false)] {
            let mut m = mouse(false, "");
            m.set_accel(accel);
            m.set_trackball(trackball);
            let back = mouse(!trackball, &m.render());
            assert_eq!((back.accel_on(), back.trackball_on()), (accel, trackball));
        }
    }
}
