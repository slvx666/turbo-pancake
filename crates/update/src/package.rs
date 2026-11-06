//! The installer package (ADR 0017): `TPPK`, a little-endian u32 header length, a JSON
//! header (version, files with size/SHA-256/compressed size) and one raw deflate stream
//! per file. Extraction checks every path, size and digest.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::manifest::{MAX_FILES, MAX_TOTAL_BYTES, UpdateError, Version, valid_path};

const MAGIC: &[u8; 4] = b"TPPK";
const FORMAT: u32 = 1;
const MAX_HEADER: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackedFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub compressed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageHeader {
    pub format: u32,
    pub version: String,
    pub files: Vec<PackedFile>,
}

impl PackageHeader {
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

fn sha256(b: &[u8]) -> String {
    crate::hex(aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, b).as_ref())
}

/// Packs files (`path`, contents) for `version`.
pub fn pack(version: &str, files: &[(String, Vec<u8>)]) -> Result<Vec<u8>, UpdateError> {
    if Version::parse(version).is_none() {
        return Err(UpdateError::Malformed(format!("version {version}")));
    }
    let mut streams = Vec::new();
    let mut header = PackageHeader {
        format: FORMAT,
        version: version.to_owned(),
        files: Vec::new(),
    };
    for (path, data) in files {
        if !valid_path(path) {
            return Err(UpdateError::BadFiles(format!("path {path}")));
        }
        let z = miniz_oxide::deflate::compress_to_vec(data, 10);
        header.files.push(PackedFile {
            path: path.clone(),
            size: data.len() as u64,
            sha256: sha256(data),
            compressed: z.len() as u64,
        });
        streams.push(z);
    }
    let json = serde_json::to_vec(&header).map_err(|e| UpdateError::Malformed(e.to_string()))?;
    let mut out = Vec::with_capacity(8 + json.len() + streams.iter().map(Vec::len).sum::<usize>());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    for s in streams {
        out.extend_from_slice(&s);
    }
    Ok(out)
}

/// The header and the offset of the first stream; checks limits and paths.
pub fn read_header(bytes: &[u8]) -> Result<(PackageHeader, usize), UpdateError> {
    let bad = |s: &str| UpdateError::Malformed(format!("package: {s}"));
    if bytes.len() < 8 || &bytes[..4] != MAGIC {
        return Err(bad("not a package"));
    }
    let len = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    if len > MAX_HEADER || 8 + len > bytes.len() {
        return Err(bad("header size"));
    }
    let h: PackageHeader =
        serde_json::from_slice(&bytes[8..8 + len]).map_err(|e| bad(&e.to_string()))?;
    if h.format != FORMAT || Version::parse(&h.version).is_none() {
        return Err(bad("format or version"));
    }
    if h.files.is_empty() || h.files.len() > MAX_FILES || h.total_size() > MAX_TOTAL_BYTES {
        return Err(bad("file list"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut end = 8 + len as u64;
    for f in &h.files {
        if !valid_path(&f.path) || !seen.insert(f.path.to_ascii_lowercase()) {
            return Err(bad(&format!("path {}", f.path)));
        }
        end = end.saturating_add(f.compressed);
    }
    if end != bytes.len() as u64 {
        return Err(bad("truncated or trailing data"));
    }
    Ok((h, 8 + len))
}

/// Extracts the files `place` maps to a destination (None skips a file), verifying
/// each file's size and SHA-256 before writing it.
pub fn extract(
    bytes: &[u8],
    mut place: impl FnMut(&str) -> Option<PathBuf>,
) -> Result<PackageHeader, UpdateError> {
    let (h, mut off) = read_header(bytes)?;
    for f in &h.files {
        let z = &bytes[off..off + f.compressed as usize];
        off += f.compressed as usize;
        let Some(dest) = place(&f.path) else { continue };
        let data = miniz_oxide::inflate::decompress_to_vec_with_limit(z, f.size as usize)
            .map_err(|_| UpdateError::SizeMismatch(f.path.clone()))?;
        if data.len() as u64 != f.size {
            return Err(UpdateError::SizeMismatch(f.path.clone()));
        }
        if sha256(&data) != f.sha256 {
            return Err(UpdateError::HashMismatch(f.path.clone()));
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, &data)?;
    }
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<(String, Vec<u8>)> {
        vec![
            ("turbo-pancake.exe".into(), b"launcher".repeat(500)),
            (
                "turbo-pancake-desktop.exe".into(),
                (0..100_000u32).map(|i| (i % 251) as u8).collect(),
            ),
            ("res/a.bin".into(), Vec::new()),
        ]
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tp-pack-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn round_trip_compresses_and_verifies() {
        let input = files();
        let p = pack("0.3.0", &input).unwrap();
        let raw: usize = input.iter().map(|f| f.1.len()).sum();
        assert!(p.len() < raw / 2, "{} vs {raw}", p.len());
        let dir = tmp("ok");
        let h = extract(&p, |path| Some(dir.join(path))).unwrap();
        assert_eq!(h.version, "0.3.0");
        for (path, data) in &input {
            assert_eq!(&std::fs::read(dir.join(path)).unwrap(), data, "{path}");
        }
        // Selective extraction.
        let dir2 = tmp("some");
        extract(&p, |path| (path == "res/a.bin").then(|| dir2.join(path))).unwrap();
        assert!(dir2.join("res/a.bin").exists() && !dir2.join("turbo-pancake.exe").exists());
    }

    #[test]
    fn damaged_packages_are_rejected() {
        let p = pack("0.3.0", &files()).unwrap();
        let dir = tmp("bad");
        let to = |path: &str| Some(dir.join(path));
        assert!(extract(&p[..p.len() - 1], to).is_err(), "truncated");
        let mut longer = p.clone();
        longer.push(0);
        assert!(extract(&longer, to).is_err(), "trailing data");
        let mut flipped = p.clone();
        let last = flipped.len() - 10;
        flipped[last] ^= 0x55;
        assert!(extract(&flipped, to).is_err(), "corrupted stream");
        assert!(extract(b"PK\x03\x04....", to).is_err(), "not ours");
        assert!(extract(&[], to).is_err());
        // A header lying about the size cannot inflate beyond it.
        let (mut h, off) = read_header(&p).unwrap();
        h.files[1].size = 10;
        let json = serde_json::to_vec(&h).unwrap();
        let mut forged = Vec::new();
        forged.extend_from_slice(MAGIC);
        forged.extend_from_slice(&(json.len() as u32).to_le_bytes());
        forged.extend_from_slice(&json);
        forged.extend_from_slice(&p[off..]);
        assert!(matches!(
            extract(&forged, |path| Some(dir.join(path))),
            Err(UpdateError::SizeMismatch(_))
        ));
        assert!(pack("1.2", &files()).is_err());
        assert!(pack("1.2.3", &[("../x".into(), vec![1])]).is_err());
    }
}
