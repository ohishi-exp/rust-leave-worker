//! 管理 API の口 (7 本、worker が `/api` の下に nest するので実際の path は `/api/leave/…`)。
//!
//! | method | path | 中身 |
//! |---|---|---|
//! | GET / PUT | `/leave/settings` | 自 tenant の設定 (無ければ既定値) / upsert |
//! | GET | `/leave/departments` | 一番星の部門一覧をそのまま中継 |
//! | GET | `/leave/employees` | 一番星の社員を自 tenant の `dept_codes` で絞る |
//! | GET | `/leave/pages` | ページの一覧 (`status`・`employee_cd`・`limit`・`offset`) |
//! | GET / PUT | `/leave/pages/{id}` | ページ + 期間 / 紐づけの更新 |
//! | GET | `/leave/pages/{id}/pdf` | そのページの PDF (自 tenant に見える行だけ) |
//!
//! **全部の口を [`require_admin`] が包む** (管理者でなければ 403。handler にも DB にも届かない)。
//! tenant ヘッダーの検査 (`alc_core_wasm::require_tenant_header`、無ければ 401) は worker がこの外側に掛ける。
//! 口を足すときは [`tenant_router`] に 1 行足す (admin の layer は route_layer なので新しい口にも掛かる)。
//!
//! 応答の本文のエラーは固定の語だけ (`{"error":"..."}`)。ログ ([`LogSink`]) に出すのも段の名前と kind だけで、
//! 識別子・入力の値・エラーの生の文を出さない。

use std::sync::Arc;

use alc_core_wasm::TenantId;
use alc_worker_db::PgClient;
use axum::body::Body;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{middleware, Extension, Json, Router};
use futures_util::lock::Mutex;
use serde_json::json;
use uuid::Uuid;

use crate::auth::{actor, require_admin};
use crate::ichiban::{filter_employees, IchibanApi, DEPARTMENTS_PATH, EMPLOYEES_PATH};
use crate::pages::{validate_update, ListQuery, ListQueryRaw, PageUpdateInput};
use crate::pg;
use crate::settings::{validate, Settings, SettingsInput};
use crate::store::PdfStore;

/// ログの出し先 (Workers では console_error)。渡す文は段の名前と kind だけ
pub type LogSink = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone)]
pub struct LeaveState {
    pub pg: Arc<Mutex<PgClient>>,
    pub pdfs: Arc<dyn PdfStore>,
    /// 一番星 Worker の binding。無い環境 (staging) では `None` で、部門・社員の口は 503
    pub ichiban: Option<Arc<dyn IchibanApi>>,
    pub log: LogSink,
}

pub fn tenant_router() -> Router<LeaveState> {
    Router::new()
        .route("/leave/settings", get(get_settings).put(put_settings))
        .route("/leave/departments", get(departments))
        .route("/leave/employees", get(employees))
        .route("/leave/pages", get(list_pages))
        .route("/leave/pages/{id}", get(get_page).put(put_page))
        .route("/leave/pages/{id}/pdf", get(get_pdf))
        .route_layer(middleware::from_fn(require_admin))
}

fn error(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({ "error": code }))).into_response()
}

fn bad_request(code: &str) -> Response {
    error(StatusCode::BAD_REQUEST, code)
}

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not_found")
}

/// DB のエラー: kind (SQLSTATE か固定の語) だけをログに出し、500
fn db_error(state: &LeaveState, stage: &str, e: &tokio_postgres::Error) -> Response {
    (state.log)(&format!("{stage}: {}", alc_worker_db::kind(e)));
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
}

/// path の id。UUID でなければその行は在り得ないので 404
fn parse_id(id: &str) -> Option<Uuid> {
    Uuid::parse_str(id).ok()
}

async fn get_settings(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
) -> Response {
    let got = pg::get_settings(&mut *state.pg.lock().await, t.0).await;
    match got {
        Ok(s) => Json(s.unwrap_or_default()).into_response(),
        Err(e) => db_error(&state, "get_settings", &e),
    }
}

async fn put_settings(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
    body: Result<Json<SettingsInput>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return bad_request("invalid_body");
    };
    let valid = match validate(input) {
        Ok(v) => v,
        Err(code) => return bad_request(code),
    };
    let put = pg::put_settings(&mut *state.pg.lock().await, t.0, valid).await;
    match put {
        Ok(Some(s)) => Json::<Settings>(s).into_response(),
        Ok(None) => bad_request("unknown_lineworks_channel"),
        // mailbox の UNIQUE (別の tenant が使っている)
        Err(e) if alc_worker_db::kind(&e) == "23505" => {
            error(StatusCode::CONFLICT, "mailbox_taken")
        }
        Err(e) => db_error(&state, "put_settings", &e),
    }
}

fn ichiban_not_configured() -> Response {
    error(StatusCode::SERVICE_UNAVAILABLE, "ichiban_not_configured")
}

