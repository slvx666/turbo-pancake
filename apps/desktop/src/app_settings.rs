//! Local application settings (spec §9: tray mode, theme), one small `key=value` file per
//! profile. Unknown keys and bad values fall back to the defaults, so a damaged or newer
//! file never stops the client from starting; writes go to a temporary file first and
//! replace the old one in one rename, so a crash leaves either the old or the new file.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppSettings {
    /// Closing the main window hides it to the notification area (the app keeps running,
    /// calls continue); off — closing the window exits.
    pub close_to_tray: bool,
    pub dark_theme: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            close_to_tray: true,
            dark_theme: true,
        }
    }
}

fn parse_bool(v: &str) -> Option<bool> {
    match v {
        "true" | "1" | "on" => Some(true),
        "false" | "0" | "off" => Some(false),
        _ => None,
    }
}

impl AppSettings {
    pub fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = parse_bool(value.trim());
            match key.trim() {
                "close_to_tray" => s.close_to_tray = value.unwrap_or(s.close_to_tray),
                "dark_theme" => s.dark_theme = value.unwrap_or(s.dark_theme),
                _ => {}
            }
        }
        s
    }

    pub fn to_text(&self) -> String {
        format!(
            "# turbo-pancake settings\nclose_to_tray={}\ndark_theme={}\n",
            self.close_to_tray, self.dark_theme
        )
    }

    /// The stored settings; defaults when the file is missing or unreadable.
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .map(|b| Self::parse(&String::from_utf8_lossy(&b)))
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, self.to_text())?;
        std::fs::rename(&tmp, path)
    }
}

/// `%LOCALAPPDATA%\turbo-pancake\settings\<profile>.conf`.
pub fn path(profile: &str) -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("HOME"))?;
    Some(
        PathBuf::from(base)
            .join("turbo-pancake")
            .join("settings")
            .join(format!("{profile}.conf")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_defaults() {
        let s = AppSettings {
            close_to_tray: false,
            dark_theme: false,
        };
        assert_eq!(AppSettings::parse(&s.to_text()), s);
        assert_eq!(AppSettings::parse(""), AppSettings::default());
    }

    #[test]
    fn damaged_or_newer_files_fall_back_per_key() {
        let s = AppSettings::parse(
            "close_to_tray=maybe\n\u{fffd}garbage\nfuture_key=1\ndark_theme=off",
        );
        assert!(s.close_to_tray, "bad value keeps the default");
        assert!(!s.dark_theme);
    }

    #[test]
    fn save_replaces_the_file_atomically() {
        let dir = std::env::temp_dir().join(format!("tp-settings-{}", std::process::id()));
        let file = dir.join("p.conf");
        assert_eq!(
            AppSettings::load(&file),
            AppSettings::default(),
            "missing file"
        );
        let mut s = AppSettings::default();
        s.save(&file).unwrap();
        s.close_to_tray = false;
        s.save(&file).unwrap();
        assert_eq!(AppSettings::load(&file), s);
        assert!(!file.with_extension("tmp").exists());
        std::fs::write(&file, [0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(
            AppSettings::load(&file),
            AppSettings::default(),
            "corrupted file"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
