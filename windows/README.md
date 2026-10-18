# Bridge for Windows

Work in progress. Milestone 1 covers the Bluetooth features (notifications,
clipboard, open-app twins); mirroring comes next.

## Build

Built by GitHub Actions on every push (`.github/workflows/windows.yml`);
download `Bridge-windows` from the workflow run. To build locally you need
Rust and the Visual Studio Build Tools (C++ workload), then `cargo build --release`.

## Test

1. On the phone, tap **Copy** under Ticket.
2. Get that text onto the PC clipboard (message yourself, etc.).
3. Run `Bridge.exe`, right-click the tray icon → **Pair from clipboard**.
4. Accept the Bluetooth pairing on both screens.

Everything it does is in `%APPDATA%\Bridge\Bridge.log` (tray → Open log folder).

## Receive files shared from Android

Use updated builds on **both** devices. With Bridge running on Windows and its
Bluetooth link verified, select files in an Android gallery/file manager, tap
**Share → Share to PC**. With exactly one connected, supported computer, sending
starts immediately; otherwise choose the computer. No mirroring or file-browser
window is needed. Mac peers are listed but currently cannot receive shares.

Android also offers **Open with → Share to PC** for any file type (including
files without a MIME type), using the same sending flow. The sender should grant
read access to its `content://` URI. Legacy `file://` URIs are attempted only for
readable shared-storage files, not app-private paths; no broad storage permission
is requested. HTTP/HTTPS links are not handled as files.

The phone shows a foreground transfer notification and a progress/report page,
with human-readable sizes, current send speed, and per-file/overall percentages.
Speed samples update about every 250 ms and drop to zero when stalled or saving.
You can leave the page; tap the notification to return. Each file is confirmed
only after the PC saves it, and the report counts successful and failed files.
The page can cancel the batch. Windows receives into the actual system Downloads
folder, numbers duplicate names, and removes incomplete temporary files on a
normal interrupted transfer. The PC shows one completion notification per batch;
click it or **Show in folder** to select the saved file in Explorer (for multiple
files, the first successful file is selected). Collision-renamed files use their
actual saved names. Click activation requires Bridge to remain running.
See the PC log for its batch result.

Initial limits: one batch at a time, an online BLE peer is needed to initiate,
no resume/offline queue, and the phone stages the batch in private cache first
(so it needs enough free space). The last report is in memory, not a permanent
history; process death does not resume a batch.

Quick checks: share a photo, multiple files including a duplicate filename and
an empty file, leave the Android page, close a running mirror during transfer,
and interrupt/cancel a transfer. The Android counts should match committed files;
a dropped acknowledgment may be reported as unconfirmed even if the PC saved it.

## Open mirroring from a phone notification

In **Settings → Notifications**, enable **Open mirroring when a phone notification
is clicked** (off by default). Clicking the body of a mirrored phone toast starts
mirroring, or restores the existing mirror window. Repeated clicks while connecting
do not start another tunnel. The setting is checked at click time, including for
notifications that were already shown before the switch changed.

Replying and dismissing keep their existing behavior. File-receipt notifications
still open Explorer. This starts the screen view only; it does not automatically
open the specific notification's app on Android. Bridge must still be running.

## Paste a PC screenshot into the mirrored phone (experimental)

Update both apps. Take a Windows screenshot **to the clipboard**, focus the
Android chat input in the mirror window, and press **Ctrl+V**. Bridge uploads the
clipboard bitmap as PNG, sets a grantable image URI on Android's clipboard, and
only after the phone acknowledges it requests Android's Paste action. This does
not press Send, switch apps, or launch a share sheet. If there is no bitmap, the
normal text Ctrl+V path is retained. Image-file copies from Explorer are not
specifically supported by this prototype.

The Android chat app must support image URI clipboard paste and the Android
Paste key action. If no preview appears, try the input's long-press **Paste**:
that distinguishes a keyboard-action issue from image clipboard incompatibility.
A successful upload does not prove that the chat app accepted the image.

Uploads only start from the mirror's Ctrl+V, not from background clipboard
watching. Closing/replacing the mirror, changing the Windows clipboard, or moving
focus to another PC window cancels the automatic paste. Wait for upload before
changing the Android input target. Errors show a toast and never paste stale
content. Limits: 40 megapixels, 32 MB encoded PNG, 256 MB phone image cache; images
are retained for delayed app reads and cleaned after 24 hours on a later upload.
The URI provider is read-only and non-exported, with Android clipboard URI grants.

## LAN connection timeouts

**Settings → Phone → LAN connection timeouts** controls new connections:

- BLE address wait: **1.5 s** by default (skipped when a fresh endpoint is cached).
- TCP connect limit: **2 s** per candidate, capped by the remaining probe budget.
- Parallel LAN probe budget: **5 s**, shared by TCP, TLS and AUTH/STATUS.

Thus initial LAN selection can take up to about **6.5 s** before iroh fallback,
not including iroh startup. A successful probe returns immediately. All candidates
are probed in parallel, so the budget is not multiplied by the number of IPs.
The settings are persisted in `settings.json`; old configurations use the new
defaults. Active streams are not interrupted. Changing settings does not move an
existing iroh tunnel to LAN: reconnect to try LAN again.

The log records configured/effective budgets and candidate failures, plus the
reason for iroh fallback. A TCP timeout happens before TLS and may indicate Wi-Fi
wake-up, packet loss, isolation, or an unreachable endpoint; increasing a timeout
cannot fix a permanently blocked route. The phone's own TLS authentication timeout
is unchanged.
