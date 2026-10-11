//! What the tray shows for a status, platform aside: the controller the icon follows, the icon
//! and its animation, the tooltip, and a notification as each battery charges or runs down. The
//! platform puts it on screen and steps the animation when it's due.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::battery::Gauge;
use crate::icons::{BADGES, FLASH_FOR, Icon};
use crate::menu::{Item, charge_state, menu_items, name_of, tip_lines};
use crate::tr;
use crate::{Connected, Status};

/// A kicked controller may come back a few times before it gives up; say so only once a while
const MUST_CHARGE_EVERY: Duration = Duration::from_secs(600);
/// The longest the platform waits between looks with nothing animating
pub(crate) const IDLE_TICK: Duration = Duration::from_millis(100);

/// What a notification looks and sounds like
#[derive(Debug, Clone, Copy)]
pub(crate) enum Notice {
    Info,
    Warning,
    /// The error icon and the critical battery alarm
    Alarm,
}

/// One controller's battery, as the tray follows it
#[derive(Debug, Default)]
struct Watch {
    /// Its place in the order controllers connected, while it is
    connection: Option<u64>,
    gauge: Gauge,
    /// The gauge's last answer
    pct: Option<u8>,
    critical_alerted: bool,
    /// Told it's low, this discharge
    low_alerted: bool,
    /// Last refresh found it on the charger and full
    was_charged: bool,
    /// Last must-charge notification
    must_charge_told: Option<Instant>,
    /// Last refresh found it on the charger
    was_charging: bool,
    /// Its charging bolt flashes until then
    charge_flash_until: Option<Instant>,
}

/// The tray's state, as it follows the status shieldd sends
pub(crate) struct Model {
    pub(crate) status: Status,
    /// Every controller seen this run, by MAC
    watches: HashMap<[u8; 6], Watch>,
    /// The controller the battery icon shows: the lowest, discharging first
    focus: Option<[u8; 6]>,
    /// The next controller to connect gets this as its connection
    connections: u64,
    /// The count badge to show, with more than one controller connected
    badge: Option<u8>,
    /// What's on screen, if the icon is shown; the platform's to set once it's there
    pub(crate) shown: Option<Icon>,
    /// When the animation on screen last moved
    frame_at: Instant,
    /// Last refresh found nothing connected, pairing or joining
    was_disconnected: bool,
    /// A controller was connected since the last time nothing was
    had_controller: bool,
    /// No refresh yet
    first: bool,
    /// The disconnected icon flashes until then
    flash_until: Option<Instant>,
    /// What the battery icon settles on once a flash is over
    settled: Option<Icon>,
}

/// What a refresh found to show
pub(crate) struct View {
    pub(crate) icon: Icon,
    pub(crate) tip: String,
    /// Notifications: what each looks like, its title and its text
    pub(crate) alerts: Vec<(Notice, String, String)>,
}

/// The controller the icon follows: of those with a battery reading, one running down before
/// one on the charger, charged or not, then the lowest percent shown
fn lowest(controllers: &[Connected], pct: impl Fn(&[u8; 6]) -> Option<u8>) -> Option<&Connected> {
    controllers
        .iter()
        .filter_map(|c| Some((c, c.battery?)))
        .min_by_key(|(c, b)| (b.charging, pct(&c.mac).unwrap_or(u8::MAX)))
        .map(|(c, _)| c)
}

impl Model {
    pub(crate) fn new() -> Model {
        Model {
            status: Status::default(),
            watches: HashMap::new(),
            focus: None,
            connections: 0,
            badge: None,
            shown: None,
            frame_at: Instant::now(),
            was_disconnected: false,
            had_controller: false,
            first: true,
            flash_until: None,
            settled: None,
        }
    }

