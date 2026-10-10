//! SQL を流す 1 か所。すべて `PgClient::tenant_tx` (transaction 単位の RLS) の中で、型付きの名前なしの文
//! (`query_typed` 系・`execute_typed`) だけを使う (Hyperdrive 経由では名前付き prepared statement で接続が切れる)。
//! RLS に加えて、どの文も `tenant_id = <ヘッダーの tenant>` を WHERE / VALUES に明示する (所有者で繋いだときの備え)。

use alc_worker_db::{PgClient, TenantTx};
use chrono::{DateTime, Utc};
use tokio_postgres::types::Type;
use tokio_postgres::{Error, Row};
use uuid::Uuid;

use crate::pages::{ListQuery, Page, PageDetail, PageUpdate, Period};
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
