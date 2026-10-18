//! The Bluetooth LE link to the phone, as the *central*: scan for the Bridge
//! service, connect, pair (bond) if needed, subscribe to the phone's TX
//! characteristic, run the handshake, then exchange chunked messages.
//!
//! This is BluetoothLink.swift on WinRT. Same service and characteristic
//! UUIDs, same framing, same handshake (protocol.rs).
//!
//! Windows differences that matter:
//!  - Bonding is explicit: `DeviceInformation.Pairing.Custom.PairAsync`, with
//!    a handler that accepts the pairing kinds the phone offers.
//!  - The encrypted CCCD on the phone means the subscribe fails until the
//!    link is encrypted; setting the characteristic's protection level to
//!    EncryptionAndAuthentication makes WinRT bring the encryption up first.
//!  - There is no "connect": WinRT connects lazily when a GATT operation
//!    needs the link, and drops it when nothing holds the device object.
//!    We keep the `BluetoothLEDevice` alive for the life of the link.

use crate::protocol::{self, Handshake, Inbox, Kind, SessionCrypto, Step};
use anyhow::{anyhow, bail, Context, Result};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::{GUID, HSTRING};
use windows::Devices::Bluetooth::Advertisement::{
    BluetoothLEAdvertisementReceivedEventArgs, BluetoothLEAdvertisementWatcher,
};
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristic, GattClientCharacteristicConfigurationDescriptorValue, GattCommunicationStatus,
    GattDeviceService, GattOpenStatus, GattSession, GattSharingMode, GattValueChangedEventArgs, GattWriteOption,
};
use windows::Devices::Bluetooth::{
    BluetoothAddressType, BluetoothCacheMode, BluetoothConnectionStatus, BluetoothLEDevice,
    BluetoothLEPreferredConnectionParameters, BluetoothLEPreferredConnectionParametersRequest,
};
use windows::Devices::Enumeration::DeviceInformation;
use windows::Foundation::TypedEventHandler;
use windows::Storage::Streams::{DataReader, DataWriter};

// Same UUIDs as BleLink.kt.
const SERVICE: GUID = GUID::from_u128(0xb71d0001_5c8f_4b1e_9a3a_3f1f0a7c9e11);
// The phone's *plain* door (see BleLink.kt): no bond, no link-layer
// encryption requirement; the session is AES-GCM encrypted at our layer once
// the handshake has passed. Windows' bond store was not reliable enough with
// Android as a peripheral to depend on.
const TX: GUID = GUID::from_u128(0xb71d0004_5c8f_4b1e_9a3a_3f1f0a7c9e11);
const RX: GUID = GUID::from_u128(0xb71d0005_5c8f_4b1e_9a3a_3f1f0a7c9e11);

/// What the link tells the rest of the app.
#[derive(Debug)]
pub enum Event {
    Searching,
    Linked,
    Dropped(String),
    Notification(protocol::PhoneNotification),
    /// A mirrored notification left the phone (read, swiped, answered there).
    NotificationRemoved(u32),
    /// Now Playing changed (see protocol::parse_media).
    Media(std::collections::HashMap<String, String>),
    /// Album art: key and JPEG bytes.
    Art { key: String, jpeg: Vec<u8> },
    /// From a toast (notify.rs): reply typed, or swiped away.
    ToastReply { id: u32, text: String },
    ToastDismissed(u32),
    /// Body click on a mirrored phone toast (not a reply or file notification).
    NotificationClicked,
    /// An app icon the phone sent: package and PNG bytes.
    Icon { package: String, png: Vec<u8> },
    Clipboard(String),
    Status(std::collections::HashMap<String, String>),
    /// From the tunnel thread: which Connect attempt, and START's reply or why it failed.
    Connected(u32, Result<String, String>),
    ImagePasteReady { attempt: u32, sequence: u32, result: Result<bool, String> },
    /// Set up over USB (usb.rs): a step started, then the outcome.
    UsbProgress(String),
    UsbDone(Result<crate::store::Credentials, String>),
    /// Phone files (files.rs): a folder listing, transfer progress, and the outcome.
    FilesListed(Result<(String, Vec<crate::files::Entry>), String>),
    /// Transfer progress: what's moving, and how far (0.0 to 1.0).
    FileProgress(String, f32),
    FileDone(Result<String, String>),
    QueuedPushDone(u64, Result<String, String>),
    QueuedPushProgress(u64, String, f32),
    ShareOffer(String),
    /// LAN-first incoming share selected iroh and needs the phone's tunnel.
    ShareNeedsTunnel,
    ShareFinished(Result<crate::incoming::Report, String>),
    /// A local transfer toast was clicked; never executes the received file.
    RevealReceived(std::path::PathBuf),
    /// The phone is paired (bonded) in Windows' Bluetooth settings, which
    /// breaks the link: Bridge connects unpaired, and encrypts by itself.
    PairedInWindows { name: String, address: u64 },
    /// The outcome of unpairing it for the user.
    UnpairedInWindows(Result<String, String>),
    /// The mirroring watchdog's ping: did the phone answer STATUS?
    SessionPing(bool),
    /// The phone window's "Phone files" button.
    OpenFiles,
    /// This PC's Bluetooth radio: Some(on) when known, None if it has none.
    Radio(Option<bool>),
    /// Credentials have a redacted Debug implementation; never log the wire payload.
    HotspotReply { id: String, credentials: Result<crate::hotspot::Credentials, String> },
    HotspotJoined { id: String, result: Result<(), String> },
}

