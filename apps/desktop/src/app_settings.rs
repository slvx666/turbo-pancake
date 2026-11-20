//! Local application settings (spec §9: tray mode, theme), one small `key=value` file per
//! profile. Unknown keys and bad values fall back to the defaults, so a damaged or newer
//! file never stops the client from starting; writes go to a temporary file first and
//! replace the old one in one rename, so a crash leaves either the old or the new file.

use std::path::{Path, PathBuf};

pub const UI_SCALE_MIN: u32 = 100;
pub const UI_SCALE_MAX: u32 = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppSettings {
    /// Closing the main window hides it to the notification area (the app keeps running,
    /// calls continue); off — closing the window exits.
    pub close_to_tray: bool,
    pub dark_theme: bool,
    /// Interface scale on top of the system DPI, percent (100–200); applied at start.
    pub ui_scale: u32,
    /// The one-time notification "the app keeps running in the tray" was shown.
    pub tray_hint_shown: bool,
    /// Desktop notifications for messages and calls while the window is not in front.
    pub notifications: bool,
    /// UI sounds (messages, ringtone, call cues).
    pub sounds: bool,
    /// Volume of the UI sounds, percent (0–100).
    pub sound_volume: u8,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            close_to_tray: true,
            dark_theme: true,
            ui_scale: 100,
            tray_hint_shown: false,
            notifications: true,
            sounds: true,
            sound_volume: 70,
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
            let raw = value.trim();
            let value = parse_bool(raw);
            match key.trim() {
                "ui_scale" => {
                    if let Ok(p) = raw.parse::<u32>() {
                        s.ui_scale = p.clamp(UI_SCALE_MIN, UI_SCALE_MAX);
                    }
                }
                "close_to_tray" => s.close_to_tray = value.unwrap_or(s.close_to_tray),
                "dark_theme" => s.dark_theme = value.unwrap_or(s.dark_theme),
                "tray_hint_shown" => s.tray_hint_shown = value.unwrap_or(s.tray_hint_shown),
                "notifications" => s.notifications = value.unwrap_or(s.notifications),
                "sounds" => s.sounds = value.unwrap_or(s.sounds),
                "sound_volume" => {
                    if let Ok(v) = raw.parse::<u32>() {
                        s.sound_volume = v.min(100) as u8;
                    }
                }
                _ => {}
            }
        }
        s
    }

    pub fn to_text(&self) -> String {
        format!(
            "# turbo-pancake settings\nclose_to_tray={}\ndark_theme={}\nui_scale={}\n\
             tray_hint_shown={}\nnotifications={}\nsounds={}\nsound_volume={}\n",
            self.close_to_tray,
            self.dark_theme,
            self.ui_scale,
            self.tray_hint_shown,
            self.notifications,
            self.sounds,
            self.sound_volume
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
            ui_scale: 150,
            tray_hint_shown: true,
            notifications: false,
            sounds: false,
            sound_volume: 35,
        };
        assert_eq!(AppSettings::parse(&s.to_text()), s);
        assert_eq!(AppSettings::parse(""), AppSettings::default());
    }

    #[test]
    fn files_without_alert_keys_get_the_defaults() {
        // A file written before notifications existed.
        let s = AppSettings::parse("close_to_tray=false\ndark_theme=true\nui_scale=125\n");
        assert!(s.notifications && s.sounds);
        assert_eq!(s.sound_volume, 70);
        assert_eq!(AppSettings::parse("sound_volume=400").sound_volume, 100);
        assert_eq!(AppSettings::parse("sound_volume=-3").sound_volume, 70);
        assert!(!AppSettings::parse("sounds=off").sounds);
    }

    #[test]
    fn damaged_or_newer_files_fall_back_per_key() {
        let s = AppSettings::parse(
            "close_to_tray=maybe\n\u{fffd}garbage\nfuture_key=1\ndark_theme=off",
        );
        assert!(s.close_to_tray, "bad value keeps the default");
        assert!(!s.dark_theme);
        assert_eq!(AppSettings::parse("ui_scale=500").ui_scale, UI_SCALE_MAX);
        assert_eq!(AppSettings::parse("ui_scale=50").ui_scale, UI_SCALE_MIN);
        assert_eq!(AppSettings::parse("ui_scale=big").ui_scale, 100);
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
