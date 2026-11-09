//! Database outage (S8 step 4): this binary owns its PostgreSQL cluster and stops it, so
//! it holds exactly one test. Requests get `503` + `Retry-After` (never a crash, never a
//! wrong-password answer), readiness reports the database, liveness stays up, and the
//! server recovers by itself when the database returns.

mod support;

use std::time::{Duration, Instant};

use reqwest::{Method, StatusCode};
use support::Server;
use turbo_pancake_protocol::v1::{self, ApiErrorCode};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn database_outage_answers_503_and_recovers() {
    let db = turbo_pancake_pg_test::database_url().expect("S8 step 4 requires PostgreSQL");
    let srv = Server::start(Some(&db), |c| {
        c.database_acquire_timeout = Duration::from_secs(1);
    })
    .await;
    let api = srv.api();
    let alice = api.register("alice").await;
    assert_eq!(api.text("/readyz").await.0, StatusCode::OK);

    turbo_pancake_pg_test::stop();
    let t0 = Instant::now();
    let me = api
        .raw(Method::GET, "/v1/me", Some(&alice.token), None)
        .await
        .unwrap();
    assert_eq!(
        me.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{:?}",
        me.error()
    );
    assert_eq!(me.error().code, ApiErrorCode::Unavailable as i32);
    assert!(me.retry_after.is_some());
    assert!(t0.elapsed() < Duration::from_secs(4), "{:?}", t0.elapsed());
    let login = api.login("alice").await.unwrap();
    assert_eq!(
        login.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "not 401: {:?}",
        login.error()
    );
    assert_eq!(
        api.text("/readyz").await,
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "database unavailable".into()
        )
    );
    assert_eq!(api.text("/healthz").await.0, StatusCode::OK);

    turbo_pancake_pg_test::restart();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let r = api
            .raw(Method::GET, "/v1/me", Some(&alice.token), None)
            .await
            .unwrap();
        if r.status == StatusCode::OK {
            let user: v1::User = r.decode();
            assert_eq!(user.username, "alice");
            break;
        }
        assert!(Instant::now() < deadline, "no recovery: {:?}", r.error());
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(api.text("/readyz").await.0, StatusCode::OK);
    assert_eq!(api.login("alice").await.unwrap().status, StatusCode::OK);
}