/// Watches this PC's Bluetooth radio, so the app can say "Bluetooth is off"
/// instead of "looking for your phone" forever, and start the link the
/// moment it's switched back on. Keep the returned Radio alive: dropping it
/// drops the subscription.
pub fn watch_radio(events: Sender<Event>) -> Option<windows::Devices::Radios::Radio> {
    use windows::Devices::Radios::{Radio, RadioState};
    let adapter = match windows::Devices::Bluetooth::BluetoothAdapter::GetDefaultAsync().and_then(|op| op.get()) {
        Ok(a) => a,
        Err(_) => {
            let _ = events.send(Event::Radio(None));
            return None;
        }
    };
    let radio = adapter.GetRadioAsync().and_then(|op| op.get()).ok()?;
    let _ = events.send(Event::Radio(Some(radio.State().map(|s| s == RadioState::On).unwrap_or(true))));
    let tx = events.clone();
    radio
        .StateChanged(&TypedEventHandler::new(move |r: &Option<Radio>, _: &Option<windows::core::IInspectable>| {
            if let Some(r) = r {
                let _ = tx.send(Event::Radio(Some(r.State()? == RadioState::On)));
            }
            Ok(())
        }))
        .ok()?;
    Some(radio)
}

/// The live link, shared between the WinRT callbacks and the app.
struct Link {
    device: Option<BluetoothLEDevice>,
    rx: Option<GattCharacteristic>,
    /// Kept alive on purpose: releasing the TX characteristic (or its
    /// service) drops the ValueChanged subscription with it, and the phone's
    /// notifications then arrive at nothing.
    tx: Option<GattCharacteristic>,
    service: Option<GattDeviceService>,
    /// Held with MaintainConnection on: Windows treats a link nobody holds a
    /// session for as idle and lets it go after a while, which looked like
    /// the phone dropping us every minute or two for no reason.
    session: Option<GattSession>,
    /// Our request for relaxed link parameters; Windows keeps it in force
    /// only while this object lives (see request_relaxed_link).
    params: Option<BluetoothLEPreferredConnectionParametersRequest>,
    inbox: Inbox,
    handshake: Handshake,
    verified: bool,
    last_heard: Instant,
    /// When we last asked a quiet phone to speak (see tick).
    probed_at: Option<Instant>,
    mtu_payload: usize,
    crypto: Option<SessionCrypto>,
    /// Set by stop(): a connect still running for this link must give up
    /// rather than finish (it would subscribe and answer the challenge with
    /// a secret that may since have changed, and hold CONNECTING meanwhile).
    stopped: bool,
    image_reply: Option<String>,
}

/// One connect attempt at a time: a rescan mid-attempt used to start a second.
static CONNECTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub struct Ble {
    secret: String,
    events: Sender<Event>,
    link: Arc<Mutex<Link>>,
    watcher: Option<BluetoothLEAdvertisementWatcher>,
    /// Outgoing messages. A writer thread drains this, so a slow or dead
    /// link never blocks the window (a write to a bonded phone could take
    /// 60 s to time out, and it used to do so on the UI thread).
    outbox: Sender<Outgoing>,
}

enum Outgoing {
    Text(Kind, String),
    Image(u32),
}

impl Ble {
    pub fn new(secret: &str, events: Sender<Event>) -> Self {
        let (outbox, outbox_rx) = std::sync::mpsc::channel::<Outgoing>();
        let link = Arc::new(Mutex::new(Link {
                device: None,
                rx: None,
                tx: None,
                service: None,
                session: None,
                params: None,
                inbox: Inbox::default(),
                handshake: Handshake::new(secret),
                verified: false,
                last_heard: Instant::now(),
                probed_at: None,
                mtu_payload: 20,
                crypto: None,
                stopped: false,
                image_reply: None,
            }));
        let writer_link = link.clone();
        let writer_secret = secret.to_string();
        std::thread::spawn(move || {
            for job in outbox_rx {
                let (kind, text) = match job {
                    Outgoing::Text(k, t) => (k, t),
                    Outgoing::Image(seq) => {
                        if let Err(e) = transfer_image(&writer_link, seq, &writer_secret) {
                            crate::log!("clipboard", "image sync: {e:#}");
                        }
                        continue;
                    }
                };
                let mut l = writer_link.lock().unwrap();
                if !l.verified && kind != Kind::Command {
                    continue; // nobody to talk to; the message is stale anyway
                }
                let verified = l.verified;
                let payload = match l.crypto.as_mut() {
                    Some(c) if verified => c.seal(text.as_bytes()),
                    _ => text.as_bytes().to_vec(),
                };
                if let Err(e) = write_chunks(&l, kind, &payload) {
                    crate::log!("bluetooth", "send failed: {e:#}");
                }
            }
        });
        Ble { secret: secret.to_string(), events, link, watcher: None, outbox }
    }

