//! IPv4 LAN discovery over authenticated BLE, pinned TLS, and a loopback
//! byte-stream adapter. Existing Stream consumers (including cloned control
//! sockets) do not need to understand TLS. One route is fixed per Tunnel.
use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex, OnceLock, atomic::{AtomicBool, AtomicU8, Ordering}};
use std::time::{Duration, Instant};

#[derive(Clone)]
struct Candidate { addr: SocketAddr, pin: Vec<u8> }
struct Advertisement { secret: String, seen: Instant, candidates: Vec<Candidate>, issue: Option<&'static str> }
#[derive(Clone, Copy)]
struct Timeouts { ble_wait: Duration, tcp_connect: Duration, probe: Duration }
impl Timeouts {
    fn from_settings(settings: &crate::settings::Settings) -> Self {
        let mut settings = settings.clone();
        settings.normalize_lan_timeouts();
        Self {
            ble_wait: Duration::from_millis(settings.lan_ble_wait_ms as u64),
            tcp_connect: Duration::from_millis(settings.lan_tcp_connect_ms as u64),
            probe: Duration::from_millis(settings.lan_probe_ms as u64),
        }
    }
    // Read once per selection, not per byte/probe iteration. Saved UI changes
    // apply to the next connection without mutating an in-flight deadline.
    fn load() -> Self { Self::from_settings(&crate::settings::Settings::load()) }
}
static DISCOVERY: OnceLock<Mutex<Option<Advertisement>>> = OnceLock::new();
fn discovery() -> &'static Mutex<Option<Advertisement>> { DISCOVERY.get_or_init(|| Mutex::new(None)) }

/// Only call with a status received through the verified BLE session.
pub fn update(secret: &str, fields: &HashMap<String, String>) {
    let candidates = parse_candidates(fields);
    let issue = advertisement_issue(fields);
    *discovery().lock().unwrap() = Some(Advertisement { secret: secret.into(), seen: Instant::now(), candidates, issue });
}
pub fn clear() { *discovery().lock().unwrap() = None; }
fn parse_candidates(fields: &HashMap<String, String>) -> Vec<Candidate> {
    let Some(port) = fields.get("lan_port").and_then(|p| p.parse::<u16>().ok()).filter(|p| *p != 0) else { return vec![] };
    let Some(pin) = fields.get("lan_fp").and_then(|p| hex::decode(p).ok()).filter(|p| p.len() == 32) else { return vec![] };
    fields.get("lan_ips").into_iter().flat_map(|s| s.split(',')).take(8)
        .filter_map(|s| s.parse::<Ipv4Addr>().ok())
        .filter(|ip| !ip.is_unspecified() && !ip.is_loopback() && !ip.is_multicast() && !ip.is_broadcast())
        .map(|ip| Candidate { addr: SocketAddr::from((ip, port)), pin: pin.clone() }).collect()
}

/// Fixed descriptions only: do not log the BLE payload, pin, or pairing secret.
fn advertisement_issue(fields: &HashMap<String, String>) -> Option<&'static str> {
    if !fields.contains_key("lan_port") && !fields.contains_key("lan_fp") && !fields.contains_key("lan_ips") {
        return Some("BLE status contains no LAN endpoint (listener not ready or phone build lacks LAN)");
    }
    if fields.get("lan_port").and_then(|v| v.parse::<u16>().ok()).filter(|p| *p != 0).is_none() {
        return Some("missing or invalid LAN port");
    }
    if fields.get("lan_fp").and_then(|v| hex::decode(v).ok()).filter(|p| p.len() == 32).is_none() {
        return Some("missing or invalid LAN certificate fingerprint");
    }
    if parse_candidates(fields).is_empty() { return Some("no usable advertised IPv4 address"); }
    None
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() { bail!("LAN deadline exceeded"); }
    Ok(left)
}

