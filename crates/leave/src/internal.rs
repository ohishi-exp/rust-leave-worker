//! メール取り込みの口 `POST /internal/ingest` (ippoan/email-receiver が Service Binding で呼ぶ。契約は [`crate::ingest`])。
//!
//! **管理 API (`/api/leave/…`) の外**: `require_tenant_header` も `require_admin` も掛からない。tenant は body の
//! mailbox から決める (`X-Tenant-ID` を付けても見ない)。auth-worker の `ALC_BINDING_ROUTES` は `/api/leave/` しか
//! 回さないので、ブラウザからは届かない。守るのは共有 secret (`X-Internal-Shared-Secret`、constant-time 比較)。
//!
//! 流れ: secret → body → 検証 ([`check_input`]) → mailbox から tenant と許可リスト → 送信元の照合 →
//! ページ数 ([`count_pages`]。PDF の parser には許可された送信元の PDF だけを渡す) → R2 に put → `tenant_tx` で全ページを INSERT。INSERT が失敗したら R2 の object を消してから 500
//! (R2 の await は transaction の外 = `tenant_tx` を 2 回に分けている)。
//! ページ数以降 (本体) は [`ingest_pdf`] で、スキャンの inbox の取り込み ([`crate::inbox`]) と共用する。
//!
//! 同じ形の口 `POST /internal/scan-check` (nuxt-leave の /dav が、PUT の本体を受けたときに受信アドレスと資格情報の
//! tenant を照合する): secret → body ([`check_scan_check`]) → `pg::resolve_mailbox` → tenant 一致なら 204、
//! 未知・無効・別 tenant は区別せず 403 `mailbox_mismatch` (tenant や mailbox の有無を漏らさない)。
//!
//! 応答・ログは固定の語だけ。PDF の中身・差出人のアドレス・secret は出さない (tenant の識別子は 202・200 のログに出す)。

use std::sync::Arc;

use alc_worker_db::PgClient;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use futures_util::lock::Mutex;
use serde_json::json;
use uuid::Uuid;

use crate::ingest::{
    check_input, check_scan_check, clip_mail_from, clip_subject, count_pages, object_key,
    secret_matches, seeded_object_key, sender_allowed, IngestInput, Reject, MAX_BODY_BYTES,
    MAX_SCAN_CHECK_BODY_BYTES,
};
use crate::pg;
use crate::routes::LogSink;
use crate::store::PdfStore;

/// 口の path (worker はこれを `/api` の nest の外に置く)
pub const INGEST_PATH: &str = "/internal/ingest";
/// 共有 secret を載せるヘッダー (email-receiver と同じ名前)
/// 受信アドレスと会社の照合の口 (worker は INGEST_PATH と同じく nest の外に置き、secret もこの path で取る)
pub const SCAN_CHECK_PATH: &str = "/internal/scan-check";
const SECRET_HEADER: &str = "X-Internal-Shared-Secret";

#[derive(Clone)]
pub struct IngestState {
    pub pg: Arc<Mutex<PgClient>>,
    pub pdfs: Arc<dyn PdfStore>,
    /// Secrets Store の `INTERNAL_SHARED_SECRET`。取れなかった (`None`) なら全部 401
    pub secret: Option<Arc<str>>,
    pub log: LogSink,
}

pub fn ingest_router() -> Router<IngestState> {
    Router::new()
        .route(INGEST_PATH, post(ingest))
        .route(SCAN_CHECK_PATH, post(scan_check))
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "unauthorized" })),
    )
        .into_response()
}

fn bad_request(code: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": code }))).into_response()
}

fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal_error" })),
    )
        .into_response()
}

/// 意図して捨てた (202)。理由はログにも出す (`tenant` が分かっていれば添える)
fn dropped(state: &IngestState, reason: &str, tenant: Option<Uuid>) -> Response {
    match tenant {
        Some(t) => (state.log)(&format!("ingest dropped: {reason} tenant={t}")),
        None => (state.log)(&format!("ingest dropped: {reason}")),
    }
    (StatusCode::ACCEPTED, Json(json!({ "dropped": reason }))).into_response()
}

