//! Backup and restore (S8 step 5): the real `deploy/linux/backup.sh` and `restore.sh`
//! (bash: Git's bash on Windows, never WSL) against a database filled through the API.
//! Checks the archive, the retention rule, a restore into a new database with
//! `check-db`, identical table fingerprints, that a second server on the restored copy
//! serves the same history to the same sessions, that a message deleted before the
//! backup is not in it (and one deleted after it still is: the retention cycle of
//! docs/BACKUP.md), and that `check-db` catches a broken copy.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use prost::Message as _;
use reqwest::Method;
use support::{Api, Server};
use turbo_pancake_protocol::v1;

/// Git's bash. On Windows `bash` on PATH is usually WSL's launcher, which must not run
/// these scripts; Git for Windows keeps bash next to `git.exe` (`cmd\\..\\bin`).
fn bash() -> PathBuf {
    if let Some(b) = std::env::var_os("TURBO_PANCAKE_BASH") {
        return b.into();
    }
    if cfg!(windows) {
        let out = Command::new("where")
            .arg("git")
            .output()
            .expect("where git");
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let git = Path::new(line.trim());
            if let Some(root) = git.parent().and_then(Path::parent) {
                for cand in [
                    root.join("bin").join("bash.exe"),
                    root.join("usr/bin/bash.exe"),
                ] {
                    if cand.is_file() {
                        return cand;
                    }
                }
            }
        }
        panic!("Git's bash not found: install Git for Windows or set TURBO_PANCAKE_BASH");
    }
    "bash".into()
}

fn tool(name: &str) -> String {
    let dir = turbo_pancake_pg_test::tools().expect("pg-test cluster");
    dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
        .to_string_lossy()
        .replace('\\', "/")
}

