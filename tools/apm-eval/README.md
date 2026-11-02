# apm-eval — WebRTC AudioProcessing (APM) evaluation

Measures, at 48 kHz mono on 10 ms frames with synthetic speech (`testsignal::speech_like`):
CPU per frame (NS; AEC3 + NS + HPF; + AGC2, median of 5 runs over 20 s), single-talk
echo reduction (40 ms / −6 dB echo path with a second reflection), double-talk near-end
level, noise-floor reduction at ≈ 10 dB SNR. Prints JSON lines. Results: ADR 0006.

## Linux / macOS

`meson` and `ninja` on `PATH` (abseil is fetched by meson), then
`cargo run --release` in this directory.

## Windows (MSVC)

The bundled build of `webrtc-audio-processing-sys` 2.1.0 does not work out of the box:

* MSVC `cl` cannot compile WebRTC's `rtc_base/checks.h` templates (and needs C++20 for
  designated initializers) → build with **clang-cl** (`CC=clang-cl`, `CXX=clang-cl`,
  shipped with VS 2022 under `VC\Tools\Llvm\x64\bin`).
* Under clang-cl, `aec3/matched_filter_avx2.cc` picks the MSVC-only `m128_f32` branch;
  the wrapper is compiled with the GNU flag `-std=c++17` (ignored by clang-cl); symbol
  prefixing with objcopy breaks the link (the objects carry `/EXPORT` directives).
  All three are fixed by `third_party/webrtc-audio-processing-sys/2.1.0-windows.patch`.
* Tools: `meson`, `ninja`, `nm` (copy `llvm-nm.exe` as `nm.exe`), `cp` (Git
  `usr\bin`), `rust-objcopy` (`rustup component add llvm-tools`), and the VS environment
  (`vcvars64.bat`) because the build script runs `ninja` directly.

```bat
rem patched copy of the crate (outside the repo)
xcopy /e /i %USERPROFILE%\.cargo\registry\src\index.crates.io-*\webrtc-audio-processing-sys-2.1.0 D:\wap-sys
cd /d D:\wap-sys && patch -p1 < <repo>\third_party\webrtc-audio-processing-sys\2.1.0-windows.patch
call "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
set CC=clang-cl
set CXX=clang-cl
cd <repo>\tools\apm-eval
cargo run --release --config "patch.crates-io.webrtc-audio-processing-sys.path='D:/wap-sys'"
```