fn connect(candidate: &Candidate) -> Result<native_tls::TlsStream<TcpStream>> {
    let timeouts = Timeouts::load();
    let mut tls = connect_until(candidate, Instant::now() + timeouts.probe, timeouts.tcp_connect)?;
    tls.get_mut().set_nonblocking(false)?;
    tls.get_ref().set_read_timeout(Some(timeouts.probe))?;
    tls.get_ref().set_write_timeout(Some(timeouts.probe))?;
    Ok(tls)
}

/// Nonblocking TLS ensures a slow/dribbling handshake cannot outlive the shared
/// candidate deadline. TCP, TLS and STATUS share the configured probe budget.
fn connect_until(candidate: &Candidate, deadline: Instant, tcp_cap: Duration) -> Result<native_tls::TlsStream<TcpStream>> {
    let timeout = tcp_cap.min(remaining(deadline)?);
    let tcp = TcpStream::connect_timeout(&candidate.addr, timeout)
        .with_context(|| format!("TCP connect failed ({} ms effective cap)", timeout.as_millis()))?;
    tcp.set_nodelay(true)?;
    tcp.set_nonblocking(true)?;
    // No CA/hostname trust: identity comes exclusively from authenticated BLE.
    // Never send AUTH or application bytes before certificate pin validation.
    let connector = native_tls::TlsConnector::builder()
        .min_protocol_version(Some(native_tls::Protocol::Tlsv12))
        .danger_accept_invalid_certs(true).danger_accept_invalid_hostnames(true).use_sni(false).build()?;
    let mut handshake = connector.connect("bridge-lan", tcp);
    let tls = loop {
        remaining(deadline).context("TLS handshake timeout")?;
        match handshake {
            Ok(tls) => break tls,
            Err(native_tls::HandshakeError::Failure(e)) => return Err(anyhow!("TLS handshake failed: {e}")),
            Err(native_tls::HandshakeError::WouldBlock(mid)) => {
                std::thread::sleep(Duration::from_millis(5));
                handshake = mid.handshake();
            }
        }
    };
    let cert = tls.peer_certificate()?.context("TLS peer has no certificate")?.to_der()?;
    if Sha256::digest(&cert).as_slice() != candidate.pin.as_slice() { bail!("TLS certificate pin mismatch"); }
    Ok(tls)
}

fn probe(candidate: &Candidate, secret: &str, deadline: Instant, tcp_cap: Duration) -> Result<()> {
    let mut tls = connect_until(candidate, deadline, tcp_cap)?;
    let request = format!("AUTH {secret}\nSTATUS\n");
    let mut sent = 0;
    while sent < request.len() {
        remaining(deadline).context("AUTH/STATUS write timeout")?;
        match tls.write(&request.as_bytes()[sent..]) {
            Ok(0) => bail!("connection closed while writing AUTH/STATUS"),
            Ok(n) => sent += n,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) =>
                std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(e).context("AUTH/STATUS write failed"),
        }
    }
    let mut response = Vec::new();
    let mut byte = [0];
    while response.len() < 4096 {
        remaining(deadline).context("AUTH/STATUS response timeout")?;
        match tls.read(&mut byte) {
            Ok(0) => bail!("connection closed before AUTH/STATUS response"),
            Ok(_) if byte[0] == b'\n' => {
                // Never log the peer's raw response or the AUTH request.
                if !response.starts_with(b"OK ") { bail!("AUTH/STATUS rejected or invalid response"); }
                return Ok(());
            }
            Ok(_) => response.push(byte[0]),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) =>
                std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(e).context("AUTH/STATUS read failed"),
        }
    }
    bail!("AUTH/STATUS response exceeds 4096 bytes")
}

