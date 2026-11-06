//! The per-user install layout (ADR 0016):
//!
//! ```text
//! <root>/turbo-pancake.exe     launcher
//! <root>/state.json            current / previous / pending / bad
//! <root>/versions/<v>/…        installed versions
//! <root>/staging/<v>/…         downloads in progress
//! ```
//!
//! `state.json` is replaced atomically (temp file, flush, rename); a version directory
//! is referenced only after it was fully downloaded, verified and renamed into place.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::manifest::{Manifest, UpdateError, valid_path};

const STATE: &str = "state.json";
const STATE_TMP: &str = "state.json.tmp";
/// Versions remembered as failed (never offered again).
const MAX_BAD: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub version: String,
    /// Starts of the trial so far; a second start without confirmation rolls back.
    pub attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct State {
    pub current: String,
    #[serde(default)]
    pub previous: Option<String>,
    #[serde(default)]
    pub pending: Option<Pending>,
    #[serde(default)]
    pub bad: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchPlan {
    Run {
        version: String,
    },
    /// Start the new version and wait for its confirmation; `fallback` otherwise.
    Trial {
        version: String,
        fallback: String,
    },
}

#[derive(Debug, Clone)]
pub struct Install {
    root: PathBuf,
}

impl Install {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The install and the version directory of an executable at
    /// `<root>/versions/<v>/<exe>`, if it runs from an install.
    pub fn from_exe(exe: &Path) -> Option<(Install, String)> {
        let dir = exe.parent()?;
        let version = dir.file_name()?.to_str()?.to_owned();
        let versions = dir.parent()?;
        if versions.file_name()? != "versions" {
            return None;
        }
        let root = versions.parent()?;
        root.join(STATE)
            .is_file()
            .then(|| (Install::new(root), version))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn version_dir(&self, v: &str) -> PathBuf {
        self.root.join("versions").join(v)
    }

    pub fn exe(&self, v: &str) -> PathBuf {
        self.version_dir(v).join(crate::CLIENT_EXE)
    }

    pub fn launcher(&self) -> PathBuf {
        self.root.join(crate::LAUNCHER_EXE)
    }

    fn staging_dir(&self, v: &str) -> PathBuf {
        self.root.join("staging").join(v)
    }

    pub fn read_state(&self) -> Result<State, UpdateError> {
        let bytes = std::fs::read(self.root.join(STATE))?;
        let s: State =
            serde_json::from_slice(&bytes).map_err(|e| UpdateError::Io(format!("state: {e}")))?;
        if !valid_path(&s.current) {
            return Err(UpdateError::Io("state: invalid current version".into()));
        }
        Ok(s)
    }

    /// Atomic replace: readers see the old or the new file, never a partial one.
    pub fn write_state(&self, s: &State) -> Result<(), UpdateError> {
        use std::io::Write;
        let tmp = self.root.join(STATE_TMP);
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(
                &serde_json::to_vec_pretty(s).map_err(|e| UpdateError::Io(e.to_string()))?,
            )?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, self.root.join(STATE))?;
        Ok(())
    }

    /// A fresh install with one version (installer, tests).
    pub fn init(&self, version: &str) -> Result<(), UpdateError> {
        std::fs::create_dir_all(self.version_dir(version))?;
        self.write_state(&State {
            current: version.to_owned(),
            ..State::default()
        })
    }

    /// Downloads every file of `manifest` from `<files_base>/<version>/<path>` into
    /// staging with size limits and SHA-256 checks, moves the verified directory into
    /// `versions/` and records it as pending. Nothing referenced changes on failure.
    #[cfg(feature = "download")]
    pub async fn stage(
        &self,
        client: &reqwest::Client,
        files_base: &str,
        manifest: &Manifest,
    ) -> Result<(), UpdateError> {
        let v = manifest.version.as_str();
        if !valid_path(v) || v.contains('/') {
            return Err(UpdateError::Malformed(format!("version {v}")));
        }
        let state = self.read_state()?;
        if state.bad.iter().any(|b| b == v) {
            return Err(UpdateError::KnownBad(v.to_owned()));
        }
        if state.current == v || state.previous.as_deref() == Some(v) {
            return Err(UpdateError::NotNewer(v.to_owned()));
        }
        let staging = self.staging_dir(v);
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        std::fs::create_dir_all(&staging)?;
        for f in &manifest.files {
            let url = format!("{}/{v}/{}", files_base.trim_end_matches('/'), f.path);
            let dest = staging.join(&f.path);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            download(client, &url, &dest, f.size, &f.sha256, &f.path).await?;
        }
        let target = self.version_dir(v);
        if target.exists() {
            // Left over from an earlier attempt that never became pending.
            std::fs::remove_dir_all(&target)?;
        }
        std::fs::create_dir_all(self.root.join("versions"))?;
        std::fs::rename(&staging, &target)?;
        let mut state = self.read_state()?;
        state.pending = Some(Pending {
            version: v.to_owned(),
            attempts: 0,
        });
        self.write_state(&state)
    }

    /// The launcher's decision at start. A pending version gets exactly one trial: one
    /// that was already started (crash, power loss) or whose files are gone rolls back.
    pub fn launch_plan(&self) -> Result<LaunchPlan, UpdateError> {
        let mut s = self.read_state()?;
        let Some(mut p) = s.pending.take() else {
            return Ok(LaunchPlan::Run { version: s.current });
        };
        if p.attempts >= 1 || !self.exe(&p.version).is_file() {
            mark_bad(&mut s, &p.version);
            self.write_state(&s)?;
            return Ok(LaunchPlan::Run { version: s.current });
        }
        p.attempts += 1;
        let version = p.version.clone();
        s.pending = Some(p);
        self.write_state(&s)?;
        Ok(LaunchPlan::Trial {
            version,
            fallback: s.current,
        })
    }

    /// The new version runs well: make it current. True when this switched.
    pub fn confirm(&self, version: &str) -> Result<bool, UpdateError> {
        let mut s = self.read_state()?;
        if s.pending.as_ref().map(|p| p.version.as_str()) != Some(version) {
            return Ok(false);
        }
        s.pending = None;
        s.previous = Some(std::mem::replace(&mut s.current, version.to_owned()));
        self.write_state(&s)?;
        self.cleanup(&s);
        Ok(true)
    }

    /// The trial ended without a confirmation: back to the previous version.
    pub fn fail_trial(&self, version: &str) -> Result<(), UpdateError> {
        let mut s = self.read_state()?;
        if s.pending.as_ref().map(|p| p.version.as_str()) == Some(version) {
            s.pending = None;
            mark_bad(&mut s, version);
            self.write_state(&s)?;
        }
        Ok(())
    }

    /// Removes versions other than current/previous/pending and stale staging (best
    /// effort: a file still in use stays until the next time).
    pub fn cleanup(&self, s: &State) {
        let keep = |name: &str| {
            name == s.current
                || s.previous.as_deref() == Some(name)
                || s.pending.as_ref().is_some_and(|p| p.version == name)
        };
        if let Ok(dir) = std::fs::read_dir(self.root.join("versions")) {
            for e in dir.flatten() {
                if let Some(name) = e.file_name().to_str()
                    && !keep(name)
                {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        let _ = std::fs::remove_dir_all(self.root.join("staging"));
    }
}

fn mark_bad(s: &mut State, v: &str) {
    if !s.bad.iter().any(|b| b == v) {
        s.bad.push(v.to_owned());
    }
    if s.bad.len() > MAX_BAD {
        s.bad.remove(0);
    }
}

#[cfg(feature = "download")]
async fn download(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    size: u64,
    sha256: &str,
    name: &str,
) -> Result<(), UpdateError> {
    use std::io::Write;
    let dl = |e: reqwest::Error| UpdateError::Download(format!("{name}: {e}"));
    let mut resp = client
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(dl)?;
    if resp.content_length().is_some_and(|l| l != size) {
        return Err(UpdateError::SizeMismatch(name.to_owned()));
    }
    let mut ctx = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
    let mut file = std::io::BufWriter::new(std::fs::File::create(dest)?);
    let mut got: u64 = 0;
    while let Some(chunk) = resp.chunk().await.map_err(dl)? {
        got += chunk.len() as u64;
        // Never more than declared, whatever the server sends.
        if got > size {
            return Err(UpdateError::SizeMismatch(name.to_owned()));
        }
        ctx.update(&chunk);
        file.write_all(&chunk)?;
    }
    if got != size {
        return Err(UpdateError::SizeMismatch(name.to_owned()));
    }
    if crate::hex(ctx.finish().as_ref()) != sha256 {
        return Err(UpdateError::HashMismatch(name.to_owned()));
    }
    file.into_inner()
        .map_err(|e| UpdateError::Io(e.to_string()))?
        .sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(
            format!(
                "tp-update-{name}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
            )
            .replace([':', ' '], "_"),
        );
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn with_version(i: &Install, v: &str) {
        std::fs::create_dir_all(i.version_dir(v)).unwrap();
        std::fs::write(i.exe(v), b"exe").unwrap();
    }

    fn set_pending(i: &Install, v: &str) {
        with_version(i, v);
        let mut s = i.read_state().unwrap();
        s.pending = Some(Pending {
            version: v.into(),
            attempts: 0,
        });
        i.write_state(&s).unwrap();
    }

    #[test]
    fn from_exe_finds_only_real_installs() {
        let root = tmp("from-exe");
        let i = Install::new(&root);
        i.init("0.1.0").unwrap();
        let (found, v) = Install::from_exe(&i.exe("0.1.0")).unwrap();
        assert_eq!(found.root(), root.as_path());
        assert_eq!(v, "0.1.0");
        assert!(Install::from_exe(&root.join("turbo-pancake-desktop.exe")).is_none());
        assert!(Install::from_exe(&root.join("other").join("0.1.0").join("x.exe")).is_none());
        std::fs::remove_file(root.join(STATE)).unwrap();
        assert!(Install::from_exe(&i.exe("0.1.0")).is_none());
    }

    #[test]
    fn trial_confirm_switches_and_cleans_up() {
        let i = Install::new(tmp("confirm"));
        i.init("0.1.0").unwrap();
        with_version(&i, "0.1.0");
        assert_eq!(
            i.launch_plan().unwrap(),
            LaunchPlan::Run {
                version: "0.1.0".into()
            }
        );
        set_pending(&i, "0.1.1");
        assert_eq!(
            i.launch_plan().unwrap(),
            LaunchPlan::Trial {
                version: "0.1.1".into(),
                fallback: "0.1.0".into()
            }
        );
        assert!(
            !i.confirm("0.1.0").unwrap(),
            "only the pending version confirms"
        );
        assert!(i.confirm("0.1.1").unwrap());
        let s = i.read_state().unwrap();
        assert_eq!(
            (s.current.as_str(), s.previous.as_deref()),
            ("0.1.1", Some("0.1.0"))
        );
        assert_eq!(s.pending, None);
        // The next update removes the version before previous.
        set_pending(&i, "0.1.2");
        i.launch_plan().unwrap();
        assert!(i.confirm("0.1.2").unwrap());
        assert!(!i.version_dir("0.1.0").exists());
        assert!(i.version_dir("0.1.1").exists() && i.version_dir("0.1.2").exists());
    }

    #[test]
    fn failed_or_interrupted_trials_roll_back_and_are_not_retried() {
        let i = Install::new(tmp("rollback"));
        i.init("0.1.0").unwrap();
        with_version(&i, "0.1.0");
        // The trial process exits without confirming.
        set_pending(&i, "0.1.1");
        assert!(matches!(i.launch_plan().unwrap(), LaunchPlan::Trial { .. }));
        i.fail_trial("0.1.1").unwrap();
        let s = i.read_state().unwrap();
        assert_eq!(s.current, "0.1.0");
        assert_eq!(s.bad, vec!["0.1.1".to_string()]);
        // Crash/power loss during a trial: the next start rolls back.
        set_pending(&i, "0.1.2");
        assert!(matches!(i.launch_plan().unwrap(), LaunchPlan::Trial { .. }));
        assert_eq!(
            i.launch_plan().unwrap(),
            LaunchPlan::Run {
                version: "0.1.0".into()
            }
        );
        assert!(i.read_state().unwrap().bad.contains(&"0.1.2".to_string()));
        assert!(
            !i.confirm("0.1.2").unwrap(),
            "a rolled-back version cannot confirm later"
        );
        // A pending version whose files vanished.
        let mut s = i.read_state().unwrap();
        s.pending = Some(Pending {
            version: "0.1.3".into(),
            attempts: 0,
        });
        i.write_state(&s).unwrap();
        assert_eq!(
            i.launch_plan().unwrap(),
            LaunchPlan::Run {
                version: "0.1.0".into()
            }
        );
    }

    #[test]
    fn an_interrupted_state_write_leaves_the_old_state() {
        let i = Install::new(tmp("interrupted"));
        i.init("0.1.0").unwrap();
        // A crash after writing the temp file but before the rename.
        std::fs::write(i.root().join(STATE_TMP), b"{\"current\":\"0.9.").unwrap();
        assert_eq!(i.read_state().unwrap().current, "0.1.0");
        // The next write replaces both cleanly.
        let mut s = i.read_state().unwrap();
        s.bad.push("x".into());
        i.write_state(&s).unwrap();
        assert_eq!(i.read_state().unwrap().bad, vec!["x".to_string()]);
        // A corrupted state file is an error, not a silent default.
        std::fs::write(i.root().join(STATE), b"garbage").unwrap();
        assert!(i.read_state().is_err());
        std::fs::write(i.root().join(STATE), b"{\"current\":\"../x\"}").unwrap();
        assert!(i.read_state().is_err());
    }

    #[test]
    fn bad_list_is_bounded() {
        let mut s = State::default();
        for n in 0..(MAX_BAD + 3) {
            mark_bad(&mut s, &format!("0.0.{n}"));
        }
        mark_bad(&mut s, "0.0.18");
        assert_eq!(s.bad.len(), MAX_BAD);
        assert_eq!(s.bad.last().unwrap(), "0.0.18");
    }
}
