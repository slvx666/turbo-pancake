//! HTTPS + WSS (ADR 0014): a real server with a certificate from a private CA; clients
//! that trust that CA work, every other trust setup is refused — never a silent fallback.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::mpsc;
use turbo_pancake_app_server::Config;
use turbo_pancake_app_server::ratelimit::Limits;
use turbo_pancake_messenger_client::api::{Api, ApiError, UNTRUSTED_CERTIFICATE};
use turbo_pancake_messenger_client::{Command, Service, ServiceConfig, Status, Update};
use turbo_pancake_protocol::v1;

async fn tls_server(db: &str, names: &[&str]) -> (SocketAddr, turbo_pancake_tls::Pki) {
    let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    let pki = turbo_pancake_tls::generate(&names, 30).unwrap();
    let tls = turbo_pancake_tls::server_config(
        pki.server_chain_pem.as_bytes(),
        pki.server_key_pem.as_bytes(),
    )
    .unwrap();
    let config = Config {
        database_url: Some(db.to_owned()),
        database_pool: 4,
        limits: Limits::relaxed(),
        media_udp: false,
        tls: Some(tls),
        ..Default::default()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(turbo_pancake_app_server::serve(
        listener,
        config,
        std::future::pending(),
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    (addr, pki)
}

fn register_req(name: &str) -> v1::RegisterRequest {
    v1::RegisterRequest {
        username: name.into(),
        password: "correct horse".into(),
        ..Default::default()
    }
}

fn database() -> Option<String> {
    let url = turbo_pancake_pg_test::database_url();
    if url.is_none() {
        eprintln!("skipped: no PostgreSQL");
    }
    url
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn https_and_wss_with_a_private_ca() {
    let Some(db) = database() else { return };
    let (addr, pki) = tls_server(&db, &["127.0.0.1"]).await;
    let base = format!("https://{addr}");
    let trust = turbo_pancake_tls::client_config(Some(pki.ca_cert_pem.as_bytes())).unwrap();

    // Trusting the server's CA: works.
    let api = Api::with_tls(&base, trust.clone());
    let auth = api.register(&register_req("alice")).await.unwrap();
    assert!(!auth.token.is_empty());

    // Public web PKI only (no --server-ca): refused, and said why.
    let e = Api::new(&base)
        .register(&register_req("bob"))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, ApiError::Network(m) if m == UNTRUSTED_CERTIFICATE),
        "{e:?}"
    );

    // Some other CA: refused.
    let other = turbo_pancake_tls::generate(&["127.0.0.1".into()], 30).unwrap();
    let wrong = turbo_pancake_tls::client_config(Some(other.ca_cert_pem.as_bytes())).unwrap();
    let e = Api::with_tls(&base, wrong)
        .register(&register_req("bob"))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, ApiError::Network(m) if m == UNTRUSTED_CERTIFICATE),
        "{e:?}"
    );

    // Plain HTTP to the TLS port: no answer.
    let e = Api::new(&format!("http://{addr}"))
        .register(&register_req("bob"))
        .await
        .unwrap_err();
    assert!(matches!(e, ApiError::Network(_)), "{e:?}");

    // The client service over HTTPS + WSS: signs in and goes online.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let service = Service::start(
        ServiceConfig {
            server: base.clone(),
            device_name: "tls test".into(),
            client_version: "test".into(),
            cache_dir: None,
            view_limit: 50,
            tls: Some(trust),
        },
        move |u| {
            let _ = tx.send(u);
        },
    );
    service.send(Command::Login {
        username: "alice".into(),
        password: "correct horse".into(),
    });
    let online = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(u) = rx.recv().await {
            if matches!(u, Update::Status(Status::Online)) {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(online, "signed in over HTTPS and connected over WSS");
    tokio::task::spawn_blocking(move || drop(service))
        .await
        .unwrap();

    // Same service without the CA: never connects, reports the certificate.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let service = Service::start(
        ServiceConfig {
            server: base,
            device_name: "tls test".into(),
            client_version: "test".into(),
            cache_dir: None,
            view_limit: 50,
            tls: None,
        },
        move |u| {
            let _ = tx.send(u);
        },
    );
    service.send(Command::Login {
        username: "alice".into(),
        password: "correct horse".into(),
    });
    let reason = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(u) = rx.recv().await {
            if let Update::Error(e) = u {
                return e;
            }
        }
        String::new()
    })
    .await
    .unwrap();
    assert!(reason.contains(UNTRUSTED_CERTIFICATE), "{reason}");
    tokio::task::spawn_blocking(move || drop(service))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certificate_for_another_address_is_refused() {
    let Some(db) = database() else { return };
    // Certificate names 192.0.2.10 (documentation range), the client dials 127.0.0.1.
    let (addr, pki) = tls_server(&db, &["192.0.2.10"]).await;
    let trust = turbo_pancake_tls::client_config(Some(pki.ca_cert_pem.as_bytes())).unwrap();
    let e = Api::with_tls(&format!("https://{addr}"), trust)
        .register(&register_req("alice"))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, ApiError::Network(m) if m == UNTRUSTED_CERTIFICATE),
        "{e:?}"
    );
}
