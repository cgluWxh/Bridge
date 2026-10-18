# LAN prewarm and business power ownership

Only a verified peer's explicit BLE `status` or `status <request-id>` command
triggers prewarm. Automatic status broadcasts and the 30-second heartbeat do not
acquire locks. Windows now waits for the matching fresh status reply before any
LAN TCP probing; addresses within that reply are still probed in parallel.

## Windows connection order

1. Register a unique 32-hex request ID and send encrypted BLE `status <request-id>`.
2. Android prewarms CPU/Wi-Fi, reads the current listener/IP information, and
   returns a status containing `lan_request=<request-id>`, IPs, port and TLS pin.
3. Windows accepts only the matching authenticated reply for the paired phone.
   Heartbeats, cached endpoints and other selections' replies cannot satisfy it.
   The reply is delivered directly in the BLE callback, independently of the UI
   event queue and image outbox worker.
4. Probe only those newly returned addresses in parallel: TCP, pinned TLS, and
   authenticated STATUS. First successful candidate wins; active business streams
   subsequently connect to the selected endpoint and take over the power locks.
5. Missing/invalid replies or failed LAN probes fall back to iroh for tunnels;
   clipboard images use their existing BLE fallback/size limit instead.

Default budgets are 1500 ms for the fresh BLE reply, 2000 ms TCP per candidate,
and 5000 ms total parallel probing after the reply. These are maximum waits,
not fixed delays. Update both Windows and Android: an older phone that does not
echo request IDs cannot pass this barrier and will use the fallback.

- Prewarm: partial CPU wake lock and Wi-Fi HIGH_PERF hint for a 10-second lease.
  Another explicit request refreshes that lease instead of stacking references.
- Business takeover: authenticated screen/file/image streams acquire separate CPU
  and Wi-Fi locks before releasing prewarm. The shared open-stream count controls
  ownership, so one stream ending cannot release another stream's locks.
- Last business stream ending releases business locks. No automatic trailing
  prewarm is created. Requests during business do not create a trailing lease.
- Service shutdown cancels prewarm callbacks and releases both sets of locks.
- TCP/TLS acceptance alone and AUTH/STATUS probes are not business ownership.
- Existing iroh tunnel wake-lock behavior is unchanged; these locks cover direct
  LAN preparation and active business independently of whether iroh is running.

These are CPU/network power hints, not screen wake/unlock, and do not bypass
Doze or vendor background restrictions. HIGH_PERF was chosen over LOW_LATENCY
because Android 10's low-latency mode has foreground/screen-state conditions.

## Device checks

1. Keep Bridge running in Nearby with the phone locked and no active session.
2. Connect from Windows. Look for `LAN prewarm: explicit BLE status` in the phone
   log, then compare the Windows TCP/LAN result with previous runs.
3. Probe without starting business: expect `LAN prewarm expired` after about 10 s.
4. Start a mirror or file/image transfer: expect `business takeover`, with no
   prewarm expiration releasing those business locks mid-transfer.
5. Keep two streams active, close one, then the last: release is logged only
   after the last business stream ends.
6. Leave only automatic BLE heartbeats running: no new prewarm log should appear.
7. Stop Bridge during prewarm or transfer: all service-owned locks are released.

Build and real-device power/network behavior still need CI/device verification.
