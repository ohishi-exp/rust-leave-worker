//! SQL を流す 1 か所。すべて `PgClient::tenant_tx` (transaction 単位の RLS) の中で、型付きの名前なしの文
//! (`query_typed` 系・`execute_typed`) だけを使う (Hyperdrive 経由では名前付き prepared statement で接続が切れる)。
//! RLS に加えて、どの文も `tenant_id = <ヘッダーの tenant>` を WHERE / VALUES に明示する (所有者で繋いだときの備え)。

use alc_worker_db::{PgClient, TenantTx, TxOutput};
use chrono::{DateTime, NaiveDate, Utc};
use tokio_postgres::types::Type;
use tokio_postgres::{Error, Row};
use uuid::Uuid;

use crate::grants::{self, AutoGrant, Grant, GrantWrite};
use crate::pages::{ListQuery, Page, PageDetail, PageUpdate, Period};
use crate::reconcile::{LinkedPeriod, Mismatch, PendingAlert};
use crate::settings::{Settings, ValidSettings};

pub mod sql {
    pub const SETTINGS_SELECT: &str = "SELECT mailbox, allowed_senders, dept_codes, lineworks_channel_id, grace_days, enabled, updated_at \
         FROM alc_api.leave_settings WHERE tenant_id = $1";

    /// lineworks_channels は FORCE RLS で、自 tenant の行しか見えない。tenant も明示する
    pub const CHANNEL_EXISTS: &str =
        "SELECT 1 FROM alc_api.lineworks_channels WHERE id = $1 AND tenant_id = $2";

    pub const SETTINGS_UPSERT: &str = "INSERT INTO alc_api.leave_settings \
         (tenant_id, mailbox, allowed_senders, dept_codes, lineworks_channel_id, grace_days, enabled, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, NOW()) \
         ON CONFLICT (tenant_id) DO UPDATE SET mailbox = EXCLUDED.mailbox, allowed_senders = EXCLUDED.allowed_senders, \
         dept_codes = EXCLUDED.dept_codes, lineworks_channel_id = EXCLUDED.lineworks_channel_id, \
         grace_days = EXCLUDED.grace_days, enabled = EXCLUDED.enabled, updated_at = NOW() \
         RETURNING mailbox, allowed_senders, dept_codes, lineworks_channel_id, grace_days, enabled, updated_at";

    const PAGE_COLUMNS: &str =
        "id, r2_key, page_no, mail_from, subject, received_at, employee_cd, status, \
         linked_by, linked_at, note, created_at";

    pub fn pages_list() -> String {
        format!(
            "SELECT {PAGE_COLUMNS} FROM alc_api.leave_pages \
             WHERE tenant_id = $1 AND ($2::text IS NULL OR status = $2) AND ($3::text IS NULL OR employee_cd = $3) \
             ORDER BY received_at DESC, page_no, id LIMIT $4 OFFSET $5"
        )
    }

    pub fn page_by_id() -> String {
        format!("SELECT {PAGE_COLUMNS} FROM alc_api.leave_pages WHERE tenant_id = $1 AND id = $2")
    }

    pub fn page_lock() -> String {
        format!("{} FOR UPDATE", page_by_id())
    }

    pub fn page_update() -> String {
        format!(
            "UPDATE alc_api.leave_pages SET employee_cd = $3, status = $4, note = $5, \
             linked_by = CASE WHEN $4 = 'linked' THEN $6 END, \
             linked_at = CASE WHEN $4 = 'linked' THEN NOW() END \
             WHERE tenant_id = $1 AND id = $2 RETURNING {PAGE_COLUMNS}"
        )
    }

    pub const PERIODS_BY_PAGE: &str =
        "SELECT kind, start_date, end_date, half FROM alc_api.leave_periods \
         WHERE tenant_id = $1 AND page_id = $2 ORDER BY start_date, end_date, kind, id";

    pub const PERIODS_DELETE: &str =
        "DELETE FROM alc_api.leave_periods WHERE tenant_id = $1 AND page_id = $2";

