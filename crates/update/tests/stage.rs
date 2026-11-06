//! Staged download against a real local HTTP server: verified files land in
//! `versions/`, while truncated, oversized and tampered files leave nothing referenced.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use turbo_pancake_update::{FileEntry, Install, Manifest, UpdateError, hex};

/// What the server sends for each path (may differ from the manifest on purpose).
type Files = Arc<Mutex<std::collections::HashMap<String, (Vec<u8>, Option<u64>)>>>;

async fn file(State(files): State<Files>, Path(path): Path<String>) -> impl IntoResponse {
    let Some((body, length)) = files.lock().unwrap().get(&path).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut h = HeaderMap::new();
    if let Some(l) = length {
        h.insert(header::CONTENT_LENGTH, l.to_string().parse().unwrap());
    }
    (h, body).into_response()
}

async fn server() -> (String, Files) {
    let files: Files = Arc::default();
    let app = Router::new()
        .route("/files/{*path}", get(file))
        .with_state(files.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/files"), files)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .use_preconfigured_tls((*turbo_pancake_tls::client_config(None).unwrap()).clone())
        .build()
        .unwrap()
}

fn sha(b: &[u8]) -> String {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("tp-sha-{}-{n}", std::process::id()));
    std::fs::write(&p, b).unwrap();
    let h = turbo_pancake_update::sha256_file(&p).unwrap();
    let _ = std::fs::remove_file(&p);
    h
}

#[test]
fn sha256_matches_a_known_vector() {
    assert_eq!(
        sha(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(hex(&[0, 255, 16]), "00ff10");
}

fn root(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tp-stage-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn manifest(version: &str, files: &[(&str, &[u8])]) -> Manifest {
    Manifest {
        format: 1,
        product: turbo_pancake_update::PRODUCT.into(),
        platform: turbo_pancake_update::PLATFORM.into(),
        version: version.into(),
        published_at_ms: 0,
        expires_at_ms: i64::MAX,
        notes: String::new(),
        files: files
            .iter()
            .map(|(p, b)| FileEntry {
                path: (*p).into(),
                size: b.len() as u64,
                sha256: sha(b),
            })
            .collect(),
    }
}

#[tokio::test]
async fn verified_files_are_staged_and_recorded_as_pending() {
    let (base, files) = server().await;
    let exe = b"new client".repeat(1000);
    let data = b"resource".to_vec();
    files.lock().unwrap().insert(
        "0.1.1/turbo-pancake-desktop.exe".into(),
        (exe.clone(), None),
    );
    files.lock().unwrap().insert(
        "0.1.1/res/data.bin".into(),
        (data.clone(), Some(data.len() as u64)),
    );
    let i = Install::new(root("ok"));
    i.init("0.1.0").unwrap();
    let m = manifest(
        "0.1.1",
        &[("turbo-pancake-desktop.exe", &exe), ("res/data.bin", &data)],
    );
    i.stage(&client(), &base, &m).await.unwrap();
    assert_eq!(std::fs::read(i.exe("0.1.1")).unwrap(), exe);
    assert_eq!(
        std::fs::read(i.version_dir("0.1.1").join("res/data.bin")).unwrap(),
        data
    );
    let s = i.read_state().unwrap();
    assert_eq!(s.current, "0.1.0");
    assert_eq!(s.pending.unwrap().version, "0.1.1");
    assert!(!i.root().join("staging/0.1.1").exists());
}

#[tokio::test]
async fn truncated_oversized_tampered_and_missing_files_are_rejected() {
    let (base, files) = server().await;
    let exe = b"0123456789".repeat(100);
    let client = client();
    let i = Install::new(root("bad"));
    i.init("0.1.0").unwrap();
    let m = manifest("0.1.1", &[("turbo-pancake-desktop.exe", &exe)]);
    let key = "0.1.1/turbo-pancake-desktop.exe".to_string();
    type Case = (Vec<u8>, Option<u64>, fn(&UpdateError) -> bool);
    let cases: Vec<Case> = vec![
        // Truncated body (server closes early, correct header absent).
        (exe[..500].to_vec(), None, |e| {
            matches!(e, UpdateError::SizeMismatch(_))
        }),
        // Larger body than declared.
        ([exe.clone(), b"extra".to_vec()].concat(), None, |e| {
            matches!(e, UpdateError::SizeMismatch(_))
        }),
        // Wrong Content-Length announced.
        (exe.clone(), Some(5), |e| {
            matches!(e, UpdateError::SizeMismatch(_) | UpdateError::Download(_))
        }),
        // Same size, different bytes.
        (exe.iter().map(|b| b ^ 1).collect(), None, |e| {
            matches!(e, UpdateError::HashMismatch(_))
        }),
    ];
    for (body, length, expected) in cases {
        files.lock().unwrap().insert(key.clone(), (body, length));
        let err = i.stage(&client, &base, &m).await.unwrap_err();
        assert!(expected(&err), "{err:?}");
        let s = i.read_state().unwrap();
        assert_eq!((s.current.as_str(), s.pending.as_ref()), ("0.1.0", None));
        assert!(
            !i.version_dir("0.1.1").exists(),
            "nothing referenced on {err:?}"
        );
    }
    files.lock().unwrap().remove(&key);
    assert!(matches!(
        i.stage(&client, &base, &m).await,
        Err(UpdateError::Download(_))
    ));
    // After all failures the good file still installs (staging is reset each time).
    files.lock().unwrap().insert(key, (exe.clone(), None));
    i.stage(&client, &base, &m).await.unwrap();
    assert_eq!(std::fs::read(i.exe("0.1.1")).unwrap(), exe);
}

#[tokio::test]
async fn bad_and_installed_versions_are_not_staged() {
    let (base, _files) = server().await;
    let i = Install::new(root("policy"));
    i.init("0.1.0").unwrap();
    let mut s = i.read_state().unwrap();
    s.bad.push("0.1.1".into());
    i.write_state(&s).unwrap();
    let m = manifest("0.1.1", &[("turbo-pancake-desktop.exe", b"x")]);
    assert_eq!(
        i.stage(&client(), &base, &m).await,
        Err(UpdateError::KnownBad("0.1.1".into()))
    );
    let m = manifest("0.1.0", &[("turbo-pancake-desktop.exe", b"x")]);
    assert_eq!(
        i.stage(&client(), &base, &m).await,
        Err(UpdateError::NotNewer("0.1.0".into()))
    );
}