async fn ingest(State(state): State<IngestState>, req: Request<Body>) -> Response {
    let given = req
        .headers()
        .get(SECRET_HEADER)
        .and_then(|v| v.to_str().ok());
    let expected = state.secret.as_deref().unwrap_or("");
    if !secret_matches(expected, given) {
        (state.log)("ingest: unauthorized");
        return unauthorized();
    }
    let Ok(body) = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES).await else {
        return bad_request("invalid_body");
    };
    let Ok(input) = serde_json::from_slice::<IngestInput>(&body) else {
        return bad_request("invalid_body");
    };
    drop(body);
    let checked = match check_input(&input) {
        Ok(c) => c,
        Err(Reject::Bad(code)) => return bad_request(code),
        Err(Reject::Drop(reason)) => return dropped(&state, reason, None),
    };

    // tenant の解決 (mailbox は検証済みの小文字)
    let resolved = pg::resolve_mailbox(&mut *state.pg.lock().await, checked.mailbox).await;
    let (tenant_id, allowed) = match resolved {
        Ok(Some(found)) => found,
        Ok(None) => return dropped(&state, "unknown_mailbox", None),
        Err(e) => {
            (state.log)(&format!("ingest resolve: {}", alc_worker_db::kind(&e)));
            return internal_error();
        }
    };
    if !sender_allowed(&input.header_from, &allowed) {
        return dropped(&state, "sender_not_allowed", Some(tenant_id));
    }

    let pdf = NewPdf {
        tenant_id,
        pdf: checked.pdf,
        received_at: input.received_at,
        mail_from: Some(clip_mail_from(&input.header_from)),
        subject: input.subject.as_deref().map(clip_subject),
        key_seed: None,
    };
    match ingest_pdf(&state.pg, state.pdfs.as_ref(), &state.log, pdf).await {
        Ok(pages) => {
            (state.log)(&format!("ingest stored: tenant={tenant_id} pages={pages}"));
            Json(json!({ "pages": pages })).into_response()
        }
        Err(IngestFailure::Drop(reason)) => dropped(&state, reason, Some(tenant_id)),
        Err(IngestFailure::Failed) => internal_error(),
    }
}

async fn scan_check(State(state): State<IngestState>, req: Request<Body>) -> Response {
    let given = req
        .headers()
        .get(SECRET_HEADER)
        .and_then(|v| v.to_str().ok());
    let expected = state.secret.as_deref().unwrap_or("");
    if !secret_matches(expected, given) {
        (state.log)("scan-check: unauthorized");
        return unauthorized();
    }
    let Ok(body) = axum::body::to_bytes(req.into_body(), MAX_SCAN_CHECK_BODY_BYTES).await else {
        return bad_request("invalid_body");
    };
    let (mailbox, tenant_id) = match check_scan_check(&body) {
        Ok(v) => v,
        Err(code) => return bad_request(code),
    };
    match pg::resolve_mailbox(&mut *state.pg.lock().await, mailbox).await {
        Ok(Some((found, _))) if found == tenant_id => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => {
            (state.log)("scan-check: mailbox_mismatch");
            (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "mailbox_mismatch" })),
            )
                .into_response()
        }
        Err(e) => {
            (state.log)(&format!("scan-check resolve: {}", alc_worker_db::kind(&e)));
            internal_error()
        }
    }
}

/// 取り込む PDF (tenant は呼び手が確定済み: メール = mailbox と送信元、スキャン = auth-worker が検証したチケット)
pub struct NewPdf {
    pub tenant_id: Uuid,
    /// `%PDF-` で始まり、大きさの上限以内 (呼び手が確かめる)
    pub pdf: Vec<u8>,
    pub received_at: DateTime<Utc>,
    /// メール経路だけ (スキャンは `None` = NULL)
    pub mail_from: Option<String>,
    pub subject: Option<String>,
    /// 在れば R2 の key をこれから決定的に作る (Queue の再配送で同じ key・同じ行になる)。`None` なら乱数の UUID
    pub key_seed: Option<String>,
}

/// 取り込みの本体が返す失敗
#[derive(Debug, PartialEq)]
pub enum IngestFailure {
    /// 意図して捨てる (ページ数が読めない・多すぎる)。語は固定
    Drop(&'static str),
    /// R2 か DB の失敗 (一時的なもの。メールは 500、Queue は retry)。段の名前はログに出してある
    Failed,
}

/// 取り込みの本体 (メールの口と Queue の consumer で共用): ページ数 ([`count_pages`]) → R2 に put → 全ページを INSERT。
/// INSERT が失敗したら R2 の object を消す。ただし `key_seed` が在る (決定的な key) ときは消さない — 前の配送で
/// 入った行がその object を指していることがあり、再試行は同じ key に put し直すため。
/// R2 の await は transaction の外 (`tenant_tx` は INSERT の 1 回だけ)
pub async fn ingest_pdf(
    pg: &Mutex<PgClient>,
    pdfs: &dyn PdfStore,
    log: &LogSink,
    p: NewPdf,
) -> Result<usize, IngestFailure> {
    let pages = count_pages(&p.pdf).map_err(IngestFailure::Drop)?;
    let key = match &p.key_seed {
        Some(seed) => seeded_object_key(p.tenant_id, p.received_at, seed),
        None => object_key(p.tenant_id, p.received_at, Uuid::new_v4()),
    };
    if pdfs.put(&key, p.pdf).await.is_err() {
        log("ingest: r2 put failed");
        return Err(IngestFailure::Failed);
    }
    let new_pages = pg::NewPages {
        r2_key: key.clone(),
        mail_from: p.mail_from,
        subject: p.subject,
        received_at: p.received_at,
        pages: i32::try_from(pages).unwrap_or(i32::MAX),
    };
    let inserted = pg::insert_pages(&mut *pg.lock().await, p.tenant_id, new_pages).await;
    if let Err(e) = inserted {
        log(&format!("ingest insert: {}", alc_worker_db::kind(&e)));
        if p.key_seed.is_none() && pdfs.delete(&key).await.is_err() {
            log("ingest: r2 delete failed");
        }
        return Err(IngestFailure::Failed);
    }
    Ok(pages)
}
