//! The signed release manifest: format, limits, Ed25519 signing and verification.

use std::fmt;

use aws_lc_rs::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};

pub const FORMAT: u32 = 1;
pub const MAX_ENVELOPE_BYTES: usize = 64 * 1024;
pub const MAX_FILES: usize = 32;
pub const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_PATH_LEN: usize = 128;
/// Tolerated clock difference for `published_at_ms` in the future.
pub const CLOCK_SKEW_MS: i64 = 10 * 60 * 1000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UpdateError {
    #[error("manifest too large")]
    TooLarge,
    #[error("malformed manifest: {0}")]
    Malformed(String),
    #[error("unknown signing key {0}")]
    UnknownKey(String),
    #[error("bad signature")]
    BadSignature,
    #[error("manifest for another product or platform")]
    WrongTarget,
    #[error("manifest expired")]
    Expired,
    #[error("manifest published in the future")]
    FromFuture,
    #[error("version {0} is not newer than the installed one")]
    NotNewer(String),
    #[error("version {0} failed before and is skipped")]
    KnownBad(String),
    #[error("invalid file list: {0}")]
    BadFiles(String),
    #[error("download: {0}")]
    Download(String),
    #[error("{0}: size differs from the manifest")]
    SizeMismatch(String),
    #[error("{0}: SHA-256 differs from the manifest")]
    HashMismatch(String),
    #[error("install: {0}")]
    Io(String),
    #[error("no update keys in this build")]
    NoKeys,
}

impl From<std::io::Error> for UpdateError {
    fn from(e: std::io::Error) -> Self {
        UpdateError::Io(e.to_string())
    }
}

/// `major.minor.patch`, numeric only (no pre-release tags in release manifests).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    pub fn parse(s: &str) -> Option<Self> {
        let mut it = s.split('.');
        let mut part = || -> Option<u32> {
            let p = it.next()?;
            if p.is_empty() || p.len() > 9 || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            if p.len() > 1 && p.starts_with('0') {
                return None;
            }
            p.parse().ok()
        };
        let v = Version(part()?, part()?, part()?);
        it.next().is_none().then_some(v)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    /// Lower-case hex.
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    pub product: String,
    pub platform: String,
    pub version: String,
    pub published_at_ms: i64,
    pub expires_at_ms: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    pub files: Vec<FileEntry>,
}

impl Manifest {
    pub fn parsed_version(&self) -> Option<Version> {
        Version::parse(&self.version)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub key_id: String,
    /// Base64 of the exact signed manifest bytes.
    pub manifest: String,
    /// Base64 Ed25519 signature over those bytes.
    pub signature: String,
}

/// A pinned public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedKey {
    pub id: String,
    pub public: Vec<u8>,
}

impl TrustedKey {
    /// `"<id>:<base64 32-byte key>"`, several separated by commas (build-time list).
    pub fn parse_list(s: &str) -> Result<Vec<TrustedKey>, String> {
        s.split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (id, key) = p
                    .split_once(':')
                    .ok_or_else(|| format!("key without id: {p}"))?;
                let public = B64.decode(key.trim()).map_err(|e| e.to_string())?;
                if public.len() != 32 || !valid_key_id(id) {
                    return Err(format!("invalid key {id}"));
                }
                Ok(TrustedKey {
                    id: id.to_owned(),
                    public,
                })
            })
            .collect()
    }
}

fn valid_key_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A new Ed25519 key: (PKCS#8 private key, raw 32-byte public key).
pub fn generate_key() -> Result<(Vec<u8>, Vec<u8>), UpdateError> {
    let rng = aws_lc_rs::rand::SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|_| UpdateError::Io("key generation failed".into()))?;
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
        .map_err(|_| UpdateError::Io("key generation failed".into()))?;
    Ok((pkcs8.as_ref().to_vec(), pair.public_key().as_ref().to_vec()))
}

/// Signs a manifest into envelope bytes (release tool, tests).
pub fn sign(manifest: &Manifest, pkcs8: &[u8], key_id: &str) -> Result<Vec<u8>, UpdateError> {
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8)
        .map_err(|_| UpdateError::Malformed("private key is not Ed25519 PKCS#8".into()))?;
    let bytes =
        serde_json::to_vec_pretty(manifest).map_err(|e| UpdateError::Malformed(e.to_string()))?;
    let sig = pair.sign(&bytes);
    sign_raw(&bytes, sig.as_ref(), key_id)
}