    pub const PERIOD_INSERT: &str = "INSERT INTO alc_api.leave_periods \
         (tenant_id, page_id, kind, start_date, end_date, half) VALUES ($1, $2, $3, $4, $5, $6)";

    /// mailbox から tenant と許可リストを引く。SECURITY DEFINER で tenant をまたぐ (無効な設定・未知の mailbox は 0 行)
    pub const MAILBOX_RESOLVE: &str =
        "SELECT tenant_id, allowed_senders FROM alc_api.leave_resolve_mailbox($1)";

    /// 1 通の PDF の全ページ (page_no 1..=$6) を 1 文で入れる
    pub const PAGES_INSERT: &str = "INSERT INTO alc_api.leave_pages \
         (tenant_id, r2_key, page_no, mail_from, subject, received_at, status) \
         SELECT $1, $2, g, $3, $4, $5, 'unlinked' FROM generate_series(1, $6::int) AS g";

    pub const PAGE_R2_KEY: &str =
        "SELECT r2_key FROM alc_api.leave_pages WHERE tenant_id = $1 AND id = $2";

    /// 日数は 0.1 日単位の整数で受け渡す (NUMERIC を Rust の型に通さない)
    const GRANT_COLUMNS: &str =
        "id, employee_cd, grant_date, (days * 10)::int4, expires_on, source, note, \
         created_at, updated_at";

    pub fn grants_by_employee() -> String {
        format!(
            "SELECT {GRANT_COLUMNS} FROM alc_api.leave_grants \
             WHERE tenant_id = $1 AND employee_cd = $2 ORDER BY grant_date, id"
        )
    }

    /// 同じ (乗務員, 付与日) が在れば何も書かない (行が返らない = 409)
    pub fn grant_insert() -> String {
        format!(
            "INSERT INTO alc_api.leave_grants (tenant_id, employee_cd, grant_date, days, expires_on, source, note) \
             VALUES ($1, $2, $3, $4 / 10.0, $5, 'manual', $6) \
             ON CONFLICT (tenant_id, employee_cd, grant_date) DO NOTHING RETURNING {GRANT_COLUMNS}"
        )
    }

    /// 手で直した付与は manual になる (auto の再計算で上書きされない)
    pub fn grant_update() -> String {
        format!(
            "UPDATE alc_api.leave_grants SET days = $3 / 10.0, expires_on = $4, note = $5, source = 'manual', \
             updated_at = NOW() WHERE tenant_id = $1 AND id = $2 RETURNING {GRANT_COLUMNS}"
        )
    }

    pub const GRANT_DELETE: &str =
        "DELETE FROM alc_api.leave_grants WHERE tenant_id = $1 AND id = $2";

    /// 法定付与をまとめて入れる。既に同じ付与日の行 (auto も manual も) が在れば何もしない
    pub const GRANTS_AUTO_INSERT: &str = "INSERT INTO alc_api.leave_grants \
         (tenant_id, employee_cd, grant_date, days, expires_on, source) \
         SELECT $1, u.e, u.g, u.t / 10.0, u.x, 'auto' \
         FROM unnest($2::text[], $3::date[], $4::int4[], $5::date[]) AS u(e, g, t, x) \
         ON CONFLICT (tenant_id, employee_cd, grant_date) DO NOTHING";

    /// 紐づけ済みのページの期間のうち `$3`〜`$4` に掛かるもの (`$2` が NULL なら全乗務員)
    pub const LINKED_PERIODS: &str = "SELECT p.employee_cd, p.id, pr.kind, pr.start_date, pr.end_date, pr.half \
         FROM alc_api.leave_periods pr \
         JOIN alc_api.leave_pages p ON p.id = pr.page_id AND p.tenant_id = pr.tenant_id \
         WHERE pr.tenant_id = $1 AND p.tenant_id = $1 AND p.status = 'linked' AND p.employee_cd IS NOT NULL \
         AND ($2::text IS NULL OR p.employee_cd = $2) AND pr.end_date >= $3 AND pr.start_date <= $4 \
         ORDER BY p.employee_cd, pr.start_date, pr.id";

