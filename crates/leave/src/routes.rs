//! 管理 API の口 (method ごとに数えて 22 本、worker が `/api` の下に nest するので実際の path は `/api/leave/…`)。
//!
//! | method | path | 中身 |
//! |---|---|---|
//! | GET / PUT | `/leave/settings` | 自 tenant の設定 (無ければ既定値) / upsert |
//! | GET | `/leave/departments` | 一番星の部門一覧をそのまま中継 |
//! | GET | `/leave/employees` | 一番星の社員を自 tenant の `dept_codes` で絞る |
//! | GET | `/leave/documents` | PDF 単位の一覧 (`state` = open / done / all・`limit`・`offset`) |
//! | GET | `/leave/pages` | ページの一覧 (`status`・`employee_cd`・`r2_key`・`limit`・`offset`) |
//! | GET / PUT | `/leave/pages/{id}` | ページ + 期間 / 紐づけの更新 (`ichiban_row_ids` があれば一番星で確かめて保存) |
//! | GET | `/leave/pages/{id}/pdf` | そのページの PDF (自 tenant に見える行だけ) |
//! | GET | `/leave/pages/{id}/candidates` | 基準日か受信日の前後 7 日の一番星の休暇の行を期間にまとめる (`ledger`) |
//! | PUT | `/leave/pages/{id}/base-date` | PDF の基準日 (同じ PDF の全ページ。`null` で受信日に戻す) |
//! | GET | `/leave/calendar` | 乗務員の月のカレンダー (`ledger`) |
//! | GET / POST | `/leave/grants` | 有給の付与台帳と残 / 手で付与を足す (`ledger`) |
//! | POST | `/leave/grants/auto` | 法定付与を足す (`ledger`) |
//! | PUT / DELETE | `/leave/grants/{id}` | 付与を直す / 消す (`ledger`) |
//! | GET / PUT | `/leave/opening` | 全員分の有給の期首残 / 一括保存 (`ledger`) |
//! | GET | `/leave/alerts` | 未解消の警告 (毎朝の通知と同じ条件・同じ窓。乗務員・日の順) |
//! | POST | `/leave/uploads` | 管理画面からの PDF の取り込み (本文 = PDF そのもの。毎回別の行。201 `{"pages": n}`) |
//! | POST | `/leave/settings/test-notify` | 保存済みの通知先へ固定の文面を 1 通送る (毎朝の cron と同じ宛先・同じ送信) |
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
use axum::body::Bytes;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{middleware, Extension, Json, Router};
use chrono::{DateTime, Utc};
use futures_util::lock::Mutex;
use serde_json::json;
use uuid::Uuid;

use crate::auth::{actor, require_admin};
use crate::cron::{window, Notifier};
use crate::ichiban::{filter_employees, IchibanApi, DEPARTMENTS_PATH, EMPLOYEES_PATH};
use crate::ingest::{check_pdf_bytes, MAX_PDF_BYTES};
use crate::internal::{ingest_pdf, IngestFailure, NewPdf};
use crate::ledger;
use crate::pages::{
    validate_update, BaseDateInput, DocumentsQuery, DocumentsQueryRaw, ListQuery, ListQueryRaw,
    PageUpdateInput,
};
use crate::pg::{self, PutSettings};
use crate::settings::{validate, Settings, SettingsInput};
use crate::store::PdfStore;

/// ログの出し先 (Workers では console_error)。渡す文は段の名前と kind だけ
pub type LogSink = Arc<dyn Fn(&str) + Send + Sync>;

/// 今の時刻 (Workers では JS の Date。テストは固定の値を差す)。「今日」は日本時間で決める
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

#[derive(Clone)]
pub struct LeaveState {
    pub pg: Arc<Mutex<PgClient>>,
    pub pdfs: Arc<dyn PdfStore>,
    /// 一番星 Worker の binding。無い環境 (staging) では `None` で、部門・社員の口は 503
    pub ichiban: Option<Arc<dyn IchibanApi>>,
    /// LINE WORKS への送信 (cron と同じもの)。テスト送信の口だけが使う。
    /// 送れない環境 (staging など) では [`Notifier::available`] が `false` で、その口は 503
    pub notifier: Arc<dyn Notifier>,
    pub log: LogSink,
    pub now: Clock,
    /// staging の表示制限 (分)。受信から この分数を過ぎたページは一覧・詳細・更新・PDF で「無いもの」(404) にする。
    /// `None` = 制限なし (本番。vars `STAGING_VISIBLE_MINUTES` を置かない)。[`visible_minutes`] で読む
    pub visible_minutes: Option<i64>,
}