fn select(secret: &str, stop: &AtomicBool) -> std::result::Result<Candidate, String> {
    let timeouts = Timeouts::load();
    let started = Instant::now();
    crate::log!("transport", "LAN selection budgets: BLE {} ms, TCP {} ms, parallel probe {} ms", timeouts.ble_wait.as_millis(), timeouts.tcp_connect.as_millis(), timeouts.probe.as_millis());
    // Existing fresh advertisements take the fast path; no mandatory wait.
    let until = started + timeouts.ble_wait;
    let candidates = loop {
        if stop.load(Ordering::Relaxed) { return Err("selection cancelled".into()); }
        {
            let cached = discovery().lock().unwrap();
            if let Some(a) = cached.as_ref() {
                if a.secret == secret && a.seen.elapsed() < Duration::from_secs(90) && !a.candidates.is_empty() {
                    break a.candidates.clone();
                }
            }
            if Instant::now() >= until {
                let reason = match cached.as_ref() {
                    None => "no authenticated BLE status received",
                    Some(a) if a.secret != secret => "cached BLE advertisement belongs to different pairing",
                    Some(a) if a.seen.elapsed() >= Duration::from_secs(90) => "BLE LAN advertisement expired (older than 90 s)",
                    Some(a) => a.issue.unwrap_or("BLE status contains no LAN candidates"),
                };
                return Err(format!("BLE address wait timed out after {} ms: {reason}", timeouts.ble_wait.as_millis()));
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let count = candidates.len();
    crate::log!("transport", "LAN: {} candidate(s), BLE wait {} ms; parallel probe budget {} ms, TCP cap {} ms", count, started.elapsed().as_millis(), timeouts.probe.as_millis(), timeouts.tcp_connect.as_millis());
    let until = Instant::now() + timeouts.probe;
    let (tx, rx) = std::sync::mpsc::channel();
    for candidate in candidates {
        let tx = tx.clone();
        let secret = secret.to_string();
        std::thread::spawn(move || {
            let result = probe(&candidate, &secret, until, timeouts.tcp_connect).map_err(|e| format!("{e:#}"));
            let _ = tx.send((candidate, result));
        });
    }
    drop(tx);
    let mut failed = 0;
    let mut first_failure = None;
    while Instant::now() < until && !stop.load(Ordering::Relaxed) {
        let wait = Duration::from_millis(25).min(until.saturating_duration_since(Instant::now()));
        match rx.recv_timeout(wait) {
            Ok((candidate, Ok(()))) => {
                crate::log!("transport", "LAN probe succeeded: {} (selection {} ms)", candidate.addr, started.elapsed().as_millis());
                return Ok(candidate);
            }
            Ok((candidate, Err(reason))) => {
                failed += 1;
                crate::log!("transport", "LAN probe failed: {}: {}", candidate.addr, reason);
                if first_failure.is_none() { first_failure = Some(format!("{}: {reason}", candidate.addr)); }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(_) => {}
        }
    }
    if stop.load(Ordering::Relaxed) { return Err("selection cancelled".into()); }
    let summary = if failed == count { "all candidates failed".to_string() }
        else { format!("{} ms parallel probe deadline reached", timeouts.probe.as_millis()) };
    Err(format!("{summary}; {failed}/{count} failures reported, {} pending; first failure: {}", count - failed, first_failure.as_deref().unwrap_or("none (probes did not complete before deadline)")))
}

fn advertised(secret: &str) -> bool {
    discovery().lock().unwrap().as_ref().map(|a|
        a.secret == secret && a.seen.elapsed() < Duration::from_secs(90) && !a.candidates.is_empty()
    ).unwrap_or(false)
}

fn image_reply(stream: &mut impl Read, sequence: u32) -> Result<String> {
    let mut bytes = Vec::new();
    let mut byte = [0];
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && bytes.len() < 4096 {
        if crate::clipboard::sequence() != sequence { bail!("clipboard changed"); }
        match stream.read(&mut byte) {
            Ok(0) => bail!("LAN image connection closed"),
            Ok(_) if byte[0] == b'\n' => return Ok(String::from_utf8_lossy(&bytes).into_owned()),
            Ok(_) => bytes.push(byte[0]),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted) => {},
            Err(e) => return Err(e.into()),
        }
    }
    bail!("LAN image acknowledgement timeout")
}

/// Independent clipboard streams need no daemon/mirror or running local tunnel.
/// Call only AFTER BLE admission. False means LAN is unreachable, so the
/// still-unused grant can be consumed by BLE begin instead.
pub fn send_image(secret: &str, sequence: u32, png: &[u8], grant: &str) -> Result<bool> {
    if !advertised(secret) { return Ok(false); }
    let candidate = match select(secret, &AtomicBool::new(false)) {
        Ok(candidate) => candidate,
        Err(reason) => {
            crate::log!("clipboard", "LAN selection failed; falling back to BLE: {reason}");
            return Ok(false);
        }
    };
    if crate::clipboard::sequence() != sequence { bail!("clipboard changed"); }
    let mut tls = match connect(&candidate) {
        Ok(tls) => tls,
        Err(e) => {
            crate::log!("clipboard", "LAN reconnect failed for {}; falling back to BLE: {e:#}", candidate.addr);
            return Ok(false);
        }
    };
    tls.get_ref().set_read_timeout(Some(Duration::from_millis(200)))?;
    tls.get_ref().set_write_timeout(Some(Duration::from_secs(1)))?;
    tls.write_all(format!("AUTH {secret}\nIMGB {} {grant}\n", png.len()).as_bytes())?;
    let ready = image_reply(&mut tls, sequence)?;
    if ready != "READY" { bail!("phone refused LAN image: {ready}"); }
    for block in png.chunks(16 * 1024) {
        if crate::clipboard::sequence() != sequence { bail!("clipboard changed"); }
        tls.write_all(block)?;
    }
    let ack = image_reply(&mut tls, sequence)?;
    if ack != "OK" { bail!("phone rejected LAN image: {ack}"); }
    crate::log!("clipboard", "image synchronized over LAN ({} bytes)", png.len());
    Ok(true)
}

pub struct LocalTunnel {
    stop: Arc<AtomicBool>,
    // 0 discovering, 1 LAN, 2 iroh, 3 ended.
    state: Arc<AtomicU8>,
}
impl LocalTunnel {
    pub fn start(ticket: &str, secret: &str, port: u16) -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).context("local tunnel port is busy")?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(AtomicU8::new(0));
        let (worker_stop, worker_state) = (stop.clone(), state.clone());
        let (ticket, secret) = (ticket.to_string(), secret.to_string());
        std::thread::spawn(move || {
            let result = serve(listener, &ticket, &secret, worker_stop.clone(), worker_state.clone());
            if let Err(e) = result { crate::log!("transport", "connection adapter stopped: {e:#}"); }
            worker_state.store(3, Ordering::Release);
            worker_stop.store(true, Ordering::Release);
        });
        Ok(Self { stop, state })
    }
    pub fn transport(&self) -> &'static str {
        match self.state.load(Ordering::Acquire) { 1 => "LAN", 2 => "iroh", 3 => "Disconnected", _ => "Connecting" }
    }
    pub fn is_running(&self) -> bool { self.state.load(Ordering::Acquire) != 3 && !self.stop.load(Ordering::Relaxed) }
    pub fn stop(&self) { self.stop.store(true, Ordering::Release); }
}
impl Drop for LocalTunnel { fn drop(&mut self) { self.stop(); } }

