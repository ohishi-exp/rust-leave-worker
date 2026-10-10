//! メール取り込みの口 `POST /internal/ingest` (ippoan/email-receiver が Service Binding で呼ぶ。契約は [`crate::ingest`])。
//!
//! **管理 API (`/api/leave/…`) の外**: `require_tenant_header` も `require_admin` も掛からない。tenant は body の
//! mailbox から決める (`X-Tenant-ID` を付けても見ない)。auth-worker の `ALC_BINDING_ROUTES` は `/api/leave/` しか
//! 回さないので、ブラウザからは届かない。守るのは共有 secret (`X-Internal-Shared-Secret`、constant-time 比較)。
//!
//! 流れ: secret → body → 検証 ([`check_input`]) → mailbox から tenant と許可リスト → 送信元の照合 →
//! ページ数 ([`count_pages`]。PDF の parser には許可された送信元の PDF だけを渡す) → R2 に put → `tenant_tx` で全ページを INSERT。INSERT が失敗したら R2 の object を消してから 500
//! (R2 の await は transaction の外 = `tenant_tx` を 2 回に分けている)。
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
use futures_util::lock::Mutex;
use serde_json::json;
use uuid::Uuid;

use crate::ingest::{
    check_input, clip_mail_from, clip_subject, count_pages, object_key, secret_matches,
    sender_allowed, IngestInput, Reject, MAX_BODY_BYTES,
};
use crate::pg;
use crate::routes::LogSink;
use crate::store::PdfStore;

/// 口の path (worker はこれを `/api` の nest の外に置く)
pub const INGEST_PATH: &str = "/internal/ingest";
/// 共有 secret を載せるヘッダー (email-receiver と同じ名前)
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
    Router::new().route(INGEST_PATH, post(ingest))
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

    let pages = match count_pages(&checked.pdf) {
        Ok(n) => n,
        Err(reason) => return dropped(&state, reason, Some(tenant_id)),
    };

    // R2 に置いてから DB に入れる (R2 の await は transaction に挟まない)。key はサーバ側で作る
    let key = object_key(tenant_id, input.received_at, Uuid::new_v4());
    if state.pdfs.put(&key, checked.pdf).await.is_err() {
        (state.log)("ingest: r2 put failed");
        return internal_error();
    }
    let new_pages = pg::NewPages {
        r2_key: key.clone(),
        mail_from: clip_mail_from(&input.header_from),
        subject: input.subject.as_deref().map(clip_subject),
        received_at: input.received_at,
        pages: i32::try_from(pages).unwrap_or(i32::MAX),
    };
    let inserted = pg::insert_pages(&mut *state.pg.lock().await, tenant_id, new_pages).await;
    if let Err(e) = inserted {
        (state.log)(&format!("ingest insert: {}", alc_worker_db::kind(&e)));
        if state.pdfs.delete(&key).await.is_err() {
            (state.log)("ingest: r2 delete failed");
        }
        return internal_error();
    }
    (state.log)(&format!("ingest stored: tenant={tenant_id} pages={pages}"));
    Json(json!({ "pages": pages })).into_response()
}