/// The envelope for given manifest bytes and signature (tests build forged ones too).
pub fn sign_raw(bytes: &[u8], sig: &[u8], key_id: &str) -> Result<Vec<u8>, UpdateError> {
    serde_json::to_vec_pretty(&Envelope {
        key_id: key_id.to_owned(),
        manifest: B64.encode(bytes),
        signature: B64.encode(sig),
    })
    .map_err(|e| UpdateError::Malformed(e.to_string()))
}

/// What the running client is and what it must refuse.
pub struct Expect<'a> {
    pub product: &'a str,
    pub platform: &'a str,
    pub current: Version,
    pub bad: &'a [String],
    pub now_ms: i64,
}

/// Verifies an envelope fully (signature first, then content and policy).
pub fn verify(
    envelope: &[u8],
    keys: &[TrustedKey],
    expect: &Expect,
) -> Result<Manifest, UpdateError> {
    if keys.is_empty() {
        return Err(UpdateError::NoKeys);
    }
    if envelope.len() > MAX_ENVELOPE_BYTES {
        return Err(UpdateError::TooLarge);
    }
    let env: Envelope =
        serde_json::from_slice(envelope).map_err(|e| UpdateError::Malformed(e.to_string()))?;
    let key = keys
        .iter()
        .find(|k| k.id == env.key_id)
        .ok_or_else(|| UpdateError::UnknownKey(env.key_id.clone()))?;
    let bytes = B64
        .decode(env.manifest.as_bytes())
        .map_err(|e| UpdateError::Malformed(e.to_string()))?;
    let sig = B64
        .decode(env.signature.as_bytes())
        .map_err(|_| UpdateError::BadSignature)?;
    UnparsedPublicKey::new(&ED25519, &key.public)
        .verify(&bytes, &sig)
        .map_err(|_| UpdateError::BadSignature)?;
    let m: Manifest =
        serde_json::from_slice(&bytes).map_err(|e| UpdateError::Malformed(e.to_string()))?;
    if m.format != FORMAT {
        return Err(UpdateError::Malformed(format!("format {}", m.format)));
    }
    if m.product != expect.product || m.platform != expect.platform {
        return Err(UpdateError::WrongTarget);
    }
    if m.expires_at_ms <= expect.now_ms {
        return Err(UpdateError::Expired);
    }
    if m.published_at_ms > expect.now_ms + CLOCK_SKEW_MS {
        return Err(UpdateError::FromFuture);
    }
    let version = m
        .parsed_version()
        .ok_or_else(|| UpdateError::Malformed(format!("version {}", m.version)))?;
    if version <= expect.current {
        return Err(UpdateError::NotNewer(m.version.clone()));
    }
    if expect.bad.iter().any(|b| b == &m.version) {
        return Err(UpdateError::KnownBad(m.version.clone()));
    }
    check_files(&m.files)?;
    Ok(m)
}

