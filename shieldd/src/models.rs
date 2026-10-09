//! Controller model names by MAC. WPS says the model when a controller pairs, but a rejoin
//! doesn't, so they're kept next to the group's conf, one `<mac> <model>` per line.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

type Mac = [u8; 6];

fn path(conf: &Path) -> PathBuf {
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

fn parse(text: &str) -> HashMap<Mac, String> {
    text.lines()
        .filter_map(|l| {
            let (mac, model) = l.trim().split_once(' ')?;
            Some((parse_mac(mac)?, model.trim().to_owned()))
        })
        .collect()
}

fn format(models: &HashMap<Mac, String>) -> String {
    let mut lines: Vec<String> =
        models.iter().map(|(m, model)| format!("{} {model}\n", crate::adapters::fmt_mac(m))).collect();
    lines.sort();
    lines.concat()
}

/// Empty if there's no file yet
pub fn load(conf: &Path) -> HashMap<Mac, String> {
    std::fs::read_to_string(path(conf)).map(|t| parse(&t)).unwrap_or_default()
}

pub fn save(conf: &Path, models: &HashMap<Mac, String>) -> Result<()> {
    let p = path(conf);
    std::fs::write(&p, format(models)).with_context(|| format!("writing {}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut m = HashMap::new();
        m.insert([0x00, 0x04, 0x4b, 0x21, 0x92, 0x06], "nVidiaBlake".to_owned());
        let text = format(&m);
        assert_eq!(text, "00:04:4b:21:92:06 nVidiaBlake\n");
        assert_eq!(parse(&text), m);
        // Junk lines are skipped
        assert_eq!(parse("garbage\n00:04:4b:21:92 short\n").len(), 0);
    }
}
