//! 実 DB のテストの準備・接続・偽物 (ippoan/alc-vein-worker の tests/embedded/mod.rs と、ippoan/alc-worker-kit の
//! tests/support の型)。
//!
//! - **`LEAVE_TEST_ADMIN_DATABASE_URL` が未設定なら失敗する** (skip して緑にしない)。値は準備用の superuser の
//!   接続文字列で、**使い捨ての DB (database 名 `postgres`) に向けること** (ロール・schema・全 migration を作る)。表示はしない
//! - 準備の SQL は ippoan/alc-migrations から版を固定して取る (`scripts/ALC_MIGRATIONS_REV`、
//!   `bash scripts/fetch-migrations.sh` が repo 直下の `.alc-migrations/` に取り出す)。**rev を書くのはそのファイルだけ**。
//!   流す順は init_local_db.sql → 空の `_sqlx_migrations` → migrations (1 ファイル 1 transaction) → local_app_grants.sql
//!   (alc-vein-worker の container/start.sh と同じ)
//! - 本体のテストは、表の所有者ではない・`NOSUPERUSER`・`NOBYPASSRLS` の `alc_api_app` で繋ぐ ([`app_client`] が毎回確かめる)。
//!   [`APP_PASSWORD`] は使い捨ての DB 専用の固定の文字列で、秘密ではない

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use alc_core_wasm::require_tenant_header;
use alc_worker_db::PgClient;
use axum::body::Body;
use axum::http::{Request, Response};
use axum::{middleware, Router};
use futures_util::future::BoxFuture;
use futures_util::lock::Mutex;
use leave::ichiban::{IchibanApi, IchibanError, IchibanResponse};
use leave::store::{PdfStore, StoreError};
use leave::{tenant_router, LeaveState};
use tokio::sync::OnceCell;
use tokio_postgres::types::Type;
use tokio_postgres::{Client, Config, NoTls};
use tower::ServiceExt;
use uuid::Uuid;

pub const ADMIN_URL_ENV: &str = "LEAVE_TEST_ADMIN_DATABASE_URL";
pub const APP_ROLE: &str = "alc_api_app";
/// 使い捨ての DB 専用 (秘密ではない)
pub const APP_PASSWORD: &str = "leave-test-app-throwaway";
const SEARCH_PATH: &str = "-c search_path=alc_api,public";
/// 準備を直列にする advisory lock の鍵 (並列に走るテストの binary どうし)
const PREPARE_LOCK: i64 = 1_003_001;

fn migrations_dir() -> PathBuf {
    let dir = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../.alc-migrations"
    ));
    assert!(
        dir.join("migrations").is_dir(),
        ".alc-migrations が無い。先に `bash scripts/fetch-migrations.sh` を流す"
    );
    dir
}

pub fn admin_config() -> Config {
    let url = std::env::var(ADMIN_URL_ENV).unwrap_or_else(|_| {
        panic!("{ADMIN_URL_ENV} が未設定。実 DB のテストは接続先が無ければ失敗させる (skip しない)")
    });
    url.parse()
        .unwrap_or_else(|_| panic!("{ADMIN_URL_ENV} が接続文字列として読めない"))
}

