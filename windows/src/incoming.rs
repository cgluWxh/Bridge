//! Android-initiated shares. Each worker owns its tunnel, independently of the
//! mirror/file-browser UI. The phone offers a short-lived capability over BLE.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct Report {
    pub succeeded: usize,
    pub failed: usize,
    /// Actual committed names, including collision suffixes, for Explorer selection.
    pub saved: Vec<PathBuf>,
}

fn line(s: &mut (impl Read + Write)) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    let mut b = [0u8; 1];
    loop {
        s.read_exact(&mut b)?;
        if b[0] == b'\n' {
            return String::from_utf8(bytes).map_err(|_| std::io::Error::other("invalid UTF-8"));
        }
        bytes.push(b[0]);
        if bytes.len() > 4096 { return Err(std::io::Error::other("header too long")); }
    }
}

/// Never trust a peer's filename: no paths, ADS, device names, or trailing dots.
fn safe_name(name: &str) -> String {
    let mut name: String = name.chars().map(|c| {
        if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c }
    }).take(120).collect();
    name = name.trim_matches([' ', '.']).to_string();
    if name.is_empty() { name = "phone-file".into(); }
    let stem = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$")
        || ["COM", "LPT"].iter().any(|prefix| stem.strip_prefix(prefix).is_some_and(|n|
            matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"))) {
        name.insert(0, '_');
    }
    name
}

fn decode_name(s: &str) -> std::io::Result<String> {
    if s.len() % 2 != 0 || s.len() > 2048 { return Err(std::io::Error::other("invalid filename")); }
    let bytes: Result<Vec<u8>, _> = s.as_bytes().chunks_exact(2).map(|pair| {
        let h = (pair[0] as char).to_digit(16).ok_or(())?;
        let l = (pair[1] as char).to_digit(16).ok_or(())?;
        Ok::<u8, ()>((h * 16 + l) as u8)
    }).collect();
    String::from_utf8(bytes.map_err(|_| std::io::Error::other("invalid filename"))?)
        .map_err(|_| std::io::Error::other("invalid filename"))
}

/// Atomic, no-replace publication; Windows MoveFileW also works on FAT/exFAT.
fn publish(source: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(windows)] {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe { windows::Win32::Storage::FileSystem::MoveFileW(PCWSTR(source.as_ptr()), PCWSTR(target.as_ptr())) }
            .map_err(|e| std::io::Error::from_raw_os_error(e.code().0 & 0xffff))
    }
    #[cfg(not(windows))] { fs::hard_link(source, target) }
}

