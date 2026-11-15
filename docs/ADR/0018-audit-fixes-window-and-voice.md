# ADR 0018 — Opaque windows, audio-class scheduling and adaptive voice resilience

Status: accepted (2026-11-15, after the owner-requested audit). Spec §4 (dynamic
resilience, continuity before quality), §3 (deadlines on dedicated threads), §9 (UI).
Amends ADR 0002 (window creation) and ADR 0006 (voice defaults, jitter buffer).

## Context

The audit of 2026-11-15 found, by reading the code and its dependencies:

1. **Windows could turn see-through.** Slint's winit backend creates every window with
   `with_transparent(true)` except on macOS; winit then enables DWM blur-behind with an
   empty region, so the compositor blends the window with the desktop by the alpha of its
   surface. Our default renderer is the software one (ADR 0002), which presents through
   GDI and repaints only changed regions. Whenever the surface is not fully painted
   (sleep / wake, display change, compositor restart, full screen) the window shows
   through and nothing repaints it. Slint itself switches transparency off for windows
   with a menu bar because of translucency in full screen. Our attribute hook was only
   installed for trial flags, and a window hidden to the tray is re-created by Slint
   without the hook.
2. **Voice threads ran at normal priority.** The engine, the pump and the call thread
   lose the CPU when a game saturates every core; 40 ms of queued audio does not survive
   that.
3. **The encoder never followed the network.** 32 kbit/s, expected loss fixed at 5 %;
   `AudioEngine::set_encoder` existed but nothing called it (spec §4 MUST).
4. **Delay correction cut the sound.** Excess delay was removed by discarding a whole
   20 ms frame, also a real one next to a hole; the mix of several voices was clipped.

## Decision

1. Windows are created opaque (attribute hook always installed) and blur-behind is
   switched off on the live window after every show (`apps/desktop/src/opaque.rs`). The
   window repaints in full after a show, on focus, and when the tray's top-level window
   sees a display change, a resume from sleep, a compositor change or a restarted shell.
   Closing to the tray shows a one-time notification.
2. `media_audio::rt::boost_current_thread` registers the voice threads with MMCSS ("Pro
   Audio"); the engine reports whether it was granted. `CallConfig::thread_init` lets the
   client do the same for the call thread without a platform dependency in the transport.
3. Defaults: Opus 48 kbit/s, expected loss 10 %. `LossAdapter` raises the expected loss
   (up to 30 %, in steps of 5) and the bitrate (+25 % / +50 %) with the measured loss, at
   once upward and slowly downward. Input: the worse of our uplink loss as the server
   reports it and our downlink loss — the sender cannot see its listeners' paths; a
   per-listener report through the SFU would be the exact signal and is left open.
4. The jitter buffer removes excess delay by skipping a hole, else by handing out two
   consecutive frames for one tick (`Playout::Merge`); the receiver decodes both, leaves
   a silent first frame out or fades from the first into the second. A real frame is no
   longer discarded while its successor is missing. The mix goes through a limiter
   (−1 dBFS ceiling, 5 ms attack, ≈ 300 ms release).
5. A share whose pipeline stops on its own is started again (3 attempts per minute).

## Consequences

* Voice costs ≈ 16 kbit/s more per member; with heavy loss up to 72 kbit/s.
* A blended tick plays 40 ms of sound in 20 ms: audible as a brief blur, not as a click.
  A time-stretcher (WSOLA) would be better still and is left open.
* Not verified on screen or by ear in this session (no interactive GUI or audio runs
  while the owner used the machine): the see-through window was not reproduced before
  the change, and the fix is checked by code and unit tests only. Owner-assisted checks
  are listed in `PROJECT_STATE.md`.

## Alternatives not taken

* GPU renderer by default: fails the idle RAM target (ADR 0002) and does not remove the
  transparent window attribute.
* Keeping the hidden window alive instead of letting Slint re-create it: Slint's own
  visibility state (used by the tray trials) would no longer match the native window.
* Audio NACK/RTX or RED: more delay or a new payload format; in-band FEC with an adaptive
  expected loss covers single losses without either.