/// vars `STAGING_VISIBLE_MINUTES` の値。無い・数でない・0 以下は `None` (制限なし)
pub fn visible_minutes(raw: Option<&str>) -> Option<i64> {
    raw?.trim().parse::<i64>().ok().filter(|n| *n > 0)
}

/// 見えるページの受信の下限 (`None` = 制限なし)
pub(crate) fn visible_since(state: &LeaveState) -> Option<DateTime<Utc>> {
    state
        .visible_minutes
        .map(|m| (state.now)() - chrono::Duration::minutes(m))
}

/// テスト送信の口の path (worker が、この口へのリクエストでだけ送信の口を組み立てるのに使う)
pub const TEST_NOTIFY_PATH: &str = "/api/leave/settings/test-notify";

/// テスト送信の文面 (固定)
pub const TEST_NOTIFY_TEXT: &str =
    "勤怠申請システムからのテスト通知です。この通知が届けば毎朝の警告も届きます。";

pub fn tenant_router() -> Router<LeaveState> {
    Router::new()
        .route("/leave/settings", get(get_settings).put(put_settings))
        .route("/leave/settings/test-notify", post(test_notify))
        .route("/leave/alerts", get(list_alerts))
        // 既定の 2MB を広げる。+1024 は上限をわずかに超えたものを handler の `pdf_too_large` (400) にするための余白
        .route(
            "/leave/uploads",
            post(upload_pdf).layer(DefaultBodyLimit::max(MAX_PDF_BYTES + 1024)),
        )
        .route("/leave/departments", get(departments))
        .route("/leave/employees", get(employees))
        .route("/leave/documents", get(list_documents))
        .route("/leave/pages", get(list_pages))
        .route("/leave/pages/{id}", get(get_page).put(put_page))
        .route("/leave/pages/{id}/pdf", get(get_pdf))
        .route("/leave/pages/{id}/candidates", get(ledger::candidates))
        .route("/leave/pages/{id}/base-date", put(put_base_date))
        .route("/leave/calendar", get(ledger::calendar))
        .route(
            "/leave/grants",
            get(ledger::list_grants).post(ledger::create_grant),
        )
        .route("/leave/grants/auto", post(ledger::auto_grants))
        .route(
            "/leave/opening",
            get(ledger::list_openings).put(ledger::put_openings),
        )
        .route(
            "/leave/grants/{id}",
            put(ledger::update_grant).delete(ledger::delete_grant),
        )
        .route_layer(middleware::from_fn(require_admin))
}

pub(crate) fn error(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({ "error": code }))).into_response()
}

pub(crate) fn bad_request(code: &str) -> Response {
    error(StatusCode::BAD_REQUEST, code)
}

pub(crate) fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not_found")
}

/// DB のエラー: kind (SQLSTATE か固定の語) だけをログに出し、500
pub(crate) fn db_error(state: &LeaveState, stage: &str, e: &tokio_postgres::Error) -> Response {
    (state.log)(&format!("{stage}: {}", alc_worker_db::kind(e)));
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
}

/// path の id。UUID でなければその行は在り得ないので 404
pub(crate) fn parse_id(id: &str) -> Option<Uuid> {
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

/// 保存済みの通知先 (グループか個人) へ固定の文面を 1 通送る。宛先は cron と同じ [`pg::notify_target`]。
/// 応答・ログは固定の語だけ (宛先・生のエラーは出さない)
async fn test_notify(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
) -> Response {
    let got = pg::notify_target(&mut *state.pg.lock().await, t.0).await;
    let target = match got {
        Ok(Some(target)) => target,
        Ok(None) => return bad_request("no_notify_target"),
        Err(e) => return db_error(&state, "test_notify", &e),
    };
    if !state.notifier.available() {
        return error(StatusCode::SERVICE_UNAVAILABLE, "notify_unavailable");
    }
    match state.notifier.send(target, TEST_NOTIFY_TEXT).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => {
            (state.log)(&format!("test_notify: send {}", e.0));
            error(StatusCode::BAD_GATEWAY, "notify_failed")
        }
    }
}

