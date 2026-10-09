//! Controller names by MAC, in `known-clients` next to the group's conf. Like a hosts file: one
//! controller a line, its MAC, whitespace, then its name; a line starting w/ `#` is a comment, so
//! a name can still have one. The name starts
//! out as the model WPS gives at its first pairing, and the tray can rename it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{info, warn};

use shieldutl::files;
pub use shieldutl::names::clean;

type Mac = [u8; 6];

fn path(conf: &Path) -> PathBuf {
    conf.with_file_name("known-clients")
}

/// Where these lived before, as `<mac> <model>` lines
fn old_path(conf: &Path) -> PathBuf {
    conf.with_extension("models")
}

fn parse_mac(s: &str) -> Option<Mac> {
    let mut mac = [0u8; 6];
    let mut parts = s.split(':');
    for b in &mut mac {
        *b = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

/// One line's entry, if it has one
fn parse_line(line: &str) -> Option<(Mac, String)> {
    let line = line.trim();
    if line.starts_with('#') {
        return None;
    }
    let (mac, name) = line.split_once(char::is_whitespace)?;
    // Hand edits and an old p2p.models get the same limits as a rename
    Some((parse_mac(mac)?, clean(name)?))
}

fn parse(text: &str) -> HashMap<Mac, String> {
    text.lines().filter_map(parse_line).collect()
}

/// `text` w/ `mac`'s line saying `name`: the first line for it rewritten, any later ones dropped,
/// or a new line at the end. Comments and everything else stay as they were.
fn set_line(text: &str, mac: &Mac, name: &str) -> String {
    let line = format!("{} {name}", shieldutl::mac::fmt_mac(mac));
    let mut out = String::with_capacity(text.len() + line.len() + 1);
    let mut done = false;
    for l in text.lines() {
        if parse_line(l).is_some_and(|(m, _)| m == *mac) {
            if done {
                continue;
            }
            out.push_str(&line);
            done = true;
        } else {
            out.push_str(l);
        }
        out.push('\n');
    }
    if !done {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Empty if there's no file yet. Moves an old `p2p.models` over first.
pub fn load(conf: &Path) -> HashMap<Mac, String> {
    let (p, old) = (path(conf), old_path(conf));
    if !p.exists() && old.exists() {
        match std::fs::rename(&old, &p) {
            Ok(()) => info!(from = %old.display(), to = %p.display(), "controller names moved"),
            Err(e) => warn!("moving {} to {}: {e}", old.display(), p.display()),
        }
    }
    std::fs::read_to_string(p).map(|t| parse(&t)).unwrap_or_default()
}

/// Saves `mac`'s name, leaving every other line be
pub fn save(conf: &Path, mac: &Mac, name: &str) -> Result<()> {
    let p = path(conf);
    let text = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", p.display())),
    };
    let tmp = files::stage(&p, set_line(&text, mac, name))?;
    files::commit(&tmp, &p)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: Mac = [0x00, 0x04, 0x4b, 0x21, 0x92, 0x06];

    #[test]
    fn parses_like_a_hosts_file() {
        let m = parse(
            "# controllers\n  # 00:04:4b:21:92:07 off\n00:04:4b:21:92:06\tPad #2  \n\ngarbage\n00:04:4b:21:92 short\n",
        );
        assert_eq!(m.len(), 1);
        assert_eq!(m[&MAC], "Pad #2");
    }

    #[test]
    fn set_keeps_the_rest() {
        let text = "# mine\n00:04:4b:21:92:06 nVidiaBlake\n00:04:4b:00:00:01 Other\n";
        let out = set_line(text, &MAC, "Couch");
        assert_eq!(out, "# mine\n00:04:4b:21:92:06 Couch\n00:04:4b:00:00:01 Other\n");
        assert_eq!(set_line("", &MAC, "nVidiaBlake"), "00:04:4b:21:92:06 nVidiaBlake\n");
        // A duplicate goes
        assert_eq!(set_line("00:04:4b:21:92:06 a\n00:04:4b:21:92:06 b\n", &MAC, "c"), "00:04:4b:21:92:06 c\n");
    }
}
