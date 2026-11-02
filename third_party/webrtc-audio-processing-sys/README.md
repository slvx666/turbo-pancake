# webrtc-audio-processing-sys 2.1.0 (vendored, patched)

`crate/` is the crates.io package `webrtc-audio-processing-sys` 2.1.0 (BSD-3-Clause, see
`crate/COPYING`; bundles PulseAudio's repackaging of WebRTC AudioProcessing) with
`2.1.0-windows.patch` applied. It is used through `[patch.crates-io]` in the workspace
`Cargo.toml` when `turbo-pancake-media-audio` is built with the `apm` feature.

Why vendored: the upstream bundled build does not work on Windows/MSVC (ADR 0006):

1. `aec3/matched_filter_avx2.cc` uses the MSVC-only `m128_f32` member under clang-cl;
2. the C++ wrapper is compiled with the GNU flag `-std=c++17` (ignored by clang-cl);
3. objcopy symbol prefixing breaks the MSVC link (the objects carry `/EXPORT`
   directives), so prefixing is skipped on MSVC.

Abseil is fetched by meson from the wrap file (`subprojects/abseil-cpp.wrap`, SHA-256
pinned). To update: copy the new crate from `~/.cargo/registry/src/*/`, re-apply the patch,
rebuild with `--features apm` on all three OS (CI job `apm`).
