//! Embeds an `.ico` file as the icon of the Windows executables of a package.
//!
//! Called from a build script. It writes a 32-bit `.res` file (one `RT_ICON` per image
//! of the `.ico` and one `RT_GROUP_ICON` with id [`ICON_ID`]) into `OUT_DIR` and hands it
//! to the MSVC linker, so no resource compiler (`rc.exe`) is needed on the build machine.
//! Explorer, the taskbar and `LoadImageW(module, ICON_ID)` then use that icon. On other
//! targets (and non-MSVC Windows toolchains) it does nothing.

use std::path::Path;

/// Resource id of the icon group (the first group is the executable's icon).
pub const ICON_ID: u16 = 1;

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
/// `MOVEABLE | DISCARDABLE` (icons), `| PURE` for the group, as `rc.exe` writes them.
const FLAGS_ICON: u16 = 0x1010;
const FLAGS_GROUP: u16 = 0x1030;
/// LANG_NEUTRAL / SUBLANG_NEUTRAL.
const LANGUAGE: u16 = 0;

/// One image of an `.ico` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconImage {
    /// The 12 bytes of the directory entry before the size/offset fields:
    /// width, height, colour count, reserved, planes (u16), bit count (u16).
    pub header: [u8; 8],
    pub data: Vec<u8>,
}

