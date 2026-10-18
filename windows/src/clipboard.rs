//! Clipboard both ways. Windows has a proper change listener, but polling the
//! clipboard *sequence number* every half second is simpler, costs nothing,
//! and needs no window to receive messages on.

use arboard::Clipboard;
use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;
use std::sync::atomic::{AtomicBool, Ordering};

pub static IMAGE_PASTE_BUSY: AtomicBool = AtomicBool::new(false);
pub fn sequence() -> u32 { unsafe { GetClipboardSequenceNumber() } }
pub fn image_uri(text: &str) -> bool { text.starts_with("content://com.bonevane.bridge.images/") }

/// Called on a worker, only after Ctrl+V in the mirror. No background image sync.
/// False means there is no image: preserve the ordinary text Ctrl+V behavior.
pub fn upload_image(secret: &str, expected_sequence: u32) -> anyhow::Result<bool> {
    use anyhow::{bail, Context};
    use std::io::Cursor;
    use std::time::Duration;
    let mut clipboard = Clipboard::new().context("opening clipboard")?;
    let image = match clipboard.get_image() {
        Ok(image) => image,
        Err(arboard::Error::ContentNotAvailable) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if sequence() != expected_sequence { bail!("Clipboard changed; paste again"); }
    let pixels = image.width.checked_mul(image.height).context("invalid image size")?;
    if pixels == 0 || pixels > 40_000_000 { bail!("Screenshot exceeds 40 megapixel limit"); }
    let rgba = image::RgbaImage::from_raw(image.width as u32, image.height as u32, image.bytes.into_owned())
        .context("invalid screenshot pixels")?;
    let mut png = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(rgba).write_to(&mut png, image::ImageFormat::Png)?;
    let png = png.into_inner();
    if png.len() > 32 * 1024 * 1024 { bail!("Screenshot exceeds 32 MB PNG limit"); }
    let mut stream = crate::tunnel::Stream::open(secret, Duration::from_secs(60))?;
    stream.write_all(format!("IMGC {}\n", png.len()).as_bytes())?;
    let ready = stream.read_line()?;
    if ready != "READY" { bail!("Phone refused image: {ready}"); }
    for chunk in png.chunks(128 * 1024) {
        stream.write_all(chunk)?;
        crate::session::note_transfer();
        crate::session::yield_to_video();
    }
    let ack = stream.read_line()?;
    if ack != "OK" { bail!("Phone did not set image clipboard: {ack}"); }
    Ok(true)
}

pub struct Watcher {
    last_seq: u32,
    /// The last text we put on the clipboard ourselves (or sent), so a
    /// change that is just our own write isn't echoed back to the phone.
    pub last_synced: Option<String>,
}

impl Watcher {
    pub fn new() -> Self {
        Watcher { last_seq: unsafe { GetClipboardSequenceNumber() }, last_synced: None }
    }

    /// Returns new text if the clipboard changed to something we didn't write.
    pub fn poll(&mut self) -> Option<String> {
        if IMAGE_PASTE_BUSY.load(Ordering::SeqCst) { return None; }
        let seq = unsafe { GetClipboardSequenceNumber() };
        if seq == self.last_seq {
            return None;
        }
        self.last_seq = seq;
        let mut clipboard = Clipboard::new().ok()?;
        // Screenshot tools may publish text alongside pixels: never sync that
        // text over the image clipboard we are about to paste on Android.
        if clipboard.get_image().is_ok() { return None; }
        let text = clipboard.get_text().ok()?;
        if text.is_empty() || self.last_synced.as_deref() == Some(text.as_str()) {
            return None;
        }
        self.last_synced = Some(text.clone());
        Some(text)
    }

    /// Text from the phone: put it on the clipboard without echoing it back.
    pub fn set(&mut self, text: &str) {
        if IMAGE_PASTE_BUSY.load(Ordering::SeqCst) || image_uri(text) { return; }
        self.last_synced = Some(text.to_string());
        if let Ok(mut c) = Clipboard::new() {
            let _ = c.set_text(text);
        }
        self.last_seq = unsafe { GetClipboardSequenceNumber() };
    }
}
