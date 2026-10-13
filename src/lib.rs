//! 休暇申請書 (PDF) の管理 API の Worker (Refs #1)。crates/leave の口 (`tenant_router`、axum) を
//! workers-rs (`http` / `axum` feature) に載せる。型は ippoan/alc-vein-worker・ippoan/alc-dtako-worker と同じ。
//! DB への経路 (staging = Workers VPC の先の PgBouncer、本番 = Hyperdrive) は [`db::connect`] の 1 か所で出し分ける。
//! 口はすべて `/api/leave/…` (auth-worker の `/alc-proxy` が `/api/…` のまま Service Binding で転送する)。
//!
//! **この Worker は JWT を検証せず、auth-worker が付け直した tenant / role ヘッダーを信頼する**
//! (`alc_core_wasm::require_tenant_header` と crates/leave の `auth::require_admin`)。本番・staging とも
//! workers.dev を開けず (`workers_dev` / `preview_urls` = false、`routes` を持たない)、入口は Service Binding だけ
//! (wrangler.toml と scripts/check-exposure.sh が保証する)。
//!
//! 定時実行 (`scheduled`、本番だけ) は毎朝一番星と申請書を突き合わせ、ずれを LINE WORKS へ送る (crates/leave の `cron`)。
//!
//! Queue (`queue`) はスキャンの inbox (R2 bucket `leave-inbox` のイベント通知) を 1 メッセージずつ取り込む
//! (中身は crates/leave の `inbox::handle`。tenant は auth-worker の RPC `verifyTicket` が返したものだけ)。

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
use chrono::{DateTime, Utc};
use futures_util::lock::Mutex;
use leave::cron::Notifier;
use leave::{tenant_router, Clock, LeaveState, LogSink, TEST_NOTIFY_PATH};
use tower_service::Service;
use worker::{
    console_error, console_log, event, Context, Date, Env, HttpRequest, MessageBatch, MessageExt,
    Result, ScheduleContext, ScheduledEvent, WorkerVersionMetadata,
};

use crate::bindings::{
    AuthTicketVerifier, AuthWorkerNotifier, IchibanBinding, NoNotifier, R2Inbox, R2Pdfs,
};

/// R2 の binding (本番と staging で別の bucket)
const LEAVE_R2_BINDING: &str = "LEAVE_R2";
/// 一番星 Worker の Service Binding (本番だけ。staging には置かない — 部門・社員・カレンダー・付与の口は 503)
const ICHIBAN_DB_BINDING: &str = "ICHIBAN_DB";
/// auth-worker との共有 secret (LINE WORKS 送信 = auth-worker の内部 proxy の通知に使う)
/// (Secrets Store の binding。`.get()` で実値にする)
const NOTIFY_SECRET_BINDING: &str = "INTERNAL_SHARED_SECRET";
/// auth-worker の Service Binding (本番だけ)。LINE WORKS への送信 (定時実行の通知) に使う
const AUTH_WORKER_BINDING: &str = "AUTH_WORKER";
/// スキャンの inbox の R2 (本番と staging で別の bucket)
const LEAVE_INBOX_BINDING: &str = "LEAVE_INBOX";
/// auth-worker の named entrypoint `MachineCredentialEntrypoint` (チケットの検証 `verifyTicket`)
const AUTH_MCRED_BINDING: &str = "AUTH_MCRED";
/// staging の表示制限 (分)。env.staging の vars にだけ置く (本番は無い = 制限なし)
const VISIBLE_MINUTES_VAR: &str = "STAGING_VISIBLE_MINUTES";

/// 今の時刻 (JS の Date)
fn now() -> DateTime<Utc> {
    DateTime::from_timestamp_millis(Date::now().as_millis() as i64).unwrap_or_default()
}

fn clock() -> Clock {
    Arc::new(now)
}