/// Parses an `.ico` file into its images.
pub fn parse_ico(bytes: &[u8]) -> Result<Vec<IconImage>, String> {
    let u16_at = |o: usize| -> Result<u16, String> {
        bytes
            .get(o..o + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .ok_or_else(|| "truncated .ico".to_string())
    };
    let u32_at = |o: usize| -> Result<u32, String> {
        bytes
            .get(o..o + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .ok_or_else(|| "truncated .ico".to_string())
    };
    if u16_at(0)? != 0 || u16_at(2)? != 1 {
        return Err("not an .ico file".into());
    }
    let count = u16_at(4)? as usize;
    if count == 0 {
        return Err(".ico without images".into());
    }
    let mut images = Vec::with_capacity(count);
    for i in 0..count {
        let e = 6 + 16 * i;
        let mut header = [0u8; 8];
        header.copy_from_slice(bytes.get(e..e + 8).ok_or("truncated .ico")?);
        let size = u32_at(e + 8)? as usize;
        let offset = u32_at(e + 12)? as usize;
        let data = bytes
            .get(offset..offset + size)
            .ok_or_else(|| format!("image {i} outside the file"))?
            .to_vec();
        images.push(IconImage { header, data });
    }
    Ok(images)
}

fn push_resource(out: &mut Vec<u8>, kind: u16, id: u16, flags: u16, data: &[u8]) {
    // Header: DataSize, HeaderSize, TYPE (ordinal), NAME (ordinal), DataVersion,
    // MemoryFlags, LanguageId, Version, Characteristics = 32 bytes, DWORD aligned.
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&32u32.to_le_bytes());
    out.extend_from_slice(&[0xff, 0xff]);
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&[0xff, 0xff]);
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&LANGUAGE.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(data);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

/// The `.res` bytes for these images: icons get ids `ICON_ID + 1 ..`, the group
/// [`ICON_ID`].
pub fn res_bytes(images: &[IconImage]) -> Vec<u8> {
    let mut out = Vec::new();
    // The empty first entry that marks a 32-bit resource file.
    push_resource(&mut out, 0, 0, 0, &[]);
    let mut group = Vec::with_capacity(6 + 14 * images.len());
    group.extend_from_slice(&0u16.to_le_bytes());
    group.extend_from_slice(&1u16.to_le_bytes());
    group.extend_from_slice(&(images.len() as u16).to_le_bytes());
    for (i, img) in images.iter().enumerate() {
        let id = ICON_ID + 1 + i as u16;
        push_resource(&mut out, RT_ICON, id, FLAGS_ICON, &img.data);
        group.extend_from_slice(&img.header);
        group.extend_from_slice(&(img.data.len() as u32).to_le_bytes());
        group.extend_from_slice(&id.to_le_bytes());
    }
    push_resource(&mut out, RT_GROUP_ICON, ICON_ID, FLAGS_GROUP, &group);
    out
}

/// Build-script entry: embeds `ico` (path relative to the package) into the package's
/// binaries on Windows/MSVC targets.
pub fn embed(ico: &Path) {
    println!("cargo:rerun-if-changed={}", ico.display());
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os != "windows" || env != "msvc" {
        return;
    }
    let bytes = std::fs::read(ico).unwrap_or_else(|e| panic!("{}: {e}", ico.display()));
    let images = parse_ico(&bytes).unwrap_or_else(|e| panic!("{}: {e}", ico.display()));
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let res = out.join("icon.res");
    std::fs::write(&res, res_bytes(&images)).expect("write icon.res");
    // link.exe converts a .res input itself (cvtres).
    println!("cargo:rustc-link-arg-bins={}", res.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ico(images: &[(&[u8; 8], &[u8])]) -> Vec<u8> {
        let mut out = vec![0, 0, 1, 0];
        out.extend_from_slice(&(images.len() as u16).to_le_bytes());
        let mut offset = 6 + 16 * images.len();
        for (h, d) in images {
            out.extend_from_slice(*h);
            out.extend_from_slice(&(d.len() as u32).to_le_bytes());
            out.extend_from_slice(&(offset as u32).to_le_bytes());
            offset += d.len();
        }
        for (_, d) in images {
            out.extend_from_slice(d);
        }
        out
    }

    #[test]
    fn parses_and_writes_a_resource_file() {
        let a = [16u8, 16, 0, 0, 1, 0, 32, 0];
        let b = [0u8, 0, 0, 0, 1, 0, 32, 0];
        let file = ico(&[(&a, b"abc"), (&b, b"defgh")]);
        let images = parse_ico(&file).unwrap();
        assert_eq!(images.len(), 2);
        assert_eq!(images[1].data, b"defgh");
        let res = res_bytes(&images);
        // marker entry: 32 zero-sized header
        assert_eq!(&res[0..8], &[0, 0, 0, 0, 32, 0, 0, 0]);
        assert_eq!(res.len() % 4, 0);
        // first icon: 3 bytes data, type RT_ICON, id 2, padded to 4
        assert_eq!(&res[32..36], &3u32.to_le_bytes());
        assert_eq!(&res[40..44], &[0xff, 0xff, 3, 0]);
        assert_eq!(&res[44..48], &[0xff, 0xff, 2, 0]);
        assert_eq!(&res[64..67], b"abc");
        // group at the end: 6 + 2 * 14 bytes, id 1
        let group_hdr = res.len() - 32 - 36;
        assert_eq!(&res[group_hdr..group_hdr + 4], &34u32.to_le_bytes());
        assert_eq!(
            &res[group_hdr + 8..group_hdr + 16],
            &[0xff, 0xff, 14, 0, 0xff, 0xff, 1, 0]
        );
        let g = &res[group_hdr + 32..group_hdr + 32 + 34];
        assert_eq!(&g[0..6], &[0, 0, 1, 0, 2, 0]);
        assert_eq!(&g[6..14], &a);
        assert_eq!(&g[14..18], &3u32.to_le_bytes());
        assert_eq!(&g[18..20], &2u16.to_le_bytes());
        assert_eq!(&g[32..34], &3u16.to_le_bytes());
    }

    #[test]
    fn rejects_other_files() {
        assert!(parse_ico(b"\x89PNG....").is_err());
        assert!(parse_ico(&[0, 0, 1, 0, 0, 0]).is_err());
        let mut bad = ico(&[(&[16, 16, 0, 0, 1, 0, 32, 0], b"abc")]);
        bad.truncate(bad.len() - 1);
        assert!(parse_ico(&bad).is_err());
    }

    #[test]
    fn the_project_icon_parses() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/icon/turbo-pancake.ico");
        let images = parse_ico(&std::fs::read(path).unwrap()).unwrap();
        assert!(images.len() >= 5);
        assert!(images.iter().any(|i| i.header[0] == 16));
        assert!(images.iter().any(|i| i.header[0] == 0)); // 256 px
    }
}
