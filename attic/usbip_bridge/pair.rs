//! `shieldd pair`: pairing probe. Runs a non-legacy Wi-Fi Direct GO that
//! accepts WPS push-button, the only kind of GO that can pair a device on
//! Windows, and logs everything Windows reports while the NVIDIA button is
//! held. First question to answer: does the controller's WPS enrollment get
//! anywhere against a Windows GO at all.

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};
use windows::Devices::Enumeration::{
    DeviceInformation, DeviceInformationCustomPairing, DeviceInformationUpdate, DevicePairingKinds,
    DevicePairingProtectionLevel, DevicePairingRequestedEventArgs, DeviceWatcher,
};
use windows::Devices::WiFiDirect::{
    WiFiDirectAdvertisementListenStateDiscoverability, WiFiDirectAdvertisementPublisher,
    WiFiDirectAdvertisementPublisherStatus, WiFiDirectAdvertisementPublisherStatusChangedEventArgs,
    WiFiDirectConfigurationMethod, WiFiDirectConnectionListener, WiFiDirectConnectionParameters,
    WiFiDirectConnectionRequestedEventArgs, WiFiDirectDevice, WiFiDirectDeviceSelectorType, WiFiDirectError,
    WiFiDirectPairingProcedure,
};
use windows::Foundation::TypedEventHandler;

use crate::adapters;

enum Note {
    Publisher(WiFiDirectAdvertisementPublisherStatus, WiFiDirectError),
    Request(DeviceInformation),
    PeerAdded(String, String),
    PeerUpdated(String),
    PeerRemoved(String),
}

/// Everything WinRT hands callbacks to, unhooked and stopped on drop.
struct Probe {
    publisher: WiFiDirectAdvertisementPublisher,
    publisher_token: i64,
    listener: WiFiDirectConnectionListener,
    listener_token: i64,
    watcher: DeviceWatcher,
    watcher_tokens: [i64; 3],
}

impl Probe {
    fn start(tx: Sender<Note>) -> Result<Self> {
        let publisher = WiFiDirectAdvertisementPublisher::new()?;
        let adv = publisher.Advertisement()?;
        adv.SetIsAutonomousGroupOwnerEnabled(true)?;
        adv.SetListenStateDiscoverability(WiFiDirectAdvertisementListenStateDiscoverability::Normal)?;
        adv.LegacySettings()?.SetIsEnabled(false)?;
        adv.SupportedConfigurationMethods()?.Append(WiFiDirectConfigurationMethod::PushButton)?;

        let t = tx.clone();
        let publisher_token = publisher.StatusChanged(&TypedEventHandler::<_, WiFiDirectAdvertisementPublisherStatusChangedEventArgs>::new(
            move |_, a| {
                if let Ok(a) = a.ok() {
                    let _ = t.send(Note::Publisher(a.Status()?, a.Error()?));
                }
                Ok(())
            },
        ))?;

        let listener = WiFiDirectConnectionListener::new()?;
        let t = tx.clone();
        let listener_token = listener.ConnectionRequested(&TypedEventHandler::<_, WiFiDirectConnectionRequestedEventArgs>::new(
            move |_, a| {
                if let Ok(a) = a.ok() {
                    let req = a.GetConnectionRequest()?;
                    let _ = t.send(Note::Request(req.DeviceInformation()?));
                }
                Ok(())
            },
        ))?;

        // Peers in range, whether or not they ask to connect
        let selector = WiFiDirectDevice::GetDeviceSelector2(WiFiDirectDeviceSelectorType::AssociationEndpoint)?;
        let watcher = DeviceInformation::CreateWatcherAqsFilter(&selector)?;
        let t = tx.clone();
        let added = watcher.Added(&TypedEventHandler::<DeviceWatcher, DeviceInformation>::new(move |_, d| {
            if let Ok(d) = d.ok() {
                let _ = t.send(Note::PeerAdded(d.Id()?.to_string(), d.Name()?.to_string()));
            }
            Ok(())
        }))?;
        let t = tx.clone();
        let updated = watcher.Updated(&TypedEventHandler::<DeviceWatcher, DeviceInformationUpdate>::new(move |_, d| {
            if let Ok(d) = d.ok() {
                let _ = t.send(Note::PeerUpdated(d.Id()?.to_string()));
            }
            Ok(())
        }))?;
        let t = tx;
        let removed = watcher.Removed(&TypedEventHandler::<DeviceWatcher, DeviceInformationUpdate>::new(move |_, d| {
            if let Ok(d) = d.ok() {
                let _ = t.send(Note::PeerRemoved(d.Id()?.to_string()));
            }
            Ok(())
        }))?;

        let probe = Probe {
            publisher,
            publisher_token,
            listener,
            listener_token,
            watcher,
            watcher_tokens: [added, updated, removed],
        };
        probe.publisher.Start()?;
        probe.watcher.Start()?;
        Ok(probe)
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = self.watcher.Stop();
        let [a, u, r] = self.watcher_tokens;
        let _ = self.watcher.RemoveAdded(a);
        let _ = self.watcher.RemoveUpdated(u);
        let _ = self.watcher.RemoveRemoved(r);
        let _ = self.listener.RemoveConnectionRequested(self.listener_token);
        let _ = self.publisher.RemoveStatusChanged(self.publisher_token);
        let _ = self.publisher.Stop();
        info!("pairing GO stopped");
    }
}

