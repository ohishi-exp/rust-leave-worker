//! 乗務員ごとのカレンダーと有給の付与台帳の口 (`routes::tenant_router` が `require_admin` の内側に載せる)。
//!
//! | method | path | 中身 |
//! |---|---|---|
//! | GET | `/leave/calendar?employee_cd=&month=YYYY-MM` | 日ごとの一番星と申請書の突き合わせ・月末の有給残・労災日数 |
//! | GET | `/leave/grants?employee_cd=` | 付与台帳と今日 (JST) の残 |
//! | POST | `/leave/grants` | 手で付与を足す (source = manual) |
//! | PUT / DELETE | `/leave/grants/{id}` | 付与を直す (source は manual になる) / 消す |
//! | POST | `/leave/grants/auto?employee_cd=` | 法定付与を計算し、無い付与日だけ足す (manual は上書きしない) |
//!
//! `employee_cd` を取る口は、自 tenant の `dept_codes` に入る一番星の乗務員だけを通す (外れたら 404)。一番星の binding が
//! 無い環境 (staging) では 503。
//!
//! **有給残は近似**: 一番星の休暇の口は 1 回 92 日まで・Service Binding の呼び出しは 1 リクエスト 32 回までなので、
//! 消化は基準日から [`CONSUMPTION_MONTHS`] か月 (入社日より前は取らない) だけをさかのぼって FIFO で引く。
//! 付与が残ったまま失効した時点より前の消化は以後の残に効かないので、ほとんどの乗務員で正確と一致する。
//! 応答の `consumption_from` が実際に数えた消化の最初の日。

// 口の途中の失敗をそのまま応答として返す (`Result<_, Response>`)。応答は 1 回作って返すだけなので box しない
#![allow(clippy::result_large_err)]

use std::collections::BTreeSet;
use std::sync::Arc;

use alc_core_wasm::TenantId;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use chrono::{DateTime, Datelike, Months, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::grants::{
    self, add_months, balance, statutory, valid_employee_cd, Grant, GrantUpdateInput, NewGrantInput,
};
use crate::ichiban::{IchibanApi, EMPLOYEES_PATH};
use crate::pg;
use crate::reconcile::{
    calendar_days, days_paths, expand_periods, parse_days, parse_employees, rousai_days, rows_of,
    yukyu_usages, Employee, IchibanDays,
};
use crate::routes::{
    bad_request, db_error, error, ichiban_get, ichiban_not_configured, not_found, parse_id,
    LeaveState,
};

/// 消化をさかのぼる月数 (92 日 × 16 回)
pub const CONSUMPTION_MONTHS: u32 = 48;

/// 日本時間の日付
pub fn jst_date(now: DateTime<Utc>) -> NaiveDate {
    (now + chrono::Duration::hours(9)).date_naive()
}

/// `YYYY-MM` の月初と月末
pub fn parse_month(raw: &str) -> Option<(NaiveDate, NaiveDate)> {
    if raw.len() != 7 {
        return None;
    }
    let start = NaiveDate::parse_from_str(&format!("{raw}-01"), "%Y-%m-%d").ok()?;
    let end = add_months(start, 1)?.pred_opt()?;
    Some((start, end))
}

/// 消化を数え始める日 (`as_of` の 48 か月前の翌日と入社日の遅い方)
fn consumption_from(as_of: NaiveDate, hire: Option<NaiveDate>) -> NaiveDate {
    let back = as_of
        .checked_sub_months(Months::new(CONSUMPTION_MONTHS))
        .and_then(|d| d.succ_opt())
        .unwrap_or(as_of);
    hire.map_or(back, |h| h.max(back))
}

#[derive(Deserialize, Debug, Default)]
pub struct EmployeeQuery {
    pub employee_cd: Option<String>,
    pub month: Option<String>,
}

fn employee_cd_of(
    raw: Result<Query<EmployeeQuery>, QueryRejection>,
) -> Result<EmployeeQuery, Response> {
    let Ok(Query(mut q)) = raw else {
        return Err(bad_request("invalid_query"));
    };
    q.employee_cd = Some(valid_employee_cd(q.employee_cd).map_err(bad_request)?);
    Ok(q)
}

fn ichiban_of(state: &LeaveState) -> Result<Arc<dyn IchibanApi>, Response> {
    state.ichiban.clone().ok_or_else(ichiban_not_configured)
}

/// 自 tenant の部門に入る乗務員 1 人 (外れたら 404)
async fn find_employee(
    state: &LeaveState,
    api: &Arc<dyn IchibanApi>,
    tenant_id: Uuid,
    code: &str,
) -> Result<Employee, Response> {
    let got = pg::get_settings(&mut *state.pg.lock().await, tenant_id).await;
    let dept_codes = match got {
        Ok(s) => s.unwrap_or_default().dept_codes,
        Err(e) => return Err(db_error(state, "employee settings", &e)),
    };
    if dept_codes.is_empty() {
        return Err(not_found());
    }
    let body = ichiban_get(state, api, EMPLOYEES_PATH)
        .await
        .map_err(|code| error(StatusCode::BAD_GATEWAY, code))?;
    let employees = parse_employees(&body, &dept_codes).map_err(|code| {
        (state.log)(&format!("ichiban {EMPLOYEES_PATH}: {code}"));
        error(StatusCode::BAD_GATEWAY, code)
    })?;
    employees
        .into_iter()
        .find(|e| e.employee_code == code)
        .ok_or_else(not_found)
}

/// 一番星の休暇を `from`〜`to` で取り、`codes` の乗務員だけを正規化する (92 日ごとに呼ぶ)
pub(crate) async fn fetch_days<'a>(
    state: &LeaveState,
    api: &Arc<dyn IchibanApi>,
    codes: &'a BTreeSet<String>,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<IchibanDays<'a>, &'static str> {
    let mut out = IchibanDays::default();
    for path in days_paths(from, to) {
        let body = ichiban_get(state, api, &path).await?;
        parse_days(&body, codes, &mut out).inspect_err(|code| {
            (state.log)(&format!("ichiban {}: {code}", crate::reconcile::DAYS_PATH));
        })?;
    }
    Ok(out)
}