/// 管理 API は `/api` の下にだけ出す。tenant の layer は nest の中の全 route (と、その下で当たらない path) に掛かる
fn router(state: LeaveState) -> Router {
    let leave = tenant_router()
        .layer(middleware::from_fn(require_tenant_header))
        .with_state(state);
    Router::new().nest("/api", leave)
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
    // 送信の口 (secret の取得を伴う) はテスト送信の口へのリクエストでだけ組む。ほかの口は使わない
    let notifier: Arc<dyn Notifier> = if req.uri().path() == TEST_NOTIFY_PATH {
        Arc::from(notifier(&env).await)
    } else {
        Arc::new(NoNotifier)
    };
    let pg = Arc::new(Mutex::new(pg));
    let pdfs = Arc::new(R2Pdfs::new(bucket));
    let state = LeaveState {
        pg,
        pdfs,
        ichiban,
        notifier,
        log: log_sink(),
        now: clock(),
        visible_minutes: leave::visible_minutes(
            env.var(VISIBLE_MINUTES_VAR)
                .ok()
                .map(|v| v.to_string())
                .as_deref(),
        ),
    };
    let mut resp = match router(state).call(req).await {
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

/// 送信の口 (auth-worker の binding と共有 secret)。どちらかが無ければ送らない (警告は未通知のまま次の回へ残る。
/// テスト送信の口は 503 `notify_unavailable`)。cron とテスト送信の口で共用する
async fn notifier(env: &Env) -> Box<dyn Notifier> {
    let Ok(fetcher) = env.service(AUTH_WORKER_BINDING) else {
        console_error!("leave: cron no {AUTH_WORKER_BINDING} binding");
        return Box::new(NoNotifier);
    };
    let secret = match env.secret_store(NOTIFY_SECRET_BINDING) {
        Ok(store) => store.get().await.ok().flatten(),
        Err(_) => None,
    };
    match secret {
        Some(s) => Box::new(AuthWorkerNotifier::new(fetcher, s)),
        None => {
            console_error!("leave: cron no {NOTIFY_SECRET_BINDING} secret");
            Box::new(NoNotifier)
        }
    }
}

/// 毎朝の突き合わせ (wrangler.toml の `[triggers]`、本番だけ)。中身は crates/leave の `cron::run`
#[event(scheduled)]
async fn scheduled(_event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    let Ok(fetcher) = env.service(ICHIBAN_DB_BINDING) else {
        console_error!("leave: cron no {ICHIBAN_DB_BINDING} binding");
        return;
    };
    let ichiban = IchibanBinding::new(fetcher);
    let mut pg = match db::connect(&env).await {
        Ok(c) => c,
        Err(e) => {
            console_error!("leave: cron {e}");
            return;
        }
    };
    let notifier = notifier(&env).await;
    let today = leave::ledger::jst_date(now());
    let summary = leave::cron::run(&mut pg, &ichiban, notifier.as_ref(), today).await;
    console_log!("leave: {}", summary.line());
}

/// スキャンの inbox の取り込み (wrangler.toml の `[[queues.consumers]]`)。メッセージごとに ack / retry を決める。
/// binding が無ければ全部 retry、DB に繋がらなければ検証を通ったものを retry (Queue の max_retries の後は dead letter queue)
#[event(queue)]
async fn queue(batch: MessageBatch<serde_json::Value>, env: Env, _ctx: Context) -> Result<()> {
    let (Ok(inbox), Ok(bucket), Ok(auth)) = (
        env.bucket(LEAVE_INBOX_BINDING),
        env.bucket(LEAVE_R2_BINDING),
        env.service(AUTH_MCRED_BINDING),
    ) else {
        console_error!("leave: inbox bindings missing");
        batch.retry_all();
        return Ok(());
    };
    let pdfs = R2Pdfs::new(bucket);
    let inbox = R2Inbox::new(inbox);
    let verifier = AuthTicketVerifier::new(auth);
    let log = log_sink();
    let deps = leave::inbox::InboxDeps {
        pdfs: &pdfs,
        inbox: &inbox,
        verifier: &verifier,
        log: &log,
    };
    // 検証 (R2・verifyTicket) を全部済ませてから、通ったものがあるときだけ DB に繋ぐ。verifyTicket の先が同じ DB
    // (DO は同時に 1 本) に繋ぎにいくので、先に握らない。繋がらなければ検証を通ったものを retry
    let messages = batch.messages()?;
    let bodies = messages.iter().map(|m| m.body().clone()).collect();
    let outcomes = leave::inbox::handle_batch(&deps, bodies, || db::connect(&env)).await;
    for (message, outcome) in messages.iter().zip(outcomes) {
        match outcome {
            leave::inbox::Outcome::Ack => message.ack(),
            leave::inbox::Outcome::Retry => message.retry(),
        }
    }
    Ok(())
}
