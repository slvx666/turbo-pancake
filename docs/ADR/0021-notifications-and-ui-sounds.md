# ADR 0021 — Notifications through the tray icon; UI sounds synthesized, played with PlaySound

Status: accepted (2026-11-20). Spec §9 (tray, background work). Windows scope per ADR 0012.

## Context

The client needs the cues of a finished messenger: a sound on a new message, a ringtone
for an incoming call, call join/leave/mute sounds, desktop notifications and an unread
badge. Constraints: idle RAM target ≤ 50 MiB, no heavy dependencies, no message content in
logs, and trials that run silently on an invisible desktop.

## Decision

* **Notifications: the tray icon's notifications** (`Shell_NotifyIconW` with `NIF_INFO`,
  `NIIF_USER | NIIF_LARGE_ICON`, `NIIF_NOSOUND`, `NIIF_RESPECT_QUIET_TIME`). Windows 10/11
  show them as toasts, respect Focus Assist, and the click arrives at our existing tray
  window (`NIN_BALLOONUSERCLICK` → `TrayEvent::NotificationClicked(tag)`). Rejected for
  now: WinRT toasts (`ToastNotificationManager`) — they need a registered AppUserModelID
  with a Start-menu shortcut and a COM activator for clicks after the app exits, plus a
  WinRT binding crate; they would give action buttons ("Принять"/"Отклонить"). Revisit
  if those are wanted.
* **Sounds: synthesized at first use, played with `PlaySoundW(SND_MEMORY | SND_ASYNC)`**
  from one worker thread. No sound files, no audio library; a few hundred kilobytes of
  cached PCM at most. Rejected: mixing cues into the call's audio engine (would need the
  engine running outside calls and a second mixer path) and clip-player crates
  (rodio/cpal streams: a constant output stream and more RAM).
* **Policy separate from OS calls** (`alerts.rs`, trait `Sink`): tested with a recording
  sink; trials use a logging sink (silent mode: `--no-activate` or
  `TURBO_PANCAKE_SILENT=1`) that prints `TURBO_PANCAKE_ALERT` lines without message text.
* **Badge**: `ITaskbarList3::SetOverlayIcon` with an icon drawn at run time (GDI), set on
  the UI thread inside winit's COM apartment.

## Consequences

* Cues play on the default output device, not on the call's chosen device; one cue at a
  time (the ringtone is never cut; other cues are dropped while it loops).
* No action buttons on notifications; answering a call needs the window.
* Our process stays the only player, so screen-share system-audio capture (which excludes
  our process tree) never sends the cues to viewers.