/// 付与 1 件と、基準日の内訳 (日数)
#[derive(Serialize)]
struct GrantView {
    #[serde(flatten)]
    grant: Grant,
    used: f64,
    remaining: f64,
    expired: f64,
}

/// 口の本体は `Result<Response, Response>` で書き、途中の失敗 (`Err` の応答) もそのまま返す
fn respond(r: Result<Response, Response>) -> Response {
    r.unwrap_or_else(|e| e)
}

/// 一番星の失敗の語を 502 に
fn bad_gateway(code: &'static str) -> Response {
    error(StatusCode::BAD_GATEWAY, code)
}

pub async fn calendar(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
    raw: Result<Query<EmployeeQuery>, QueryRejection>,
) -> Response {
    respond(calendar_inner(state, t.0, raw).await)
}

async fn calendar_inner(
    state: LeaveState,
    tenant_id: Uuid,
    raw: Result<Query<EmployeeQuery>, QueryRejection>,
) -> Result<Response, Response> {
    let q = employee_cd_of(raw)?;
    let month = q.month.unwrap_or_default();
    let (month_start, month_end) =
        parse_month(&month).ok_or_else(|| bad_request("invalid_month"))?;
    let code = q.employee_cd.unwrap_or_default();
    let api = ichiban_of(&state)?;
    let emp = find_employee(&state, &api, tenant_id, &code).await?;
    let grant_rows = pg::list_grants(&mut *state.pg.lock().await, tenant_id, code.clone())
        .await
        .map_err(|e| db_error(&state, "calendar grants", &e))?;
    let periods = pg::linked_periods(
        &mut *state.pg.lock().await,
        tenant_id,
        Some(code.clone()),
        month_start,
        month_end,
    )
    .await
    .map_err(|e| db_error(&state, "calendar periods", &e))?;
    let used_from = consumption_from(month_end, emp.hire_date);
    let year_start = NaiveDate::from_ymd_opt(month_start.year(), 1, 1).unwrap_or(month_start);
    let codes = BTreeSet::from([code.clone()]);
    let days = fetch_days(&state, &api, &codes, used_from.min(year_start), month_end)
        .await
        .map_err(bad_gateway)?;
    let mine = rows_of(&days.rows, &code);
    let in_month: Vec<_> = mine
        .iter()
        .copied()
        .filter(|r| month_start <= r.date && r.date <= month_end)
        .collect();
    let usages: Vec<_> = yukyu_usages(&mine)
        .into_iter()
        .filter(|(d, _)| *d >= used_from)
        .collect();
    let inputs: Vec<_> = grant_rows.iter().map(Grant::input).collect();
    let b = balance(&inputs, &usages, month_end);
    Ok(Json(json!({
        "employee_cd": code,
        "employee_name": emp.employee_name,
        "month": month,
        "days": calendar_days(&in_month, &expand_periods(&periods, month_start, month_end)),
        "yukyu": {
            "as_of": month_end,
            "remaining": grants::days(b.remaining),
            "expired": grants::days(b.expired),
            "unallocated": grants::days(b.unallocated),
            "consumption_from": used_from,
        },
        "rousai": {
            "year": rousai_days(&mine, year_start, month_end),
            "month": rousai_days(&mine, month_start, month_end),
        },
        "unknown_items": days.unknown.values().sum::<usize>(),
    }))
    .into_response())
}

