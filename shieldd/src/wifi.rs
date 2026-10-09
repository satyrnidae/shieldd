//! Radio state, channel anchoring and the Wi-Fi Direct group owner (WinRT).

use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};
use windows::Devices::Radios::{Radio, RadioAccessStatus, RadioState};
use windows::Devices::WiFi::{WiFiAdapter, WiFiAvailableNetwork, WiFiConnectionStatus, WiFiReconnectionKind};
use windows::Devices::WiFiDirect::{
    WiFiDirectAdvertisementListenStateDiscoverability, WiFiDirectAdvertisementPublisher,
    WiFiDirectAdvertisementPublisherStatus, WiFiDirectAdvertisementPublisherStatusChangedEventArgs, WiFiDirectError,
};
use windows::Foundation::TypedEventHandler;
use windows::Security::Credentials::PasswordCredential;
use windows::core::HSTRING;

/// Turns the named radio on. usbipd hands the adapter back w/ its radio off,
/// and a GO on an off radio aborts w/ ERROR_NDIS_DOT11_POWER_STATE_INVALID.
pub fn ensure_radio_on(name: &str) -> Result<()> {
    let access = Radio::RequestAccessAsync()?.join()?;
    if access != RadioAccessStatus::Allowed {
        bail!("radio access denied ({access:?})");
    }
    for radio in Radio::GetRadiosAsync()?.join()? {
        if radio.Name()? != name {
            continue;
        }
        if radio.State()? != RadioState::On {
            let rc = radio.SetStateAsync(RadioState::On)?.join()?;
            info!(radio = name, result = ?rc, "radio was off, switched on");
        }
        return Ok(());
    }
    bail!("no radio named {name:?}")
}

/// Connects the adapter as a station to `ssid` on `mhz`, using a saved
/// profile. Windows puts the GO on the same channel as the station link, and
/// the controller only looks for its group on channel 1.
pub fn anchor(adapter_guid: &str, ssid: &str, mhz: u32) -> Result<()> {
    let access = WiFiAdapter::RequestAccessAsync()?.join()?;
    if access != windows::Devices::WiFi::WiFiAccessStatus::Allowed {
        bail!("Wi-Fi adapter access denied ({access:?})");
    }
    let adapter = WiFiAdapter::FindAllAdaptersAsync()?
        .join()?
        .into_iter()
        .find(|a| {
            a.NetworkAdapter()
                .and_then(|n| n.NetworkAdapterId())
                .is_ok_and(|id| format!("{id:?}").eq_ignore_ascii_case(adapter_guid))
        })
        .context("radio adapter not found among WiFiAdapters")?;
    let find = || -> Result<Option<WiFiAvailableNetwork>> {
        Ok(adapter.NetworkReport()?.AvailableNetworks()?.into_iter().find(|n| {
            n.Ssid().is_ok_and(|s| s == ssid)
                && n.ChannelCenterFrequencyInKilohertz().is_ok_and(|k| k == (mhz * 1000) as i32)
        }))
    };
    // The cached report is usually enough. Scans on this adapter tend to hang
    // ~20 s and then abort, so only scan when the network isn't in the cache.
    let network = match find()? {
        Some(n) => n,
        None => {
            if let Err(e) = adapter.ScanAsync().and_then(|op| op.join()) {
                warn!("scan failed: {e}");
            }
            find()?.with_context(|| format!("{ssid:?} not seen on {mhz} MHz"))?
        }
    };
    let rc = adapter.ConnectAsync(&network, WiFiReconnectionKind::Manual)?.join()?.ConnectionStatus()?;
    if rc != WiFiConnectionStatus::Success {
        bail!("connecting to {ssid:?} failed: {rc:?}");
    }
    info!(ssid, mhz, "anchored radio to station link");
    Ok(())
}

/// An autonomous GO advertising the controller's saved group. Stops the
/// publisher and unhooks the status handler on drop.
pub struct GroupOwner {
    publisher: WiFiDirectAdvertisementPublisher,
    token: i64,
    status: Receiver<(WiFiDirectAdvertisementPublisherStatus, WiFiDirectError)>,
}

impl GroupOwner {
    /// Starts the GO, retrying for a while: after a hard kill Windows keeps
    /// the old group up for ~20-30 s, and a new one aborts until it's gone.
    pub fn start(ssid: &str, passphrase: &str) -> Result<Self> {
        const TRIES: u32 = 8;
        for attempt in 1..=TRIES {
            match Self::try_start(ssid, passphrase) {
                Ok(go) => return Ok(go),
                Err(e) if attempt < TRIES => {
                    warn!(attempt, "{e:#}; retrying in 5 s");
                    std::thread::sleep(Duration::from_secs(5));
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    fn try_start(ssid: &str, passphrase: &str) -> Result<Self> {
        let publisher = WiFiDirectAdvertisementPublisher::new()?;
        let adv = publisher.Advertisement()?;
        adv.SetIsAutonomousGroupOwnerEnabled(true)?;
        adv.SetListenStateDiscoverability(WiFiDirectAdvertisementListenStateDiscoverability::Normal)?;
        let legacy = adv.LegacySettings()?;
        legacy.SetIsEnabled(true)?;
        legacy.SetSsid(&HSTRING::from(ssid))?;
        let cred = PasswordCredential::new()?;
        cred.SetPassword(&HSTRING::from(passphrase))?;
        legacy.SetPassphrase(&cred)?;

        let (tx, status) = mpsc::channel();
        let handler =
            TypedEventHandler::<_, WiFiDirectAdvertisementPublisherStatusChangedEventArgs>::new(move |_, args| {
                if let Ok(a) = args.ok() {
                    let _ = tx.send((a.Status()?, a.Error()?));
                }
                Ok(())
            });
        let token = publisher.StatusChanged(&handler)?;
        let go = GroupOwner { publisher, token, status };
        go.publisher.Start()?;

        loop {
            match go.status.recv_timeout(Duration::from_secs(15)) {
                Ok((WiFiDirectAdvertisementPublisherStatus::Started, _)) => break,
                Ok((WiFiDirectAdvertisementPublisherStatus::Aborted, err)) => {
                    bail!("GO aborted ({err:?}); check the radio is on and not in use by usbipd")
                }
                Ok(_) => continue,
                Err(_) => bail!("GO didn't start within 15 s"),
            }
        }
        info!(ssid, "group owner started");
        Ok(go)
    }

    /// Status changes since the last call. The GO can be stopped from under
    /// us, e.g. when the adapter is unplugged.
    pub fn stopped(&self) -> Option<WiFiDirectError> {
        self.status.try_iter().find_map(|(s, err)| {
            matches!(
                s,
                WiFiDirectAdvertisementPublisherStatus::Aborted | WiFiDirectAdvertisementPublisherStatus::Stopped
            )
            .then_some(err)
        })
    }
}

impl Drop for GroupOwner {
    fn drop(&mut self) {
        if let Err(e) = self.publisher.RemoveStatusChanged(self.token) {
            warn!("removing GO status handler: {e}");
        }
        if let Err(e) = self.publisher.Stop() {
            warn!("stopping GO: {e}");
        }
        info!("group owner stopped");
    }
}