    /// tenant をまたぐ一覧 (SECURITY DEFINER の関数。164_leave_enabled_tenants.sql)
    pub const ENABLED_TENANTS: &str =
        "SELECT tenant_id, dept_codes, lineworks_channel_id, grace_days \
         FROM alc_api.leave_enabled_tenants()";

    /// ずれを入れる。解決済みの行が再びずれたら未解決・未通知に戻す
    pub const ALERTS_UPSERT: &str = "INSERT INTO alc_api.leave_alerts (tenant_id, employee_cd, day, kind, direction) \
         SELECT $1, u.e, u.d, u.k, u.r FROM unnest($2::text[], $3::date[], $4::text[], $5::text[]) AS u(e, d, k, r) \
         ON CONFLICT (tenant_id, employee_cd, day, kind, direction) DO UPDATE \
         SET resolved_at = NULL, notified_at = NULL, first_seen_at = NOW() \
         WHERE leave_alerts.resolved_at IS NOT NULL";

    /// 窓 (`$2`〜`$3`) の中の未解決の行のうち、今回のずれ (`$4`〜`$7`) に無いものを解決にする
    pub const ALERTS_RESOLVE: &str = "UPDATE alc_api.leave_alerts a SET resolved_at = NOW() \
         WHERE a.tenant_id = $1 AND a.resolved_at IS NULL AND a.day BETWEEN $2 AND $3 \
         AND NOT EXISTS (SELECT 1 FROM unnest($4::text[], $5::date[], $6::text[], $7::text[]) AS u(e, d, k, r) \
         WHERE u.e = a.employee_cd AND u.d = a.day AND u.k = a.kind AND u.r = a.direction)";

    pub const ALERTS_PENDING: &str =
        "SELECT id, employee_cd, day, kind, direction FROM alc_api.leave_alerts \
         WHERE tenant_id = $1 AND notified_at IS NULL AND resolved_at IS NULL \
         ORDER BY employee_cd, day, kind, direction";

    pub const ALERTS_NOTIFIED: &str = "UPDATE alc_api.leave_alerts SET notified_at = NOW() \
         WHERE tenant_id = $1 AND id = ANY($2::uuid[]) AND notified_at IS NULL";
}

fn grant_from_row(row: &Row) -> Grant {
    Grant {
        id: row.get(0),
        employee_cd: row.get(1),
        grant_date: row.get(2),
        days: grants::days(row.get(3)),
        expires_on: row.get(4),
        source: row.get(5),
        note: row.get(6),
        created_at: row.get(7),
        updated_at: row.get(8),
    }
}

/// 乗務員の付与台帳 (付与日順)
pub async fn list_grants(
    pg: &mut PgClient,
    tenant_id: Uuid,
    employee_cd: String,
) -> Result<Vec<Grant>, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let rows = tx
                .query_typed(
                    &sql::grants_by_employee(),
                    &[(&tenant_id, Type::UUID), (&employee_cd, Type::TEXT)],
                )
                .await?;
            Ok(rows.iter().map(grant_from_row).collect())
        })
    })
    .await
}

/// manual の付与を 1 件足す。同じ付与日の行が在れば `None`
pub async fn insert_grant(
    pg: &mut PgClient,
    tenant_id: Uuid,
    g: GrantWrite,
) -> Result<Option<Grant>, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let row = tx
                .query_typed_opt(
                    &sql::grant_insert(),
                    &[
                        (&tenant_id, Type::UUID),
                        (&g.employee_cd, Type::TEXT),
                        (&g.grant_date, Type::DATE),
                        (&g.tenths, Type::INT4),
                        (&g.expires_on, Type::DATE),
                        (&g.note, Type::TEXT),
                    ],
                )
                .await?;
            Ok(row.as_ref().map(grant_from_row))
        })
    })
    .await
}

