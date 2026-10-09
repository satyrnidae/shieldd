//! Raw Ozmo frames in and out through Npcap.

use anyhow::{Context, Result, bail};
use ozproto::Mac;
use ozproto::wire::ETHERTYPE;
use pcap::{Active, Capture};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use windows::Win32::System::LibraryLoader::SetDllDirectoryW;
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::core::HSTRING;

const ETH_HDR: usize = 14;

/// Points the dll search path at System32\Npcap. Must run before anything
/// touches pcap, since wpcap.dll is delay-loaded on first use.
pub fn init_dll_path() -> Result<()> {
    let mut buf = [0u16; 260];
    // SAFETY: buf is a valid, writable buffer of the length passed
    let n = unsafe { GetSystemDirectoryW(Some(&mut buf)) } as usize;
    if n == 0 || n >= buf.len() {
        bail!("GetSystemDirectoryW failed");
    }
    let dir = format!("{}\\Npcap", String::from_utf16_lossy(&buf[..n]));
    if !std::path::Path::new(&dir).join("wpcap.dll").exists() {
        bail!("no wpcap.dll in {dir}; is Npcap installed?");
    }
    // SAFETY: HSTRING is null terminated and outlives the call
    unsafe { SetDllDirectoryW(&HSTRING::from(dir.as_str())) }.context("SetDllDirectoryW")?;
    Ok(())
}

/// A capture handle on the GO's virtual adapter, filtered to the Ozmo
/// ethertype. Closes w/ the handle on drop.
pub struct Link {
    cap: Capture<Active>,
    mac: Mac,
}

impl Link {
    /// Receive handle. `guid` is the adapter name from GetAdaptersAddresses,
    /// braces included.
    pub fn open_rx(guid: &str, mac: Mac) -> Result<Self> {
        Self::open(guid, mac, &format!("ether proto {ETHERTYPE:#06x}"))
    }

    /// Send-only handle; its filter matches nothing so no capture buffer fills up.
    pub fn open_tx(guid: &str, mac: Mac) -> Result<Self> {
        Self::open(guid, mac, "less 1")
    }

    fn open(guid: &str, mac: Mac, filter: &str) -> Result<Self> {
        let device = format!(r"\Device\NPF_{guid}");
        let mut cap = Capture::from_device(device.as_str())
            .and_then(|c| c.immediate_mode(true).snaplen(2048).timeout(1).open())
            .with_context(|| format!("opening {device}"))?;
        cap.filter(filter, true).context("setting capture filter")?;
        Ok(Link { cap, mac })
    }

    /// Next Ozmo frame from someone else, as (source, payload after the
    /// Ethernet header, driver arrival time). None when the read times out.
    pub fn recv(&mut self) -> Result<Option<(Mac, Vec<u8>, SystemTime)>> {
        loop {
            let pkt = match self.cap.next_packet() {
                Ok(p) => p,
                Err(pcap::Error::TimeoutExpired) => return Ok(None),
                Err(e) => return Err(e).context("reading capture"),
            };
            let d = pkt.data;
            if d.len() < ETH_HDR || u16::from_be_bytes([d[12], d[13]]) != ETHERTYPE {
                continue;
            }
            let src: Mac = d[6..12].try_into().unwrap();
            if src == self.mac {
                continue; // our own frames, looped back by npcap
            }
            let ts = &pkt.header.ts;
            let at = UNIX_EPOCH + Duration::new(ts.tv_sec as u64, ts.tv_usec as u32 * 1000);
            return Ok(Some((src, d[ETH_HDR..].to_vec(), at)));
        }
    }

    pub fn send(&mut self, dst: Mac, payload: &[u8]) -> Result<()> {
        let mut frame = Vec::with_capacity(ETH_HDR + payload.len());
        frame.extend_from_slice(&dst);
        frame.extend_from_slice(&self.mac);
        frame.extend_from_slice(&ETHERTYPE.to_be_bytes());
        frame.extend_from_slice(payload);
        self.cap.sendpacket(frame).context("sending frame")
    }
}
