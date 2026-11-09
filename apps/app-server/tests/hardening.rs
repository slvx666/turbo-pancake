//! Hardening tests (S8 step 4): bounded password hashing, pool exhaustion, request
//! timeouts, insufficient-resource database errors, the TCP connection cap and graceful
//! shutdown. Real server over TCP, real PostgreSQL (crates/pg-test).

mod support;

use std::time::{Duration, Instant};

use prost::Message as _;
use reqwest::{Method, StatusCode};
use sqlx::Connection as _;
use support::{Api, Server};
use turbo_pancake_app_server::api::{ApiError, db_unavailable};
use turbo_pancake_protocol::v1::{self, ApiErrorCode};

fn database() -> String {
    turbo_pancake_pg_test::database_url().expect("S8 step 4 requires PostgreSQL")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn password_hash_burst_is_bounded_and_refused_with_503_not_wrong_password() {
    let db = database();
    let srv = Server::start(Some(&db), |c| {
        c.hash_concurrency = 2;
        c.hash_queue = 4;
    })
    .await;
    let api = srv.api();
    api.register("alice").await;

    let logins: Vec<_> = (0..40)
        .map(|_| {
            let api = Api::new(srv.addr);
            tokio::spawn(async move { api.login("alice").await.unwrap() })
        })
        .collect();
    let (mut ok, mut busy) = (0, 0);
    for t in logins {
        let r = t.await.unwrap();
        match r.status {
            StatusCode::OK => ok += 1,
            StatusCode::SERVICE_UNAVAILABLE => {
                busy += 1;
                assert_eq!(r.error().code, ApiErrorCode::Unavailable as i32);
                assert_eq!(r.retry_after.as_deref(), Some("2"));
            }
            s => panic!("a correct password got {s}: {:?}", r.error()),
        }
    }
    let stats = api.stats().await;
    let h = &stats["password_hashing"];
    assert!(ok >= 6, "running + queued are served: {ok} ok, {busy} busy");
    assert!(busy >= 1, "a burst beyond the queue is refused: {stats}");
    assert_eq!(h["peak_running"], 2, "{stats}");
    assert_eq!(h["refused"].as_u64().unwrap(), busy as u64, "{stats}");
    // After the burst the server answers normally.
    assert_eq!(api.login("alice").await.unwrap().status, StatusCode::OK);
}

/// Holds the row lock of a conversation from outside the server, so a send blocks
/// inside its transaction while it holds a pool connection.
async fn lock_conversation(db: &str, conv: &str) -> sqlx::PgConnection {
    let mut c = sqlx::PgConnection::connect(db).await.unwrap();
    sqlx::query("BEGIN").execute(&mut c).await.unwrap();
    sqlx::query("SELECT id FROM conversations WHERE id = $1::uuid FOR UPDATE")
        .bind(conv)
        .execute(&mut c)
        .await
        .unwrap();
    c
}

async fn direct(api: &Api, me: &v1::AuthResponse, other: &v1::AuthResponse) -> String {
    let req = v1::CreateDirectRequest {
        user_id: other.user.as_ref().unwrap().user_id.clone(),
    };
    api.raw(
        Method::POST,
        "/v1/conversations/direct",
        Some(&me.token),
        Some(req.encode_to_vec()),
    )
    .await
    .unwrap()
    .decode::<v1::Conversation>()
    .conversation_id
}

fn send_req(nonce: &str) -> Vec<u8> {
    v1::SendMessageRequest {
        client_nonce: nonce.into(),
        body: "hello".into(),
        reply_to_id: String::new(),
    }
    .encode_to_vec()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pool_exhaustion_and_slow_requests_answer_503_then_recover() {
    let db = database();
    let srv = Server::start(Some(&db), |c| {
        c.database_pool = 1;
        c.database_acquire_timeout = Duration::from_millis(300);
        c.request_timeout = Duration::from_secs(1);
    })
    .await;
    let api = srv.api();
    let alice = api.register("alice").await;
    let bob = api.register("bob").await;
    let conv = direct(&api, &alice, &bob).await;
    let path = format!("/v1/conversations/{conv}/messages");

    let lock = lock_conversation(&db, &conv).await;
    let blocked = {
        let (api, token, path) = (Api::new(srv.addr), alice.token.clone(), path.clone());
        tokio::spawn(async move {
            let t0 = Instant::now();
            let r = api
                .raw(Method::POST, &path, Some(&token), Some(send_req("n-1")))
                .await
                .unwrap();
            (r, t0.elapsed())
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    // The only pool connection is stuck: other requests wait 300 ms, then 503.
    let t0 = Instant::now();
    let me = api
        .raw(Method::GET, "/v1/me", Some(&bob.token), None)
        .await
        .unwrap();
    assert_eq!(
        me.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{:?}",
        me.error()
    );
    assert_eq!(me.error().message, "database busy, try again");
    assert!(me.retry_after.is_some());
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    let (code, text) = api.text("/readyz").await;
    assert_eq!(
        (code, text.as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, "database unavailable")
    );
    assert_eq!(api.text("/healthz").await.0, StatusCode::OK);

    // The blocked send hits the request timeout; its transaction is dropped.
    let (r, took) = blocked.await.unwrap();
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{:?}", r.error());
    assert_eq!(r.error().message, "request timed out, try again");
    assert!(took < Duration::from_secs(3), "{took:?}");
    drop(lock);

    // Recovery: the pool works again and the timed-out send left nothing behind.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let r = api
            .raw(Method::GET, "/v1/me", Some(&bob.token), None)
            .await
            .unwrap();
        if r.status == StatusCode::OK {
            break;
        }
        assert!(Instant::now() < deadline, "no recovery: {:?}", r.error());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let page: v1::MessagePage = api
        .raw(Method::GET, &path, Some(&bob.token), None)
        .await
        .unwrap()
        .decode();
    assert!(page.messages.is_empty(), "rolled back: {page:?}");
    let sent: v1::ChatMessage = api
        .raw(
            Method::POST,
            &path,
            Some(&alice.token),
            Some(send_req("n-1")),
        )
        .await
        .unwrap()
        .decode();
    assert_eq!(sent.server_sequence, 1);
    assert_eq!(api.text("/readyz").await.0, StatusCode::OK);
}

#[tokio::test]
async fn insufficient_resource_errors_map_to_unavailable() {
    let db = database();
    let mut c = sqlx::PgConnection::connect(&db).await.unwrap();
    // Class 53 (insufficient resources) without filling a disk: a temp file limit makes
    // a sort that spills fail with 53400; disk_full (53100) is the same class.
    sqlx::query("SET work_mem = '64kB'")
        .execute(&mut c)
        .await
        .unwrap();
    sqlx::query("SET temp_file_limit = '64kB'")
        .execute(&mut c)
        .await
        .unwrap();
    let err = sqlx::query("SELECT g FROM generate_series(1, 500000) g ORDER BY g DESC")
        .fetch_all(&mut c)
        .await
        .unwrap_err();
    let code = err.as_database_error().and_then(|d| d.code()).unwrap();
    assert!(code.starts_with("53"), "{code}");
    assert!(db_unavailable(&err).is_some());
    assert_eq!(ApiError::from(err).code, ApiErrorCode::Unavailable);

    let syntax = sqlx::query("SELEC 1").execute(&mut c).await.unwrap_err();
    assert!(db_unavailable(&syntax).is_none());
    assert_eq!(ApiError::from(syntax).code, ApiErrorCode::Internal);
    assert_eq!(
        ApiError::from(sqlx::Error::PoolTimedOut).code,
        ApiErrorCode::Unavailable
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_connection_cap_makes_extra_clients_wait() {
    let srv = Server::start(None, |c| c.max_tcp_connections = 2).await;
    // Server::start's readiness probe used and closed a connection already.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let a = tokio::net::TcpStream::connect(srv.addr).await.unwrap();
    let b = tokio::net::TcpStream::connect(srv.addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let api = srv.api();
    let third = tokio::time::timeout(
        Duration::from_millis(800),
        api.http.get(api.url("/healthz")).send(),
    )
    .await;
    assert!(third.is_err(), "a third connection must wait: {third:?}");
    drop(a);
    let r = tokio::time::timeout(
        Duration::from_secs(3),
        api.http.get(api.url("/healthz")).send(),
    )
    .await
    .expect("served once a slot is free")
    .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    drop(b);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn graceful_shutdown_finishes_in_flight_requests() {
    let db = database();
    let mut srv = Server::start(Some(&db), |c| {
        c.shutdown_grace = Duration::from_secs(10);
    })
    .await;
    let api = srv.api();
    let alice = api.register("alice").await;
    let bob = api.register("bob").await;
    let conv = direct(&api, &alice, &bob).await;
    // The send is inside the server (waiting for the row lock) when the stop comes.
    let lock = lock_conversation(&db, &conv).await;
    let in_flight = {
        let (api, token) = (Api::new(srv.addr), alice.token.clone());
        let path = format!("/v1/conversations/{conv}/messages");
        tokio::spawn(async move {
            api.raw(Method::POST, &path, Some(&token), Some(send_req("g-1")))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    srv.request_stop();
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Stopping: no new connections, the server task still waits for the request.
    assert!(!srv.task.as_ref().unwrap().is_finished());
    assert!(tokio::net::TcpStream::connect(srv.addr).await.is_err());
    drop(lock);
    let r = in_flight
        .await
        .unwrap()
        .expect("in-flight request answered");
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.error());
    let ended = tokio::time::timeout(Duration::from_secs(5), srv.task.take().unwrap())
        .await
        .expect("server stopped after the request")
        .unwrap();
    assert!(ended.is_ok(), "{ended:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_grace_bounds_a_stuck_request() {
    let db = database();
    let mut srv = Server::start(Some(&db), |c| {
        c.request_timeout = Duration::from_secs(60);
        c.shutdown_grace = Duration::from_millis(300);
    })
    .await;
    let api = srv.api();
    let alice = api.register("alice").await;
    let bob = api.register("bob").await;
    let conv = direct(&api, &alice, &bob).await;
    let lock = lock_conversation(&db, &conv).await;
    let stuck = {
        let (api, token) = (Api::new(srv.addr), alice.token.clone());
        let path = format!("/v1/conversations/{conv}/messages");
        tokio::spawn(async move {
            api.raw(Method::POST, &path, Some(&token), Some(send_req("s-1")))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let t0 = Instant::now();
    srv.request_stop();
    let ended = tokio::time::timeout(Duration::from_secs(5), srv.task.take().unwrap())
        .await
        .expect("grace bounds the shutdown")
        .unwrap();
    assert!(ended.is_ok(), "{ended:?}");
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    // `serve` returned; the binary's `main` then exits, which ends the stuck connection
    // with the runtime. In this test process its task would live on, so stop it here.
    stuck.abort();
    drop(lock);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_request_headers_are_cut_off() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let srv = Server::start(None, |c| {
        c.header_read_timeout = Duration::from_millis(500);
    })
    .await;
    let mut slow = tokio::net::TcpStream::connect(srv.addr).await.unwrap();
    slow.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n")
        .await
        .unwrap();
    let t0 = Instant::now();
    let mut buf = [0u8; 512];
    let closed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match slow.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {} // hyper may answer 408 first
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "a half-sent request must be closed");
    assert!(
        t0.elapsed() >= Duration::from_millis(400),
        "{:?}",
        t0.elapsed()
    );
    // A normal request on a new connection is served.
    let api = srv.api();
    assert_eq!(api.text("/healthz").await.0, StatusCode::OK);
}
