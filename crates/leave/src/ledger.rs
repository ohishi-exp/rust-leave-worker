//! 乗務員ごとのカレンダーと有給の付与台帳の口 (`routes::tenant_router` が載せる。参照の GET は leave.use 層、付与・期首残の書き込みは admin 層)。
//!
//! | method | path | 中身 |
//! |---|---|---|
//! | GET | `/leave/calendar?employee_cd=&month=YYYY-MM` | 日ごとの一番星と申請書の突き合わせ (労災のページはそのあと一番星の労災が続く日も覆う)・月末の有給残・労災日数 |
//! | GET | `/leave/grants?employee_cd=` | 付与台帳と今日 (JST) の残・期首残 |
//! | POST | `/leave/grants` | 手で付与を足す (source = manual) |
//! | PUT / DELETE | `/leave/grants/{id}` | 付与を直す (source は manual になる) / 消す |
//! | POST | `/leave/grants/auto?employee_cd=` | 法定付与を計算し、無い付与日だけ足す (manual は上書きしない) |
//! | GET / PUT | `/leave/opening` | 全員分の期首残 / 一括保存 (両方 null の行は削除) |
//! | GET | `/leave/ledger?as_of=YYYY-MM-DD` | 全員分の有給の年度 (4 月始まり) の一覧 (基準日の省略は今日 (JST)) |
//! | GET | `/leave/pages/{id}/candidates?employee_cd=` | ページの窓 (基準日か受信日の前後 7 日と保存済みの期間を合わせた範囲) の一番星の休暇の行を期間にまとめる |
//!
//! 紐づけの更新 (`PUT /leave/pages/{id}`) が受け取る一番星の行の確かめ ([`verified_link_rows`]) もここに置く。
//!
//! **期首残** (`leave_opening`、1 人 1 行): 期首日 X 時点の残 (繰越・今年度) が在り、残の基準日が X 以降なら、
//! 付与日 <= X の付与台帳の行を外して (`superseded`) 合成の付与 2 件に置き換え、消化は X の翌日以降だけを数える
//! (`grants::balance_with_opening`)。基準日が X より前なら従来の計算。
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