/// 付与を直す (source は manual になる)。自 tenant に見えなければ `None`
pub async fn update_grant(
    pg: &mut PgClient,
    tenant_id: Uuid,
    id: Uuid,
    g: GrantWrite,
) -> Result<Option<Grant>, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let row = tx
                .query_typed_opt(
                    &sql::grant_update(),
                    &[
                        (&tenant_id, Type::UUID),
                        (&id, Type::UUID),
                        (&g.tenths, Type::INT4),
                        (&g.expires_on, Type::DATE),
                        (&g.note, Type::TEXT),
                    ],
                )
                .await?;
            Ok(row.as_ref().map(grant_from_row))
        })
    })
    .await
}

/// 付与を消す。消した行が在れば `true`
pub async fn delete_grant(pg: &mut PgClient, tenant_id: Uuid, id: Uuid) -> Result<bool, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let n = tx
                .execute_typed(
                    sql::GRANT_DELETE,
                    &[(&tenant_id, Type::UUID), (&id, Type::UUID)],
                )
                .await?;
            Ok(n > 0)
        })
    })
    .await
}

/// 法定付与 (乗務員コード, 付与) をまとめて入れ、入った件数を返す (在る付与日は飛ばす)
pub async fn insert_auto_grants(
    pg: &mut PgClient,
    tenant_id: Uuid,
    grants: Vec<(String, AutoGrant)>,
) -> Result<u64, Error> {
    if grants.is_empty() {
        return Ok(0);
    }
    let codes: Vec<String> = grants.iter().map(|(c, _)| c.clone()).collect();
    let dates: Vec<NaiveDate> = grants.iter().map(|(_, g)| g.grant_date).collect();
    let tenths: Vec<i32> = grants.iter().map(|(_, g)| g.tenths).collect();
    let expires: Vec<NaiveDate> = grants.iter().map(|(_, g)| g.expires_on).collect();
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            tx.execute_typed(
                sql::GRANTS_AUTO_INSERT,
                &[
                    (&tenant_id, Type::UUID),
                    (&codes, Type::TEXT_ARRAY),
                    (&dates, Type::DATE_ARRAY),
                    (&tenths, Type::INT4_ARRAY),
                    (&expires, Type::DATE_ARRAY),
                ],
            )
            .await
        })
    })
    .await
}

/// 紐づけ済みの期間のうち `from`〜`to` に掛かるもの (`employee_cd` が `None` なら全乗務員)
pub async fn linked_periods(
    pg: &mut PgClient,
    tenant_id: Uuid,
    employee_cd: Option<String>,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<LinkedPeriod>, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let rows = tx
                .query_typed(
                    sql::LINKED_PERIODS,
                    &[
                        (&tenant_id, Type::UUID),
                        (&employee_cd, Type::TEXT),
                        (&from, Type::DATE),
                        (&to, Type::DATE),
                    ],
                )
                .await?;
            Ok(rows
                .iter()
                .map(|r| LinkedPeriod {
                    employee_cd: r.get(0),
                    page_id: r.get(1),
                    kind: r.get(2),
                    start_date: r.get(3),
                    end_date: r.get(4),
                    half: r.get(5),
                })
                .collect())
        })
    })
    .await
}

/// 勤怠申請を有効にしている tenant 1 つ
#[derive(Debug, Clone, PartialEq)]
pub struct EnabledTenant {
    pub tenant_id: Uuid,
    pub dept_codes: Vec<String>,
    pub lineworks_channel_id: Option<Uuid>,
    pub grace_days: i32,
}

impl TxOutput for EnabledTenant {}

