# IPv4 LAN transport

Android exposes an ephemeral IPv4 TLS listener while Bridge's foreground service is running. Its IPv4 addresses, port and SHA-256 certificate fingerprint are included in the existing authenticated BLE status (`lan_ips`, `lan_port`, `lan_fp`). The loopback ControlProxy stays private.

Windows probes at most eight addresses concurrently (400 ms TCP connect timeout, 800 ms socket handshake/probe timeout, 1.6 s selection budget). Without an advertisement it allows 300 ms for a requested BLE status. A successful pinned-TLS `AUTH` + `STATUS` probe selects LAN; otherwise it starts the existing dumbpipe/iroh transport. The selected route is fixed until disconnect. There is no mid-session failover or transfer replay; reconnect after a network change.

The regular loopback Stream API remains unchanged, including cloned read handles and all file/video/audio/control protocols. LAN sessions do not start dumbpipe or wake the phone's iroh tunnel. A Windows local byte-stream adapter keeps TLS out of scrcpy's control-reader/control-writer locking. Files and mirror titles show `LAN` or `iroh` (and Connecting/Disconnected outside a session). Incoming phone shares use their own independent LAN-first adapters, so they can run without a mirror or survive closing one. BLE carries the single-use share offer; file bytes use LAN, with iroh fallback. Updated Windows clients advertise `lan-share on`; only an iroh fallback requests the phone's tunnel over BLE. Older clients retain the original phone-side automatic iroh wake.

## Clipboard

Text clipboard behavior during mirroring is preserved from git: Windows sends text over the scrcpy control stream and applies incoming scrcpy clipboard messages. This follows the session's LAN/iroh route. Outside mirroring, text synchronization uses BLE and has no screen-state admission check; it never opens a separate LAN text connection.

Outside mirroring, Windows-to-phone images prefer independently opened pinned LAN streams when a recent BLE advertisement is available; they do not require the daemon or a files window. If LAN is unavailable before admission, images may fall back to BLE.

Images use `IMGB <PNG byte length>` -> `READY` -> PNG -> `OK`. LAN and mirroring accept PNGs up to **32 MiB / 40 megapixels**. Only the BLE fallback is limited to **1 MiB**. If LAN is unavailable for an image larger than 1 MiB, a toast explains that the image cannot be synchronized over BLE.

Background images require the phone screen to be interactive at admission; a dark screen returns an error before any image bytes are sent. Turning the screen off after READY does not cancel. Windows clipboard sequence changes cancel both LAN and BLE image uploads, including copying text over an image. LAN cancellation closes the stream; incomplete files are deleted and never published. ACK waits poll cancellation every 200 ms; individual LAN writes have a 1 s timeout. As with any cross-device clipboard, changes racing with final publication cannot be made atomic; the following update supersedes the previous image.

LAN admission/publication errors are not automatically retried over BLE after admission, avoiding duplicate or stale publication. No background image auto-retry occurs merely because the screen wakes. Copy again to retry. Existing mirror Ctrl+V still uses IMGC and its paste-key behavior.

## Security and compatibility

TLS is mandatory. Windows verifies the exact BLE-advertised certificate fingerprint **before sending AUTH or any application bytes**; no CA or hostname trust is used. Android checks pairing AUTH before opening its private proxy. No cleartext LAN fallback exists. Discovery is scoped to the pairing secret and expires after 90 seconds. New connections re-check the pin. Old clients/phones ignore unknown status keys and retain the existing iroh path.

## Device verification checklist (no build performed during implementation)

- Same Wi-Fi + verified BLE: open Phone files and mirror; both titles show LAN. Browse, upload/download, check video/audio and input, including simultaneous clipboard/control reads and writes.
- Phone's iroh tunnel off: LAN still connects without enabling it.
- Multiple interfaces: an unreachable address must not delay a reachable candidate serially.
- AP client isolation/different networks/no LAN fields: route falls back to iroh, waking the phone tunnel over BLE if needed; titles show iroh.
- Wrong certificate fingerprint: reject LAN before AUTH; never send plaintext pairing credentials.
- Break Wi-Fi during a session: report failure; reconnect chooses a fresh route, without replaying a partial upload.
- Close/disconnect during discovery or a session: listener, relays and any dumbpipe child terminate.
- Multiline Unicode text: outside mirroring uses BLE even with the screen off; during mirroring retains scrcpy session synchronization, including with BLE disconnected.
- Phone-to-PC sharing on the same LAN: BLE offer then LAN file bytes, no new iroh tunnel; closing a mirror must not stop the independent share.
- Phone-to-PC sharing with LAN unreachable: BLE offer, iroh fallback and BLE tunnel wake; no replay once the single-use SHAR capability has been consumed.
- Older Windows client with updated phone: automatic iroh wake remains available.
- Background LAN image over 1 MiB but at most 32 MiB: accepted. Same image with no LAN: rejected rather than sent over BLE. BLE images at most 1 MiB still work.
- PNG above 32 MiB or above 40 MP: rejected for LAN and mirror as well.
- Screen off before begin: no image bytes sent. Screen off during upload: transfer can complete.
- Copy new text or another image during upload or admission wait: old upload cancels and latest clipboard wins. Include re-copying text that was copied before the image.
- Older phone/client: original iroh path remains usable.

Rust unit tests cover candidate validation and full-duplex byte forwarding; they have not been run because builds were explicitly disabled.