pub async fn list_grants(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
    raw: Result<Query<EmployeeQuery>, QueryRejection>,
) -> Response {
    respond(list_grants_inner(state, t.0, raw).await)
}

async fn list_grants_inner(
    state: LeaveState,
    tenant_id: Uuid,
    raw: Result<Query<EmployeeQuery>, QueryRejection>,
) -> Result<Response, Response> {
    let code = employee_cd_of(raw)?.employee_cd.unwrap_or_default();
    let api = ichiban_of(&state)?;
    let emp = find_employee(&state, &api, tenant_id, &code).await?;
    let grant_rows = pg::list_grants(&mut *state.pg.lock().await, tenant_id, code.clone())
        .await
        .map_err(|e| db_error(&state, "list_grants", &e))?;
    let today = jst_date((state.now)());
    let used_from = consumption_from(today, emp.hire_date);
    let codes = BTreeSet::from([code.clone()]);
    let days = fetch_days(&state, &api, &codes, used_from, today)
        .await
        .map_err(bad_gateway)?;
    let inputs: Vec<_> = grant_rows.iter().map(Grant::input).collect();
    let b = balance(&inputs, &yukyu_usages(&days.rows), today);
    let data: Vec<GrantView> = grant_rows
        .into_iter()
        .zip(&b.grants)
        .map(|(grant, gb)| GrantView {
            grant,
            used: grants::days(gb.used),
            remaining: grants::days(gb.remaining),
            expired: grants::days(gb.expired),
        })
        .collect();
    let v = b.view(today);
    Ok(Json(json!({
        "employee_cd": code,
        "employee_name": emp.employee_name,
        "hire_date": emp.hire_date,
        "balance": {
            "as_of": v.as_of,
            "remaining": v.remaining,
            "expired": v.expired,
            "unallocated": v.unallocated,
            "consumption_from": used_from,
        },
        "data": data,
    }))
    .into_response())
}

pub async fn create_grant(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
    body: Result<Json<NewGrantInput>, JsonRejection>,
) -> Response {
    respond(create_grant_inner(state, t.0, body).await)
}

async fn create_grant_inner(
    state: LeaveState,
    tenant_id: Uuid,
    body: Result<Json<NewGrantInput>, JsonRejection>,
) -> Result<Response, Response> {
    let Json(input) = body.map_err(|_| bad_request("invalid_body"))?;
    let g = grants::validate_new(input).map_err(bad_request)?;
    let api = ichiban_of(&state)?;
    find_employee(&state, &api, tenant_id, &g.employee_cd).await?;
    let got = pg::insert_grant(&mut *state.pg.lock().await, tenant_id, g)
        .await
        .map_err(|e| db_error(&state, "create_grant", &e))?;
    let g = got.ok_or_else(|| error(StatusCode::CONFLICT, "grant_exists"))?;
    Ok((StatusCode::CREATED, Json(g)).into_response())
}