#[derive(Clone)]
enum Route { Lan(Candidate), Iroh(u16) }
fn serve(listener: TcpListener, ticket: &str, secret: &str, stop: Arc<AtomicBool>, state: Arc<AtomicU8>) -> Result<()> {
    let lan = select(secret, &stop);
    if stop.load(Ordering::Relaxed) { return Ok(()); }
    let mut fallback = None;
    let route = match lan {
        Ok(candidate) => {
            state.store(1, Ordering::Release);
            crate::log!("transport", "using LAN (TLS, BLE certificate pin)");
            Route::Lan(candidate)
        }
        Err(reason) => {
            crate::log!("transport", "LAN unavailable; selecting iroh because: {reason}");
            // Keep the public loopback adapter stable; dumbpipe gets a private port.
            let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
            let port = reservation.local_addr()?.port();
            drop(reservation);
            fallback = Some(crate::tunnel::Tunnel::start_at(ticket, port)?);
            state.store(2, Ordering::Release);
            crate::log!("transport", "using iroh; LAN fallback reason: {reason}");
            Route::Iroh(port)
        }
    };
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    while !stop.load(Ordering::Relaxed) {
        if let Some(t) = fallback.as_mut() { if !t.is_running() { bail!("iroh process exited"); } }
        match listener.accept() {
            Ok((local, _)) => {
                if active.fetch_add(1, Ordering::Relaxed) >= 32 { active.fetch_sub(1, Ordering::Relaxed); continue; }
                let (route, stop, active) = (route.clone(), stop.clone(), active.clone());
                std::thread::spawn(move || {
                    let result = (|| -> Result<()> {
                        local.set_nodelay(true)?;
                        local.set_nonblocking(true)?;
                        match route {
                            Route::Lan(c) => {
                                let mut remote = connect(&c)?;
                                remote.get_ref().set_read_timeout(None)?;
                                remote.get_ref().set_write_timeout(None)?;
                                remote.get_ref().set_nonblocking(true)?;
                                pump(local, &mut remote, &stop)
                            }
                            Route::Iroh(port) => {
                                // dumbpipe can take a moment to bind its private port.
                                // Retry only the TCP connect, before forwarding any AUTH/SHAR bytes.
                                let deadline = Instant::now() + Duration::from_secs(3);
                                let mut remote = loop {
                                    if stop.load(Ordering::Relaxed) { return Ok(()); }
                                    match TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, port)), Duration::from_millis(250)) {
                                        Ok(s) => break s,
                                        Err(e) if Instant::now() >= deadline => return Err(e.into()),
                                        Err(_) => std::thread::sleep(Duration::from_millis(50)),
                                    }
                                };
                                remote.set_nodelay(true)?;
                                remote.set_nonblocking(true)?;
                                pump(local, &mut remote, &stop)
                            }
                        }
                    })();
                    if let Err(e) = result { crate::log!("transport", "stream closed: {e:#}"); }
                    active.fetch_sub(1, Ordering::Relaxed);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(e.into()),
        }
    }
    // Dropping fallback terminates dumbpipe. Existing relay tasks also observe stop.
    Ok(())
}