/// 有効な tenant の一覧。tenant の確定前なので nil の tenant の transaction で、SECURITY DEFINER の関数だけを読む
/// (RLS の表は 1 行も見えない)
pub async fn enabled_tenants(pg: &mut PgClient) -> Result<Vec<EnabledTenant>, Error> {
    pg.tenant_tx(Uuid::nil(), move |tx| {
        Box::pin(async move {
            let rows = tx.query_typed(sql::ENABLED_TENANTS, &[]).await?;
            Ok(rows
                .iter()
                .map(|r| EnabledTenant {
                    tenant_id: r.get(0),
                    dept_codes: r.get(1),
                    lineworks_channel_id: r.get(2),
                    grace_days: r.get(3),
                })
                .collect())
        })
    })
    .await
}

/// 突き合わせの結果を書く (1 つの transaction で): ずれを入れる (再発は開き直す) → 窓の中で今回ずれていない行を解決 →
/// 未通知・未解決の行を返す。戻り値は (入れた / 開き直した件数, 解決した件数, 未通知)
pub async fn record_alerts(
    pg: &mut PgClient,
    tenant_id: Uuid,
    mismatches: Vec<Mismatch>,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<(u64, u64, Vec<PendingAlert>), Error> {
    let codes: Vec<String> = mismatches.iter().map(|m| m.employee_cd.clone()).collect();
    let days: Vec<NaiveDate> = mismatches.iter().map(|m| m.day).collect();
    let kinds: Vec<String> = mismatches.iter().map(|m| m.kind.clone()).collect();
    let dirs: Vec<String> = mismatches.iter().map(|m| m.direction.to_owned()).collect();
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let inserted = tx
                .execute_typed(
                    sql::ALERTS_UPSERT,
                    &[
                        (&tenant_id, Type::UUID),
                        (&codes, Type::TEXT_ARRAY),
                        (&days, Type::DATE_ARRAY),
                        (&kinds, Type::TEXT_ARRAY),
                        (&dirs, Type::TEXT_ARRAY),
                    ],
                )
                .await?;
            let resolved = tx
                .execute_typed(
                    sql::ALERTS_RESOLVE,
                    &[
                        (&tenant_id, Type::UUID),
                        (&from, Type::DATE),
                        (&to, Type::DATE),
                        (&codes, Type::TEXT_ARRAY),
                        (&days, Type::DATE_ARRAY),
                        (&kinds, Type::TEXT_ARRAY),
                        (&dirs, Type::TEXT_ARRAY),
                    ],
                )
                .await?;
            let rows = tx
                .query_typed(sql::ALERTS_PENDING, &[(&tenant_id, Type::UUID)])
                .await?;
            let pending = rows
                .iter()
                .map(|r| PendingAlert {
                    id: r.get(0),
                    employee_cd: r.get(1),
                    day: r.get(2),
                    kind: r.get(3),
                    direction: r.get(4),
                })
                .collect();
            Ok((inserted, resolved, pending))
        })
    })
    .await
}

/// 送れた警告に通知済みの印を付ける
pub async fn mark_notified(
    pg: &mut PgClient,
    tenant_id: Uuid,
    ids: Vec<Uuid>,
) -> Result<u64, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            tx.execute_typed(
                sql::ALERTS_NOTIFIED,
                &[(&tenant_id, Type::UUID), (&ids, Type::UUID_ARRAY)],
            )
            .await
        })
    })
    .await
}

fn settings_from_row(row: &Row) -> Settings {
    Settings {
        mailbox: row.get(0),
        allowed_senders: row.get(1),
        dept_codes: row.get(2),
        lineworks_channel_id: row.get(3),
        grace_days: row.get(4),
        enabled: row.get(5),
        updated_at: Some(row.get::<_, DateTime<Utc>>(6)),
    }
}

fn page_from_row(row: &Row) -> Page {
    Page {
        id: row.get(0),
        r2_key: row.get(1),
        page_no: row.get(2),
        mail_from: row.get(3),
        subject: row.get(4),
        received_at: row.get(5),
        employee_cd: row.get(6),
        status: row.get(7),
        linked_by: row.get(8),
        linked_at: row.get(9),
        note: row.get(10),
        created_at: row.get(11),
    }
}

