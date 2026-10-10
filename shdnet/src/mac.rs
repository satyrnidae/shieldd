//! MAC addresses, and how logs, the tray and `known-clients` show them: lowercase hex, `:` between
//! bytes

pub type Mac = [u8; 6];

/// `mac.fmt()`: `00:04:4b:21:92:06`
pub trait MacFmt {
    fn fmt(&self) -> String;
}

impl MacFmt for Mac {
    fn fmt(&self) -> String {
        self.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase_and_colons() {
        assert_eq!([0x00, 0x04, 0x4B, 0x21, 0x92, 0x06].fmt(), "00:04:4b:21:92:06");
    }
}