    /// Icon, tooltip and battery notifications for the current status
    pub(crate) fn refresh(&mut self) -> View {
        let now = Instant::now();
        let s = self.status.clone();
        // Not for pairing that timed out, a join that gave up, or a controller we sent away to
        // sleep, which would otherwise flash every time a flat one came back
        let disconnected = !s.pairing && !s.joining && s.controllers.is_empty();
        self.had_controller |= !s.controllers.is_empty();
        let left = self.had_controller && !s.sent_away;
        if disconnected && !self.was_disconnected && (self.first || left) {
            self.flash_until = Some(now + FLASH_FOR);
        }
        if disconnected {
            self.had_controller = false;
        }
        self.was_disconnected = disconnected;
        self.first = false;

        // Each controller's gauge and alerts. What the charger did belongs to a connection.
        let reading = |m: &[u8; 6]| s.controllers.iter().find(|c| c.mac == *m).and_then(|c| c.battery);
        for (mac, w) in &mut self.watches {
            if reading(mac).is_none() {
                w.gauge.forget_charger();
                w.was_charging = false;
                w.was_charged = false;
            }
        }
        for (mac, w) in &mut self.watches {
            if !s.controllers.iter().any(|c| c.mac == *mac) {
                w.connection = None;
            }
        }
        for c in &s.controllers {
            let w = self.watches.entry(c.mac).or_default();
            if w.connection.is_none() {
                w.connection = Some(self.connections);
                self.connections += 1;
            }
        }
        let mut alerts = Vec::new();
        for c in &s.controllers {
            let Some(b) = c.battery else { continue };
            let w = self.watches.entry(c.mac).or_default();
            let (shown, fresh) = w.gauge.feed(&b, now);
            w.pct = Some(shown);
            if fresh || b.charging {
                w.critical_alerted = false;
                w.low_alerted = false;
            }
            let tell_must_charge =
                b.must_charge && w.must_charge_told.is_none_or(|t| now.duration_since(t) >= MUST_CHARGE_EVERY);
            if b.must_charge {
                // Says it all already
                w.critical_alerted = true;
            }
            if tell_must_charge {
                w.must_charge_told = Some(now);
            }
            // Once per discharge, when rumble cuts out
            let alert = !b.charging && b.critical && !w.critical_alerted;
            if alert {
                w.critical_alerted = true;
            }
            // Once per discharge, at the controller's 25% band; critical says it instead if it
            // gets there first
            let low = !b.charging && b.band < 50 && !w.low_alerted && !w.critical_alerted;
            if !b.charging && (b.band < 50 || w.critical_alerted) {
                w.low_alerted = true;
            }
            let name = name_of(c.name.as_deref());
            let charged = charge_state(&b, shown) == "charged";
            // The charging bit coming on. The gauge only reads full once the charger lets go, so
            // this is nearly always the charging notice, then the charged one up to 30 s later.
            if b.charging && !w.was_charging {
                w.charge_flash_until = Some(now + FLASH_FOR);
                // Already showing full: just that it's charged
                if !charged {
                    let notice = (tr!("alert-charging-title", name = &name), tr!("alert-charging", pct = shown));
                    alerts.push((Notice::Info, notice.0, notice.1));
                }
            }
            if charged && !w.was_charged {
                alerts.push((Notice::Info, tr!("alert-charged-title", name = &name), tr!("alert-charged")));
            }
            w.was_charging = b.charging;
            w.was_charged = charged;
            if low {
                alerts.push((Notice::Warning, tr!("alert-low-title", name = &name), tr!("alert-low", pct = shown)));
            }
            if alert {
                let notice = (tr!("alert-critical-title", name = &name), tr!("alert-critical", pct = shown));
                alerts.push((Notice::Alarm, notice.0, notice.1));
            }
            if tell_must_charge {
                alerts.push((Notice::Alarm, tr!("alert-empty-title", name = &name), tr!("alert-empty")));
            }
        }

        let n = s.controllers.len();
        self.badge = u8::try_from(n).ok().filter(|n| BADGES.contains(n));
        let pct = |m: &[u8; 6]| self.watches.get(m).and_then(|w| w.pct);
        let up = lowest(&s.controllers, pct).cloned();
        self.focus = up.as_ref().map(|c| c.mac);
        let (icon, tip) = if s.pairing {
            (self.animate(Icon::Pairing(0), now), tr!("tip-pairing"))
        } else if s.joining && up.is_none() {
            (self.animate(Icon::Spinner(0), now), tr!("tip-joining"))
        } else if let Some(c) = up {
            let b = c.battery.expect("lowest has a reading");
            let shown = self.watches.get(&c.mac).and_then(|w| w.pct).unwrap_or(0);
            // Plugged in: the bolt flashes for a bit, already charged or not, then the icon settles
            let settled = Icon::of(&b, shown);
            self.settled = Some(settled);
            let icon = match settled {
                Icon::Charging | Icon::Charged if self.charge_flashing(now) => self.animate(Icon::Charging, now),
                _ => settled,
            };
            (icon, self.tip_lines(&s.controllers))
        } else {
            let tip = match s.controllers.is_empty() {
                false => self.tip_lines(&s.controllers),
                true => tr!("tip-disconnected"),
            };
            // Flashing only with nothing connected, and only for a bit
            let icon = if self.flashing(now) { self.animate(Icon::Disconnected, now) } else { Icon::Disconnected };
            (icon, tip)
        };
        View { icon, tip, alerts }
    }

