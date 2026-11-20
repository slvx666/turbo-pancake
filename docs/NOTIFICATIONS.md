# Notifications and UI sounds

Client modules: `apps/desktop/src/alerts.rs` (policy, sinks, silent mode),
`sounds.rs` (synthesis, player thread), `taskbar.rs` (badge, flash, focus), `tray.rs`
(notifications, click). Decision record: [ADR 0021](ADR/0021-notifications-and-ui-sounds.md).

## Policy

"Focused" means that the main window is shown, not minimized and the foreground window
(`GetForegroundWindow`, checked when the alert fires). "Open" means that the chat of that
conversation is on screen (not the call view).

| Event | Focused, conversation open | Focused, other view | Not focused / hidden / minimized |
|---|---|---|---|
| New message from someone else | nothing | sound | sound + notification + taskbar flash |
| Incoming call | ringtone (in-app card) | ringtone | ringtone + notification + flash |

* Messages: only live `message.created` events (`Update::Incoming` of the messenger
  service) by another account that are new to this device. Own messages (also from
  another device of the same account), edits, deletions, receipts and history loaded by
  a resync never alert. The same message id never alerts twice.
* At most one message sound per 700 ms. Notifications within 5 s of each other merge into
  "N новых сообщений" (the click opens the newest conversation); otherwise the title is
  the author (" · group" in groups) and the text is the first 100 characters on one line.
* Incoming call: the ringtone loops until the invitation is no longer pending —
  accepted or declined here (stops at once and never rings again for that call),
  cancelled, ended, timed out or answered on another device (the call list says so). A
  forgotten loop stops by itself after 90 s.
* Our call waiting for answers (state "inviting", started by us): a soft ringback loop.
* Calls: our join / leave (the server confirmed the room; a rejoin after a reconnect is
  silent), others joining / leaving our call, our mute / unmute, our screen share start /
  stop.
* Connection: "lost" when the banner shows offline/reconnecting after the app was online
  once (at most once per 30 s against flapping links), "restored" only after an announced
  loss.
* Unread badge: the sum of unread messages over all conversations on the taskbar button
  (red circle, 1–9 or "9+", hidden at 0) and in the tray tooltip.
* Clicking a notification restores the window and opens the conversation (for a call: the
  conversation of the call).
* Settings (`settings/<profile>.conf`): `notifications`, `sounds`, `sound_volume` (0–100,
  default 70). Files without these keys load the defaults.

## Silent mode

With `--no-activate` (every trial and smoke run) or `TURBO_PANCAKE_SILENT=1` nothing is
played, shown or flashed. Each alert that would fire prints one line on stderr instead:

```
TURBO_PANCAKE_ALERT kind=message conv=<id> actions=sound:message,notify,flash
TURBO_PANCAKE_ALERT kind=incoming_ring conv=<id> actions=loop:incoming_ring,notify,flash
TURBO_PANCAKE_ALERT kind=ring_stop actions=loop:stop
TURBO_PANCAKE_ALERT kind=call_join actions=sound:call_join
TURBO_PANCAKE_ALERT kind=badge unread=2 actions=badge:2
```

No message text or author appears in these lines (spec: no message content in logs).
The S6 trial (`benchmarks/scripts/s6-trial.ps1`) requires `message`, `incoming_ring`,
`ring_stop` and `call_join` lines from real client processes. Unit tests drive the
policy with a recording sink and never call the playback or notification APIs.

## Sounds

Original cues synthesized in Rust at first use (24 kHz mono 16-bit, a few enveloped
tones each; attack and release ramps, so no clicks; peak below -3 dBFS; volume applied at
synthesis, cache rebuilt when it changes). Played with
`PlaySoundW(SND_MEMORY | SND_ASYNC | SND_NODEFAULT)` (`SND_LOOP` for the ringtones) by one
worker thread fed through a channel: callers on the UI, call or messenger threads never
wait.

## Limitations

* UI sounds play on the **default output device**, not on the device chosen for the
  call. `PlaySound` has no device choice; routing cues through the call's audio engine
  would mean mixing them into the voice output path (and keeping that engine running
  outside calls).
* One sound at a time per process (`PlaySound`): while the incoming ringtone loops, other
  cues are dropped; the outgoing ringback is interrupted by a cue and resumes after it.
* Notifications are tray-icon notifications (`Shell_NotifyIconW`, `NIF_INFO` with the
  app's large icon, `NIIF_NOSOUND`, `NIIF_RESPECT_QUIET_TIME`), which Windows 10/11 show as
  toasts and keep in the notification center. There are no action buttons (no
  "Принять"/"Отклонить" on the call toast) and no reply box; a click on an entry left in the
  notification center after the toast was dismissed may not reach the app. Without a tray
  icon (smoke runs, a shell without a notification area) there are no notifications.
* Our own process is excluded from screen-share system-audio capture, so cues never reach
  viewers; they stay in-process for that reason (no helper process).