/// Single nonblocking worker per stream, bounded buffers, no TLS mutex shared
/// between reader and writer (which would deadlock scrcpy's control channel).
fn pump(mut local: TcpStream, remote: &mut (impl Read + Write), stop: &AtomicBool) -> Result<()> {
    let mut up = [0u8; 64 * 1024];
    let mut down = [0u8; 64 * 1024];
    let (mut up_pos, mut up_len, mut down_pos, mut down_len) = (0, 0, 0, 0);
    let mut eof = false;
    while !stop.load(Ordering::Relaxed) {
        let mut progress = false;
        if up_pos == up_len && !eof {
            match local.read(&mut up) {
                Ok(0) => eof = true,
                Ok(n) => { up_pos = 0; up_len = n; progress = true; }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
        if up_pos < up_len {
            match remote.write(&up[up_pos..up_len]) {
                Ok(0) => bail!("remote write closed"),
                Ok(n) => { up_pos += n; progress = true; }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
        if down_pos == down_len && !eof {
            match remote.read(&mut down) {
                Ok(0) => eof = true,
                Ok(n) => { down_pos = 0; down_len = n; progress = true; }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
        if down_pos < down_len {
            match local.write(&down[down_pos..down_len]) {
                Ok(0) => bail!("local write closed"),
                Ok(n) => { down_pos += n; progress = true; }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
        if eof && up_pos == up_len && down_pos == down_len { return Ok(()); }
        if !progress { std::thread::sleep(Duration::from_millis(2)); }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relay_preserves_full_duplex_bytes() {
        let remote_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let remote_addr = remote_listener.local_addr().unwrap();
        let echo = std::thread::spawn(move || {
            let (mut socket, _) = remote_listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            socket.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut buf = [0u8; 4096];
            while let Ok(n) = socket.read(&mut buf) {
                if n == 0 { break; }
                socket.write_all(&buf[..n]).unwrap();
            }
        });
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        client.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
        let (local, _) = listener.accept().unwrap();
        local.set_nonblocking(true).unwrap();
        let mut remote = TcpStream::connect(remote_addr).unwrap();
        remote.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || pump(local, &mut remote, &worker_stop));
        for round in 0..32 {
            let sent: Vec<u8> = (0..16384).map(|i| (i + round) as u8).collect();
            client.write_all(&sent).unwrap();
            let mut received = vec![0; sent.len()];
            client.read_exact(&mut received).unwrap();
            assert_eq!(sent, received);
        }
        stop.store(true, Ordering::Release);
        worker.join().unwrap().unwrap();
        drop(client);
        echo.join().unwrap();
    }

    #[test]
    fn selection_budgets_and_advertisement_diagnostics() {
        let defaults = Timeouts::from_settings(&crate::settings::Settings::default());
        assert_eq!(defaults.ble_wait.as_millis(), 1500);
        assert_eq!(defaults.probe.as_millis(), 5000);
        assert_eq!(defaults.tcp_connect.as_millis(), 2000);
        let mut custom = crate::settings::Settings::default();
        custom.lan_ble_wait_ms = 3000;
        custom.lan_tcp_connect_ms = 5000;
        custom.lan_probe_ms = 8000;
        let configured = Timeouts::from_settings(&custom);
        assert_eq!(configured.ble_wait.as_millis(), 3000);
        assert_eq!(configured.tcp_connect.as_millis(), 5000);
        assert_eq!(configured.probe.as_millis(), 8000);
        custom.lan_ble_wait_ms = -1;
        custom.lan_tcp_connect_ms = i32::MAX;
        custom.lan_probe_ms = 500;
        let bounded = Timeouts::from_settings(&custom);
        assert_eq!(bounded.ble_wait.as_millis(), 100);
        assert_eq!(bounded.tcp_connect.as_millis(), 500);
        assert_eq!(bounded.probe.as_millis(), 500);
        assert!(remaining(Instant::now() - Duration::from_millis(1)).is_err());
        let mut fields = HashMap::new();
        assert_eq!(advertisement_issue(&fields), Some("BLE status contains no LAN endpoint (listener not ready or phone build lacks LAN)"));
        fields.insert("lan_port".into(), "0".into());
        assert_eq!(advertisement_issue(&fields), Some("missing or invalid LAN port"));
        fields.insert("lan_port".into(), "12345".into());
        fields.insert("lan_fp".into(), "ab".repeat(32));
        fields.insert("lan_ips".into(), "::1,127.0.0.1".into());
        assert_eq!(advertisement_issue(&fields), Some("no usable advertised IPv4 address"));
    }

    #[test]
    fn discovery_requires_pin_port_and_ipv4() {
        let mut fields = HashMap::from([
            ("lan_port".into(), "12345".into()),
            ("lan_fp".into(), "ab".repeat(32)),
            ("lan_ips".into(), "127.0.0.1,192.168.1.5,::1,0.0.0.0,224.0.0.1".into()),
        ]);
        let candidates = parse_candidates(&fields);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].addr.to_string(), "192.168.1.5:12345");
        assert_eq!(advertisement_issue(&fields), None);
        fields.insert("lan_fp".into(), "bad".into());
        assert!(parse_candidates(&fields).is_empty());
        assert_eq!(advertisement_issue(&fields), Some("missing or invalid LAN certificate fingerprint"));
    }
}