    fn tip_lines(&self, controllers: &[Connected]) -> String {
        tip_lines(controllers, |m| self.watches.get(m).map_or((None, None), |w| (w.pct, w.connection)))
    }

    /// The animation's next frame, if what's on screen is animated
    fn next_frame(&self) -> Option<Icon> {
        match self.shown {
            // Once the flash is over, back to the plain frame and stay there
            Some(Icon::Disconnected) if !self.flashing(Instant::now()) => None,
            Some(Icon::DisconnectedRed) if !self.flashing(Instant::now()) => Some(Icon::Disconnected),
            // Likewise the bolt, settling on Charging, or Charged if it's full
            Some(i @ (Icon::Charging | Icon::ChargingLit)) if !self.charge_flashing(Instant::now()) => {
                self.settled.filter(|&s| s != i)
            }
            shown => shown.and_then(Icon::next_frame),
        }
    }

    /// Until the frame on screen is due to change. A flash's last frame ends with the flash.
    fn frame_left(&self) -> Duration {
        let Some(i) = self.shown else { return IDLE_TICK };
        let now = Instant::now();
        let charge_over = matches!(i, Icon::Charging | Icon::ChargingLit) && !self.charge_flashing(now);
        if (i == Icon::DisconnectedRed && !self.flashing(now)) || charge_over {
            return Duration::ZERO;
        }
        i.frame_time().saturating_sub(self.frame_at.elapsed())
    }

    /// The disconnected icon should be flashing
    fn flashing(&self, now: Instant) -> bool {
        self.was_disconnected && self.flash_until.is_some_and(|t| now < t)
    }

    /// The charging bolt should be flashing, on the controller the icon shows
    fn charge_flashing(&self, now: Instant) -> bool {
        let w = self.focus.and_then(|m| self.watches.get(&m));
        w.is_some_and(|w| w.was_charging && w.charge_flash_until.is_some_and(|t| now < t))
    }

    /// The frame of `icon` to show; a new animation starts its clock. The run loop steps it
    /// from there.
    fn animate(&mut self, icon: Icon, now: Instant) -> Icon {
        let kept = Icon::keep_frame(icon, self.shown);
        if kept == icon && self.shown != Some(icon) {
            self.frame_at = now;
        }
        kept
    }

    /// The count badge `icon` shows with, if any
    pub(crate) fn badge(&self, icon: Icon) -> Option<u8> {
        self.badge.filter(|_| icon.takes_badge())
    }

    /// The menu for the current status
    pub(crate) fn menu_items(&self) -> Vec<Item> {
        menu_items(&self.status, |m| self.watches.get(m).and_then(|w| w.pct))
    }

    /// Gives every gauge its look, not just the first that's due. True if any was, which wants a
    /// refresh.
    pub(crate) fn looks_due(&mut self, now: Instant) -> bool {
        self.watches.values_mut().map(|w| w.gauge.look_due(now)).fold(false, |a, due| a | due)
    }

    /// The animation's next frame, once it's due, its clock started over
    pub(crate) fn step(&mut self) -> Option<Icon> {
        let next = self.next_frame().filter(|_| self.frame_left().is_zero())?;
        self.frame_at = Instant::now();
        Some(next)
    }

    /// Until the next look: up to the next animation frame, at most IDLE_TICK
    pub(crate) fn wait(&self) -> Duration {
        if self.next_frame().is_some() { self.frame_left().min(IDLE_TICK) } else { IDLE_TICK }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::tests::up;

    #[test]
    fn the_icon_follows_the_lowest() {
        let cs = [up(1, "A", false), up(2, "B", false), Connected { mac: [3; 6], ..Default::default() }];
        let pct = |m: &[u8; 6]| [None, Some(60), Some(30), None][usize::from(m[0])];
        assert_eq!(lowest(&cs, pct).map(|c| c.mac), Some([2; 6]));
        assert_eq!(lowest(&cs[2..], pct).map(|c| c.mac), None, "no reading, nothing to show");
        // On the charger, lower or charged, it still loses to one running down
        let mut charging = cs.clone();
        charging[1].battery.as_mut().unwrap().charging = true;
        assert_eq!(lowest(&charging, pct).map(|c| c.mac), Some([1; 6]));
        let full = |m: &[u8; 6]| [None, Some(100), Some(100), None][usize::from(m[0])];
        charging.swap(0, 1);
        assert_eq!(lowest(&charging, full).map(|c| c.mac), Some([1; 6]));
        charging[1].battery.as_mut().unwrap().charging = true;
        assert_eq!(lowest(&charging, pct).map(|c| c.mac), Some([2; 6]), "all charging, the lowest");
    }
}