fn period_from_row(row: &Row) -> Period {
    Period {
        kind: row.get(0),
        start_date: row.get(1),
        end_date: row.get(2),
        half: row.get(3),
    }
}

/// 自 tenant の設定。行が無ければ `None`
pub async fn get_settings(pg: &mut PgClient, tenant_id: Uuid) -> Result<Option<Settings>, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let row = tx
                .query_typed_opt(sql::SETTINGS_SELECT, &[(&tenant_id, Type::UUID)])
                .await?;
            Ok(row.as_ref().map(settings_from_row))
        })
    })
    .await
}

/// 設定の upsert。`lineworks_channel_id` が自 tenant に見えなければ何も書かずに `Ok(None)`
pub async fn put_settings(
    pg: &mut PgClient,
    tenant_id: Uuid,
    s: ValidSettings,
) -> Result<Option<Settings>, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            if let Some(channel) = s.lineworks_channel_id {
                let seen = tx
                    .query_typed_opt(
                        sql::CHANNEL_EXISTS,
                        &[(&channel, Type::UUID), (&tenant_id, Type::UUID)],
                    )
                    .await?;
                if seen.is_none() {
                    return Ok(None);
                }
            }
            let row = tx
                .query_typed_one(
                    sql::SETTINGS_UPSERT,
                    &[
                        (&tenant_id, Type::UUID),
                        (&s.mailbox, Type::TEXT),
                        (&s.allowed_senders, Type::TEXT_ARRAY),
                        (&s.dept_codes, Type::TEXT_ARRAY),
                        (&s.lineworks_channel_id, Type::UUID),
                        (&s.grace_days, Type::INT4),
                        (&s.enabled, Type::BOOL),
                    ],
                )
                .await?;
            Ok(Some(settings_from_row(&row)))
        })
    })
    .await
}

pub async fn list_pages(
    pg: &mut PgClient,
    tenant_id: Uuid,
    q: ListQuery,
) -> Result<Vec<Page>, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let rows = tx
                .query_typed(
                    &sql::pages_list(),
                    &[
                        (&tenant_id, Type::UUID),
                        (&q.status, Type::TEXT),
                        (&q.employee_cd, Type::TEXT),
                        (&q.limit, Type::INT8),
                        (&q.offset, Type::INT8),
                    ],
                )
                .await?;
            Ok(rows.iter().map(page_from_row).collect())
        })
    })
    .await
}

async fn periods_of(tx: &TenantTx<'_>, tenant_id: Uuid, id: Uuid) -> Result<Vec<Period>, Error> {
    let rows = tx
        .query_typed(
            sql::PERIODS_BY_PAGE,
            &[(&tenant_id, Type::UUID), (&id, Type::UUID)],
        )
        .await?;
    Ok(rows.iter().map(period_from_row).collect())
}

/// ページと期間。自 tenant に見えなければ `None`
pub async fn get_page(
    pg: &mut PgClient,
    tenant_id: Uuid,
    id: Uuid,
) -> Result<Option<PageDetail>, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let Some(row) = tx
                .query_typed_opt(
                    &sql::page_by_id(),
                    &[(&tenant_id, Type::UUID), (&id, Type::UUID)],
                )
                .await?
            else {
                return Ok(None);
            };
            let periods = periods_of(tx, tenant_id, id).await?;
            Ok(Some(PageDetail {
                page: page_from_row(&row),
                periods,
            }))
        })
    })
    .await
}