use std::collections::{BTreeMap, BTreeSet};
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
    self, add_months, balance_with_opening, opening_grants, statutory, valid_employee_cd, Balance,
    Grant, GrantUpdateInput, NewGrantInput, Opening, OpeningGrants, OpeningPutInput,
};
use crate::holidays;
use crate::ichiban::{IchibanApi, EMPLOYEES_PATH};
use crate::pages::{Page, PageUpdate, Period};
use crate::pg;
use crate::reconcile::{
    calendar_days, candidate_periods, days_paths, expand_periods, extension_span, parse_employees,
    pick_link_rows, push_days, read_day_rows, rousai_days, rousai_extension, rows_of, yukyu_usages,
    Employee, IchibanDays, IchibanRow, LinkRow, DAYS_MAX_SPAN,
};
use crate::routes::{
    bad_request, db_error, error, ichiban_get, ichiban_not_configured, not_found, parse_id,
    visible_since, LeaveState,
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

/// 消化を数え始める日 (`as_of` の 48 か月前の翌日と入社日の遅い方。期首残を使うなら期首日の翌日とも比べる)
fn consumption_from(
    as_of: NaiveDate,
    hire: Option<NaiveDate>,
    opening: Option<&OpeningGrants>,
) -> NaiveDate {
    let back = as_of
        .checked_sub_months(Months::new(CONSUMPTION_MONTHS))
        .and_then(|d| d.succ_opt())
        .unwrap_or(as_of);
    let from = hire.map_or(back, |h| h.max(back));
    opening
        .filter(|o| o.applies(as_of))
        .and_then(|o| o.opening.as_of.succ_opt())
        .map_or(from, |x| x.max(from))
}

#[derive(Deserialize, Debug, Default)]
pub struct EmployeeQuery {
    pub employee_cd: Option<String>,
    pub month: Option<String>,
    /// 候補の口だけが読む探す期間 (両方あるときだけ使う。[`parse_search_range`])
    pub from: Option<String>,
    pub to: Option<String>,
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

/// 自 tenant の部門に入る一番星の乗務員 (部門を選んでいない tenant は空で、一番星も呼ばない)
async fn tenant_employees(
    state: &LeaveState,
    api: &Arc<dyn IchibanApi>,
    tenant_id: Uuid,
) -> Result<Vec<Employee>, Response> {
    let got = pg::get_settings(&mut *state.pg.lock().await, tenant_id).await;
    let dept_codes = match got {
        Ok(s) => s.unwrap_or_default().dept_codes,
        Err(e) => return Err(db_error(state, "employee settings", &e)),
    };
    if dept_codes.is_empty() {
        return Ok(Vec::new());
    }
    let body = ichiban_get(state, api, EMPLOYEES_PATH)
        .await
        .map_err(bad_gateway)?;
    parse_employees(&body, &dept_codes).map_err(|code| {
        (state.log)(&format!("ichiban {EMPLOYEES_PATH}: {code}"));
        bad_gateway(code)
    })
}

/// 自 tenant の部門に入る乗務員 1 人 (外れたら 404)
async fn find_employee(
    state: &LeaveState,
    api: &Arc<dyn IchibanApi>,
    tenant_id: Uuid,
    code: &str,
) -> Result<Employee, Response> {
    tenant_employees(state, api, tenant_id)
        .await?
        .into_iter()
        .find(|e| e.employee_code == code)
        .ok_or_else(not_found)
}

/// 乗務員 1 人の期首残 (無ければ `None`)
async fn opening_of(
    state: &LeaveState,
    tenant_id: Uuid,
    emp: &Employee,
) -> Result<Option<OpeningGrants>, Response> {
    let got = pg::get_opening(
        &mut *state.pg.lock().await,
        tenant_id,
        emp.employee_code.clone(),
    )
    .await
    .map_err(|e| db_error(state, "opening", &e))?;
    Ok(got.map(|o| opening_grants(o, emp.hire_date, emp.retire_date)))
}

/// 一番星の休暇の行を `from`〜`to` で取り、`codes` の乗務員の行だけを返す (92 日ごとに呼ぶ。正規化はしない)
pub(crate) async fn fetch_rows<'a>(
    state: &LeaveState,
    api: &Arc<dyn IchibanApi>,
    codes: &'a BTreeSet<String>,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<IchibanRow<'a>>, &'static str> {
    let mut out = Vec::new();
    for path in days_paths(from, to) {
        let body = ichiban_get(state, api, &path).await?;
        out.extend(read_day_rows(&body, codes).inspect_err(|code| {
            (state.log)(&format!("ichiban {}: {code}", crate::reconcile::DAYS_PATH));
        })?);
    }
    Ok(out)
}

/// 一番星の休暇を `from`〜`to` で取り、`codes` の乗務員だけを正規化する ([`fetch_rows`])
pub(crate) async fn fetch_days<'a>(
    state: &LeaveState,
    api: &Arc<dyn IchibanApi>,
    codes: &'a BTreeSet<String>,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<IchibanDays<'a>, &'static str> {
    let mut out = IchibanDays::default();
    push_days(&fetch_rows(state, api, codes, from, to).await?, &mut out);
    Ok(out)
}

/// 候補を探す窓の幅 (中心の日の前後の日数)
pub const CANDIDATE_DAYS: i64 = 7;

/// 候補の窓を保存済みの期間で広げる上限 (月)。超えたら中心の前後だけ (異常な保存値で一番星を叩きすぎない)
pub const CANDIDATE_MAX_MONTHS: u32 = 12;

/// 候補を探す窓 (両端を含む)。PDF の基準日、無ければ受信日 (JST) の前後 [`CANDIDATE_DAYS`] 日と、そのページの保存済みの
/// 期間 (`periods`) の最初の日〜最後の日を合わせた範囲 (保存済みの行を候補に出し、紐づけ直せるように)。
/// 合わせた範囲が [`CANDIDATE_MAX_MONTHS`] か月を超えたら中心の前後だけ
pub fn candidate_window(page: &Page, periods: &[Period]) -> (NaiveDate, NaiveDate) {
    let center = page.base_date.unwrap_or_else(|| jst_date(page.received_at));
    let span = chrono::Duration::days(CANDIDATE_DAYS);
    let around = (center - span, center + span);
    let from = periods
        .iter()
        .map(|p| p.start_date)
        .fold(around.0, NaiveDate::min);
    let to = periods
        .iter()
        .map(|p| p.end_date)
        .fold(around.1, NaiveDate::max);
    match add_months(from, CANDIDATE_MAX_MONTHS) {
        Some(limit) if to <= limit => (from, to),
        _ => around,
    }
}