pub async fn update_grant(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
    Path(id): Path<String>,
    body: Result<Json<GrantUpdateInput>, JsonRejection>,
) -> Response {
    let Some(id) = parse_id(&id) else {
        return not_found();
    };
    let Ok(Json(input)) = body else {
        return bad_request("invalid_body");
    };
    let g = match grants::validate_update(input) {
        Ok(g) => g,
        Err(code) => return bad_request(code),
    };
    let got = pg::update_grant(&mut *state.pg.lock().await, t.0, id, g).await;
    match got {
        Ok(Some(g)) => Json(g).into_response(),
        Ok(None) => not_found(),
        // expires_on > grant_date の CHECK
        Err(e) if alc_worker_db::kind(&e) == "23514" => bad_request("expires_before_grant"),
        Err(e) => db_error(&state, "update_grant", &e),
    }
}

pub async fn delete_grant(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
    Path(id): Path<String>,
) -> Response {
    let Some(id) = parse_id(&id) else {
        return not_found();
    };
    let got = pg::delete_grant(&mut *state.pg.lock().await, t.0, id).await;
    match got {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found(),
        Err(e) => db_error(&state, "delete_grant", &e),
    }
}

pub async fn auto_grants(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
    raw: Result<Query<EmployeeQuery>, QueryRejection>,
) -> Response {
    respond(auto_grants_inner(state, t.0, raw).await)
}

async fn auto_grants_inner(
    state: LeaveState,
    tenant_id: Uuid,
    raw: Result<Query<EmployeeQuery>, QueryRejection>,
) -> Result<Response, Response> {
    let code = employee_cd_of(raw)?.employee_cd.unwrap_or_default();
    let api = ichiban_of(&state)?;
    let emp = find_employee(&state, &api, tenant_id, &code).await?;
    let today = jst_date((state.now)());
    let computed = emp
        .hire_date
        .map(|h| statutory(h, emp.retire_date, today))
        .unwrap_or_default();
    let rows = computed.iter().map(|g| (code.clone(), *g)).collect();
    let inserted = pg::insert_auto_grants(&mut *state.pg.lock().await, tenant_id, rows)
        .await
        .map_err(|e| db_error(&state, "auto_grants", &e))?;
    let computed: Vec<_> = computed
        .iter()
        .map(|g| json!({"grant_date": g.grant_date, "days": grants::days(g.tenths), "expires_on": g.expires_on}))
        .collect();
    Ok(Json(json!({
        "employee_cd": code,
        "hire_date": emp.hire_date,
        "inserted": inserted,
        "computed": computed,
    }))
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn months() {
        assert_eq!(
            parse_month("2026-02"),
            Some((d("2026-02-01"), d("2026-02-28")))
        );
        assert_eq!(
            parse_month("2024-12"),
            Some((d("2024-12-01"), d("2024-12-31")))
        );
        for bad in ["2026-2", "2026-13", "2026-02-01", "abcd-ef", ""] {
            assert_eq!(parse_month(bad), None, "{bad}");
        }
    }

    #[test]
    fn jst_and_consumption_window() {
        assert_eq!(
            jst_date("2026-10-09T15:00:00Z".parse().unwrap()),
            d("2026-10-10")
        );
        assert_eq!(
            jst_date("2026-10-09T14:59:59Z".parse().unwrap()),
            d("2026-10-09")
        );
        assert_eq!(consumption_from(d("2026-10-31"), None), d("2022-11-01"));
        assert_eq!(
            consumption_from(d("2026-10-31"), Some(d("2024-04-01"))),
            d("2024-04-01")
        );
        assert_eq!(
            consumption_from(d("2026-10-31"), Some(d("2001-04-01"))),
            d("2022-11-01")
        );
        assert_eq!(consumption_from(NaiveDate::MIN, None), NaiveDate::MIN);
    }
}
