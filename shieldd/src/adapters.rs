//! Network adapter lookup through GetAdaptersAddresses.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use ozproto::Mac;
use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
use windows::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_INCLUDE_ALL_INTERFACES, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
    GAA_FLAG_SKIP_UNICAST, GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Networking::WinSock::AF_UNSPEC;

const WFD_DESCRIPTION: &str = "Wi-Fi Direct Virtual Adapter";

#[derive(Debug, Clone)]
pub struct Adapter {
    /// `{GUID}`, braces included; what Npcap device names are built from
    pub guid: String,
    pub friendly_name: String,
    pub description: String,
    pub mac: Option<Mac>,
    pub up: bool,
}

impl Adapter {
    pub fn is_wifi_direct(&self) -> bool {
        self.description.contains(WFD_DESCRIPTION)
    }

    /// Same GUID w/o braces, for comparing against WinRT adapter IDs
    pub fn bare_guid(&self) -> &str {
        self.guid.trim_start_matches('{').trim_end_matches('}')
    }
}

pub fn list() -> Result<Vec<Adapter>> {
    let flags = GAA_FLAG_INCLUDE_ALL_INTERFACES
        | GAA_FLAG_SKIP_UNICAST
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size: u32 = 16 * 1024;
    // u64 backing keeps the structs aligned
    let mut buf: Vec<u64>;
    loop {
        buf = vec![0u64; (size as usize).div_ceil(8)];
        let head = buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        // SAFETY: buf holds `size` writable bytes, aligned for the struct
        let rc = unsafe { GetAdaptersAddresses(AF_UNSPEC.0 as u32, flags, None, Some(head), &mut size) };
        match rc {
            x if x == ERROR_SUCCESS.0 => break,
            x if x == ERROR_BUFFER_OVERFLOW.0 => continue,
            x => bail!("GetAdaptersAddresses failed: {x}"),
        }
    }

    let mut out = Vec::new();
    let mut p = buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    while !p.is_null() {
        // SAFETY: p walks the list GetAdaptersAddresses built inside buf, which
        // is still alive; the strings it points at live in the same buffer.
        let a = unsafe { &*p };
        let (guid, friendly_name, description) = unsafe {
            (
                a.AdapterName.to_string().unwrap_or_default(),
                a.FriendlyName.to_string().unwrap_or_default(),
                a.Description.to_string().unwrap_or_default(),
            )
        };
        let mac = (a.PhysicalAddressLength == 6).then(|| a.PhysicalAddress[..6].try_into().unwrap());
        out.push(Adapter { guid, friendly_name, description, mac, up: a.OperStatus == IfOperStatusUp });
        p = a.Next;
    }
    Ok(out)
}

pub fn by_name(friendly_name: &str) -> Result<Adapter> {
    match list()?.into_iter().find(|a| a.friendly_name == friendly_name) {
        Some(a) => Ok(a),
        None => bail!("no network adapter named {friendly_name:?}"),
    }
}

/// Waits for the Wi-Fi Direct port that carries the group, identified by
/// sharing the physical adapter's MAC, to come up.
pub fn wait_for_go(mac: Mac, timeout: Duration) -> Result<Adapter> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(a) = list()?.into_iter().find(|a| a.is_wifi_direct() && a.mac == Some(mac) && a.up) {
            return Ok(a);
        }
        if Instant::now() > deadline {
            bail!("no Wi-Fi Direct adapter w/ MAC {} came up", fmt_mac(&mac));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

pub fn fmt_mac(mac: &Mac) -> String {
    mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}
