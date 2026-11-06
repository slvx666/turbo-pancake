//! Per-user installation (S7 step 6, ADR 0017): installing a package into the ADR 0016
//! layout, the Start-menu shortcut and the uninstall entry, and uninstalling.
//! [`layout`] is platform-neutral; [`win`] holds the Windows integration.

pub mod layout;
#[cfg(windows)]
pub mod win;

use std::path::{Path, PathBuf};

pub use layout::{
    Action, Outcome, SetupError, install_package, remove_install, remove_profile_data,
};

/// Where things go for this user. `trial` suffixes the registry key and the shortcut so
/// a test never touches a real installation.
#[derive(Debug, Clone)]
pub struct Target {
    pub root: PathBuf,
    pub shortcut: PathBuf,
    pub registry_key: String,
    pub display_name: String,
    pub trial: Option<String>,
    /// Stored sessions to remove on a purge (`turbo-pancake/…`).
    pub credential_prefix: String,
}

/// Trial ids: short, `[A-Za-z0-9-]` (they end up in registry and file names).
pub fn valid_trial_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 40 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl Target {
    pub fn new(
        install_dir: Option<PathBuf>,
        shortcut_dir: Option<PathBuf>,
        trial: Option<String>,
    ) -> Result<Self, String> {
        if let Some(t) = &trial
            && !valid_trial_id(t)
        {
            return Err(format!("invalid trial id {t}"));
        }
        let env = |k: &str| {
            std::env::var_os(k)
                .map(PathBuf::from)
                .ok_or(format!("{k} is not set"))
        };
        let root = match install_dir {
            Some(d) => d,
            None => env("LOCALAPPDATA")?.join("Programs").join("turbo-pancake"),
        };
        let shortcut_dir = match shortcut_dir {
            Some(d) => d,
            None => env("APPDATA")?
                .join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs"),
        };
        let (registry_key, display_name, lnk, credential_prefix) = match &trial {
            None => (
                "turbo-pancake".to_string(),
                "turbo-pancake".to_string(),
                "turbo-pancake.lnk".to_string(),
                "turbo-pancake/".to_string(),
            ),
            Some(t) => (
                format!("turbo-pancake-{t}"),
                format!("turbo-pancake ({t})"),
                format!("turbo-pancake ({t}).lnk"),
                // Trial profiles are named after the trial id.
                format!("turbo-pancake/{t}"),
            ),
        };
        Ok(Self {
            shortcut: shortcut_dir.join(lnk),
            root,
            registry_key,
            display_name,
            trial,
            credential_prefix,
        })
    }

    pub fn launcher(&self) -> PathBuf {
        self.root.join(turbo_pancake_update::LAUNCHER_EXE)
    }

    /// Arguments that let the uninstaller find this target again.
    pub fn uninstall_args(&self) -> Vec<String> {
        let mut a = vec![
            "--uninstall".to_string(),
            "--install-dir".into(),
            self.root.display().to_string(),
        ];
        if let Some(t) = &self.trial {
            a.push("--trial-id".into());
            a.push(t.clone());
            if let Some(d) = self.shortcut.parent() {
                a.push("--shortcut-dir".into());
                a.push(d.display().to_string());
            }
        }
        a
    }
}

/// Command line with each argument quoted (registry `UninstallString`).
pub fn command_line(exe: &Path, args: &[String]) -> String {
    std::iter::once(exe.display().to_string())
        .chain(args.iter().cloned())
        .map(|a| format!("\"{}\"", a.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parsed common options of the installer and the uninstall mode of the launcher.
#[derive(Debug, Default, Clone)]
pub struct Options {
    pub quiet: bool,
    pub no_launch: bool,
    pub purge: bool,
    pub install_dir: Option<PathBuf>,
    pub shortcut_dir: Option<PathBuf>,
    pub trial: Option<String>,
}

impl Options {
    /// Takes the known options out of `args`, leaving the rest.
    pub fn take(args: &mut Vec<String>) -> Self {
        let mut o = Options::default();
        let mut rest = Vec::new();
        let mut it = std::mem::take(args).into_iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--quiet" => o.quiet = true,
                "--no-launch" => o.no_launch = true,
                "--purge" => o.purge = true,
                "--install-dir" => o.install_dir = it.next().map(PathBuf::from),
                "--shortcut-dir" => o.shortcut_dir = it.next().map(PathBuf::from),
                "--trial-id" => o.trial = it.next(),
                _ => rest.push(a),
            }
        }
        *args = rest;
        o
    }

    pub fn target(&self) -> Result<Target, String> {
        Target::new(
            self.install_dir.clone(),
            self.shortcut_dir.clone(),
            self.trial.clone(),
        )
    }

    /// The same options for a child process (uninstaller copy).
    pub fn to_args(&self) -> Vec<String> {
        let mut a = Vec::new();
        if self.quiet {
            a.push("--quiet".into());
        }
        if self.purge {
            a.push("--purge".into());
        }
        if let Some(d) = &self.install_dir {
            a.extend(["--install-dir".into(), d.display().to_string()]);
        }
        if let Some(d) = &self.shortcut_dir {
            a.extend(["--shortcut-dir".into(), d.display().to_string()]);
        }
        if let Some(t) = &self.trial {
            a.extend(["--trial-id".into(), t.clone()]);
        }
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_and_options() {
        let t = Target::new(
            Some("C:/x/tp".into()),
            Some("C:/sm".into()),
            Some("t1".into()),
        )
        .unwrap();
        assert_eq!(t.registry_key, "turbo-pancake-t1");
        assert_eq!(
            t.shortcut,
            PathBuf::from("C:/sm").join("turbo-pancake (t1).lnk")
        );
        assert_eq!(t.credential_prefix, "turbo-pancake/t1");
        assert!(Target::new(None, None, Some("bad id".into())).is_err());
        let mut args: Vec<String> = t.uninstall_args();
        let o = Options::take(&mut args);
        assert_eq!(args, vec!["--uninstall".to_string()]);
        assert_eq!(o.install_dir, Some(PathBuf::from("C:/x/tp")));
        assert_eq!(o.trial.as_deref(), Some("t1"));
        assert_eq!(o.shortcut_dir, Some(PathBuf::from("C:/sm")));
        let mut again = o.to_args();
        let o2 = Options::take(&mut again);
        assert!(again.is_empty());
        assert_eq!(o2.install_dir, o.install_dir);
        assert_eq!(
            command_line(Path::new("C:/a b/x.exe"), &["--uninstall".into()]),
            "\"C:/a b/x.exe\" \"--uninstall\""
        );
    }
}
