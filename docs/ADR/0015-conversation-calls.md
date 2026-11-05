# ADR 0015 — Conversation calls and account ownership

* Status: accepted (S6, Windows; 2026-11-05)

## Decision

Calls are live server-owned sessions, separate from durable conversation/message data.
The single application server keeps their bounded state in memory, like presence and
the SFU. Restart ends all calls; clients refetch snapshots and cannot resurrect an old
call id. No media session can survive a server restart, and no call-history feature is
required. PostgreSQL remains authoritative for account sessions, blocks and membership.

Authenticated HTTP create/join/leave/decline and snapshots precede media attachment.
Each participation has a fresh UUID bound to an account session. A call revision orders
typed started/joined/left/ended events; reconnect refetches the active-call list.
Leaving requires the matching participation UUID and owning session, so an old device
cannot end a new device's call. A join replaces any previous account participation,
including on another device, before accepting media. Direct calls check blocks and
report busy; accepting a group call may switch the user's current participation.

A single async control lock serializes lifecycle operations, membership removal and
direct blocking. Media never performs database queries; attachment validates the lease
under this lock. Eviction removes the hub peer and SFU resources before notifying the
old device. Anonymous rooms remain an explicit development path, isolated from the
reserved `call_` namespace and unavailable to authenticated accounts.

Unattached participations and unanswered direct invitations expire after 30 seconds.
A disconnected attached peer has a 10-second reconnect grace with the same lease;
revoked sessions and removed members lose participation. An empty call ends. The
server periodically checks session validity and expires abandoned reservations, so
HTTP success without a media connection cannot leave an indefinitely active call.

The native client sends the same account token to its media Session. Chat selection is
independent of the ongoing call; moving between chat and call keeps per-chat drafts.
Device/audio failures stop media and leave the participation, and remain visible.
