//! Update checks and staged installs for the desktop client (S7 step 5, ADR 0016).
//! A worker thread fetches the signed manifest from the server, verifies it against the
//! keys pinned at build time and, on request, stages the release into the install
//! layout. Switching happens through the launcher on restart; the new version confirms
//! itself with [`confirm_if_pending`].

use std::time::Duration;

use turbo_pancake_update::manifest::Expect;
use turbo_pancake_update::{Install, Manifest, TrustedKey, UpdateError, Version};

/// Version of this build: `TURBO_PANCAKE_BUILD_VERSION` at compile time, else the crate.
pub fn build_version() -> &'static str {
    option_env!("TURBO_PANCAKE_BUILD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// Keys pinned at compile time (`TURBO_PANCAKE_UPDATE_PUBKEYS`); empty = not configured.
pub fn pinned_keys() -> Vec<TrustedKey> {
    option_env!("TURBO_PANCAKE_UPDATE_PUBKEYS")
        .map(|s| TrustedKey::parse_list(s).unwrap_or_default())
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    NotConfigured,
    Checking,
    UpToDate {
        version: String,
    },
    /// `installable`: running from an install layout (else only reported).
    Available {
        version: String,
        notes: String,
        installable: bool,
    },
    Downloading {
        version: String,
    },
    Ready {
        version: String,
    },
    Failed(String),
}

impl UpdateStatus {
    pub fn text(&self) -> String {
        match self {
            UpdateStatus::NotConfigured => "Обновления не настроены в этой сборке".into(),
            UpdateStatus::Checking => "Проверка обновлений…".into(),
            UpdateStatus::UpToDate { version } => format!("Установлена последняя версия {version}"),
            UpdateStatus::Available {
                version,
                installable: true,
                ..
            } => format!("Доступна версия {version}"),
            UpdateStatus::Available { version, .. } => format!(
                "Доступна версия {version}; эта копия запущена не из установки — обновите её установщиком"
            ),
            UpdateStatus::Downloading { version } => {
                format!("Загрузка и проверка версии {version}…")
            }
            UpdateStatus::Ready { version } => format!("Версия {version} загружена и проверена"),
            UpdateStatus::Failed(e) => format!("Не удалось обновить: {e}"),
        }
    }

    /// Label of the action button ("" = none).
    pub fn button(&self) -> &'static str {
        match self {
            UpdateStatus::NotConfigured
            | UpdateStatus::Checking
            | UpdateStatus::Downloading { .. } => "",
            UpdateStatus::Available {
                installable: true, ..
            } => "Установить и перезапустить",
            UpdateStatus::Ready { .. } => "Перезапустить",
            UpdateStatus::Failed(_) => "Повторить",
            _ => "Проверить",
        }
    }

    pub fn highlighted(&self) -> bool {
        matches!(
            self,
            UpdateStatus::Available {
                installable: true,
                ..
            } | UpdateStatus::Ready { .. }
        )
    }

    pub fn name(&self) -> &'static str {
        match self {
            UpdateStatus::NotConfigured => "not_configured",
            UpdateStatus::Checking => "checking",
            UpdateStatus::UpToDate { .. } => "up_to_date",
            UpdateStatus::Available { .. } => "available",
            UpdateStatus::Downloading { .. } => "downloading",
            UpdateStatus::Ready { .. } => "ready",
            UpdateStatus::Failed(_) => "failed",
        }
    }
}

/// Human text for a failed check/install (no internals beyond the reason).
pub fn error_text(e: &UpdateError) -> String {
    match e {
        UpdateError::BadSignature | UpdateError::UnknownKey(_) => {
            "подпись обновления не прошла проверку".into()
        }
        UpdateError::HashMismatch(_) | UpdateError::SizeMismatch(_) => {
            "загруженный файл повреждён".into()
        }
        UpdateError::Expired => "описание обновления устарело".into(),
        UpdateError::Download(_) => "нет связи с сервером обновлений".into(),
        other => other.to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateCommand {
    Check,
    Install,
}

pub struct UpdaterConfig {
    /// `https://host:port` of the app server.
    pub http_base: String,
    pub tls: std::sync::Arc<turbo_pancake_tls::rustls::ClientConfig>,
    /// The install and this executable's version directory, when installed.
    pub install: Option<(Install, String)>,
    pub keys: Vec<TrustedKey>,
    pub build_version: String,
    pub check_every: Duration,
}

impl UpdaterConfig {
    /// The version updates must exceed: the installed directory's, else the build's.
    pub fn current(&self) -> Option<Version> {
        match &self.install {
            Some((_, dir)) => Version::parse(dir),
            None => Version::parse(&self.build_version),
        }
    }
}

pub struct Updater {
    tx: tokio::sync::mpsc::UnboundedSender<UpdateCommand>,
}

impl Updater {
    pub fn start(
        cfg: UpdaterConfig,
        on_status: impl Fn(UpdateStatus) + Send + 'static,
    ) -> std::io::Result<Self> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("updater".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => return on_status(UpdateStatus::Failed(e.to_string())),
                };
                rt.block_on(worker(cfg, rx, on_status));
            })?;
        Ok(Self { tx })
    }

    pub fn send(&self, c: UpdateCommand) {
        let _ = self.tx.send(c);
    }
}

