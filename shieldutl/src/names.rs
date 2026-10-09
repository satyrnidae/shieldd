//! Controller names, as `known-clients` keeps them and the tray's rename box takes them

/// Longest name: the WPS model name field it starts out as, which the controller fills to 15
pub const MAX_NAME: usize = 15;

/// `name` as a controller name: trimmed, one line, at most MAX_NAME characters. None if
/// nothing's left.
pub fn clean(name: &str) -> Option<String> {
    let name: String = name.chars().filter(|c| !c.is_control()).collect();
    let name: String = name.trim().chars().take(MAX_NAME).collect();
    let name = name.trim_end();
    (!name.is_empty()).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean("  nVidiaBlake     ").as_deref(), Some("nVidiaBlake"));
        assert_eq!(clean("a\r\nb").as_deref(), Some("ab"));
        assert_eq!(clean("0123456789abcdefgh").as_deref(), Some("0123456789abcde"));
        assert_eq!(clean("   "), None);
    }
}