    pub fn is_linked(&self) -> bool {
        self.link.lock().unwrap().verified
    }

    /// Start looking for the phone. Safe to call again to rescan.
    pub fn start(&mut self) -> Result<()> {
        self.stop_scan();
        self.link.lock().unwrap().stopped = false;
        let watcher = BluetoothLEAdvertisementWatcher::new()?;
        watcher.AdvertisementFilter()?.Advertisement()?.ServiceUuids()?.Append(SERVICE)?;
        let link = self.link.clone();
        let events = self.events.clone();
        let secret = self.secret.clone();
        watcher.Received(&TypedEventHandler::new(
            move |w: &Option<BluetoothLEAdvertisementWatcher>, args: &Option<BluetoothLEAdvertisementReceivedEventArgs>| {
                let (Some(w), Some(args)) = (w, args) else { return Ok(()) };
                if link.lock().unwrap().verified {
                    return Ok(()); // already linked; a rescan mustn't tear that down
                }
                // A phone whose advert says it belongs to another pairing (a
                // tag from its secret; see Pairing.advertTag on the phone) is
                // skipped without connecting, so a second Bridge phone nearby
                // can't keep us from reaching ours. No tag (older app): try it.
                if let Ok(makers) = args.Advertisement().and_then(|a| a.ManufacturerData()) {
                    for maker in makers {
                        if maker.CompanyId()? != 0xFFFF {
                            continue;
                        }
                        let buffer = maker.Data()?;
                        let reader = DataReader::FromBuffer(&buffer)?;
                        let mut tag = vec![0u8; buffer.Length()? as usize];
                        reader.ReadBytes(&mut tag)?;
                        let ours = crate::protocol::hmac_hex(&secret, "bridge-advertise");
                        if tag.len() >= 4 && hex::encode(&tag[..4]) != ours[..8] {
                            return Ok(());
                        }
                    }
                }
                // One at a time: stop scanning while we try this one.
                let _ = w.Stop();
                let address = args.BluetoothAddress()?;
                // Android advertises from a *random* address. Opening it as
                // "public" (the default) connects to nothing, which was the
                // string of Unreachable/Disconnected attempts in the log.
                let addr_type = args.BluetoothAddressType()?;
                let link = link.clone();
                let events = events.clone();
                let secret = secret.clone();
                if CONNECTING.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    return Ok(()); // an attempt is already running
                }
                std::thread::spawn(move || {
                    if let Err(e) = connect(address, addr_type, &secret, link, events.clone()) {
                        crate::log!("bluetooth", "connect failed: {e:#}");
                        let _ = events.send(Event::Dropped(format!("{e:#}")));
                    }
                    CONNECTING.store(false, std::sync::atomic::Ordering::SeqCst);
                });
                Ok(())
            },
        ))?;
        watcher.Start()?;
        self.watcher = Some(watcher);
        let _ = self.events.send(Event::Searching);
        Ok(())
    }

    fn stop_scan(&mut self) {
        if let Some(w) = self.watcher.take() {
            let _ = w.Stop();
        }
    }

    /// Removes any Windows bond with a Bridge phone. Off the UI thread: each
    /// probe of a bonded-but-absent device can take a minute to time out.
    /// Removes one device's Windows pairing, on its own thread; the result
    /// comes back as an event.
    pub fn unpair_device(address: u64, events: Sender<Event>) {
        std::thread::spawn(move || {
            let result = (|| -> Result<String> {
                let d = BluetoothLEDevice::FromBluetoothAddressAsync(address)?.get()?;
                let name = d.Name().map(|n| n.to_string_lossy()).unwrap_or_default();
                let r = d.DeviceInformation()?.Pairing()?.UnpairAsync()?.get()?;
                let status = r.Status()?;
                use windows::Devices::Enumeration::DeviceUnpairingResultStatus as S;
                if status == S::Unpaired || status == S::AlreadyUnpaired {
                    Ok(name)
                } else {
                    bail!("Windows said {status:?}")
                }
            })();
            let _ = events.send(Event::UnpairedInWindows(result.map_err(|e| format!("{e:#}"))));
        });
    }

    pub fn unpair_all() {
        std::thread::spawn(Self::unpair_all_now);
    }

    fn unpair_all_now() {
        let Ok(selector) = BluetoothLEDevice::GetDeviceSelectorFromPairingState(true) else { return };
        let Ok(infos) = DeviceInformation::FindAllAsyncAqsFilter(&selector).and_then(|op| op.get()) else { return };
        for i in 0..infos.Size().unwrap_or(0) {
            let Ok(info) = infos.GetAt(i) else { continue };
            let Ok(d) = BluetoothLEDevice::FromIdAsync(&info.Id().unwrap_or_default()).and_then(|op| op.get()) else { continue };
            // Only phones running Bridge: the ones advertising our service.
            let has_service = d
                .GetGattServicesForUuidWithCacheModeAsync(SERVICE, BluetoothCacheMode::Cached)
                .and_then(|op| op.get())
                .and_then(|r| r.Services())
                .and_then(|s| s.Size())
                .map(|n| n > 0)
                .unwrap_or(false);
            if !has_service {
                continue;
            }
            let name = d.Name().map(|n| n.to_string_lossy()).unwrap_or_default();
            match d.DeviceInformation().and_then(|i| i.Pairing()).and_then(|p| p.UnpairAsync()).and_then(|op| op.get()) {
                Ok(r) => crate::log!("bluetooth", "unpaired {name}: {:?}", r.Status()),
                Err(e) => crate::log!("bluetooth", "unpair {name} failed: {e}"),
            }
        }
    }

    pub fn stop(&mut self) {
        self.stop_scan();
        let mut l = self.link.lock().unwrap();
        l.stopped = true;
        l.verified = false;
        l.rx = None;
        l.tx = None;
        l.service = None;
        l.session = None;
        l.params = None;
        l.device = None; // dropping the device object lets WinRT close the link
    }

    /// Called every few seconds by the app: notices a silent link and rescans.
    /// A link still waiting for the phone's challenge gets 20 s, like the Mac.
    /// A verified one that goes quiet for 60 s is *asked* for its status
    /// first: an idle phone's CPU sleeps and its 30 s heartbeat timer with it,
    /// while the link itself is fine. A write from us wakes its app, which
    /// answers. Only no answer within 20 s counts as dead (dropping on silence
    /// alone redialled a healthy link every ~100 s whenever the phone idled).
    pub fn tick(&mut self) {
        let mut probe = false;
        let (dead, why) = {
            let mut l = self.link.lock().unwrap();
            let silent = l.last_heard.elapsed();
            if l.device.is_none() {
                (false, "")
            } else if l.verified {
                if silent > Duration::from_secs(60) {
                    match l.probed_at {
                        Some(at) if at > l.last_heard => (at.elapsed() > Duration::from_secs(20), "phone didn't answer"),
                        _ => {
                            l.probed_at = Some(Instant::now());
                            probe = true;
                            (false, "")
                        }
                    }
                } else {
                    (false, "")
                }
            } else {
                (silent > Duration::from_secs(20), "the phone never sent its challenge")
            }
        };
        if probe {
            let _ = self.send_command("status");
        }
        if dead {
            crate::log!("bluetooth", "{why}, reconnecting");
            self.link.lock().unwrap().probed_at = None;
            self.stop();
            let _ = self.events.send(Event::Dropped("no heartbeat".into()));
            let _ = self.start();
        }
    }

    /// Queues a message for the phone. Returns Err only if there is no link
    /// at all; a queued message to a link that then drops is logged, not raised.
    pub fn send(&self, kind: Kind, text: &str) -> Result<()> {
        if !self.link.lock().unwrap().verified && kind != Kind::Command {
            bail!("not linked");
        }
        self.outbox.send(Outgoing::Text(kind, text.to_string())).map_err(|_| anyhow!("writer gone"))
    }

    pub fn send_image(&self, sequence: u32) -> Result<()> {
        self.outbox.send(Outgoing::Image(sequence)).map_err(|_| anyhow!("writer gone"))
    }

    pub fn send_command(&self, text: &str) -> Result<()> {
        self.send(Kind::Command, text)
    }
}