/// 候補の口の探す期間 (`from`/`to`、両端を含む)。どちらも無ければ `None` (ページの窓)。片方だけ・日付として読めない・
/// `from > to`・幅が [`DAYS_MAX_SPAN`] 日 (両端を含む) を超えるときは `invalid_range`
pub fn parse_search_range(
    from: Option<&str>,
    to: Option<&str>,
) -> Result<Option<(NaiveDate, NaiveDate)>, &'static str> {
    let (from, to) = match (from, to) {
        (None, None) => return Ok(None),
        (Some(f), Some(t)) => (grants::parse_date(f), grants::parse_date(t)),
        _ => return Err("invalid_range"),
    };
    match (from, to) {
        (Some(f), Some(t)) if f <= t && (t - f).num_days() < DAYS_MAX_SPAN => Ok(Some((f, t))),
        _ => Err("invalid_range"),
    }
}

/// ページの窓 ([`candidate_window`]) で、乗務員 `code` の一番星の行を取る (`codes` は `code` 1 つだけの集合)。
/// `range` があればその範囲 (ページの基準日・保存済みの期間は見ない)。窓は保存済みの期間に `extra` (紐づけの保存で送られた期間) を足して広げる (候補の口は空)。
/// ページが自 tenant に見えない・乗務員が自 tenant の部門から外れたら 404、一番星が無ければ 503、一番星の失敗は 502
async fn page_window_rows<'a>(
    state: &LeaveState,
    tenant_id: Uuid,
    page_id: Uuid,
    code: &str,
    codes: &'a BTreeSet<String>,
    extra: &[Period],
    range: Option<(NaiveDate, NaiveDate)>,
) -> Result<Vec<IchibanRow<'a>>, Response> {
    let got = pg::get_page(
        &mut *state.pg.lock().await,
        tenant_id,
        page_id,
        visible_since(state),
    )
    .await
    .map_err(|e| db_error(state, "page window", &e))?;
    let detail = got.ok_or_else(not_found)?;
    let api = ichiban_of(state)?;
    find_employee(state, &api, tenant_id, code).await?;
    let periods = [detail.periods.as_slice(), extra].concat();
    let (from, to) = range.unwrap_or_else(|| candidate_window(&detail.page, &periods));
    fetch_rows(state, &api, codes, from, to)
        .await
        .map_err(bad_gateway)
}

/// `GET /leave/pages/{id}/candidates?employee_cd=&from=&to=`: ページの窓 (`from`/`to` があればその期間) の一番星の休暇の行を、続く日ごとの期間にまとめて返す
pub async fn candidates(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
    Path(id): Path<String>,
    raw: Result<Query<EmployeeQuery>, QueryRejection>,
) -> Response {
    respond(candidates_inner(state, t.0, id, raw).await)
}

async fn candidates_inner(
    state: LeaveState,
    tenant_id: Uuid,
    id: String,
    raw: Result<Query<EmployeeQuery>, QueryRejection>,
) -> Result<Response, Response> {
    let id = parse_id(&id).ok_or_else(not_found)?;
    let q = employee_cd_of(raw)?;
    let range = parse_search_range(q.from.as_deref(), q.to.as_deref()).map_err(bad_request)?;
    let code = q.employee_cd.unwrap_or_default();
    let codes = BTreeSet::from([code.clone()]);
    let rows = page_window_rows(&state, tenant_id, id, &code, &codes, &[], range).await?;
    Ok(Json(json!({ "periods": candidate_periods(&rows, &code) })).into_response())
}

/// 紐づけの更新で受け取った一番星の行 (`ichiban_row_ids`) を確かめ、保存する中身を返す。
/// `status = linked` で行が在るときだけ、ページの窓 (送られた期間 `u.periods` でも広げる) で一番星を引き直す (全部が在り、本文の乗務員の行でなければ 400
/// `unknown_row`)。それ以外は空で、一番星も部門も見ない
pub(crate) async fn verified_link_rows(
    state: &LeaveState,
    tenant_id: Uuid,
    page_id: Uuid,
    u: &PageUpdate,
) -> Result<Vec<LinkRow>, Response> {
    if u.status != "linked" || u.ichiban_row_ids.is_empty() {
        return Ok(Vec::new());
    }
    let code = u.employee_cd.clone().unwrap_or_default();
    let codes = BTreeSet::from([code.clone()]);
    let rows = page_window_rows(state, tenant_id, page_id, &code, &codes, &u.periods, None).await?;
    pick_link_rows(&rows, &code, &u.ichiban_row_ids).ok_or_else(|| bad_request("unknown_row"))
}

