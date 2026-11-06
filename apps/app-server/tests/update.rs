//! Update distribution end to end (ADR 0016): a release signed offline is served from
//! `--update-dir`; a client verifies the manifest and stages it over HTTP. Paths
//! outside the rules and servers without releases answer 404.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use turbo_pancake_app_server::Config;
use turbo_pancake_update::manifest::Expect;
use turbo_pancake_update::{FileEntry, Install, Manifest, TrustedKey, Version, sha256_file};

async fn start(config: Config) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(turbo_pancake_app_server::serve(
        listener,
        config,
        std::future::pending(),
    ));
    addr
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .use_preconfigured_tls((*turbo_pancake_tls::client_config(None).unwrap()).clone())
        .build()
        .unwrap()
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tp-server-update-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// What `turbo-pancake-update sign` writes, built with the library.
fn publish(dir: &Path, version: &str, exe: &[u8], pk8: &[u8]) {
    let files = dir.join("files").join(version);
    std::fs::create_dir_all(&files).unwrap();
    let path = files.join(turbo_pancake_update::CLIENT_EXE);
    std::fs::write(&path, exe).unwrap();
    let now = 1_800_000_000_000;
    let m = Manifest {
        format: 1,
        product: turbo_pancake_update::PRODUCT.into(),
        platform: turbo_pancake_update::PLATFORM.into(),
        version: version.into(),
        published_at_ms: now,
        expires_at_ms: now + 86_400_000,
        notes: "test".into(),
        files: vec![FileEntry {
            path: turbo_pancake_update::CLIENT_EXE.into(),
            size: exe.len() as u64,
            sha256: sha256_file(&path).unwrap(),
        }],
    };
    std::fs::write(
        dir.join("manifest.json"),
        turbo_pancake_update::sign(&m, pk8, "test").unwrap(),
    )
    .unwrap();
}

#[tokio::test]
async fn a_signed_release_is_served_verified_and_staged() {
    let dir = tmp("serve");
    let (pk8, public) = turbo_pancake_update::generate_key().unwrap();
    let exe = vec![7u8; 600 * 1024]; // several stream chunks
    publish(&dir, "0.2.0", &exe, &pk8);
    let addr = start(Config {
        update_dir: Some(dir.clone()),
        ..Config::default()
    })
    .await;
    let http = client();
    let env = http
        .get(format!("http://{addr}/v1/update/manifest"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let keys = [TrustedKey {
        id: "test".into(),
        public,
    }];
    let m = turbo_pancake_update::verify(
        &env,
        &keys,
        &Expect {
            product: turbo_pancake_update::PRODUCT,
            platform: turbo_pancake_update::PLATFORM,
            current: Version(0, 1, 0),
            bad: &[],
            now_ms: 1_800_000_001_000,
        },
    )
    .unwrap();
    let install = Install::new(tmp("install"));
    install.init("0.1.0").unwrap();
    install
        .stage(&http, &format!("http://{addr}/v1/update/files"), &m)
        .await
        .unwrap();
    assert_eq!(std::fs::read(install.exe("0.2.0")).unwrap(), exe);
    assert_eq!(
        install.read_state().unwrap().pending.unwrap().version,
        "0.2.0"
    );

    // Outside the rules: traversal, the manifest through the files route, bad versions.
    for path in [
        "0.2.0/..%2F..%2Fmanifest.json",
        "0.2.0/../../manifest.json",
        "..%2Fmanifest.json/x",
        "0.2/turbo-pancake-desktop.exe",
        "0.2.0/missing.exe",
        "0.2.0/a%5C..%5Cb",
    ] {
        let r = http
            .get(format!("http://{addr}/v1/update/files/{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "{path}");
    }
}

#[tokio::test]
async fn a_server_without_releases_answers_404() {
    let addr = start(Config::default()).await;
    let http = client();
    for path in ["manifest", "files/0.1.0/turbo-pancake-desktop.exe"] {
        let r = http
            .get(format!("http://{addr}/v1/update/{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "{path}");
    }
    // A configured directory with nothing published yet.
    let addr = start(Config {
        update_dir: Some(tmp("empty")),
        ..Config::default()
    })
    .await;
    let r = http
        .get(format!("http://{addr}/v1/update/manifest"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}