// Each image block is a complete encrypted message. Cancellation never leaves
// the ordinary message reassembler half full; subsequent text remains valid.
fn image_message(link: &Arc<Mutex<Link>>, text: &str) -> Result<()> {
    let mut l = link.lock().unwrap();
    if !l.verified || l.stopped { bail!("disconnected"); }
    let payload = l.crypto.as_mut().ok_or_else(|| anyhow!("no encrypted session"))?.seal(text.as_bytes());
    write_chunks(&l, Kind::ClipboardImage, &payload)
}

fn image_wait(link: &Arc<Mutex<Link>>, seq: u32, expected: &str) -> Result<()> {
    let start = Instant::now();
    loop {
        if crate::clipboard::sequence() != seq { bail!("clipboard changed"); }
        {
            let mut l = link.lock().unwrap();
            if !l.verified || l.stopped { bail!("disconnected"); }
            if let Some(reply) = l.image_reply.take() {
                if reply == expected { return Ok(()); }
                if reply.starts_with(&format!("{seq} ")) { bail!("phone: {reply}"); }
            }
        }
        if start.elapsed() > Duration::from_secs(15) { bail!("image acknowledgement timeout"); }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn transfer_image(link: &Arc<Mutex<Link>>, seq: u32, secret: &str) -> Result<()> {
    let png = crate::clipboard::sync_png(seq)?;
    if crate::lan::send_image(secret, seq, &png)? { return Ok(()); }
    if png.len() > 1024 * 1024 {
        crate::notify::show("Bridge", "Image not synchronized", "LAN unavailable. This image exceeds the 1 MiB BLE limit.");
        bail!("LAN unavailable; image exceeds BLE's 1 MiB limit");
    }
    link.lock().unwrap().image_reply = None;
    let result = (|| -> Result<()> {
        image_message(link, &format!("begin {seq} {}", png.len()))?;
        image_wait(link, seq, &format!("{seq} ACK"))?;
        for (offset, block) in png.chunks(384).enumerate() {
            if crate::clipboard::sequence() != seq { bail!("clipboard changed"); }
            if let Some(reply) = link.lock().unwrap().image_reply.take() {
                if reply.starts_with(&format!("{seq} NACK")) { bail!("phone: {reply}"); }
            }
            image_message(link, &format!("data {seq} {} {}", offset * 384, image_base64(block)))?;
        }
        if crate::clipboard::sequence() != seq { bail!("clipboard changed"); }
        image_message(link, &format!("end {seq}"))?;
        image_wait(link, seq, &format!("{seq} OK"))?;
        crate::log!("clipboard", "image synchronized over BLE ({} bytes)", png.len());
        Ok(())
    })();
    if result.is_err() { let _ = image_message(link, &format!("abort {seq}")); }
    result
}

fn image_base64(bytes: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = ((c[0] as u32) << 16) | ((c.get(1).copied().unwrap_or(0) as u32) << 8) | c.get(2).copied().unwrap_or(0) as u32;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if c.len() > 1 { T[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if c.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

fn write_chunks(l: &Link, kind: Kind, payload: &[u8]) -> Result<()> {
    let rx = l.rx.as_ref().ok_or_else(|| anyhow!("no RX characteristic"))?;
    for chunk in protocol::chunk_bytes(kind, payload, l.mtu_payload) {
        let writer = DataWriter::new()?;
        writer.WriteBytes(&chunk)?;
        let buffer = writer.DetachBuffer()?;
        let result = rx.WriteValueWithResultAndOptionAsync(&buffer, GattWriteOption::WriteWithoutResponse)?.get()?;
        if result.Status()? != GattCommunicationStatus::Success {
            bail!("write failed: {:?}", result.Status()?);
        }
    }
    Ok(())
}

/// The whole connect sequence, on its own thread: device → pair → service →
/// characteristics → subscribe. The handshake then runs in the ValueChanged
/// callback.
fn connect(address: u64, addr_type: BluetoothAddressType, secret: &str, link: Arc<Mutex<Link>>, events: Sender<Event>) -> Result<()> {
    let device = BluetoothLEDevice::FromBluetoothAddressWithBluetoothAddressTypeAsync(address, addr_type)?
        .get()
        .context("opening the device")?;
    let name = device.Name()?.to_string_lossy();
    let is_paired = device.DeviceInformation()?.Pairing()?.IsPaired()?;
    crate::log!("bluetooth", "found {name} (paired={is_paired})");
    if is_paired {
        let _ = events.send(Event::PairedInWindows { name: name.clone(), address });
        // Not needed any more, and Windows behaves worse with one: the bonded
        // link came up "Connected" with no working ATT. Say so once.
        crate::log!("bluetooth", "note: the phone is bonded in Windows settings; Bridge no longer needs that, and removing it avoids trouble");
    }

    if link.lock().unwrap().stopped {
        bail!("cancelled");
    }
    let service = find_service(&device)?;
    if link.lock().unwrap().stopped {
        bail!("cancelled");
    }
    let open = service.OpenAsync(GattSharingMode::SharedReadAndWrite)?.get()?;
    if open != GattOpenStatus::Success && open != GattOpenStatus::AlreadyOpened {
        bail!("couldn't open the Bridge service: {:?}", open);
    }
    let tx = characteristic(&service, TX)?;
    let rx = characteristic(&service, RX)?;
    crate::log!("bluetooth", "service and characteristics found");

    {
        let mut l = link.lock().unwrap();
        if l.stopped {
            bail!("cancelled");
        }
        l.rx = Some(rx.clone());
        l.tx = Some(tx.clone());
        l.service = Some(service.clone());
        l.handshake = Handshake::new(secret);
        l.verified = false;
        l.crypto = None;
        l.last_heard = Instant::now();
        l.mtu_payload = 20; // conservative; the phone's chunking allows for it
    }

    // Incoming chunks arrive here.
    let link_cb = link.clone();
    let events_cb = events.clone();
    let secret_cb = secret.to_string();
    tx.ValueChanged(&TypedEventHandler::new(
        move |_: &Option<GattCharacteristic>, args: &Option<GattValueChangedEventArgs>| {
            let Some(args) = args else { return Ok(()) };
            let buffer = args.CharacteristicValue()?;
            let reader = DataReader::FromBuffer(&buffer)?;
            let mut bytes = vec![0u8; buffer.Length()? as usize];
            reader.ReadBytes(&mut bytes)?;
            receive(&bytes, &link_cb, &events_cb, &secret_cb);
            Ok(())
        },
    ))?;

    // Notice the link going away.
    let link_cs = link.clone();
    let events_cs = events.clone();
    device.ConnectionStatusChanged(&TypedEventHandler::new(
        move |d: &Option<BluetoothLEDevice>, _: &Option<windows::core::IInspectable>| {
            if let Some(d) = d {
                if d.ConnectionStatus()? == BluetoothConnectionStatus::Disconnected {
                    let mut l = link_cs.lock().unwrap();
                    if l.device.is_some() {
                        l.verified = false;
                        l.rx = None;
                        l.tx = None;
                        l.service = None;
                        l.session = None;
        l.params = None;
                        l.device = None;
                        l.crypto = None;
                        crate::log!("bluetooth", "disconnected");
                        let _ = events_cs.send(Event::Dropped("disconnected".into()));
                    }
                }
            }
            Ok(())
        },
    ))?;

    // Subscribe: the helper first; if it errors, the descriptor by hand.
    let mut subscribed = false;
    for attempt in 1..=4 {
        let status = match tx.WriteClientCharacteristicConfigurationDescriptorAsync(
            GattClientCharacteristicConfigurationDescriptorValue::Notify,
        ) {
            Ok(op) => op.get().map(|s| format!("{s:?}")).unwrap_or_else(|e| format!("error {e}")),
            Err(e) => format!("error {e}"),
        };
        if status == format!("{:?}", GattCommunicationStatus::Success) {
            subscribed = true;
            break;
        }
        crate::log!("bluetooth", "subscribe {attempt}/4 via helper: {status}; trying the descriptor directly");
        if let Ok(true) = subscribe_direct(&tx) {
            subscribed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1500));
    }
    if !subscribed {
        bail!("subscribe failed");
    }
    // Chunk to the negotiated MTU rather than the 20-byte minimum: Windows
    // and Android usually agree on 500+, which makes long clipboard text and
    // notifications arrive in one or two chunks instead of dozens.
    let session = GattSession::FromDeviceIdAsync(&device.BluetoothDeviceId()?).and_then(|op| op.get()).ok();
    let mtu_payload = session
        .as_ref()
        .and_then(|s| s.MaxPduSize().ok())
        .map(|pdu| (pdu as usize).saturating_sub(3).clamp(20, 500))
        .unwrap_or(20);
    if let Some(s) = &session {
        if let Err(e) = s.SetMaintainConnection(true) {
            crate::log!("bluetooth", "couldn't ask Windows to keep the link: {e}");
        }
    }
    describe_params(&device, "at connect");
    let params = request_relaxed_link(&device);
    // If the phone's stack tightens the link again later (it renegotiates
    // whenever another computer connects), ask again.
    let link_pc = link.clone();
    let _ = device.ConnectionParametersChanged(&TypedEventHandler::new(
        move |d: &Option<BluetoothLEDevice>, _: &Option<windows::core::IInspectable>| {
            if let Some(d) = d {
                describe_params(d, "changed");
                let tight = d.GetConnectionParameters().and_then(|p| p.LinkTimeout()).map(|t| t < 200).unwrap_or(false);
                if tight {
                    let fresh = request_relaxed_link(d);
                    if let Ok(mut l) = link_pc.lock() {
                        if l.device.is_some() {
                            l.params = fresh;
                        }
                    }
                }
            }
            Ok(())
        },
    ));
    {
        let mut l = link.lock().unwrap();
        l.mtu_payload = mtu_payload;
        l.session = session;
        l.params = params;
        l.device = Some(device);
    }
    crate::log!("bluetooth", "subscribed (chunks of {mtu_payload} bytes); waiting for the phone's challenge");
    Ok(())
}

/// Logs the link's actual parameters. The phone's Bluetooth stack puts new
/// links on a 720 ms supervision timeout (the Mac's drops every few minutes
/// were exactly that); this line is how to see whether the PC's link is too.
fn describe_params(device: &BluetoothLEDevice, when: &str) {
    match device.GetConnectionParameters() {
        Ok(p) => crate::log!(
            "bluetooth",
            "link parameters {when}: interval {:.1} ms, latency {}, supervision timeout {} ms",
            p.ConnectionInterval().unwrap_or(0) as f32 * 1.25,
            p.ConnectionLatency().unwrap_or(0),
            p.LinkTimeout().unwrap_or(0) as u32 * 10
        ),
        Err(_) => {} // Windows 10 doesn't have this API
    }
}

/// Asks for "balanced" parameters (a supervision timeout of seconds, not
/// 720 ms), from our side as the central. The phone does the same for the
/// Mac's link from its side; for the PC it can't without opening a link back,
/// which made Windows offer to pair. Windows 11 only; on 10 this is a no-op.
fn request_relaxed_link(device: &BluetoothLEDevice) -> Option<BluetoothLEPreferredConnectionParametersRequest> {
    let balanced = BluetoothLEPreferredConnectionParameters::Balanced().ok()?;
    match device.RequestPreferredConnectionParameters(&balanced) {
        Ok(r) => {
            crate::log!("bluetooth", "asked for relaxed link parameters: {:?}", r.Status().ok());
            Some(r)
        }
        Err(e) => {
            crate::log!("bluetooth", "couldn't ask for link parameters: {e}");
            None
        }
    }
}

/// Writes the Client Characteristic Configuration descriptor (0x2902) by hand.
fn subscribe_direct(tx: &GattCharacteristic) -> Result<bool> {
    let cccd = GUID::from_u128(0x00002902_0000_1000_8000_00805f9b34fb);
    let r = tx.GetDescriptorsForUuidWithCacheModeAsync(cccd, BluetoothCacheMode::Uncached)?.get()?;
    if r.Status()? != GattCommunicationStatus::Success || r.Descriptors()?.Size()? == 0 {
        crate::log!("bluetooth", "no CCCD descriptor visible ({:?})", r.Status()?);
        return Ok(false);
    }
    let d = r.Descriptors()?.GetAt(0)?;
    let writer = DataWriter::new()?;
    writer.WriteBytes(&[1, 0])?; // notifications on
    let result = d.WriteValueWithResultAsync(&writer.DetachBuffer()?)?.get()?;
    let st = result.Status()?;
    crate::log!("bluetooth", "direct CCCD write: {st:?} (protocol error {:?})", result.ProtocolError().ok().and_then(|p| p.Value().ok()));
    Ok(st == GattCommunicationStatus::Success)
}

/// The Bridge service, with a few retries: the first query on a fresh link
/// can come back Unreachable while the connection is still being set up.
fn find_service(device: &BluetoothLEDevice) -> Result<GattDeviceService> {
    let mut last = GattCommunicationStatus::Unreachable;
    for attempt in 1..=2 {
        let r = device.GetGattServicesForUuidWithCacheModeAsync(SERVICE, BluetoothCacheMode::Uncached)?.get()?;
        last = r.Status()?;
        if last == GattCommunicationStatus::Success && r.Services()?.Size()? > 0 {
            return Ok(r.Services()?.GetAt(0)?);
        }
        crate::log!("bluetooth", "service query {attempt}/2: {last:?} (connection {:?})", device.ConnectionStatus()?);
        std::thread::sleep(Duration::from_millis(1500));
    }
    bail!("Bridge service not found on the phone ({last:?})")
}

fn characteristic(service: &GattDeviceService, uuid: GUID) -> Result<GattCharacteristic> {
    let result = service.GetCharacteristicsForUuidWithCacheModeAsync(uuid, BluetoothCacheMode::Uncached)?.get()?;
    let status = result.Status()?;
    let found = result.Characteristics()?;
    if status != GattCommunicationStatus::Success || found.Size()? == 0 {
        // Say what the phone did offer, so a mismatch is obvious from the log.
        let all = service.GetCharacteristicsWithCacheModeAsync(BluetoothCacheMode::Uncached)?.get()?;
        let listed: Vec<String> = (0..all.Characteristics()?.Size()?)
            .filter_map(|i| all.Characteristics().ok()?.GetAt(i).ok()?.Uuid().ok().map(|u| format!("{u:?}")))
            .collect();
        bail!(
            "characteristic {uuid:?} not found (query status {status:?}, all-characteristics status {:?}, offered: [{}])",
            all.Status()?,
            listed.join(", ")
        );
    }
    Ok(found.GetAt(0)?)
}

/// One chunk in from the phone.
fn receive(bytes: &[u8], link: &Arc<Mutex<Link>>, events: &Sender<Event>, secret: &str) {
    let mut l = link.lock().unwrap();
    l.last_heard = Instant::now();
    let Some((kind, raw)) = l.inbox.push_bytes(bytes) else { return };
    if !l.verified {
        crate::log!("bluetooth", "message from the phone: {kind:?}, {} bytes", raw.len());
    }

    if kind == Kind::Auth {
        let text = String::from_utf8_lossy(&raw).into_owned();
        match l.handshake.handle(&text) {
            Step::Reply(r) => {
                if let Err(e) = write_chunks(&l, Kind::Command, r.as_bytes()) {
                    crate::log!("bluetooth", "couldn't answer the challenge: {e:#}");
                }
            }
            Step::Verified => {
                if let Some((phone_nonce, our_nonce)) = l.handshake.nonces.clone() {
                    l.crypto = Some(SessionCrypto::new(secret, &phone_nonce, &our_nonce));
                }
                l.verified = true;
                crate::log!("bluetooth", "linked to the phone (verified, encrypted session)");
                let _ = events.send(Event::Linked);
            }
            Step::Failed => {
                crate::log!("bluetooth", "the phone failed our challenge; dropping it");
                l.device = None;
                l.rx = None;
                l.tx = None;
                l.service = None;
                l.session = None;
        l.params = None;
            }
            Step::Ignore => {}
        }
        return;
    }
    if !l.verified {
        return; // nothing from an unverified phone counts
    }
    let text = match l.crypto.as_ref() {
        Some(c) => match c.open(&raw) {
            Some(p) => String::from_utf8_lossy(&p).into_owned(),
            None => {
                crate::log!("bluetooth", "dropped a message that didn't decrypt");
                return;
            }
        },
        None => String::from_utf8_lossy(&raw).into_owned(),
    };
    match kind {
        Kind::ClipboardImage => { l.image_reply = Some(text); }
        Kind::Notification => {
            if let Some(n) = protocol::parse_notification(&text) {
                let _ = events.send(Event::Notification(n));
            }
        }
        Kind::Removed => {
            if let Ok(id) = text.trim().parse() {
                let _ = events.send(Event::NotificationRemoved(id));
            }
        }
        Kind::Media => {
            let _ = events.send(Event::Media(protocol::parse_media(&text)));
        }
        Kind::Art => {
            if let Some((key, b64)) = text.split_once('\t') {
                if let Ok(jpeg) = base64_decode(b64) {
                    let _ = events.send(Event::Art { key: key.to_string(), jpeg });
                }
            }
        }
        Kind::Icon => {
            if let Some((package, b64)) = text.split_once('\t') {
                if let Ok(png) = base64_decode(b64) {
                    let _ = events.send(Event::Icon { package: package.to_string(), png });
                }
            }
        }
        Kind::Clipboard => {
            let _ = events.send(Event::Clipboard(text));
        }
        Kind::Status => {
            let _ = events.send(Event::Status(protocol::parse_status(&text)));
        }
        Kind::Share => {
            if text.len() == 32 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
                let _ = events.send(Event::ShareOffer(text));
            }
        }
        Kind::Hotspot => {
            if let Some(reply) = parse_hotspot_reply(&text) { let _ = events.send(reply); }
        }
        Kind::Ping | Kind::Command | Kind::Auth => {}
    }
}

fn parse_hotspot_reply(text: &str) -> Option<Event> {
    let fields: Vec<&str> = text.split('\t').collect();
    let id = fields.first().copied().unwrap_or("");
    if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) { return None; }
    let credentials = if fields.len() == 5 && fields[1] == "OK" {
        (|| {
            let ssid = String::from_utf8(base64_decode(fields[2]).map_err(|_| "Invalid hotspot reply")?)
                .map_err(|_| "Invalid hotspot reply")?;
            let password = String::from_utf8(base64_decode(fields[3]).map_err(|_| "Invalid hotspot reply")?)
                .map_err(|_| "Invalid hotspot reply")?;
            let c = crate::hotspot::Credentials { ssid, password, security: fields[4].into() };
            if !c.validate() { return Err("Unsupported hotspot configuration".to_string()); }
            Ok(c)
        })()
    } else {
        // Malformed success payloads must not be copied into error messages.
        let code = if fields.len() == 3 && fields[1] == "ERR" {
            fields.get(2).filter(|c| c.len() <= 80 && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        } else { None };
        Err(format!("Phone hotspot unavailable ({})", code.copied().unwrap_or("invalid_reply")))
    };
    Some(Event::HotspotReply { id: id.into(), credentials })
}

/// Standard base64, no dependency needed for one use.
fn base64_decode(text: &str) -> Result<Vec<u8>> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in text.bytes() {
        if c == b'=' { break; }
        let v = T.iter().position(|&t| t == c).ok_or_else(|| anyhow!("bad base64"))? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

/// A name for the log, without the address (which is a stable identifier).
#[allow(dead_code)]
pub fn describe(device: &BluetoothLEDevice) -> String {
    device.Name().map(|n| n.to_string_lossy()).unwrap_or_else(|_| "phone".into())
}

#[allow(dead_code)]
fn hstring(s: &str) -> HSTRING {
    HSTRING::from(s)
}

#[cfg(test)]
mod hotspot_tests {
    use super::*;
    const ID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn image_blocks_roundtrip_and_fit_receiver_limit() {
        assert_eq!(image_base64(b"f"), "Zg==");
        assert_eq!(image_base64(b"fo"), "Zm8=");
        assert_eq!(image_base64(b"foo"), "Zm9v");
        for len in 0..=384 {
            let bytes: Vec<u8> = (0..len).map(|n| n as u8).collect();
            let encoded = image_base64(&bytes);
            assert!(encoded.len() <= 512);
            assert_eq!(base64_decode(&encoded).unwrap(), bytes);
        }
        assert_eq!(Kind::from_byte(13), Some(Kind::ClipboardImage));
    }

    #[test]
    fn directed_credentials_and_redacted_debug() {
        let event = parse_hotspot_reply(&format!("{ID}\tOK\tUGhvbmU=\tc2VjcmV0MTIz\twpa2")).unwrap();
        assert!(!format!("{event:?}").contains("secret123"));
        match event {
            Event::HotspotReply { id, credentials: Ok(c) } => {
                assert_eq!(id, ID);
                assert_eq!(c.ssid, "Phone");
                assert_eq!(c.password, "secret123");
                assert_eq!(c.security, "wpa2");
            }
            _ => panic!("expected credentials"),
        }
    }

    #[test]
    fn rejects_unknown_requests_and_does_not_echo_malformed_secrets() {
        assert!(parse_hotspot_reply("bad-id\tOK\tUGhvbmU=\tMTIzNDU2Nzg=\twpa2").is_none());
        let event = parse_hotspot_reply(&format!("{ID}\tOK\tSECRET_VALUE")).unwrap();
        assert!(!format!("{event:?}").contains("SECRET_VALUE"));
        assert!(matches!(parse_hotspot_reply(&format!("{ID}\tOK\tUGhvbmU=\t\topen")),
            Some(Event::HotspotReply { credentials: Ok(_), .. })));
        assert!(matches!(parse_hotspot_reply(&format!("{ID}\tERR\troot_required")),
            Some(Event::HotspotReply { credentials: Err(_), .. })));
    }
}