/// 付与 1 件と、基準日の内訳 (日数)
#[derive(Serialize)]
struct GrantView {
    #[serde(flatten)]
    grant: Grant,
    used: f64,
    remaining: f64,
    expired: f64,
    /// 期首残で置き換わった (期首残を使う基準日で、付与日 <= 期首日)。内訳は 0
    superseded: bool,
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
    let opening = opening_of(&state, tenant_id, &emp).await?;
    let used_from = consumption_from(month_end, emp.hire_date, opening.as_ref());
    let year_start = NaiveDate::from_ymd_opt(month_start.year(), 1, 1).unwrap_or(month_start);
    let codes = BTreeSet::from([code.clone()]);
    let read_from = used_from.min(year_start);
    let days = fetch_days(&state, &api, &codes, read_from, month_end)
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
    // 申請書の側: 月の期間と、労災の期間を一番星の続く労災の日へ延ばした日 (読んだ範囲で数え、月の日だけ出す)
    let holiday_days = match extension_span(&periods, read_from, month_end) {
        Some((a, b)) => {
            let (got, unavailable) = holidays::load(state.holidays.as_ref(), a, b).await;
            if unavailable {
                (state.log)(holidays::UNAVAILABLE);
            }
            got
        }
        None => BTreeSet::new(),
    };
    let mut pdf = expand_periods(&periods, month_start, month_end);
    pdf.extend(
        rousai_extension(&mine, &periods, &holiday_days, read_from, month_end)
            .into_iter()
            .filter(|p| month_start <= p.date),
    );
    let inputs: Vec<_> = grant_rows.iter().map(Grant::input).collect();
    let b = balance_with_opening(&inputs, &usages, month_end, opening.as_ref());
    Ok(Json(json!({
        "employee_cd": code,
        "employee_name": emp.employee_name,
        "month": month,
        "days": calendar_days(&in_month, &pdf),
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
    let opening = opening_of(&state, tenant_id, &emp).await?;
    let today = jst_date((state.now)());
    let used_from = consumption_from(today, emp.hire_date, opening.as_ref());
    let codes = BTreeSet::from([code.clone()]);
    let days = fetch_days(&state, &api, &codes, used_from, today)
        .await
        .map_err(bad_gateway)?;
    let b = employee_balance(
        &grant_rows,
        &yukyu_usages(&days.rows),
        used_from,
        today,
        opening.as_ref(),
    );
    let applied = opening.filter(|o| o.applies(today));
    let data: Vec<GrantView> = grant_rows
        .into_iter()
        .zip(&b.grants)
        .map(|(grant, gb)| GrantView {
            superseded: applied.is_some_and(|o| o.supersedes(grant.grant_date)),
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
        "opening": opening.map(|o| o.view()),
        "data": data,
    }))
    .into_response())
}

/// 乗務員 1 人の基準日 `as_of` の残。消化は `used_from` 以降だけを数える (付与台帳の口と全員分の一覧で同じ計算)
fn employee_balance(
    grant_rows: &[Grant],
    usages: &[(NaiveDate, i32)],
    used_from: NaiveDate,
    as_of: NaiveDate,
    opening: Option<&OpeningGrants>,
) -> Balance {
    let inputs: Vec<_> = grant_rows.iter().map(Grant::input).collect();
    let usages: Vec<_> = usages
        .iter()
        .copied()
        .filter(|(d, _)| *d >= used_from)
        .collect();
    balance_with_opening(&inputs, &usages, as_of, opening)
}

#[derive(Deserialize, Debug, Default)]
pub struct AsOfQuery {
    pub as_of: Option<String>,
}

/// 基準日 (省略は `today`) とその年度。日付でない・`today` の 1 年後より後は 400 `invalid_as_of`
fn as_of_of(
    raw: Result<Query<AsOfQuery>, QueryRejection>,
    today: NaiveDate,
) -> Result<(NaiveDate, (NaiveDate, NaiveDate)), Response> {
    let Ok(Query(q)) = raw else {
        return Err(bad_request("invalid_query"));
    };
    let as_of = match q.as_of {
        None => Some(today),
        Some(raw) => grants::parse_date(&raw),
    };
    let limit = add_months(today, 12).unwrap_or(today);
    as_of
        .filter(|d| *d <= limit)
        .and_then(|d| Some((d, grants::fiscal_year_of(d)?)))
        .ok_or_else(|| bad_request("invalid_as_of"))
}

/// 全員分の一覧の 1 行 (日数。残の内訳は基準日の [`employee_balance`])
#[derive(Serialize)]
struct LedgerRowView {
    employee_cd: String,
    employee_name: Option<String>,
    hire_date: Option<NaiveDate>,
    /// 基準日以前で一番新しい付与 (期首残で置き換わった行は除く)
    latest_grant: Option<serde_json::Value>,
    /// 付与日が年度の初日〜基準日の付与の合計
    granted_in_fy: f64,
    /// 一番星の有給の取得のうち年度の初日〜基準日の合計
    used_in_fy: f64,
    remaining: f64,
    expired: f64,
    unallocated: f64,
}

/// `GET /leave/ledger?as_of=`: 自 tenant の部門に入る全員の、基準日を含む年度の有給。
/// 一番星の休暇は全員分を 1 つの窓 (各人の消化の最初の日と年度の初日の早い方〜基準日) で 1 回だけ取る
pub async fn ledger(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
    raw: Result<Query<AsOfQuery>, QueryRejection>,
) -> Response {
    respond(ledger_inner(state, t.0, raw).await)
}

async fn ledger_inner(
    state: LeaveState,
    tenant_id: Uuid,
    raw: Result<Query<AsOfQuery>, QueryRejection>,
) -> Result<Response, Response> {
    let (as_of, (fy_start, fy_end)) = as_of_of(raw, jst_date((state.now)()))?;
    let api = ichiban_of(&state)?;
    let mut employees = tenant_employees(&state, &api, tenant_id).await?;
    employees.sort_by(|a, b| a.employee_code.cmp(&b.employee_code));
    let mut grant_rows: BTreeMap<String, Vec<Grant>> = BTreeMap::new();
    let all = pg::list_grants_all(&mut *state.pg.lock().await, tenant_id)
        .await
        .map_err(|e| db_error(&state, "ledger grants", &e))?;
    for g in all {
        grant_rows.entry(g.employee_cd.clone()).or_default().push(g);
    }
    let saved: BTreeMap<String, Opening> =
        pg::list_openings(&mut *state.pg.lock().await, tenant_id)
            .await
            .map_err(|e| db_error(&state, "ledger openings", &e))?
            .into_iter()
            .collect();
    let people: Vec<(Employee, Option<OpeningGrants>, NaiveDate)> = employees
        .into_iter()
        .map(|e| {
            let opening = saved
                .get(&e.employee_code)
                .map(|o| opening_grants(*o, e.hire_date, e.retire_date));
            let used_from = consumption_from(as_of, e.hire_date, opening.as_ref());
            (e, opening, used_from)
        })
        .collect();
    let mut usages: BTreeMap<String, Vec<(NaiveDate, i32)>> = BTreeMap::new();
    if let Some(from) = people.iter().map(|p| p.2).min() {
        let codes: BTreeSet<String> = people.iter().map(|p| p.0.employee_code.clone()).collect();
        let days = fetch_days(&state, &api, &codes, from.min(fy_start), as_of)
            .await
            .map_err(bad_gateway)?;
        for code in &codes {
            usages.insert(code.clone(), yukyu_usages(&rows_of(&days.rows, code)));
        }
    }
    let data: Vec<LedgerRowView> = people
        .into_iter()
        .map(|(e, opening, used_from)| {
            let rows = grant_rows.remove(&e.employee_code).unwrap_or_default();
            let used = usages.remove(&e.employee_code).unwrap_or_default();
            let b = employee_balance(&rows, &used, used_from, as_of, opening.as_ref());
            let applied = opening.filter(|o| o.applies(as_of));
            let latest = rows.iter().rfind(|g| {
                g.grant_date <= as_of && !applied.is_some_and(|o| o.supersedes(g.grant_date))
            });
            let in_fy = |d: NaiveDate| fy_start <= d && d <= as_of;
            LedgerRowView {
                latest_grant: latest.map(|g| json!({"grant_date": g.grant_date, "days": g.days})),
                granted_in_fy: grants::days(
                    rows.iter()
                        .filter(|g| in_fy(g.grant_date))
                        .map(|g| g.input().tenths)
                        .sum(),
                ),
                used_in_fy: grants::days(used.iter().filter(|u| in_fy(u.0)).map(|u| u.1).sum()),
                remaining: grants::days(b.remaining),
                expired: grants::days(b.expired),
                unallocated: grants::days(b.unallocated),
                employee_cd: e.employee_code,
                employee_name: e.employee_name,
                hire_date: e.hire_date,
            }
        })
        .collect();
    Ok(Json(json!({
        "as_of": as_of,
        "fiscal_year": {"start": fy_start, "end": fy_end, "label": fy_start.year()},
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

/// 期首残の一覧の 1 行 (乗務員は自 tenant の部門に入る一番星の全員)
#[derive(Serialize)]
struct OpeningRowView {
    employee_cd: String,
    employee_name: Option<String>,
    dept_code: Option<serde_json::Value>,
    hire_date: Option<NaiveDate>,
    retire_date: Option<NaiveDate>,
    opening: Option<serde_json::Value>,
}

/// `GET /leave/opening`: 全員分の保存済みの期首残 (今の残は返さない。一番星 1 回 + DB)
pub async fn list_openings(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
) -> Response {
    respond(list_openings_inner(state, t.0).await)
}

async fn list_openings_inner(state: LeaveState, tenant_id: Uuid) -> Result<Response, Response> {
    let api = ichiban_of(&state)?;
    let employees = tenant_employees(&state, &api, tenant_id).await?;
    let saved: BTreeMap<String, Opening> =
        pg::list_openings(&mut *state.pg.lock().await, tenant_id)
            .await
            .map_err(|e| db_error(&state, "list_openings", &e))?
            .into_iter()
            .collect();
    let data: Vec<OpeningRowView> = employees
        .into_iter()
        .map(|e| OpeningRowView {
            opening: saved.get(&e.employee_code).map(|o| {
                json!({"as_of": o.as_of, "carryover": grants::days(o.carryover), "current": grants::days(o.current)})
            }),
            employee_cd: e.employee_code,
            employee_name: e.employee_name,
            dept_code: e.dept_code,
            hire_date: e.hire_date,
            retire_date: e.retire_date,
        })
        .collect();
    Ok(Json(json!({ "data": data })).into_response())
}

/// `PUT /leave/opening`: 期首残の一括保存。1 行でも不正なら全体を 400 (一部だけ保存しない)
pub async fn put_openings(
    State(state): State<LeaveState>,
    Extension(t): Extension<TenantId>,
    body: Result<Json<OpeningPutInput>, JsonRejection>,
) -> Response {
    respond(put_openings_inner(state, t.0, body).await)
}

async fn put_openings_inner(
    state: LeaveState,
    tenant_id: Uuid,
    body: Result<Json<OpeningPutInput>, JsonRejection>,
) -> Result<Response, Response> {
    let Json(input) = body.map_err(|_| bad_request("invalid_body"))?;
    let put = grants::validate_opening(input).map_err(bad_request)?;
    let api = ichiban_of(&state)?;
    let employees: BTreeMap<String, Employee> = tenant_employees(&state, &api, tenant_id)
        .await?
        .into_iter()
        .map(|e| (e.employee_code.clone(), e))
        .collect();
    let mut codes = put.upserts.iter().map(|(c, _, _)| c).chain(&put.deletes);
    if codes.any(|c| !employees.contains_key(c)) {
        return Err(bad_request("unknown_employee"));
    }
    for (code, carryover, current) in &put.upserts {
        let e = &employees[code];
        let o = Opening {
            as_of: put.as_of,
            carryover: *carryover,
            current: *current,
        };
        if let Some(code) = opening_grants(o, e.hire_date, e.retire_date).error() {
            return Err(bad_request(code));
        }
    }
    let (saved, deleted) = pg::put_openings(&mut *state.pg.lock().await, tenant_id, put)
        .await
        .map_err(|e| db_error(&state, "put_openings", &e))?;
    Ok(Json(json!({ "saved": saved, "deleted": deleted })).into_response())
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
        assert_eq!(
            consumption_from(d("2026-10-31"), None, None),
            d("2022-11-01")
        );
        assert_eq!(
            consumption_from(d("2026-10-31"), Some(d("2024-04-01")), None),
            d("2024-04-01")
        );
        assert_eq!(
            consumption_from(d("2026-10-31"), Some(d("2001-04-01")), None),
            d("2022-11-01")
        );
        assert_eq!(consumption_from(NaiveDate::MIN, None, None), NaiveDate::MIN);
        // 期首残を使うなら期首日の翌日から (基準日が期首日より前なら使わない)
        let o = opening_grants(
            Opening {
                as_of: d("2026-09-30"),
                carryover: 0,
                current: 0,
            },
            Some(d("2024-04-01")),
            None,
        );
        assert_eq!(
            consumption_from(d("2026-10-31"), Some(d("2024-04-01")), Some(&o)),
            d("2026-10-01")
        );
        assert_eq!(
            consumption_from(d("2026-09-29"), Some(d("2024-04-01")), Some(&o)),
            d("2024-04-01")
        );
    }

    #[test]
    fn search_range_validation() {
        let ok = |f: &str, t: &str| parse_search_range(Some(f), Some(t));
        assert_eq!(parse_search_range(None, None), Ok(None));
        assert_eq!(
            ok("2026-10-01", "2026-10-01"),
            Ok(Some((d("2026-10-01"), d("2026-10-01"))))
        );
        // 両端を含めて 92 日までは通り、93 日は弾く
        assert!(ok("2026-10-01", "2026-12-31").is_ok());
        assert_eq!(ok("2026-10-01", "2027-01-01"), Err("invalid_range"));
        assert!(ok("2026-10-01", "2026-12-30").is_ok());
        for (f, t) in [
            (Some("2026-10-01"), None),
            (None, Some("2026-10-01")),
            (Some("2026-10-02"), Some("2026-10-01")),
            (Some("2026-13-01"), Some("2026-10-01")),
            (Some("2026-10-01"), Some("x")),
            (Some("2026-1-1"), Some("2026-10-01")),
        ] {
            assert_eq!(parse_search_range(f, t), Err("invalid_range"));
        }
    }

    #[test]
    fn candidate_window_covers_saved_periods() {
        let at: DateTime<Utc> = "2026-10-09T15:00:00Z".parse().unwrap();
        let page = |base: Option<&str>| Page {
            id: Uuid::nil(),
            r2_key: String::new(),
            page_no: 1,
            mail_from: None,
            subject: None,
            received_at: at,
            employee_cd: None,
            status: "linked".into(),
            linked_by: None,
            linked_at: None,
            note: None,
            created_at: at,
            base_date: base.map(d),
        };
        let period = |s: &str, e: &str| Period {
            kind: "yukyu".into(),
            start_date: d(s),
            end_date: d(e),
            half: None,
        };
        let based = page(Some("2026-10-17"));
        // 保存なし: 基準日 (無ければ受信日 (JST)) の前後 7 日
        assert_eq!(
            candidate_window(&based, &[]),
            (d("2026-10-10"), d("2026-10-24"))
        );
        assert_eq!(
            candidate_window(&page(None), &[]),
            (d("2026-10-03"), d("2026-10-17"))
        );
        // 窓の中の保存は窓を変えない
        assert_eq!(
            candidate_window(&based, &[period("2026-10-12", "2026-10-13")]),
            (d("2026-10-10"), d("2026-10-24"))
        );
        // 窓の前・後の保存まで広げる (複数あれば最初〜最後)
        assert_eq!(
            candidate_window(&based, &[period("2026-10-06", "2026-10-06")]),
            (d("2026-10-06"), d("2026-10-24"))
        );
        assert_eq!(
            candidate_window(
                &based,
                &[
                    period("2026-11-02", "2026-11-03"),
                    period("2026-10-20", "2026-10-21")
                ]
            ),
            (d("2026-10-10"), d("2026-11-03"))
        );
        // ちょうど 1 年までは広げ、超えたら中心の前後だけ
        assert_eq!(
            candidate_window(&based, &[period("2025-10-24", "2025-10-24")]),
            (d("2025-10-24"), d("2026-10-24"))
        );
        assert_eq!(
            candidate_window(&based, &[period("2025-10-23", "2025-10-23")]),
            (d("2026-10-10"), d("2026-10-24"))
        );
        // 遠すぎる保存値も中心の前後だけ
        assert_eq!(
            candidate_window(
                &page(Some("2026-10-17")),
                &[period("+262142-12-31", "+262142-12-31")]
            ),
            (d("2026-10-10"), d("2026-10-24"))
        );
    }
}
