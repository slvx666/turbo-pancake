# S7 step 4 (reconnect UX) — Windows, 2026-11-06

Branch: `codex/windows-environment` (local only). Release client and server, local
dev/APM build. No input injection, desktop switching, focus acquisition, screen capture
or real audio: every window is on an isolated invisible desktop and snapshots are the
renderer's own pixels.

## What changed

* One connection banner over the content (chat, empty view, sign-in and call view):
  red "Нет связи с сервером, повтор через N с. Переписка доступна из кэша, введённый
  текст сохранён" with a live countdown; amber "Переподключение к серверу…" and
  "Звонок: связь потеряна, восстановление…" / "Звонок: переподключение…"; green
  "Связь с сервером восстановлена" / "Звонок восстановлен" for 4 s. Both links are
  joined in one line, the worse state colours it. Pure state machine in
  `apps/desktop/src/banner.rs`, UI glue in `connection_ui.rs` (timer only while a
  countdown or the restored notice is shown).
* Peers' presence (sidebar dots, chat header) is shown as offline while our server link
  is down; the cached "в сети" would have been a stale claim.
* Unsent input: message field and room field were already kept (nothing resets them on
  status changes; drafts are saved by the S5/S6 draft path); now verified natively.

## Trial (`benchmarks/scripts/s7-reconnect-trial.ps1`)

Real TLS/WSS server and PostgreSQL, three release clients (registered in a first run,
then started from their stored sessions). Alice: direct chat open, one sent message,
unsent text typed into the message and room fields. Bob and Cara: a direct call with
simulated audio. The server process is killed, stays down 8 s, and is started again on
the same port and database.

| Check | Software | OpenGL (femtovg) |
|---|---|---|
| Offline banner with countdown on all three | PASS | PASS |
| Call link shown as reconnecting (Bob, Cara) | PASS | PASS |
| No banner before the outage (joining a call is not a loss) | PASS | PASS |
| Restored banner on all three after the restart | PASS (123 ms after start) | PASS (240 ms) |
| Unsent text kept (`input_kept=true` on all 8 banner states) | PASS | PASS |
| History unchanged, no duplicates (`messages=1`) | PASS | PASS |
| Own-window snapshot per banner state, clean exit 0, native window | PASS | PASS |
| Foreground samples of test windows | 0 | 0 |

After the restart the live call is gone, as specified by ADR 0015 (restart ends live
calls), so Bob and Cara end with the messenger "restored" and no call banner.

## Defects found and fixed in this step

* First native run: Bob and Cara showed "Звонок: связь потеряна" at the start of every
  call. Joining reuses the media session whose earlier server link was down (before the
  account join), which looked like a loss. A call link drop now counts as a loss only
  after that call's media was connected once (`banner::tests`).
* Visual review: during the outage the peer was still shown "в сети" (cached presence).
  Presence now follows the server link.

## Not covered

* "Звонок восстановлен" after a media-only drop with the server staying up: unit-tested
  and covered by the existing same-lease reconnect session test, not by a native run (a
  network cut without a restart needs a proxy or a firewall rule on the owner's machine).
* Real network loss (Wi-Fi off, sleep/wake): owner-assisted.
* Snapshots are not committed (they show local account names); they stay in the ignored
  build directory.
