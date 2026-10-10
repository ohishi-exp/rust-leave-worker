//! 休暇申請書 (PDF) の管理 API の Worker (Refs #1)。crates/leave の口 (`tenant_router`、axum) を
//! workers-rs (`http` / `axum` feature) に載せる。型は ippoan/alc-vein-worker・ippoan/alc-dtako-worker と同じ。
//! DB への経路 (staging = Workers VPC の先の PgBouncer、本番 = Hyperdrive) は [`db::connect`] の 1 か所で出し分ける。
//! 口はすべて `/api/leave/…` (auth-worker の `/alc-proxy` が `/api/…` のまま Service Binding で転送する)。
//!
//! **この Worker は JWT を検証せず、auth-worker が付け直した tenant / role ヘッダーを信頼する**
//! (`alc_core_wasm::require_tenant_header` と crates/leave の `auth::require_admin`)。本番の到達経路は
//! auth-worker からの Service Binding だけで、`workers_dev` / `preview_urls` / `routes` を持たない
//! (wrangler.toml と scripts/check-exposure.sh が保証する)。staging だけはテストから叩くため
//! `workers_dev = true` で、その workers.dev は Cloudflare Access で保護する。

// worker 本体は wasm32 専用。native の `cargo build` / `clippy` / `test` (workspace の root で打つもの) では空の crate にして、
// crates/leave だけを検査させる
#![cfg(target_arch = "wasm32")]

mod bindings;
mod db;
mod tcp;

use std::sync::Arc;

use alc_core_wasm::require_tenant_header;
use axum::body::Body;
use axum::http::{HeaderValue, Response, StatusCode};
use axum::{middleware, Router};
use futures_util::lock::Mutex;
use leave::{ingest_router, tenant_router, IngestState, LeaveState, LogSink, INGEST_PATH};
use tower_service::Service;
use worker::{console_error, event, Context, Env, HttpRequest, Result, WorkerVersionMetadata};

use crate::bindings::{IchibanBinding, R2Pdfs};

/// R2 の binding (本番と staging で別の bucket)
const LEAVE_R2_BINDING: &str = "LEAVE_R2";
/// 一番星 Worker の Service Binding (本番だけ。staging には置かない — 部門・社員の口は 503)
const ICHIBAN_DB_BINDING: &str = "ICHIBAN_DB";
/// メール取り込みの口の共有 secret (Secrets Store の binding。`.get()` で実値にする)
const INGEST_SECRET_BINDING: &str = "INTERNAL_SHARED_SECRET";

/// 管理 API は `/api` の下にだけ出す。tenant の layer は nest の中の全 route (と、その下で当たらない path) に掛かる。
/// メール取り込みの口 `/internal/ingest` は nest の外 (tenant・admin の layer の外で、共有 secret で守る)
fn router(state: LeaveState, ingest: IngestState) -> Router {
    let leave = tenant_router()
        .layer(middleware::from_fn(require_tenant_header))
        .with_state(state);
    Router::new()
        .nest("/api", leave)
        .merge(ingest_router().with_state(ingest))
}

/// 共有 secret の実値。取り込みの口へのリクエストでだけ引く。取れなければ `None` (口は全部 401)
async fn ingest_secret(env: &Env) -> Option<Arc<str>> {
    let store = env.secret_store(INGEST_SECRET_BINDING).ok()?;
    match store.get().await {
        Ok(Some(v)) => Some(Arc::from(v)),
        _ => {
            console_error!("leave: no {INGEST_SECRET_BINDING} value");
            None
        }
    }
}

fn log_sink() -> LogSink {
    Arc::new(|message| console_error!("leave: {message}"))
}

fn error_response(status: StatusCode, code: &str) -> Response<Body> {
    let mut resp = Response::new(Body::from(format!(r#"{{"error":"{code}"}}"#)));
    *resp.status_mut() = status;
    resp
}

#[event(fetch)]
async fn fetch(req: HttpRequest, env: Env, _ctx: Context) -> Result<Response<Body>> {
    // 全リクエストで routing の前に繋ぐ (ippoan/alc-vein-worker と同じ)
    let pg = match db::connect(&env).await {
        Ok(c) => c,
        Err(db::ConnectError::NotConfigured) => {
            console_error!("leave: {}", db::ConnectError::NotConfigured);
            return Ok(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "database_not_configured",
            ));
        }
        Err(e) => {
            console_error!("leave: {e}");
            return Ok(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
            ));
        }
    };
    let bucket = match env.bucket(LEAVE_R2_BINDING) {
        Ok(b) => b,
        Err(_) => {
            console_error!("leave: no {LEAVE_R2_BINDING} binding");
            return Ok(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
            ));
        }
    };
    let ichiban = env
        .service(ICHIBAN_DB_BINDING)
        .ok()
        .map(|f| Arc::new(IchibanBinding::new(f)) as Arc<dyn leave::ichiban::IchibanApi>);
    let secret = if req.uri().path() == INGEST_PATH {
        ingest_secret(&env).await
    } else {
        None
    };
    let pg = Arc::new(Mutex::new(pg));
    let pdfs = Arc::new(R2Pdfs::new(bucket));
    let ingest = IngestState {
        pg: pg.clone(),
        pdfs: pdfs.clone(),
        secret,
        log: log_sink(),
    };
    let state = LeaveState {
        pg,
        pdfs,
        ichiban,
        log: log_sink(),
    };
    let mut resp = match router(state, ingest).call(req).await {
        Ok(r) => r,
        Err(e) => match e {},
    };
    // どの版が応えたかを応答ヘッダーで分かるようにする。binding が取れなければ何も付けない
    if let Ok(meta) = env.get_binding::<WorkerVersionMetadata>("CF_VERSION_METADATA") {
        if let Ok(v) = HeaderValue::from_str(&meta.id()) {
            resp.headers_mut().insert("x-worker-version", v);
        }
        let tag = meta.tag();
        if !tag.is_empty() {
            if let Ok(v) = HeaderValue::from_str(&tag) {
                resp.headers_mut().insert("x-worker-tag", v);
            }
        }
    }
    Ok(resp)
}
