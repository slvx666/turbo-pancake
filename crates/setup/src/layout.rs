//! Installing a package into the ADR 0016 layout and removing it again (ADR 0017).
//! Only known entries are written or deleted; the install root itself is removed only
//! when nothing else is left in it.

use std::path::{Path, PathBuf};

use turbo_pancake_update::package::{self, PackageHeader};
use turbo_pancake_update::{CLIENT_EXE, Install, LAUNCHER_EXE, State, UpdateError, Version};

/// Entries of the install root this program owns.
const OWNED: &[&str] = &[
    LAUNCHER_EXE,
    "turbo-pancake.exe.old",
    "turbo-pancake.exe.new",
    "state.json",
    "state.json.tmp",
    "versions",
    "staging",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Fresh,
    Upgrade {
        from: String,
    },
    Repair,
    /// A newer version is installed already; only the launcher was refreshed.
    KeptNewer {
        installed: String,
    },
}

impl Action {
    pub fn name(&self) -> &'static str {
        match self {
            Action::Fresh => "fresh",
            Action::Upgrade { .. } => "upgrade",
            Action::Repair => "repair",
            Action::KeptNewer { .. } => "kept_newer",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub version: String,
    pub action: Action,
    /// Bytes of the installed files (uninstall entry's EstimatedSize).
    pub installed_bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("{0}")]
    Package(#[from] UpdateError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// The version's files are in use (the client is running from them).
    #[error("файлы turbo-pancake заняты — закройте приложение и повторите")]
    InUse,
    #[error("в этой сборке установщика нет пакета")]
    EmptyPackage,
    #[error("{0} не похож на установку turbo-pancake")]
    NotAnInstall(String),
}

fn staging(root: &Path, v: &str) -> PathBuf {
    root.join("staging").join(format!("setup-{v}"))
}

/// Installs `pkg` (launcher + client files) into `root` following the upgrade rules.
pub fn install_package(root: &Path, pkg: &[u8]) -> Result<Outcome, SetupError> {
    if pkg.is_empty() {
        return Err(SetupError::EmptyPackage);
    }
    let (header, _) = package::read_header(pkg)?;
    check_contents(&header)?;
    let v = header.version.clone();
    let new = Version::parse(&v).ok_or(UpdateError::Malformed(v.clone()))?;
    std::fs::create_dir_all(root)?;
    let install = Install::new(root);
    let existing = install.read_state().ok();
    let action = match &existing {
        None => Action::Fresh,
        Some(s) => match Version::parse(&s.current) {
            Some(cur) if cur > new => Action::KeptNewer {
                installed: s.current.clone(),
            },
            Some(cur) if cur == new => Action::Repair,
            _ => Action::Upgrade {
                from: s.current.clone(),
            },
        },
    };
    if !matches!(action, Action::KeptNewer { .. }) {
        // Client files into staging, then swapped in as one directory.
        let stage = staging(root, &v);
        if stage.exists() {
            std::fs::remove_dir_all(&stage)?;
        }
        package::extract(pkg, |p| (p != LAUNCHER_EXE).then(|| stage.join(p)))?;
        let target = install.version_dir(&v);
        if target.exists() {
            let aside = root.join("staging").join(format!("replaced-{v}"));
            let _ = std::fs::remove_dir_all(&aside);
            // Fails while the client runs from this directory.
            std::fs::rename(&target, &aside).map_err(|_| SetupError::InUse)?;
            let _ = std::fs::remove_dir_all(&aside);
        }
        std::fs::create_dir_all(root.join("versions"))?;
        std::fs::rename(&stage, &target)?;
    }
    replace_launcher(root, pkg)?;
    let state = match (&action, existing) {
        (Action::Fresh, _) | (_, None) => State {
            current: v.clone(),
            ..State::default()
        },
        (Action::Upgrade { from }, Some(mut s)) => {
            s.previous = Some(from.clone());
            s.current = v.clone();
            s.pending = None;
            s.bad.retain(|b| b != &v);
            s
        }
        (Action::Repair, Some(mut s)) => {
            s.pending = None;
            s.bad.retain(|b| b != &v);
            s
        }
        (Action::KeptNewer { .. }, Some(s)) => s,
    };
    install.write_state(&state)?;
    install.cleanup(&state);
    let installed_bytes = dir_size(root);
    Ok(Outcome {
        version: v,
        action,
        installed_bytes,
    })
}

fn check_contents(h: &PackageHeader) -> Result<(), SetupError> {
    for need in [LAUNCHER_EXE, CLIENT_EXE] {
        if !h.files.iter().any(|f| f.path == need) {
            return Err(UpdateError::BadFiles(format!("{need} missing")).into());
        }
    }
    Ok(())
}

/// A running launcher can be renamed but not overwritten: new → exe, old aside.
fn replace_launcher(root: &Path, pkg: &[u8]) -> Result<(), SetupError> {
    let exe = root.join(LAUNCHER_EXE);
    let new = root.join("turbo-pancake.exe.new");
    let old = root.join("turbo-pancake.exe.old");
    package::extract(pkg, |p| (p == LAUNCHER_EXE).then(|| new.clone()))?;
    let _ = std::fs::remove_file(&old);
    if exe.exists() {
        std::fs::rename(&exe, &old)?;
    }
    std::fs::rename(&new, &exe)?;
    let _ = std::fs::remove_file(&old);
    Ok(())
}

fn dir_size(p: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else {
        return 0;
    };
    rd.flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

/// Removes the install's own entries, then the root if it is empty. Refuses a
/// directory without `state.json` and the launcher (never deletes a wrong directory).
/// `skip` is a file to leave (the running uninstaller copy, when it sits inside).
pub fn remove_install(root: &Path) -> Result<(), SetupError> {
    if !root.join("state.json").is_file() || !root.join(LAUNCHER_EXE).is_file() {
        return Err(SetupError::NotAnInstall(root.display().to_string()));
    }
    // Client files first: if the client runs, nothing else is touched yet.
    let versions = root.join("versions");
    if versions.exists() {
        let aside = root.join("staging").join("removing");
        std::fs::create_dir_all(root.join("staging"))?;
        let _ = std::fs::remove_dir_all(&aside);
        std::fs::rename(&versions, &aside).map_err(|_| SetupError::InUse)?;
    }
    for name in OWNED {
        let p = root.join(name);
        let res = if p.is_dir() {
            std::fs::remove_dir_all(&p)
        } else if p.exists() {
            std::fs::remove_file(&p)
        } else {
            Ok(())
        };
        res?;
    }
    // Only if nothing foreign is left.
    if std::fs::read_dir(root)?.next().is_none() {
        std::fs::remove_dir(root)?;
    }
    Ok(())
}

/// Profile data of the client (`%LOCALAPPDATA%\turbo-pancake`): only its known parts.
pub fn remove_profile_data(local_app_data: &Path) -> std::io::Result<()> {
    let base = local_app_data.join("turbo-pancake");
    for sub in ["cache", "settings"] {
        let p = base.join(sub);
        if p.exists() {
            std::fs::remove_dir_all(&p)?;
        }
    }
    if base.exists() && std::fs::read_dir(&base)?.next().is_none() {
        std::fs::remove_dir(&base)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(v: &str, client: &[u8]) -> Vec<u8> {
        package::pack(
            v,
            &[
                (LAUNCHER_EXE.into(), format!("launcher {v}").into_bytes()),
                (CLIENT_EXE.into(), client.to_vec()),
            ],
        )
        .unwrap()
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tp-setup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn fresh_upgrade_repair_and_older_packages() {
        let root = tmp("rules").join("turbo-pancake");
        let i = Install::new(&root);
        let o = install_package(&root, &pkg("0.1.0", b"c0")).unwrap();
        assert_eq!(o.action, Action::Fresh);
        assert!(o.installed_bytes > 0);
        assert_eq!(std::fs::read(i.exe("0.1.0")).unwrap(), b"c0");
        assert_eq!(
            std::fs::read(root.join(LAUNCHER_EXE)).unwrap(),
            b"launcher 0.1.0"
        );
        assert_eq!(i.read_state().unwrap().current, "0.1.0");

        // A failed update left 0.1.1 marked bad; installing it explicitly clears that.
        let mut s = i.read_state().unwrap();
        s.bad.push("0.1.1".into());
        i.write_state(&s).unwrap();
        let o = install_package(&root, &pkg("0.1.1", b"c1")).unwrap();
        assert_eq!(
            o.action,
            Action::Upgrade {
                from: "0.1.0".into()
            }
        );
        let s = i.read_state().unwrap();
        assert_eq!(
            (s.current.as_str(), s.previous.as_deref()),
            ("0.1.1", Some("0.1.0"))
        );
        assert!(s.bad.is_empty());

        // Repair rewrites damaged files of the same version.
        std::fs::write(i.exe("0.1.1"), b"damaged").unwrap();
        assert_eq!(
            install_package(&root, &pkg("0.1.1", b"c1")).unwrap().action,
            Action::Repair
        );
        assert_eq!(std::fs::read(i.exe("0.1.1")).unwrap(), b"c1");

        // An older installer keeps the newer version, refreshes only the launcher.
        let o = install_package(&root, &pkg("0.0.9", b"old")).unwrap();
        assert_eq!(
            o.action,
            Action::KeptNewer {
                installed: "0.1.1".into()
            }
        );
        assert_eq!(i.read_state().unwrap().current, "0.1.1");
        assert!(!i.version_dir("0.0.9").exists());
        assert!(!root.join("staging").exists(), "staging cleaned");
    }

    #[test]
    fn empty_damaged_and_incomplete_packages_change_nothing() {
        let root = tmp("bad").join("turbo-pancake");
        install_package(&root, &pkg("0.1.0", b"c0")).unwrap();
        let before = std::fs::read(root.join("state.json")).unwrap();
        assert!(matches!(
            install_package(&root, &[]),
            Err(SetupError::EmptyPackage)
        ));
        let mut p = pkg("0.2.0", &vec![7u8; 10_000]);
        let n = p.len();
        p.truncate(n - 3);
        assert!(install_package(&root, &p).is_err());
        let only_client = package::pack("0.2.0", &[(CLIENT_EXE.into(), b"x".to_vec())]).unwrap();
        assert!(install_package(&root, &only_client).is_err());
        assert_eq!(std::fs::read(root.join("state.json")).unwrap(), before);
        assert!(!Install::new(&root).version_dir("0.2.0").exists());
    }

    #[test]
    fn removal_takes_only_own_entries() {
        let base = tmp("remove");
        let root = base.join("turbo-pancake");
        install_package(&root, &pkg("0.1.0", b"c0")).unwrap();
        std::fs::write(root.join("user-notes.txt"), b"keep").unwrap();
        remove_install(&root).unwrap();
        assert!(root.join("user-notes.txt").exists(), "foreign file kept");
        assert!(!root.join("versions").exists() && !root.join(LAUNCHER_EXE).exists());
        std::fs::remove_file(root.join("user-notes.txt")).unwrap();
        // Not an install (any more): refused, nothing deleted.
        std::fs::write(root.join("x"), b"x").unwrap();
        assert!(matches!(
            remove_install(&root),
            Err(SetupError::NotAnInstall(_))
        ));
        assert!(root.join("x").exists());
        // A clean install leaves no root behind.
        let root2 = base.join("second");
        install_package(&root2, &pkg("0.1.0", b"c0")).unwrap();
        remove_install(&root2).unwrap();
        assert!(!root2.exists());
    }

    #[test]
    fn profile_data_removal_is_limited_to_known_parts() {
        let base = tmp("profile");
        let tp = base.join("turbo-pancake");
        std::fs::create_dir_all(tp.join("cache/default")).unwrap();
        std::fs::create_dir_all(tp.join("settings")).unwrap();
        std::fs::write(tp.join("settings/default.conf"), b"x").unwrap();
        std::fs::create_dir_all(base.join("Other")).unwrap();
        remove_profile_data(&base).unwrap();
        assert!(!tp.exists());
        assert!(base.join("Other").exists());
    }
}
