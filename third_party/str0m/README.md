# str0m 0.24.1 (vendored, patched)

`crate/` is the crates.io package `str0m` 0.24.1 (MIT OR Apache-2.0; `crate/LICENSE-MIT.txt`)
without its tests, examples and documentation assets, with one change. It is used through
`[patch.crates-io]` in the workspace `Cargo.toml`.

## The change

`src/streams/register_nack.rs`: `MAX_MISORDER` 100 → 1024.

The receive register NACKs missing packets and accepts repairs only within this many
sequence numbers of the newest packet. A 1440p120 share at 20 Mbit/s is about 2 300 packets
a second, so 100 packets are 43 ms — less than one NACK round (33 ms) plus a real round
trip. A repaired packet then arrives outside the window, is dropped, and the viewer waits
for a keyframe (PROJECT_STATE known issues 31 and 49). 1 024 packets are about 450 ms at
that rate and match the sender's retransmission buffer (`send_buffer_video`, 1 000).

The constant is not configurable upstream; if a later str0m makes it a setting, drop this
copy and set it in `RtcConfig`.

## Updating

Copy `src/`, `Cargo.toml`, the license and `README.md` / `CHANGELOG.md` of the new version
from `~/.cargo/registry/src/*/str0m-<version>/`, remove the `[[example]]` / `[[test]]`
tables from `Cargo.toml`, re-apply the change, run the media tests
(`cargo test -p turbo-pancake-media-transport -p turbo-pancake-media-sfu`).