struct Pending { path: PathBuf, file: Option<File> }
impl Pending {
    fn create(dir: &Path) -> std::io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..100 {
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
            let path = dir.join(format!(".bridge-{}-{stamp}-{}.part", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok(Self { path, file: Some(file) }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::other("could not allocate temporary file"))
    }
    fn commit(&mut self, dir: &Path, name: &str) -> std::io::Result<PathBuf> {
        self.file.as_ref().unwrap().sync_all()?;
        drop(self.file.take());
        let p = Path::new(name);
        let stem = p.file_stem().unwrap_or_default().to_string_lossy();
        let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        for i in 0..10_000 {
            let target = dir.join(if i == 0 { name.to_string() } else { format!("{stem} ({i}){ext}") });
            // Never replace an existing target, even when concurrent transfers
            // choose the same filename between preflight and commit.
            match publish(&self.path, &target) {
                Ok(()) => return Ok(target),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::other("too many files with the same name"))
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}

/// Per-file preflight and durable ACKs let one bad filename/disk error fail a
/// row without desynchronizing the rest of the batch. Interrupted streams abort.
pub fn receive(s: &mut (impl Read + Write), dir: &Path) -> std::io::Result<Report> {
    let head = line(s)?;
    let count: usize = head.strip_prefix("OK ").and_then(|n| n.parse().ok())
        .filter(|n| *n <= 10_000).ok_or_else(|| std::io::Error::other("phone refused share"))?;
    let directory = fs::create_dir_all(dir);
    let mut report = Report::default();
    let mut buf = vec![0u8; 128 * 1024];
    for _ in 0..count {
        let header = line(s)?;
        if header.starts_with("ERR ") { report.failed += 1; continue; }
        let mut fields = header.splitn(3, ' ');
        if fields.next() != Some("FILE") { return Err(std::io::Error::other("invalid share header")); }
        let total: u64 = fields.next().and_then(|n| n.parse().ok()).ok_or_else(|| std::io::Error::other("invalid length"))?;
        let name = safe_name(&decode_name(fields.next().unwrap_or(""))?);
        let pending = if directory.is_ok() { Pending::create(dir) } else { Err(std::io::Error::other("Downloads is unavailable")) };
        let mut pending = match pending {
            Ok(p) => p,
            Err(_) => { s.write_all(b"ERR Cannot-create-file\n")?; s.flush()?; report.failed += 1; continue; }
        };
        s.write_all(b"READY\n")?; s.flush()?;
        let mut left = total;
        let mut write_error = false;
        while left > 0 {
            let want = buf.len().min(left.min(usize::MAX as u64) as usize);
            s.read_exact(&mut buf[..want])?;
            if !write_error && pending.file.as_mut().unwrap().write_all(&buf[..want]).is_err() { write_error = true; }
            left -= want as u64;
        }
        // Drain even on disk-full to preserve the next file's framing.
        let committed = if write_error { None } else { pending.commit(dir, &name).ok() };
        if let Some(path) = committed {
            report.saved.push(path);
            s.write_all(b"OK\n")?; report.succeeded += 1;
        } else {
            s.write_all(b"ERR Cannot-save-file\n")?; report.failed += 1;
        }
        s.flush()?;
    }
    if line(s)? != "DONE" { return Err(std::io::Error::other("share ended without DONE")); }
    Ok(report)
}

pub fn start(ticket: String, secret: String, token: String, events: std::sync::mpsc::Sender<crate::ble::Event>) {
    // One incoming batch per phone. A duplicate offer cannot start a second copy.
    static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if BUSY.swap(true, Ordering::SeqCst) { return; }
    std::thread::spawn(move || {
        let result = (|| -> anyhow::Result<Report> {
            let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
            let mut tunnel = crate::tunnel::Tunnel::start_preferred_at(&ticket, &secret, port)?;
            let mut requested_tunnel = false;
            let started = std::time::Instant::now();
            let mut stream = loop {
                if !tunnel.is_running() { anyhow::bail!("File sharing tunnel stopped"); }
                if tunnel.transport() == "Connecting" {
                    if started.elapsed().as_secs() >= 60 { anyhow::bail!("File sharing discovery timed out"); }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    continue;
                }
                if tunnel.transport() == "iroh" && !requested_tunnel {
                    let _ = events.send(crate::ble::Event::ShareNeedsTunnel);
                    requested_tunnel = true;
                }
                match crate::tunnel::Stream::open_at(&secret, std::time::Duration::from_secs(60), port) {
                    Ok(s) => break s,
                    Err(e) if started.elapsed().as_secs() >= 60 => return Err(e),
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(500)),
                }
            };
            crate::log!("share", "receiving phone files over {}", tunnel.transport());
            stream.write_all(format!("SHAR {token}\n").as_bytes())?;
            Ok(receive(&mut stream, &crate::files::downloads_dir())?)
        })().map_err(|e| format!("Phone share interrupted: {e:#}. Check the report on your phone."));
        BUSY.store(false, Ordering::SeqCst);
        let _ = events.send(crate::ble::Event::ShareFinished(result));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Wire { input: std::io::Cursor<Vec<u8>>, output: Vec<u8> }
    impl Read for Wire { fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> { self.input.read(b) } }
    impl Write for Wire {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> { self.output.extend_from_slice(b); Ok(b.len()) }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    fn wire(bytes: &[u8]) -> Wire { Wire { input: std::io::Cursor::new(bytes.to_vec()), output: vec![] } }
    fn dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("bridge-incoming-test-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&p); fs::create_dir_all(&p).unwrap(); p
    }
    #[test] fn sanitizes_windows_names() {
        assert_eq!(safe_name("../../CON.txt"), "_.._CON.txt");
        assert_eq!(safe_name("CON.txt"), "_CON.txt");
        assert_eq!(safe_name("lpt¹.txt"), "_lpt¹.txt");
        assert_eq!(safe_name("a:b?.jpg. "), "a_b_.jpg");
        assert_eq!(safe_name("..."), "phone-file");
        assert_eq!(decode_name("e4b8ad2e747874").unwrap(), "中.txt");
        assert!(decode_name("abz0").is_err());
    }
    #[test] fn batch_collision_empty_and_source_error() {
        let d = dir(); fs::write(d.join("a.txt"), b"old").unwrap();
        let mut s = wire(b"OK 3\nFILE 3 612e747874\nnewERR unreadable\nFILE 0 656d707479\nDONE\n");
        let r = receive(&mut s, &d).unwrap();
        assert_eq!((r.succeeded, r.failed), (2, 1));
        assert_eq!(r.saved, vec![d.join("a (1).txt"), d.join("empty")]);
        assert_eq!(fs::read(d.join("a.txt")).unwrap(), b"old");
        assert_eq!(fs::read(d.join("a (1).txt")).unwrap(), b"new");
        assert_eq!(fs::metadata(d.join("empty")).unwrap().len(), 0);
        assert_eq!(s.output, b"READY\nOK\nREADY\nOK\n");
        assert_eq!(fs::read_dir(&d).unwrap().count(), 3); fs::remove_dir_all(d).unwrap();
    }
    #[test] fn interrupted_file_is_removed() {
        let d = dir(); let mut s = wire(b"OK 1\nFILE 50 612e747874\nshort");
        assert!(receive(&mut s, &d).is_err());
        assert_eq!(fs::read_dir(&d).unwrap().count(), 0); fs::remove_dir_all(d).unwrap();
    }
    #[test] fn preflight_failure_continues_batch() {
        let d = dir(); let target = d.join("not-a-folder"); fs::write(&target, b"x").unwrap();
        let mut s = wire(b"OK 2\nFILE 3 61\nFILE 0 62\nDONE\n");
        let r = receive(&mut s, &target).unwrap();
        assert_eq!((r.succeeded, r.failed), (0, 2));
        assert!(r.saved.is_empty());
        assert_eq!(s.output, b"ERR Cannot-create-file\nERR Cannot-create-file\n"); fs::remove_dir_all(d).unwrap();
    }
    #[test] fn rejects_bad_framing() {
        let d = dir();
        for bytes in [b"OK 999999\n".as_slice(), b"OK 1\nFILE x 61\n", b"OK 0\nNO\n"] {
            assert!(receive(&mut wire(bytes), &d).is_err());
        }
        fs::remove_dir_all(d).unwrap();
    }
}
