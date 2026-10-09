//! --pair: a fresh group for the controller, handed over by WPS push-button (rtlusb wps.rs,
//! HANDOFF §9f M4).

use rand::Rng;
use rand::seq::SliceRandom;

use crate::p2pconf::Group;

/// Channel for a new group: 0 has rtlusb survey first and take the quietest of 1, 6 and 11
pub const CHANNEL: u8 = 0;
/// Length wpa_supplicant's P2P GO uses, so known to fit the controller
const PASSPHRASE_LEN: usize = 8;
/// Longest a WPS / 802.11 SSID can be
const MAX_SSID: usize = 32;

/// Passphrase characters
const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
/// The DIRECT-xx discriminator, uppercase like Wi-Fi Direct's own
const DISCRIMINATOR: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

fn pick(rng: &mut impl Rng, set: &[u8], n: usize) -> String {
    (0..n).map(|_| *set.choose(rng).expect("non-empty") as char).collect()
}

/// Just `[a-z]+-[a-z]+`: some of the crate's words have hyphens or spaces of their own
fn plain(name: &str) -> bool {
    let word = |w: &str| !w.is_empty() && w.bytes().all(|b| b.is_ascii_lowercase());
    name.split_once('-').is_some_and(|(adjective, noun)| word(adjective) && word(noun))
}

/// `DIRECT-<2 chars>-<adjective>-<noun>` from the names crate. Names that aren't plain or run
/// past MAX_SSID get redrawn.
fn ssid(rng: &mut impl Rng) -> String {
    let prefix = format!("DIRECT-{}-", pick(rng, DISCRIMINATOR, 2));
    names::Generator::default()
        .filter(|name| plain(name))
        .map(|name| format!("{prefix}{name}"))
        .find(|ssid| ssid.len() <= MAX_SSID)
        .expect("generator never ends")
}

/// A random group w/ a random passphrase. Its channel is CHANNEL until the radio picks one.
pub fn new_group() -> Group {
    let mut rng = rand::thread_rng();
    Group { ssid: ssid(&mut rng), passphrase: pick(&mut rng, ALNUM, PASSPHRASE_LEN), channel: CHANNEL }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_shape() {
        for _ in 0..1000 {
            let g = new_group();
            assert!(g.ssid.len() <= MAX_SSID && g.ssid.is_ascii(), "{}", g.ssid);
            let parts: Vec<_> = g.ssid.split('-').collect();
            assert_eq!(parts.len(), 4, "{}", g.ssid);
            assert_eq!(parts[0], "DIRECT");
            assert_eq!(parts[1].len(), 2);
            assert!(parts[1].bytes().all(|b| DISCRIMINATOR.contains(&b)), "{}", g.ssid);
            assert!(names::ADJECTIVES.contains(&parts[2]) && names::NOUNS.contains(&parts[3]), "{}", g.ssid);
            assert!(plain(&format!("{}-{}", parts[2], parts[3])), "{}", g.ssid);
            assert_eq!(g.passphrase.len(), PASSPHRASE_LEN);
            assert_eq!(g.channel, CHANNEL);
        }
    }

    #[test]
    fn plain_names() {
        assert!(plain("brave-bison"));
        assert!(!plain("black-and-white-cat"));
        assert!(!plain("callous-good-bye"));
        assert!(!plain("Brave-bison"));
        assert!(!plain("brave-"));
        assert!(!plain("brave"));
    }
}