async fn worker(
    cfg: UpdaterConfig,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<UpdateCommand>,
    on_status: impl Fn(UpdateStatus),
) {
    if cfg.keys.is_empty() {
        on_status(UpdateStatus::NotConfigured);
        // Commands are ignored: nothing can be verified.
        while rx.recv().await.is_some() {}
        return;
    }
    let Ok(http) = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(600))
        .use_preconfigured_tls((*cfg.tls).clone())
        .build()
    else {
        on_status(UpdateStatus::Failed("HTTP client".into()));
        return;
    };
    let mut verified: Option<Manifest> = None;
    let mut ready = false;
    let mut timer = tokio::time::interval(cfg.check_every);
    loop {
        let cmd = tokio::select! {
            _ = timer.tick() => UpdateCommand::Check,
            c = rx.recv() => match c { Some(c) => c, None => return },
        };
        if ready {
            // Staged and waiting for the restart: nothing else to do.
            continue;
        }
        match cmd {
            UpdateCommand::Check => {
                on_status(UpdateStatus::Checking);
                let status = match check(&http, &cfg).await {
                    Ok(Some(m)) => {
                        let s = UpdateStatus::Available {
                            version: m.version.clone(),
                            notes: m.notes.clone(),
                            installable: cfg.install.is_some(),
                        };
                        verified = Some(m);
                        s
                    }
                    Ok(None) => {
                        verified = None;
                        UpdateStatus::UpToDate {
                            version: cfg
                                .current()
                                .map_or(cfg.build_version.clone(), |v| v.to_string()),
                        }
                    }
                    Err(e) => UpdateStatus::Failed(error_text(&e)),
                };
                on_status(status);
            }
            UpdateCommand::Install => {
                let (Some(m), Some((install, _))) = (&verified, &cfg.install) else {
                    continue;
                };
                on_status(UpdateStatus::Downloading {
                    version: m.version.clone(),
                });
                let base = format!("{}/v1/update/files", cfg.http_base.trim_end_matches('/'));
                match install.stage(&http, &base, m).await {
                    Ok(()) => {
                        ready = true;
                        on_status(UpdateStatus::Ready {
                            version: m.version.clone(),
                        });
                    }
                    Err(e) => on_status(UpdateStatus::Failed(error_text(&e))),
                }
            }
        }
    }
}

/// The verified newer release, None when there is none (404) or it is not newer.
async fn check(
    http: &reqwest::Client,
    cfg: &UpdaterConfig,
) -> Result<Option<Manifest>, UpdateError> {
    let current = cfg
        .current()
        .ok_or_else(|| UpdateError::Malformed("own version".into()))?;
    let url = format!("{}/v1/update/manifest", cfg.http_base.trim_end_matches('/'));
    let dl = |e: reqwest::Error| UpdateError::Download(e.to_string());
    let resp = http.get(url).send().await.map_err(dl)?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let resp = resp.error_for_status().map_err(dl)?;
    if resp
        .content_length()
        .is_some_and(|l| l > turbo_pancake_update::manifest::MAX_ENVELOPE_BYTES as u64)
    {
        return Err(UpdateError::TooLarge);
    }
    let body = resp.bytes().await.map_err(dl)?;
    let bad = match &cfg.install {
        Some((i, _)) => i.read_state()?.bad,
        None => Vec::new(),
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    match turbo_pancake_update::verify(
        &body,
        &cfg.keys,
        &Expect {
            product: turbo_pancake_update::PRODUCT,
            platform: turbo_pancake_update::PLATFORM,
            current,
            bad: &bad,
            now_ms,
        },
    ) {
        Ok(m) => Ok(Some(m)),
        // The published release is ours already or was rolled back: nothing to offer.
        Err(UpdateError::NotNewer(_) | UpdateError::KnownBad(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Run by a freshly started client: when it is the pending trial version, make it
/// current. True when this start completed an update.
pub fn confirm_if_pending(exe: &std::path::Path) -> Result<Option<String>, UpdateError> {
    let Some((install, version)) = Install::from_exe(exe) else {
        return Ok(None);
    };
    Ok(install.confirm(&version)?.then_some(version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_have_texts_and_actions() {
        assert_eq!(UpdateStatus::NotConfigured.button(), "");
        let a = UpdateStatus::Available {
            version: "0.2.0".into(),
            notes: String::new(),
            installable: true,
        };
        assert_eq!(a.text(), "Доступна версия 0.2.0");
        assert_eq!(a.button(), "Установить и перезапустить");
        assert!(a.highlighted());
        let b = UpdateStatus::Available {
            version: "0.2.0".into(),
            notes: String::new(),
            installable: false,
        };
        assert_eq!(b.button(), "Проверить");
        assert!(!b.highlighted());
        assert_eq!(
            UpdateStatus::Ready {
                version: "0.2.0".into()
            }
            .button(),
            "Перезапустить"
        );
        assert_eq!(UpdateStatus::Failed("x".into()).button(), "Повторить");
        assert_eq!(
            error_text(&UpdateError::BadSignature),
            "подпись обновления не прошла проверку"
        );
        assert_eq!(
            error_text(&UpdateError::HashMismatch("a".into())),
            "загруженный файл повреждён"
        );
    }

    #[test]
    fn build_version_parses() {
        assert!(Version::parse(build_version()).is_some());
    }
}
