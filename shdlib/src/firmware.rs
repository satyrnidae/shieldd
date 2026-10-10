//! The controller's firmware version, from its answer to mailbox command 08 (shieldd's
//! controller.rs). Decoded against the SHIELD app on 2026-10-10: `08 01 60 c0 63 06 00 20 03 47`
//! shows as v1.96/99/3.71/0.32.

/// Mailbox command whose answer is the firmware version
pub const VERSION_CMD: u8 = 0x08;

/// The version the way the SHIELD app shows it, `1.96/99/3.71/0.32`, from the mailbox answer
/// with its leading command byte. Each part is a major byte and a minor one, minor as two
/// decimal digits. `c0` before the 99 and the `06` after it are unknown and left out.
pub fn version(answer: &[u8]) -> Option<String> {
    let &[VERSION_CMD, a, b, _, c, _, d, e, f, g, ..] = answer else { return None };
    Some(format!("{a}.{b:02}/{c}/{f}.{g:02}/{d}.{e:02}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_the_shield_app_shows_it() {
        let ours = [0x08, 0x01, 0x60, 0xc0, 0x63, 0x06, 0x00, 0x20, 0x03, 0x47, 0, 0];
        assert_eq!(version(&ours).as_deref(), Some("1.96/99/3.71/0.32"));
    }

    #[test]
    fn another_answer_or_too_short_is_none() {
        assert_eq!(version(&[0x01, 0x32, 0xde, 0x95, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(version(&[0x08, 0x01, 0x60]), None);
    }
}