/// Relative path of `[A-Za-z0-9._-]` segments joined by `/`; no `.`/`..` segments, no
/// Windows device names (`CON`, `NUL.txt`, `COM1`…), no trailing dot.
pub fn valid_path(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= MAX_PATH_LEN
        && p.split('/').all(|seg| {
            let stem = seg.split('.').next().unwrap_or("").to_ascii_uppercase();
            let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                || (stem.len() == 4
                    && (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && stem.as_bytes()[3].is_ascii_digit());
            !seg.is_empty()
                && !seg.ends_with('.')
                && !device
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
}

fn check_files(files: &[FileEntry]) -> Result<(), UpdateError> {
    let bad = |s: &str| Err(UpdateError::BadFiles(s.to_owned()));
    if files.is_empty() || files.len() > MAX_FILES {
        return bad("file count");
    }
    let mut total: u64 = 0;
    let mut seen = std::collections::HashSet::new();
    for f in files {
        if !valid_path(&f.path) {
            return bad(&format!("path {}", f.path));
        }
        // Windows paths are case-insensitive.
        if !seen.insert(f.path.to_ascii_lowercase()) {
            return bad(&format!("duplicate {}", f.path));
        }
        if f.sha256.len() != 64
            || !f
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return bad(&format!("sha256 of {}", f.path));
        }
        total = total.saturating_add(f.size);
    }
    if total > MAX_TOTAL_BYTES {
        return bad("total size");
    }
    if !files.iter().any(|f| f.path == crate::CLIENT_EXE) {
        return bad("client executable missing");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    fn manifest(version: &str) -> Manifest {
        Manifest {
            format: FORMAT,
            product: crate::PRODUCT.into(),
            platform: crate::PLATFORM.into(),
            version: version.into(),
            published_at_ms: NOW - 1000,
            expires_at_ms: NOW + 86_400_000,
            notes: String::new(),
            files: vec![FileEntry {
                path: crate::CLIENT_EXE.into(),
                size: 10,
                sha256: "ab".repeat(32),
            }],
        }
    }

    fn keys() -> (Vec<u8>, Vec<TrustedKey>) {
        let (pk8, public) = generate_key().unwrap();
        (
            pk8,
            vec![TrustedKey {
                id: "k1".into(),
                public,
            }],
        )
    }

    fn expect(bad: &[String]) -> Expect<'_> {
        Expect {
            product: crate::PRODUCT,
            platform: crate::PLATFORM,
            current: Version(0, 1, 0),
            bad,
            now_ms: NOW,
        }
    }

    #[test]
    fn versions_parse_strictly_and_order() {
        assert_eq!(Version::parse("0.1.10"), Some(Version(0, 1, 10)));
        assert!(Version(0, 1, 10) > Version(0, 1, 9));
        assert!(Version(1, 0, 0) > Version(0, 99, 99));
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "1.2.x",
            "01.2.3",
            "1.-2.3",
            "1.2.3-rc1",
            " 1.2.3",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad}");
        }
        assert_eq!(Version(2, 0, 1).to_string(), "2.0.1");
    }

    #[test]
    fn a_signed_newer_manifest_verifies() {
        let (pk8, keys) = keys();
        let env = sign(&manifest("0.1.1"), &pk8, "k1").unwrap();
        let m = verify(&env, &keys, &expect(&[])).unwrap();
        assert_eq!(m.version, "0.1.1");
    }

    #[test]
    fn tampering_and_foreign_keys_are_rejected() {
        let (pk8, keys) = keys();
        let env = sign(&manifest("0.1.1"), &pk8, "k1").unwrap();
        // Changed manifest under the old signature.
        let mut e: Envelope = serde_json::from_slice(&env).unwrap();
        let mut bytes = B64.decode(&e.manifest).unwrap();
        let text = String::from_utf8(bytes.clone())
            .unwrap()
            .replace("0.1.1", "0.1.2");
        bytes = text.into_bytes();
        e.manifest = B64.encode(&bytes);
        let forged = serde_json::to_vec(&e).unwrap();
        assert_eq!(
            verify(&forged, &keys, &expect(&[])),
            Err(UpdateError::BadSignature)
        );
        // Signed by another key under a trusted id.
        let (other, _) = generate_key().unwrap();
        let env2 = sign(&manifest("0.1.1"), &other, "k1").unwrap();
        assert_eq!(
            verify(&env2, &keys, &expect(&[])),
            Err(UpdateError::BadSignature)
        );
        // Unknown id, no keys, garbage signature, oversized.
        let env3 = sign(&manifest("0.1.1"), &pk8, "k2").unwrap();
        assert_eq!(
            verify(&env3, &keys, &expect(&[])),
            Err(UpdateError::UnknownKey("k2".into()))
        );
        assert_eq!(verify(&env, &[], &expect(&[])), Err(UpdateError::NoKeys));
        let bytes = serde_json::to_vec(&manifest("0.1.1")).unwrap();
        let env4 = sign_raw(&bytes, &[0u8; 64], "k1").unwrap();
        assert_eq!(
            verify(&env4, &keys, &expect(&[])),
            Err(UpdateError::BadSignature)
        );
        let big = vec![b' '; MAX_ENVELOPE_BYTES + 1];
        assert_eq!(
            verify(&big, &keys, &expect(&[])),
            Err(UpdateError::TooLarge)
        );
        assert!(matches!(
            verify(b"{", &keys, &expect(&[])),
            Err(UpdateError::Malformed(_))
        ));
    }

    #[test]
    fn policy_rejects_downgrade_replay_expiry_target_and_bad_versions() {
        let (pk8, keys) = keys();
        let check = |m: Manifest, bad: &[String]| {
            let env = sign(&m, &pk8, "k1").unwrap();
            verify(&env, &keys, &expect(bad))
        };
        assert_eq!(
            check(manifest("0.1.0"), &[]),
            Err(UpdateError::NotNewer("0.1.0".into()))
        );
        assert_eq!(
            check(manifest("0.0.9"), &[]),
            Err(UpdateError::NotNewer("0.0.9".into()))
        );
        assert_eq!(
            check(manifest("0.1.1"), &["0.1.1".into()]),
            Err(UpdateError::KnownBad("0.1.1".into()))
        );
        let mut m = manifest("0.1.1");
        m.expires_at_ms = NOW;
        assert_eq!(check(m, &[]), Err(UpdateError::Expired));
        let mut m = manifest("0.1.1");
        m.published_at_ms = NOW + CLOCK_SKEW_MS + 1;
        assert_eq!(check(m, &[]), Err(UpdateError::FromFuture));
        let mut m = manifest("0.1.1");
        m.platform = "linux-x86_64".into();
        assert_eq!(check(m, &[]), Err(UpdateError::WrongTarget));
        let mut m = manifest("0.1.1");
        m.product = "other".into();
        assert_eq!(check(m, &[]), Err(UpdateError::WrongTarget));
        let mut m = manifest("0.1.1");
        m.format = 2;
        assert!(matches!(check(m, &[]), Err(UpdateError::Malformed(_))));
        assert!(matches!(
            check(manifest("v0.1.1"), &[]),
            Err(UpdateError::Malformed(_))
        ));
    }

    #[test]
    fn file_lists_are_checked() {
        let (pk8, keys) = keys();
        let with = |files: Vec<FileEntry>| {
            let mut m = manifest("0.1.1");
            m.files = files;
            verify(&sign(&m, &pk8, "k1").unwrap(), &keys, &expect(&[]))
        };
        let f = |path: &str, size: u64| FileEntry {
            path: path.into(),
            size,
            sha256: "0f".repeat(32),
        };
        let exe = || f(crate::CLIENT_EXE, 1);
        assert!(with(vec![exe(), f("lib/data.bin", 5)]).is_ok());
        for path in [
            "../x",
            "a/../b",
            "/abs",
            "C:/x",
            "a\\b",
            "a//b",
            "./a",
            "a b",
            "a:b",
            "",
            "CON",
            "nul.txt",
            "x/com1.dll",
            "a.",
            "..",
        ] {
            assert!(
                matches!(with(vec![exe(), f(path, 1)]), Err(UpdateError::BadFiles(_))),
                "{path}"
            );
        }
        assert!(matches!(
            with(vec![f("other.exe", 1)]),
            Err(UpdateError::BadFiles(_))
        ));
        assert!(matches!(with(vec![]), Err(UpdateError::BadFiles(_))));
        assert!(matches!(
            with(vec![exe(), f("TURBO-pancake-desktop.EXE", 1)]),
            Err(UpdateError::BadFiles(_))
        ));
        assert!(matches!(
            with(vec![exe(), f("big", MAX_TOTAL_BYTES)]),
            Err(UpdateError::BadFiles(_))
        ));
        let mut bad_hash = exe();
        bad_hash.sha256 = "AB".repeat(32);
        assert!(matches!(
            with(vec![bad_hash]),
            Err(UpdateError::BadFiles(_))
        ));
        let many: Vec<FileEntry> = std::iter::once(exe())
            .chain((0..MAX_FILES).map(|i| f(&format!("f{i}"), 1)))
            .collect();
        assert!(matches!(with(many), Err(UpdateError::BadFiles(_))));
    }

    #[test]
    fn unknown_fields_are_rejected_after_the_signature() {
        let (pk8, keys) = keys();
        let mut v = serde_json::to_value(manifest("0.1.1")).unwrap();
        v["url"] = "https://elsewhere/x.exe".into();
        let bytes = serde_json::to_vec(&v).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(&pk8).unwrap();
        let env = sign_raw(&bytes, pair.sign(&bytes).as_ref(), "k1").unwrap();
        assert!(matches!(
            verify(&env, &keys, &expect(&[])),
            Err(UpdateError::Malformed(_))
        ));
    }

    #[test]
    fn key_lists_parse() {
        let (_, public) = generate_key().unwrap();
        let s = format!(
            " k1:{} , release-2:{}",
            B64.encode(&public),
            B64.encode(&public)
        );
        let keys = TrustedKey::parse_list(&s).unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[1].id, "release-2");
        assert!(TrustedKey::parse_list("").unwrap().is_empty());
        assert!(TrustedKey::parse_list("nokey").is_err());
        assert!(TrustedKey::parse_list("k:AAAA").is_err());
        assert!(TrustedKey::parse_list(&format!("bad id:{}", B64.encode(&public))).is_err());
    }
}