/// 管理画面からの PDF の取り込み。tenant は auth-worker のヘッダだけから決まる (本文からは決めない)。
/// 同じファイルでも毎回別の key・別の行 (seed はランダム)。応答・ログは固定の語だけ
async fn upload_pdf(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
    body: Bytes,
) -> Response {
    if let Err(code) = check_pdf_bytes(&body) {
        return bad_request(code);
    }
    let pdf = NewPdf {
        tenant_id: t.0,
        pdf: body.to_vec(),
        received_at: (state.now)(),
        mail_from: None,
        subject: None,
        key_seed: Uuid::new_v4().to_string(),
    };
    match ingest_pdf(&state.pg, state.pdfs.as_ref(), &state.log, pdf).await {
        Ok(pages) => (StatusCode::CREATED, Json(json!({ "pages": pages }))).into_response(),
        Err(IngestFailure::Drop(code)) => error(StatusCode::UNPROCESSABLE_ENTITY, code),
        Err(IngestFailure::Failed) => error(StatusCode::SERVICE_UNAVAILABLE, "ingest_failed"),
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
        Ok(PutSettings::Saved(s)) => Json::<Settings>(s).into_response(),
        Ok(PutSettings::UnknownChannel) => bad_request("unknown_lineworks_channel"),
        Ok(PutSettings::UnknownRecipient) => bad_request("unknown_notify_recipient"),
        Ok(PutSettings::UnknownBot) => bad_request("unknown_notify_bot"),
        Err(e) => db_error(&state, "put_settings", &e),
    }
}

pub(crate) fn ichiban_not_configured() -> Response {
    error(StatusCode::SERVICE_UNAVAILABLE, "ichiban_not_configured")
}

/// 一番星を 1 回呼び、2xx の本文だけを返す (それ以外は 502 の本文の語)
pub(crate) async fn ichiban_get(
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
    let got = pg::list_pages(&mut *state.pg.lock().await, t.0, q, visible_since(&state)).await;
    match got {
        Ok(pages) => Json(json!({ "data": pages })).into_response(),
        Err(e) => db_error(&state, "list_pages", &e),
    }
}

async fn list_documents(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
    raw: Result<Query<DocumentsQueryRaw>, QueryRejection>,
) -> Response {
    let Ok(Query(raw)) = raw else {
        return bad_request("invalid_query");
    };
    let q = match DocumentsQuery::parse(raw) {
        Ok(q) => q,
        Err(code) => return bad_request(code),
    };
    let got = pg::list_documents(&mut *state.pg.lock().await, t.0, q, visible_since(&state)).await;
    match got {
        Ok(docs) => Json(json!({ "data": docs })).into_response(),
        Err(e) => db_error(&state, "list_documents", &e),
    }
}

/// 未解消の警告 (毎朝の通知と同じ [`window`]・同じ SQL)
async fn list_alerts(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
) -> Response {
    let range = window(ledger::jst_date((state.now)()));
    let got = pg::open_alerts(&mut *state.pg.lock().await, t.0, range).await;
    match got {
        Ok(alerts) => Json(json!({ "alerts": alerts })).into_response(),
        Err(e) => db_error(&state, "list_alerts", &e),
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
    let got = pg::get_page(&mut *state.pg.lock().await, t.0, id, visible_since(&state)).await;
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
    // 一番星の行を確かめられなければ何も書かない
    let link_rows = match ledger::verified_link_rows(&state, t.0, id, &update).await {
        Ok(rows) => rows,
        Err(resp) => return resp,
    };
    let got = pg::update_page(
        &mut *state.pg.lock().await,
        t.0,
        id,
        update,
        link_rows,
        actor(&headers),
        visible_since(&state),
    )
    .await;
    match got {
        Ok(Some(d)) => Json(d).into_response(),
        Ok(None) => not_found(),
        Err(e) => db_error(&state, "put_page", &e),
    }
}

/// PDF の基準日を、そのページと同じ PDF の全ページに書く
async fn put_base_date(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
    Path(id): Path<String>,
    body: Result<Json<BaseDateInput>, JsonRejection>,
) -> Response {
    let Some(id) = parse_id(&id) else {
        return not_found();
    };
    let Ok(Json(input)) = body else {
        return bad_request("invalid_body");
    };
    let got = pg::set_base_date(
        &mut *state.pg.lock().await,
        t.0,
        id,
        input.base_date,
        visible_since(&state),
    )
    .await;
    match got {
        Ok(Some(n)) => Json(json!({ "base_date": input.base_date, "updated": n })).into_response(),
        Ok(None) => not_found(),
        Err(e) => db_error(&state, "put_base_date", &e),
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
    let got = pg::page_r2_key(&mut *state.pg.lock().await, t.0, id, visible_since(&state)).await;
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

#[cfg(test)]
mod tests {
    use super::visible_minutes;

    #[test]
    fn visible_minutes_is_none_unless_a_positive_number() {
        // 本番 (トップレベル) には vars を置かない = 制限なし
        assert_eq!(visible_minutes(None), None);
        assert_eq!(visible_minutes(Some("10")), Some(10));
        assert_eq!(visible_minutes(Some(" 10 ")), Some(10));
        assert_eq!(visible_minutes(Some("0")), None);
        assert_eq!(visible_minutes(Some("-5")), None);
        assert_eq!(visible_minutes(Some("")), None);
        assert_eq!(visible_minutes(Some("ten")), None);
    }
}