fn script(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy/linux")
        .join(name)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Runs a deploy script with the libpq environment of the test cluster.
fn run(script_name: &str, args: &[&str], env: &[(&str, String)]) -> (bool, String, String) {
    let port = turbo_pancake_pg_test::port().unwrap();
    let mut cmd = Command::new(bash());
    cmd.arg(script(script_name))
        .args(args)
        .env("PGHOST", "127.0.0.1")
        .env("PGPORT", port.to_string())
        .env("PGUSER", "postgres")
        .env("PG_DUMP", tool("pg_dump"))
        .env("PG_RESTORE", tool("pg_restore"))
        .env("PSQL", tool("psql"))
        .env("CREATEDB", tool("createdb"));
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run bash");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn db_name(url: &str) -> String {
    url.rsplit('/').next().unwrap().to_owned()
}

async fn post<T: prost::Message + Default>(
    api: &Api,
    path: &str,
    token: &str,
    body: impl prost::Message,
) -> T {
    api.raw(Method::POST, path, Some(token), Some(body.encode_to_vec()))
        .await
        .unwrap()
        .decode()
}

async fn send(api: &Api, conv: &str, token: &str, nonce: &str, body: &str) -> v1::ChatMessage {
    post(
        api,
        &format!("/v1/conversations/{conv}/messages"),
        token,
        v1::SendMessageRequest {
            client_nonce: nonce.into(),
            body: body.into(),
            reply_to_id: String::new(),
        },
    )
    .await
}

async fn delete(api: &Api, conv: &str, token: &str, id: &str) {
    let r = api
        .raw(
            Method::DELETE,
            &format!("/v1/conversations/{conv}/messages/{id}"),
            Some(token),
            None,
        )
        .await
        .unwrap();
    assert!(r.status.is_success(), "{:?}", r.error());
}

async fn history(api: &Api, conv: &str, token: &str) -> Vec<u8> {
    api.raw(
        Method::GET,
        &format!("/v1/conversations/{conv}/messages?limit=100"),
        Some(token),
        None,
    )
    .await
    .unwrap()
    .body
}

async fn report(url: &str) -> serde_json::Value {
    let pool = sqlx::PgPool::connect(url).await.unwrap();
    let r = turbo_pancake_app_server::integrity::check(&pool)
        .await
        .unwrap();
    pool.close().await;
    r
}

/// The plain SQL of a dump (the archive itself is compressed).
fn dump_sql(dump: &Path) -> String {
    let out = Command::new(tool("pg_restore"))
        .arg("--file=-")
        .arg(dump)
        .output()
        .expect("pg_restore to SQL");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backup_retention_restore_and_check() {
    let url = turbo_pancake_pg_test::database_url().expect("S8 step 5 requires PostgreSQL");
    let srv = Server::start(Some(&url), |_| {}).await;
    let api = srv.api();
    let alice = api.register("alice").await;
    let bob = api.register("bob").await;
    let carol = api.register("carol").await;
    let group: v1::Conversation = post(
        &api,
        "/v1/conversations/group",
        &alice.token,
        v1::CreateGroupRequest {
            title: "Backup group".into(),
            usernames: vec!["bob".into(), "carol".into()],
        },
    )
    .await;
    let g = group.conversation_id.clone();
    let direct: v1::Conversation = post(
        &api,
        "/v1/conversations/direct",
        &alice.token,
        v1::CreateDirectRequest {
            user_id: bob.user.as_ref().unwrap().user_id.clone(),
        },
    )
    .await;
    let d = direct.conversation_id.clone();
    let first = send(&api, &g, &alice.token, "b-1", "hello group").await;
    let _reply: v1::ChatMessage = post(
        &api,
        &format!("/v1/conversations/{g}/messages"),
        &bob.token,
        v1::SendMessageRequest {
            client_nonce: "b-2".into(),
            body: "a reply ✓".into(),
            reply_to_id: first.message_id.clone(),
        },
    )
    .await;
    let edited = send(&api, &g, &carol.token, "b-3", "typo").await;
    let r = api
        .raw(
            Method::PATCH,
            &format!("/v1/conversations/{g}/messages/{}", edited.message_id),
            Some(&carol.token),
            Some(
                v1::EditMessageRequest {
                    body: "fixed".into(),
                }
                .encode_to_vec(),
            ),
        )
        .await
        .unwrap();
    assert!(r.status.is_success());
    let gone_before = send(&api, &g, &bob.token, "b-4", "deleted-before-backup-7f3a").await;
    delete(&api, &g, &bob.token, &gone_before.message_id).await;
    let gone_after = send(&api, &d, &alice.token, "b-5", "deleted-after-backup-9c1e").await;
    let _: v1::Receipt = post(
        &api,
        &format!("/v1/conversations/{g}/read"),
        &carol.token,
        v1::MarkReadRequest {
            delivered_sequence: 4,
            read_sequence: 3,
        },
    )
    .await;
    let source = report(&url).await;
    assert_eq!(source["ok"], true, "{source}");

    // --- backup.sh, with old dumps for the retention rule -------------------------
    let dir = std::env::temp_dir().join(format!("tp-backup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let old = SystemTime::now() - Duration::from_secs(40 * 24 * 3600);
    for day in 1..=4 {
        let f = dir.join(format!("turbo-pancake-2020010{day}T000000Z.dump"));
        std::fs::write(&f, b"old").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
    let dir_s = dir.to_string_lossy().replace('\\', "/");
    let env = [
        ("TURBO_PANCAKE_BACKUP_DIR", dir_s.clone()),
        ("TURBO_PANCAKE_BACKUP_DB", db_name(&url)),
        ("TURBO_PANCAKE_BACKUP_KEEP_DAYS", "14".into()),
        ("TURBO_PANCAKE_BACKUP_KEEP_MIN", "3".into()),
    ];
    let (ok, out, err) = run("backup.sh", &[], &env);
    assert!(ok, "backup.sh failed:\n{out}\n{err}");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    // Newest 3 kept (the new dump + 2 old ones), older ones past KEEP_DAYS removed, no
    // partial file left.
    assert_eq!(names.len(), 3, "{names:?}");
    assert_eq!(
        &names[..2],
        [
            "turbo-pancake-20200103T000000Z.dump",
            "turbo-pancake-20200104T000000Z.dump"
        ]
    );
    assert!(!names.iter().any(|n| n.ends_with(".partial")));
    let dump = dir.join(&names[2]);
    assert!(out.contains("2 removed"), "{out}");

    // A deletion after the backup: still in the old archive until it ages out.
    delete(&api, &d, &alice.token, &gone_after.message_id).await;
    let sql = dump_sql(&dump);
    assert!(sql.contains("hello group") && sql.contains("a reply ✓"));
    assert!(
        !sql.contains("deleted-before-backup-7f3a"),
        "deleted before: not in the dump"
    );
    assert!(
        sql.contains("deleted-after-backup-9c1e"),
        "deleted after: in the older dump"
    );
    assert!(
        !sql.contains("correct horse battery"),
        "no plaintext password"
    );

    // --- restore.sh into a new database ------------------------------------------
    let target = format!("{}_restored", db_name(&url));
    let check_url = url.replace(&db_name(&url), &target);
    let renv = [
        (
            "TURBO_PANCAKE_SERVER_BIN",
            env!("CARGO_BIN_EXE_turbo-pancake-app-server").replace('\\', "/"),
        ),
        ("TURBO_PANCAKE_CHECK_URL", check_url.clone()),
    ];
    let dump_s = dump.to_string_lossy().replace('\\', "/");
    let (ok, out, err) = run("restore.sh", &[&dump_s, &target], &renv);
    assert!(ok, "restore.sh failed:\n{out}\n{err}");
    let printed: serde_json::Value = serde_json::from_str(&out[out.find('{').unwrap()..]).unwrap();
    assert_eq!(printed["ok"], true, "{printed}");
    // Never over an existing database.
    let (ok, _, err) = run("restore.sh", &[&dump_s, &target], &renv);
    assert!(!ok && err.contains("refusing to overwrite"), "{err}");

    // The copy equals the source as it was at backup time: everything but the one
    // message deleted afterwards.
    let restored = report(&check_url).await;
    for t in [
        "users",
        "sessions",
        "conversations",
        "conversation_members",
        "blocks",
    ] {
        assert_eq!(
            restored["tables"][t], source["tables"][t],
            "{t}: {restored} vs {source}"
        );
    }
    assert_eq!(restored["tables"]["messages"], source["tables"]["messages"]);
    assert_eq!(restored["migrations_applied"], source["migrations_applied"]);

    // A server on the restored copy serves the same sessions and history.
    let srv2 = Server::start(Some(&check_url), |_| {}).await;
    let api2 = srv2.api();
    for token in [&alice.token, &bob.token, &carol.token] {
        assert_eq!(
            history(&api2, &g, token).await,
            history(&api, &g, token).await
        );
    }
    let me = api2
        .raw(Method::GET, "/v1/me", Some(&carol.token), None)
        .await
        .unwrap()
        .decode::<v1::User>();
    assert_eq!(me.username, "carol");
    assert_eq!(
        api2.login("bob").await.unwrap().status,
        reqwest::StatusCode::OK
    );
    let next = send(&api2, &g, &alice.token, "b-6", "after restore").await;
    assert_eq!(next.server_sequence, 5);

    // check-db catches a broken copy (a hole in a conversation's sequence).
    let pool = sqlx::PgPool::connect(&check_url).await.unwrap();
    sqlx::query("DELETE FROM messages WHERE conversation_id = $1::uuid AND server_sequence = 3 AND reply_to_id IS NULL")
        .bind(&g)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let out = Command::new(env!("CARGO_BIN_EXE_turbo-pancake-app-server"))
        .arg("check-db")
        .env("TURBO_PANCAKE_DATABASE_URL", &check_url)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let broken: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(broken["ok"], false);
    assert_eq!(
        broken["checks"]["message_sequences_gap_free"], 1,
        "{broken}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