async fn raw_connect(config: &Config) -> Client {
    let (client, connection) = config
        .connect(NoTls)
        .await
        .unwrap_or_else(|_| panic!("{ADMIN_URL_ENV} の DB に繋げない"));
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// superuser の接続 (準備だけに使う)
pub async fn admin() -> Client {
    let mut config = admin_config();
    config.options(SEARCH_PATH);
    raw_connect(&config).await
}

/// 準備をプロセスの中で 1 回だけ流す。DB に既に `leave_pages` が在れば (別の binary が流した) 何もしない
pub async fn prepare() {
    static PREPARED: OnceCell<()> = OnceCell::const_new();
    PREPARED
        .get_or_init(|| async {
            let su = admin().await;
            su.batch_execute(&format!("SELECT pg_advisory_lock({PREPARE_LOCK})"))
                .await
                .unwrap();
            let done = su
                .query_one("SELECT to_regclass('alc_api.leave_pages') IS NOT NULL", &[])
                .await
                .unwrap()
                .get::<_, bool>(0);
            if !done {
                migrate(&su).await;
            }
            su.batch_execute(&format!("SELECT pg_advisory_unlock({PREPARE_LOCK})"))
                .await
                .unwrap();
            eprintln!("[leave-db] 実 DB で準備済み (migrations は {})", rev());
        })
        .await;
}

fn rev() -> String {
    std::fs::read_to_string(migrations_dir().join(".rev"))
        .unwrap()
        .trim()
        .to_owned()
}

async fn migrate(su: &Client) {
    let dir = migrations_dir();
    let read = |p: PathBuf| std::fs::read_to_string(p).unwrap();
    su.batch_execute(&read(dir.join("scripts/init_local_db.sql")))
        .await
        .unwrap();
    su.batch_execute("CREATE TABLE IF NOT EXISTS alc_api._sqlx_migrations (version BIGINT PRIMARY KEY, description TEXT NOT NULL, installed_on TIMESTAMPTZ NOT NULL DEFAULT now(), success BOOLEAN NOT NULL, checksum BYTEA NOT NULL, execution_time BIGINT NOT NULL)")
        .await
        .unwrap();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir.join("migrations"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .collect();
    files.sort();
    assert!(
        files.iter().any(|f| f
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("_leave_tables.sql")),
        "取り出した migrations に leave の表が無い (scripts/ALC_MIGRATIONS_REV が古い)"
    );
    for file in files {
        let name = file.file_name().unwrap().to_owned();
        if let Err(e) = su
            .batch_execute(&format!("BEGIN;\n{}\n;COMMIT;", read(file)))
            .await
        {
            panic!("migration {name:?}: {:?}", e.code());
        }
    }
    su.batch_execute(&read(dir.join("scripts/local_app_grants.sql")))
        .await
        .unwrap();
    su.batch_execute(&format!(
        "ALTER ROLE {APP_ROLE} LOGIN PASSWORD '{APP_PASSWORD}'"
    ))
    .await
    .unwrap();
}

/// `alc_api_app` の接続 (worker と同じく `PgClient` に包む)。**superuser / BYPASSRLS / 表の所有者なら panic**
pub async fn app_client() -> PgClient {
    prepare().await;
    let mut config = admin_config();
    config.user(APP_ROLE).password(APP_PASSWORD);
    let client = raw_connect(&config).await;
    let row = client
        .query_one(
            "SELECT (SELECT rolsuper OR rolbypassrls FROM pg_roles WHERE rolname = current_user), \
             (SELECT bool_or(tableowner = current_user) FROM pg_tables WHERE schemaname = 'alc_api' AND tablename LIKE 'leave_%')",
            &[],
        )
        .await
        .unwrap();
    assert!(!row.get::<_, bool>(0), "RLS を素通りするロールで繋いでいる");
    assert!(!row.get::<_, bool>(1), "表の所有者で繋いでいる");
    PgClient::new(client)
}

/// tenant を 1 つ作る (superuser で。名前は乱数)
pub async fn tenant() -> Uuid {
    prepare().await;
    let id = Uuid::new_v4();
    admin()
        .await
        .execute(
            "INSERT INTO alc_api.tenants (id, name) VALUES ($1, $2)",
            &[&id, &format!("leave-test-{id}")],
        )
        .await
        .unwrap();
    id
}

/// tenant の LINE WORKS のチャネルを 1 つ作る (bot の設定も合わせて。値は固定のダミー)
pub async fn lineworks_channel(tenant_id: Uuid) -> Uuid {
    let su = admin().await;
    let bot: Uuid = su
        .query_one(
            "INSERT INTO alc_api.bot_configs (tenant_id, name, client_id, client_secret_encrypted, service_account, private_key_encrypted, bot_id) \
             VALUES ($1, 'test', 'c', 's', 'sa', 'k', $2) RETURNING id",
            &[&tenant_id, &Uuid::new_v4().to_string()],
        )
        .await
        .unwrap()
        .get(0);
    su.query_one(
        "INSERT INTO alc_api.lineworks_channels (tenant_id, bot_config_id, channel_id) VALUES ($1, $2, $3) RETURNING id",
        &[&tenant_id, &bot, &Uuid::new_v4().to_string()],
    )
    .await
    .unwrap()
    .get(0)
}

/// ページを 1 枚作る (受信の口の代わり。`alc_api_app` で tenant_tx の中から入れる)
pub async fn page(tenant_id: Uuid, r2_key: &str, page_no: i32, received_at: &str) -> Uuid {
    let mut pg = app_client().await;
    let r2_key = r2_key.to_owned();
    let received_at: chrono::DateTime<chrono::Utc> = received_at.parse().unwrap();
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let row = tx
                .query_typed_one(
                    "INSERT INTO alc_api.leave_pages (tenant_id, r2_key, page_no, mail_from, subject, received_at) \
                     VALUES ($1, $2, $3, 'sender@example.com', '休暇申請', $4) RETURNING id",
                    &[
                        (&tenant_id, Type::UUID),
                        (&r2_key, Type::TEXT),
                        (&page_no, Type::INT4),
                        (&received_at, Type::TIMESTAMPTZ),
                    ],
                )
                .await?;
            Ok(row.get::<_, Uuid>(0))
        })
    })
    .await
    .unwrap()
}