/// 一番星を 1 回呼び、2xx の本文だけを返す (それ以外は 502 の本文の語)
async fn ichiban_get(
    state: &LeaveState,
    api: &Arc<dyn IchibanApi>,
    path: &str,
) -> Result<Vec<u8>, &'static str> {
    match api.get(path).await {
        Ok(r) if (200..300).contains(&r.status) => Ok(r.body),
        Ok(r) => {
            (state.log)(&format!("ichiban {path}: status {}", r.status));
            Err("ichiban_upstream")
        }
        Err(_) => {
            (state.log)(&format!("ichiban {path}: fetch failed"));
            Err("ichiban_unavailable")
        }
    }
}

fn json_bytes(body: Vec<u8>) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

async fn departments(State(state): State<LeaveState>) -> Response {
    let Some(api) = state.ichiban.clone() else {
        return ichiban_not_configured();
    };
    match ichiban_get(&state, &api, DEPARTMENTS_PATH).await {
        Ok(body) => json_bytes(body),
        Err(code) => error(StatusCode::BAD_GATEWAY, code),
    }
}

async fn employees(State(state): State<LeaveState>, Extension(t): Extension<TenantId>) -> Response {
    let Some(api) = state.ichiban.clone() else {
        return ichiban_not_configured();
    };
    let got = pg::get_settings(&mut *state.pg.lock().await, t.0).await;
    let dept_codes = match got {
        Ok(s) => s.unwrap_or_default().dept_codes,
        Err(e) => return db_error(&state, "employees settings", &e),
    };
    // 部門を選んでいない tenant には誰も見せない (一番星も呼ばない)
    if dept_codes.is_empty() {
        return Json(json!({ "data": [] })).into_response();
    }
    let filtered = match ichiban_get(&state, &api, EMPLOYEES_PATH).await {
        Ok(body) => filter_employees(&body, &dept_codes).inspect_err(|code| {
            (state.log)(&format!("ichiban {EMPLOYEES_PATH}: {code}"));
        }),
        Err(code) => Err(code),
    };
    match filtered {
        Ok(v) => Json(v).into_response(),
        Err(code) => error(StatusCode::BAD_GATEWAY, code),
    }
}

async fn list_pages(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
    raw: Result<Query<ListQueryRaw>, QueryRejection>,
) -> Response {
    let Ok(Query(raw)) = raw else {
        return bad_request("invalid_query");
    };
    let q = match ListQuery::parse(raw) {
        Ok(q) => q,
        Err(code) => return bad_request(code),
    };
    let got = pg::list_pages(&mut *state.pg.lock().await, t.0, q).await;
    match got {
        Ok(pages) => Json(json!({ "data": pages })).into_response(),
        Err(e) => db_error(&state, "list_pages", &e),
    }
}

async fn get_page(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
    Path(id): Path<String>,
) -> Response {
    let Some(id) = parse_id(&id) else {
        return not_found();
    };
    let got = pg::get_page(&mut *state.pg.lock().await, t.0, id).await;
    match got {
        Ok(Some(d)) => Json(d).into_response(),
        Ok(None) => not_found(),
        Err(e) => db_error(&state, "get_page", &e),
    }
}

async fn put_page(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<PageUpdateInput>, JsonRejection>,
) -> Response {
    let Some(id) = parse_id(&id) else {
        return not_found();
    };
    let Ok(Json(input)) = body else {
        return bad_request("invalid_body");
    };
    let update = match validate_update(input) {
        Ok(u) => u,
        Err(code) => return bad_request(code),
    };
    let got = pg::update_page(
        &mut *state.pg.lock().await,
        t.0,
        id,
        update,
        actor(&headers),
    )
    .await;
    match got {
        Ok(Some(d)) => Json(d).into_response(),
        Ok(None) => not_found(),
        Err(e) => db_error(&state, "put_page", &e),
    }
}

/// 行が自 tenant に見えたときだけ R2 を読む (DB の transaction を閉じてから。JS の値の await を tx に挟まない)
async fn get_pdf(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
    Path(id): Path<String>,
) -> Response {
    let Some(id) = parse_id(&id) else {
        return not_found();
    };
    let got = pg::page_r2_key(&mut *state.pg.lock().await, t.0, id).await;
    let key = match got {
        Ok(Some(k)) => k,
        Ok(None) => return not_found(),
        Err(e) => return db_error(&state, "get_pdf", &e),
    };
    match state.pdfs.get(&key).await {
        Ok(Some(bytes)) => (
            [
                (header::CONTENT_TYPE, "application/pdf"),
                (header::CACHE_CONTROL, "private, no-store"),
            ],
            Body::from(bytes),
        )
            .into_response(),
        Ok(None) => {
            (state.log)("get_pdf: object missing");
            error(StatusCode::NOT_FOUND, "pdf_not_found")
        }
        Err(e) => {
            (state.log)(&format!("get_pdf: store {}", e.0));
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
        }
    }
}
