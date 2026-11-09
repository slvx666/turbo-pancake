# Release build 0.1.2

Date: 2026-11-09. Script: `deploy/release.ps1`. Commit `476a9f8`; rustc 1.99.1 (58cf8f9c3 2026-10-13).

| File | Bytes | SHA-256 |
|---|---|---|
| `release.json` | 2054 | `1b8fa71af7bd41d404909282d475a075d88cb57348ef79986b2fac5b8060e8ba` |
| `server/install-server.sh` | 7836 | `8fff6c1489931334c41a45f0d910113de80dca2ca6814e308bbbde57d1459447` |
| `server/src.tar.gz` | 1725299 | `3007691842b02a43d7622be48120101dc35226cb6ce5795f61cdcbc44b332c46` |
| `tools/turbo-pancake-update.exe` | 2099712 | `8ddc85b365019e840d6928f4c056cff99e9e494fb44cd05dbcd4af745f4f0612` |
| `update/files/0.1.2/turbo-pancake-desktop.exe` | 24691200 | `05692f48dc08a4ba772ca9bc057fea5e96f86e6dcdefb02579615ee1734a9286` |
| `update/manifest.json` | 657 | `aaf8f2ec15d5a638e25ce4ec8aa44d7364e1cc79ad99a114624d8040f3283faa` |
| `windows/client/turbo-pancake.exe` | 388608 | `649353e3f5733187f87694c7988c2a673ba16f1a5f1adaef9c625cc45ab6d13b` |
| `windows/client/turbo-pancake-desktop.exe` | 24691200 | `05692f48dc08a4ba772ca9bc057fea5e96f86e6dcdefb02579615ee1734a9286` |
| `windows/turbo-pancake-setup-0.1.2.exe` | 13904384 | `2a54230b9a093a34d72aaf386ec204f566daaee7e862e1bb1b9260fa11efb19f` |

* Installer: 13.3 MiB (limit 30 MB).
* Default server built in: False; private CA built in: False; update keys pinned: 1; signed update: True.
* Checksums: 9 files match (`release.ps1 -Verify`).
* Build time: 12 s.
