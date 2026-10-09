//! MAC addresses as logs, the tray and `known-clients` show them: lowercase hex, `:` between bytes

pub fn fmt_mac(mac: &[u8]) -> String {
    mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase_and_colons() {
        assert_eq!(fmt_mac(&[0x00, 0x04, 0x4B, 0x21, 0x92, 0x06]), "00:04:4b:21:92:06");
        assert_eq!(fmt_mac(&[]), "");
    }
}