/// 1 つの transaction で: ページを行ロック → 期間を全削除 → 挿入 → ページを更新。
/// `linked_by` / `linked_at` は `status = linked` のときだけ `actor` / 今にし、それ以外は消す。
/// 自 tenant に見えなければ何も書かずに `None`
pub async fn update_page(
    pg: &mut PgClient,
    tenant_id: Uuid,
    id: Uuid,
    u: PageUpdate,
    actor: Option<String>,
) -> Result<Option<PageDetail>, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let ids = [
                (
                    &tenant_id as &(dyn tokio_postgres::types::ToSql + Sync),
                    Type::UUID,
                ),
                (&id, Type::UUID),
            ];
            if tx.query_typed_opt(&sql::page_lock(), &ids).await?.is_none() {
                return Ok(None);
            }
            tx.execute_typed(sql::PERIODS_DELETE, &ids).await?;
            for p in &u.periods {
                tx.execute_typed(
                    sql::PERIOD_INSERT,
                    &[
                        (&tenant_id, Type::UUID),
                        (&id, Type::UUID),
                        (&p.kind, Type::TEXT),
                        (&p.start_date, Type::DATE),
                        (&p.end_date, Type::DATE),
                        (&p.half, Type::TEXT),
                    ],
                )
                .await?;
            }
            let row = tx
                .query_typed_one(
                    &sql::page_update(),
                    &[
                        (&tenant_id, Type::UUID),
                        (&id, Type::UUID),
                        (&u.employee_cd, Type::TEXT),
                        (&u.status, Type::TEXT),
                        (&u.note, Type::TEXT),
                        (&actor, Type::TEXT),
                    ],
                )
                .await?;
            let periods = periods_of(tx, tenant_id, id).await?;
            Ok(Some(PageDetail {
                page: page_from_row(&row),
                periods,
            }))
        })
    })
    .await
}

/// ページの PDF の key。自 tenant に見えなければ `None` (他 tenant の key を推測で読ませない)
pub async fn page_r2_key(
    pg: &mut PgClient,
    tenant_id: Uuid,
    id: Uuid,
) -> Result<Option<String>, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let row = tx
                .query_typed_opt(
                    sql::PAGE_R2_KEY,
                    &[(&tenant_id, Type::UUID), (&id, Type::UUID)],
                )
                .await?;
            Ok(row.map(|r| r.get(0)))
        })
    })
    .await
}

/// mailbox が指す tenant と、許可された送信元 (小文字化は呼び手)。未知・無効なら `None`。
///
/// tenant が決まる前の照会。alc-vein-worker・alc-dtako-worker に前例が無く、`PgClient` にはテナントを設定しない
/// transaction の口が無いので、`tenant_tx(Uuid::nil(), ..)` の中でこの関数だけを呼ぶ。関数は SECURITY DEFINER
/// (`SET search_path = alc_api`) で、tenant をまたいで `leave_settings` を引くため、設定した tenant には依存しない
/// (nil UUID はどの tenant にも当たらない値)。
pub async fn resolve_mailbox(
    pg: &mut PgClient,
    mailbox: String,
) -> Result<Option<(Uuid, Vec<String>)>, Error> {
    pg.tenant_tx(Uuid::nil(), move |tx| {
        Box::pin(async move {
            let row = tx
                .query_typed_opt(sql::MAILBOX_RESOLVE, &[(&mailbox, Type::TEXT)])
                .await?;
            Ok(row.map(|r| (r.get::<_, Uuid>(0), r.get::<_, Vec<String>>(1))))
        })
    })
    .await
}

/// 取り込んだ PDF のページ (1..=`pages`) を未紐づけで入れる。1 文 = 全ページ (途中までは入らない)
pub struct NewPages {
    pub r2_key: String,
    pub mail_from: String,
    pub subject: Option<String>,
    pub received_at: DateTime<Utc>,
    pub pages: i32,
}

pub async fn insert_pages(pg: &mut PgClient, tenant_id: Uuid, p: NewPages) -> Result<u64, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            tx.execute_typed(
                sql::PAGES_INSERT,
                &[
                    (&tenant_id, Type::UUID),
                    (&p.r2_key, Type::TEXT),
                    (&p.mail_from, Type::TEXT),
                    (&p.subject, Type::TEXT),
                    (&p.received_at, Type::TIMESTAMPTZ),
                    (&p.pages, Type::INT4),
                ],
            )
            .await
        })
    })
    .await
}