/// 偽の PDF 置き場 (key → 中身)。`fail` なら読み出しが失敗する
#[derive(Default)]
pub struct FakePdfs {
    pub objects: HashMap<String, Vec<u8>>,
    pub fail: bool,
    pub reads: StdMutex<Vec<String>>,
}

impl PdfStore for FakePdfs {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>, StoreError>> {
        Box::pin(async move {
            self.reads.lock().unwrap().push(key.to_owned());
            if self.fail {
                return Err(StoreError("get"));
            }
            Ok(self.objects.get(key).cloned())
        })
    }
}

/// 偽の一番星 Worker (path → 応答)。登録の無い path は呼び出しの失敗
#[derive(Default)]
pub struct FakeIchiban {
    pub responses: HashMap<String, IchibanResponse>,
    pub calls: StdMutex<Vec<String>>,
}

impl IchibanApi for FakeIchiban {
    fn get<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<IchibanResponse, IchibanError>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(path.to_owned());
            self.responses.get(path).cloned().ok_or(IchibanError)
        })
    }
}

pub struct App {
    pub router: Router,
    pub pdfs: Arc<FakePdfs>,
    pub ichiban: Option<Arc<FakeIchiban>>,
    pub logs: Arc<StdMutex<Vec<String>>>,
}

/// worker の src/lib.rs と同じ組み立て (`/api` の下に nest し、tenant の layer を掛ける)
pub async fn app(pdfs: FakePdfs, ichiban: Option<FakeIchiban>) -> App {
    build(app_client().await, pdfs, ichiban)
}

/// 接続が切れた DB (backend を落とした接続) で組み立てる (DB のエラーの経路を通すため)
pub async fn app_with_broken_db(ichiban: Option<FakeIchiban>) -> App {
    let mut pg = app_client().await;
    let pid: i32 = pg
        .tenant_tx(Uuid::new_v4(), |tx| {
            Box::pin(async move {
                let row = tx.query_typed_one("SELECT pg_backend_pid()", &[]).await?;
                Ok(row.get::<_, i32>(0))
            })
        })
        .await
        .unwrap();
    let terminated: bool = admin()
        .await
        .query_one("SELECT pg_terminate_backend($1)", &[&pid])
        .await
        .unwrap()
        .get(0);
    assert!(terminated);
    build(pg, FakePdfs::default(), ichiban)
}

fn build(pg: PgClient, pdfs: FakePdfs, ichiban: Option<FakeIchiban>) -> App {
    let pdfs = Arc::new(pdfs);
    let ichiban = ichiban.map(Arc::new);
    let logs = Arc::new(StdMutex::new(Vec::new()));
    let sink = logs.clone();
    let state = LeaveState {
        pg: Arc::new(Mutex::new(pg)),
        pdfs: pdfs.clone(),
        ichiban: ichiban.clone().map(|i| i as Arc<dyn IchibanApi>),
        log: Arc::new(move |m: &str| sink.lock().unwrap().push(m.to_owned())),
    };
    let leave = tenant_router()
        .layer(middleware::from_fn(require_tenant_header))
        .with_state(state);
    App {
        router: Router::new().nest("/api", leave),
        pdfs,
        ichiban,
        logs,
    }
}

pub struct Got {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

impl Got {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

/// 1 リクエストを打つ。`role` が `None` なら `X-User-Role` を付けない
pub async fn call(
    app: &App,
    method: &str,
    uri: &str,
    tenant_id: Option<Uuid>,
    role: Option<&str>,
    body: Option<serde_json::Value>,
) -> Got {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(t) = tenant_id {
        b = b.header("X-Tenant-ID", t.to_string());
    }
    if let Some(r) = role {
        b = b
            .header("X-User-Role", r)
            .header("X-User-Email", "admin@example.com");
    }
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp: Response<Body> = app
        .router
        .clone()
        .oneshot(b.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_owned());
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    Got {
        status,
        content_type,
        body,
    }
}