/// Runs until `quit` or `timeout`.
pub fn run(radio_mac: ozproto::Mac, timeout: Duration, quit: &std::sync::atomic::AtomicBool) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let _probe = Probe::start(tx)?;
    wait_started(&rx)?;
    info!("pairing GO up; hold the NVIDIA button until it flashes. ctrl+c to stop");
    log_wfd_adapters(radio_mac);

    // Keep paired devices open; dropping a WiFiDirectDevice disconnects it.
    let mut devices: Vec<WiFiDirectDevice> = Vec::new();
    let deadline = Instant::now() + timeout;
    let mut last_adapters = Instant::now();
    while !quit.load(std::sync::atomic::Ordering::Relaxed) && Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Note::Publisher(s, e)) => info!(status = ?s, error = ?e, "publisher"),
            Ok(Note::PeerAdded(id, name)) => info!(%name, %id, "peer in range"),
            Ok(Note::PeerUpdated(id)) => info!(%id, "peer updated"),
            Ok(Note::PeerRemoved(id)) => info!(%id, "peer gone"),
            Ok(Note::Request(di)) => match accept(&di) {
                Ok(dev) => devices.push(dev),
                Err(e) => warn!("accepting connection: {e:#}"),
            },
            Err(_) => {}
        }
        if last_adapters.elapsed() > Duration::from_secs(5) {
            last_adapters = Instant::now();
            log_wfd_adapters(radio_mac);
        }
    }
    Ok(())
}

fn wait_started(rx: &Receiver<Note>) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(Note::Publisher(s, e)) = rx.recv_timeout(Duration::from_millis(200)) {
            info!(status = ?s, error = ?e, "publisher");
            match s {
                WiFiDirectAdvertisementPublisherStatus::Started => return Ok(()),
                WiFiDirectAdvertisementPublisherStatus::Aborted => bail!("pairing GO aborted ({e:?})"),
                _ => {}
            }
        }
    }
    bail!("pairing GO didn't start within 15 s")
}

/// Pairs w/ push-button, then opens the device so the connection stays up.
fn accept(di: &DeviceInformation) -> Result<WiFiDirectDevice> {
    let id = di.Id()?;
    info!(name = %di.Name()?, %id, paired = di.Pairing()?.IsPaired()?, "connection request");

    let params = WiFiDirectConnectionParameters::new()?;
    params.SetGroupOwnerIntent(15)?;
    params.PreferenceOrderedConfigurationMethods()?.Append(WiFiDirectConfigurationMethod::PushButton)?;
    params.SetPreferredPairingProcedure(WiFiDirectPairingProcedure::GroupOwnerNegotiation)?;

    if !di.Pairing()?.IsPaired()? {
        let custom: DeviceInformationCustomPairing = di.Pairing()?.Custom()?;
        let token = custom.PairingRequested(&TypedEventHandler::<_, DevicePairingRequestedEventArgs>::new(|_, a| {
            if let Ok(a) = a.ok() {
                info!(kind = ?a.PairingKind()?, "pairing requested, accepting");
                a.Accept()?;
            }
            Ok(())
        }))?;
        let kinds = DevicePairingKinds::ConfirmOnly;
        let result = custom
            .PairWithProtectionLevelAndSettingsAsync(kinds, DevicePairingProtectionLevel::Default, &params)
            .and_then(|op| op.join());
        let _ = custom.RemovePairingRequested(token);
        let result = result.context("PairAsync")?;
        info!(status = ?result.Status()?, protection = ?result.ProtectionLevelUsed()?, "pairing result");
    }

    let dev = WiFiDirectDevice::FromIdAsync2(&id, &params)?.join().context("WiFiDirectDevice::FromIdAsync")?;
    info!(status = ?dev.ConnectionStatus()?, "device opened");
    for pair in dev.GetConnectionEndpointPairs()? {
        info!(
            local = %pair.LocalHostName().and_then(|h| h.DisplayName()).unwrap_or_default(),
            remote = %pair.RemoteHostName().and_then(|h| h.DisplayName()).unwrap_or_default(),
            "endpoint pair"
        );
    }
    Ok(dev)
}

/// Which Wi-Fi Direct ports exist and are up. A new group shows up as a new
/// port; its MAC tells which radio it's on.
fn log_wfd_adapters(radio_mac: ozproto::Mac) {
    match adapters::list() {
        Ok(list) => {
            let up: Vec<String> = list
                .iter()
                .filter(|a| a.is_wifi_direct() && a.up)
                .map(|a| {
                    let mac = a.mac.map(|m| adapters::fmt_mac(&m)).unwrap_or_default();
                    let tag = if a.mac == Some(radio_mac) { " (radio)" } else { "" };
                    format!("{} {mac}{tag}", a.friendly_name)
                })
                .collect();
            info!(up = ?up, "Wi-Fi Direct ports");
        }
        Err(e) => warn!("listing adapters: {e:#}"),
    }
}
