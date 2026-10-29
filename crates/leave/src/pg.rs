//! SQL を流す 1 か所。すべて `PgClient::tenant_tx` (transaction 単位の RLS) の中で、型付きの名前なしの文
//! (`query_typed` 系・`execute_typed`) だけを使う (Hyperdrive 経由では名前付き prepared statement で接続が切れる)。
//! RLS に加えて、どの文も `tenant_id = <ヘッダーの tenant>` を WHERE / VALUES に明示する (所有者で繋いだときの備え)。

use alc_worker_db::{PgClient, TenantTx, TxOutput};
use chrono::{DateTime, NaiveDate, Utc};
use tokio_postgres::types::Type;
use tokio_postgres::{Error, Row};
use uuid::Uuid;

use crate::cron::NotifyTarget;
use crate::grants::{self, AutoGrant, Grant, GrantWrite, Opening, OpeningPut};
use crate::pages::{Document, DocumentsQuery, ListQuery, Page, PageDetail, PageUpdate, Period};
use crate::reconcile::{self, LinkRow, LinkedPeriod, Mismatch, OpenAlert, PendingAlert};
use crate::settings::{Settings, ValidSettings};

pub mod sql {
    pub const SETTINGS_SELECT: &str =
        "SELECT dept_codes, lineworks_channel_id, notify_recipient_id, \
         notify_bot_config_id, grace_days, enabled, updated_at \
         FROM alc_api.leave_settings WHERE tenant_id = $1";

    /// lineworks_channels は FORCE RLS で、自 tenant の行しか見えない。tenant も明示する
    pub const CHANNEL_EXISTS: &str =
        "SELECT 1 FROM alc_api.lineworks_channels WHERE id = $1 AND tenant_id = $2";

    /// 個人の宛先は LINE WORKS の有効な行だけ (LINE の行・無効の行は送れない)。tenant も明示する
    pub const RECIPIENT_EXISTS: &str = "SELECT 1 FROM alc_api.notify_recipients \
         WHERE id = $1 AND tenant_id = $2 AND provider = 'lineworks' AND enabled";

    /// 個人宛てに使う bot は LINE WORKS の有効な行だけ。tenant も明示する
    pub const BOT_EXISTS: &str = "SELECT 1 FROM alc_api.bot_configs \
         WHERE id = $1 AND tenant_id = $2 AND provider = 'lineworks' AND enabled";

    pub const SETTINGS_UPSERT: &str = "INSERT INTO alc_api.leave_settings \
         (tenant_id, dept_codes, lineworks_channel_id, notify_recipient_id, notify_bot_config_id, grace_days, enabled, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, NOW()) \
         ON CONFLICT (tenant_id) DO UPDATE SET dept_codes = EXCLUDED.dept_codes, lineworks_channel_id = EXCLUDED.lineworks_channel_id, \
         notify_recipient_id = EXCLUDED.notify_recipient_id, notify_bot_config_id = EXCLUDED.notify_bot_config_id, \
         grace_days = EXCLUDED.grace_days, enabled = EXCLUDED.enabled, updated_at = NOW() \
         RETURNING dept_codes, lineworks_channel_id, notify_recipient_id, \
         notify_bot_config_id, grace_days, enabled, updated_at";

    const PAGE_COLUMNS: &str =
        "id, r2_key, page_no, mail_from, subject, received_at, employee_cd, status, \
         linked_by, linked_at, note, created_at, base_date";

    /// 一覧・詳細・PDF の key に共通の「見えるページ」の条件。`$n` が NULL なら全部 (本番)、在れば受信がそれ以降の
    /// ページだけ (staging の表示制限。`STAGING_VISIBLE_MINUTES`)
    fn visible(n: u8) -> String {
        format!("(${n}::timestamptz IS NULL OR received_at >= ${n})")
    }

    pub fn pages_list() -> String {
        format!(
            "SELECT {PAGE_COLUMNS} FROM alc_api.leave_pages \
             WHERE tenant_id = $1 AND ($2::text IS NULL OR status = $2) AND ($3::text IS NULL OR employee_cd = $3) \
             AND ($7::text IS NULL OR r2_key = $7) AND {} \
             ORDER BY (CASE WHEN $7::text IS NULL THEN received_at END) DESC, page_no, id LIMIT $4 OFFSET $5",
            visible(6)
        )
    }

    /// PDF (同じ `r2_key`) ごとの集約。`$3` = open (未紐づけが 1 ページでも在る) / done (無い) / all
    pub fn documents_list() -> String {
        format!(
            "SELECT r2_key, (array_agg(id ORDER BY page_no, id))[1], min(received_at), min(mail_from), min(subject), \
             count(*)::int8, \
             (count(*) FILTER (WHERE status = 'unlinked'))::int8, \
             (count(*) FILTER (WHERE status = 'linked'))::int8, \
             (count(*) FILTER (WHERE status = 'ignored'))::int8, \
             max(base_date) \
             FROM alc_api.leave_pages WHERE tenant_id = $1 AND {} GROUP BY r2_key \
             HAVING $3::text = 'all' \
             OR ($3::text = 'open' AND count(*) FILTER (WHERE status = 'unlinked') > 0) \
             OR ($3::text = 'done' AND count(*) FILTER (WHERE status = 'unlinked') = 0) \
             ORDER BY min(received_at) DESC, r2_key LIMIT $4 OFFSET $5",
            visible(2)
        )
    }

    pub fn page_by_id() -> String {
        format!(
            "SELECT {PAGE_COLUMNS} FROM alc_api.leave_pages WHERE tenant_id = $1 AND id = $2 AND {}",
            visible(3)
        )
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

    /// 1 通の PDF の全ページ (page_no 1..=$6) を 1 文で入れる。同じ key が既に在れば何もしない
    /// (同じ object を sweep で 2 回拾う。key は inbox の object key から決まる — `ingest::seeded_object_key`)
    pub const PAGES_INSERT: &str = "INSERT INTO alc_api.leave_pages \
         (tenant_id, r2_key, page_no, mail_from, subject, received_at, status) \
         SELECT $1, $2, g, $3, $4, $5, 'unlinked' FROM generate_series(1, $6::int) AS g \
         ON CONFLICT (r2_key, page_no) DO NOTHING";

    /// PDF の基準日を同じ `r2_key` の全ページに書く (`$2` のページの PDF)
    pub const BASE_DATE_UPDATE: &str = "UPDATE alc_api.leave_pages SET base_date = $3 \
         WHERE tenant_id = $1 AND r2_key = \
         (SELECT r2_key FROM alc_api.leave_pages WHERE tenant_id = $1 AND id = $2)";

    pub const LINK_ROWS_DELETE: &str =
        "DELETE FROM alc_api.leave_link_rows WHERE tenant_id = $1 AND page_id = $2";

    /// 紐づけた一番星の行をまとめて入れる (空の配列なら 0 行)
    pub const LINK_ROWS_INSERT: &str = "INSERT INTO alc_api.leave_link_rows \
         (tenant_id, page_id, row_id, employee_cd, day, item_name, last_updated) \
         SELECT $1, $2, u.r, u.e, u.d, u.i, u.l \
         FROM unnest($3::text[], $4::text[], $5::date[], $6::text[], $7::text[]) AS u(r, e, d, i, l)";

    /// 紐づけた一番星の行のうち、日が `$2`〜`$3` のもの (毎朝の比べ)
    pub const LINK_ROWS_IN_WINDOW: &str =
        "SELECT page_id, row_id, employee_cd, day, item_name, last_updated FROM alc_api.leave_link_rows \
         WHERE tenant_id = $1 AND day BETWEEN $2 AND $3 ORDER BY page_id, row_id";

    /// 紐づけを外す (乗務員C と備考は残す。紐づけ済みのページだけ)
    pub const PAGE_UNLINK: &str = "UPDATE alc_api.leave_pages \
         SET status = 'unlinked', linked_by = NULL, linked_at = NULL \
         WHERE tenant_id = $1 AND id = $2 AND status = 'linked'";

    pub fn page_r2_key() -> String {
        format!(
            "SELECT r2_key FROM alc_api.leave_pages WHERE tenant_id = $1 AND id = $2 AND {}",
            visible(3)
        )
    }

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

    /// tenant の全員分 (乗務員・付与日の順)
    pub fn grants_all() -> String {
        format!(
            "SELECT {GRANT_COLUMNS} FROM alc_api.leave_grants \
             WHERE tenant_id = $1 ORDER BY employee_cd, grant_date, id"
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

    /// 期首残 (日数は 0.1 日単位の整数で受け渡す)
    const OPENING_COLUMNS: &str =
        "employee_cd, as_of, (carryover * 10)::int4, (\"current\" * 10)::int4";

    pub fn openings() -> String {
        format!(
            "SELECT {OPENING_COLUMNS} FROM alc_api.leave_opening WHERE tenant_id = $1 ORDER BY employee_cd"
        )
    }

    pub fn opening_by_employee() -> String {
        format!(
            "SELECT {OPENING_COLUMNS} FROM alc_api.leave_opening WHERE tenant_id = $1 AND employee_cd = $2"
        )
    }

    /// 期首残をまとめて入れる (在れば置き換え)
    pub const OPENINGS_UPSERT: &str = "INSERT INTO alc_api.leave_opening \
         (tenant_id, employee_cd, as_of, carryover, \"current\") \
         SELECT $1, u.e, $2, u.c / 10.0, u.r / 10.0 FROM unnest($3::text[], $4::int4[], $5::int4[]) AS u(e, c, r) \
         ON CONFLICT (tenant_id, employee_cd) DO UPDATE SET as_of = EXCLUDED.as_of, \
         carryover = EXCLUDED.carryover, \"current\" = EXCLUDED.\"current\", updated_at = NOW()";

    pub const OPENINGS_DELETE: &str = "DELETE FROM alc_api.leave_opening \
         WHERE tenant_id = $1 AND employee_cd = ANY($2::text[])";

    /// 紐づけ済みのページの期間のうち `$3`〜`$4` に掛かるもの (`$2` が NULL なら全乗務員)。労災は `$3` より前に終わったものも
    pub const LINKED_PERIODS: &str = "SELECT p.employee_cd, p.id, pr.kind, pr.start_date, pr.end_date, pr.half \
         FROM alc_api.leave_periods pr \
         JOIN alc_api.leave_pages p ON p.id = pr.page_id AND p.tenant_id = pr.tenant_id \
         WHERE pr.tenant_id = $1 AND p.tenant_id = $1 AND p.status = 'linked' AND p.employee_cd IS NOT NULL \
         AND ($2::text IS NULL OR p.employee_cd = $2) AND (pr.end_date >= $3 OR pr.kind = 'rousai') AND pr.start_date <= $4 \
         ORDER BY p.employee_cd, pr.start_date, pr.id";

    /// tenant をまたぐ一覧 (SECURITY DEFINER の関数。164_leave_enabled_tenants.sql)
    pub const ENABLED_TENANTS: &str =
        "SELECT tenant_id, dept_codes, grace_days FROM alc_api.leave_enabled_tenants()";

    /// 警告の宛先 (グループ, 個人, 設定した bot, その bot が使えるか)。個人は LINE WORKS の有効な行のときだけ返す
    /// (無効になった宛先に毎朝送り続けない)。bot は設定されていて使える (自 tenant・lineworks・有効) ときだけ 4 列目が入る。
    /// グループは、この tenant のトークルームの行から LINE WORKS の channelId とそのトークルームの bot を 5・6 列目に引く
    /// (送信はこれを tenant 付きで渡す。tenant をまたぐ関数で引かせない)
    pub const NOTIFY_TARGET: &str = "SELECT s.lineworks_channel_id, r.id, s.notify_bot_config_id, b.id, \
         c.channel_id, c.bot_config_id \
         FROM alc_api.leave_settings s \
         LEFT JOIN alc_api.lineworks_channels c ON c.id = s.lineworks_channel_id AND c.tenant_id = $1 \
         LEFT JOIN alc_api.notify_recipients r ON r.id = s.notify_recipient_id AND r.tenant_id = $1 \
         AND r.provider = 'lineworks' AND r.enabled \
         LEFT JOIN alc_api.bot_configs b ON b.id = s.notify_bot_config_id AND b.tenant_id = $1 \
         AND b.provider = 'lineworks' AND b.enabled \
         WHERE s.tenant_id = $1";

    /// ずれを入れる。解決済みの行が再びずれたら未解決に戻す (`first_seen_at` は再発の時刻にする)
    pub const ALERTS_UPSERT: &str = "INSERT INTO alc_api.leave_alerts (tenant_id, employee_cd, day, kind, direction) \
         SELECT $1, u.e, u.d, u.k, u.r FROM unnest($2::text[], $3::date[], $4::text[], $5::text[]) AS u(e, d, k, r) \
         ON CONFLICT (tenant_id, employee_cd, day, kind, direction) DO UPDATE \
         SET resolved_at = NULL, first_seen_at = NOW() \
         WHERE leave_alerts.resolved_at IS NOT NULL";

    /// 窓 (`$2`〜`$3`) の中の未解決の行のうち、今回のずれ (`$4`〜`$7`) に無いものを解決にする
    pub const ALERTS_RESOLVE: &str = "UPDATE alc_api.leave_alerts a SET resolved_at = NOW() \
         WHERE a.tenant_id = $1 AND a.resolved_at IS NULL AND a.day BETWEEN $2 AND $3 \
         AND NOT EXISTS (SELECT 1 FROM unnest($4::text[], $5::date[], $6::text[], $7::text[]) AS u(e, d, k, r) \
         WHERE u.e = a.employee_cd AND u.d = a.day AND u.k = a.kind AND u.r = a.direction)";

    /// 紐づけた期間の鍵 (`$2`〜`$4`) に当たる未解決の `ichiban_only` を解決にする (保存した PUT と同じ transaction)。
    /// 窓は見ない (窓の外の行は一覧にも通知にも出ないので、閉じても差は無い)。`pdf_only` は閉じない
    pub const ALERTS_RESOLVE_LINKED: &str = "UPDATE alc_api.leave_alerts SET resolved_at = NOW() \
         WHERE tenant_id = $1 AND resolved_at IS NULL AND direction = 'ichiban_only' \
         AND (employee_cd, day, kind) IN (SELECT e, d, k FROM unnest($2::text[], $3::date[], $4::text[]) AS u(e, d, k))";

    /// 未解消の警告 = 未解決かつ窓 (`$2`〜`$3`) の中。毎朝の通知 ([`super::record_alerts`]) と一覧
    /// ([`super::open_alerts`]) が同じこの 1 文を使う。窓の外へ出た未解決の行は `resolved_at` が NULL のまま表に
    /// 残るが、通知にも一覧にも出ない (仕様)
    pub const ALERTS_OPEN: &str =
        "SELECT id, employee_cd, day, kind, direction, first_seen_at, notified_at FROM alc_api.leave_alerts \
         WHERE tenant_id = $1 AND resolved_at IS NULL AND day BETWEEN $2 AND $3 \
         ORDER BY employee_cd, day, kind, direction";

    /// 送れた警告に最後に送った時刻を付ける (毎朝送るので、送れるたびに上書きする)
    pub const ALERTS_NOTIFIED: &str = "UPDATE alc_api.leave_alerts SET notified_at = NOW() \
         WHERE tenant_id = $1 AND id = ANY($2::uuid[])";
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

/// tenant の全員分の付与 (乗務員・付与日の順)
pub async fn list_grants_all(pg: &mut PgClient, tenant_id: Uuid) -> Result<Vec<Grant>, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let rows = tx
                .query_typed(&sql::grants_all(), &[(&tenant_id, Type::UUID)])
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

fn opening_from_row(row: &Row) -> (String, Opening) {
    (
        row.get(0),
        Opening {
            as_of: row.get(1),
            carryover: row.get(2),
            current: row.get(3),
        },
    )
}

/// 自 tenant の期首残の全員分 (乗務員コード順)
pub async fn list_openings(
    pg: &mut PgClient,
    tenant_id: Uuid,
) -> Result<Vec<(String, Opening)>, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let rows = tx
                .query_typed(&sql::openings(), &[(&tenant_id, Type::UUID)])
                .await?;
            Ok(rows.iter().map(opening_from_row).collect())
        })
    })
    .await
}

/// 乗務員 1 人の期首残
pub async fn get_opening(
    pg: &mut PgClient,
    tenant_id: Uuid,
    employee_cd: String,
) -> Result<Option<Opening>, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let row = tx
                .query_typed_opt(
                    &sql::opening_by_employee(),
                    &[(&tenant_id, Type::UUID), (&employee_cd, Type::TEXT)],
                )
                .await?;
            Ok(row.as_ref().map(|r| opening_from_row(r).1))
        })
    })
    .await
}

/// 期首残の一括保存 (upsert と削除を 1 回の transaction で)。(保存した行数, 消えた行数)
pub async fn put_openings(
    pg: &mut PgClient,
    tenant_id: Uuid,
    put: OpeningPut,
) -> Result<(u64, u64), Error> {
    let codes: Vec<String> = put.upserts.iter().map(|(c, _, _)| c.clone()).collect();
    let carry: Vec<i32> = put.upserts.iter().map(|&(_, c, _)| c).collect();
    let current: Vec<i32> = put.upserts.iter().map(|&(_, _, r)| r).collect();
    let (as_of, deletes) = (put.as_of, put.deletes);
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let saved = tx
                .execute_typed(
                    sql::OPENINGS_UPSERT,
                    &[
                        (&tenant_id, Type::UUID),
                        (&as_of, Type::DATE),
                        (&codes, Type::TEXT_ARRAY),
                        (&carry, Type::INT4_ARRAY),
                        (&current, Type::INT4_ARRAY),
                    ],
                )
                .await?;
            let deleted = tx
                .execute_typed(
                    sql::OPENINGS_DELETE,
                    &[(&tenant_id, Type::UUID), (&deletes, Type::TEXT_ARRAY)],
                )
                .await?;
            Ok((saved, deleted))
        })
    })
    .await
}

/// 紐づけ済みの期間のうち `from`〜`to` に掛かるもの (`employee_cd` が `None` なら全乗務員)。労災の期間は `from` より前に
/// 終わったものも返す (`to` までに始まるもの。そのあと一番星の労災が続く日へ延ばす起点 — `reconcile::rousai_extension`)
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
                    grace_days: r.get(2),
                })
                .collect())
        })
    })
    .await
}

/// tenant の警告の宛先 (グループか個人)。無い・個人が送れない行・設定した bot が使えない (消えた・無効)・
/// 設定したグループがこの tenant に無いなら `None` (bot を名前順の最初に戻して別用途の bot から送ることはしない)
pub async fn notify_target(
    pg: &mut PgClient,
    tenant_id: Uuid,
) -> Result<Option<NotifyTarget>, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let row = tx
                .query_typed_opt(sql::NOTIFY_TARGET, &[(&tenant_id, Type::UUID)])
                .await?;
            Ok(row.and_then(|r| {
                if r.get::<_, Option<Uuid>>(0).is_some() {
                    let channel_id = r.get::<_, Option<String>>(4)?;
                    let bot = r.get::<_, Option<Uuid>>(5)?;
                    return Some(NotifyTarget::Channel {
                        tenant: tenant_id,
                        channel_id,
                        bot,
                    });
                }
                let id = r.get::<_, Option<Uuid>>(1)?;
                let configured = r.get::<_, Option<Uuid>>(2).is_some();
                let bot = r.get::<_, Option<Uuid>>(3);
                if configured && bot.is_none() {
                    return None;
                }
                Some(NotifyTarget::Recipient { id, bot })
            }))
        })
    })
    .await
}

/// 未解消の警告の行 ([`sql::ALERTS_OPEN`]。通知と一覧の共用)
async fn open_alert_rows(
    tx: &TenantTx<'_>,
    tenant_id: Uuid,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<Row>, Error> {
    tx.query_typed(
        sql::ALERTS_OPEN,
        &[
            (&tenant_id, Type::UUID),
            (&from, Type::DATE),
            (&to, Type::DATE),
        ],
    )
    .await
}

/// 未解消の警告の一覧 (`from`〜`to` は [`crate::cron::window`])
pub async fn open_alerts(
    pg: &mut PgClient,
    tenant_id: Uuid,
    (from, to): (NaiveDate, NaiveDate),
) -> Result<Vec<OpenAlert>, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let rows = open_alert_rows(tx, tenant_id, from, to).await?;
            Ok(rows
                .iter()
                .map(|r| OpenAlert {
                    employee_cd: r.get(1),
                    day: r.get(2),
                    kind: r.get(3),
                    direction: r.get(4),
                    first_seen_at: r.get(5),
                    notified_at: r.get(6),
                })
                .collect())
        })
    })
    .await
}

/// 突き合わせの結果を書く (1 つの transaction で): ずれを入れる (再発は開き直す) → 窓の中で今回ずれていない行を解決 →
/// 未解消 (未解決かつ窓の中) の行を返す — 毎朝、通知済みかを問わず全部送る。戻り値は (入れた / 開き直した件数, 解決した件数, 未解消)
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
            let rows = open_alert_rows(tx, tenant_id, from, to).await?;
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

/// 送れた警告に最後に送った時刻を付ける
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
        dept_codes: row.get(0),
        lineworks_channel_id: row.get(1),
        notify_recipient_id: row.get(2),
        notify_bot_config_id: row.get(3),
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
        base_date: row.get(12),
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

/// 設定の upsert の結果。宛先が自 tenant に見えない (個人は LINE WORKS の有効な行でない) なら何も書かない
#[derive(Debug, Clone, PartialEq)]
pub enum PutSettings {
    Saved(Settings),
    UnknownChannel,
    UnknownRecipient,
    UnknownBot,
}

impl TxOutput for PutSettings {}

/// 設定の upsert。宛先 (グループ / 個人) を確かめてから書く
pub async fn put_settings(
    pg: &mut PgClient,
    tenant_id: Uuid,
    s: ValidSettings,
) -> Result<PutSettings, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            for (id, check, unknown) in [
                (
                    s.lineworks_channel_id,
                    sql::CHANNEL_EXISTS,
                    PutSettings::UnknownChannel,
                ),
                (
                    s.notify_recipient_id,
                    sql::RECIPIENT_EXISTS,
                    PutSettings::UnknownRecipient,
                ),
                (
                    s.notify_bot_config_id,
                    sql::BOT_EXISTS,
                    PutSettings::UnknownBot,
                ),
            ] {
                let Some(id) = id else { continue };
                let seen = tx
                    .query_typed_opt(check, &[(&id, Type::UUID), (&tenant_id, Type::UUID)])
                    .await?;
                if seen.is_none() {
                    return Ok(unknown);
                }
            }
            let row = tx
                .query_typed_one(
                    sql::SETTINGS_UPSERT,
                    &[
                        (&tenant_id, Type::UUID),
                        (&s.dept_codes, Type::TEXT_ARRAY),
                        (&s.lineworks_channel_id, Type::UUID),
                        (&s.notify_recipient_id, Type::UUID),
                        (&s.notify_bot_config_id, Type::UUID),
                        (&s.grace_days, Type::INT4),
                        (&s.enabled, Type::BOOL),
                    ],
                )
                .await?;
            Ok(PutSettings::Saved(settings_from_row(&row)))
        })
    })
    .await
}

/// `visible_since` は staging の表示制限 (`None` = 制限なし。一覧・詳細・更新・PDF の key で同じ値を渡す)
pub async fn list_pages(
    pg: &mut PgClient,
    tenant_id: Uuid,
    q: ListQuery,
    visible_since: Option<DateTime<Utc>>,
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
                        (&visible_since, Type::TIMESTAMPTZ),
                        (&q.r2_key, Type::TEXT),
                    ],
                )
                .await?;
            Ok(rows.iter().map(page_from_row).collect())
        })
    })
    .await
}

fn document_from_row(row: &Row) -> Document {
    Document {
        r2_key: row.get(0),
        first_page_id: row.get(1),
        received_at: row.get(2),
        mail_from: row.get(3),
        subject: row.get(4),
        page_count: row.get(5),
        unlinked_count: row.get(6),
        linked_count: row.get(7),
        ignored_count: row.get(8),
        base_date: row.get(9),
    }
}

/// PDF 単位の一覧。`visible_since` は [`list_pages`] と同じ (見えないページは数えない)
pub async fn list_documents(
    pg: &mut PgClient,
    tenant_id: Uuid,
    q: DocumentsQuery,
    visible_since: Option<DateTime<Utc>>,
) -> Result<Vec<Document>, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let rows = tx
                .query_typed(
                    &sql::documents_list(),
                    &[
                        (&tenant_id, Type::UUID),
                        (&visible_since, Type::TIMESTAMPTZ),
                        (&q.state, Type::TEXT),
                        (&q.limit, Type::INT8),
                        (&q.offset, Type::INT8),
                    ],
                )
                .await?;
            Ok(rows.iter().map(document_from_row).collect())
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
    visible_since: Option<DateTime<Utc>>,
) -> Result<Option<PageDetail>, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let Some(row) = tx
                .query_typed_opt(
                    &sql::page_by_id(),
                    &[
                        (&tenant_id, Type::UUID),
                        (&id, Type::UUID),
                        (&visible_since, Type::TIMESTAMPTZ),
                    ],
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

/// 1 つの transaction で: ページを行ロック → 期間を全削除 → 挿入 → 一番星の行を全削除 → `link_rows` を挿入 → 保存した期間に当たる `ichiban_only` の警告を解決 (`reconcile::linked_alert_keys`) → ページを更新。
/// `linked_by` / `linked_at` は `status = linked` のときだけ `actor` / 今にし、それ以外は消す。
/// `link_rows` は確かめ済みの行 (`ledger::verified_link_rows`。linked 以外・行の指定が無ければ空 = 削除だけ)。
/// 自 tenant に見えなければ何も書かずに `None`
pub async fn update_page(
    pg: &mut PgClient,
    tenant_id: Uuid,
    id: Uuid,
    u: PageUpdate,
    link_rows: Vec<LinkRow>,
    actor: Option<String>,
    visible_since: Option<DateTime<Utc>>,
) -> Result<Option<PageDetail>, Error> {
    let row_ids: Vec<String> = link_rows.iter().map(|r| r.row_id.clone()).collect();
    let codes: Vec<String> = link_rows.iter().map(|r| r.employee_cd.clone()).collect();
    let days: Vec<NaiveDate> = link_rows.iter().map(|r| r.day).collect();
    let items: Vec<String> = link_rows.iter().map(|r| r.item_name.clone()).collect();
    let updated: Vec<Option<String>> = link_rows.into_iter().map(|r| r.last_updated).collect();
    let (alert_codes, alert_days, alert_kinds) = reconcile::linked_alert_keys(id, &u);
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let ids = [
                (
                    &tenant_id as &(dyn tokio_postgres::types::ToSql + Sync),
                    Type::UUID,
                ),
                (&id, Type::UUID),
            ];
            let lock = [
                ids[0].clone(),
                ids[1].clone(),
                (&visible_since, Type::TIMESTAMPTZ),
            ];
            if tx
                .query_typed_opt(&sql::page_lock(), &lock)
                .await?
                .is_none()
            {
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
            tx.execute_typed(sql::LINK_ROWS_DELETE, &ids).await?;
            tx.execute_typed(
                sql::LINK_ROWS_INSERT,
                &[
                    (&tenant_id, Type::UUID),
                    (&id, Type::UUID),
                    (&row_ids, Type::TEXT_ARRAY),
                    (&codes, Type::TEXT_ARRAY),
                    (&days, Type::DATE_ARRAY),
                    (&items, Type::TEXT_ARRAY),
                    (&updated, Type::TEXT_ARRAY),
                ],
            )
            .await?;
            if !alert_codes.is_empty() {
                tx.execute_typed(
                    sql::ALERTS_RESOLVE_LINKED,
                    &[
                        (&tenant_id, Type::UUID),
                        (&alert_codes, Type::TEXT_ARRAY),
                        (&alert_days, Type::DATE_ARRAY),
                        (&alert_kinds, Type::TEXT_ARRAY),
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

/// PDF の基準日を、ページ `id` と同じ `r2_key` の全ページに書く (1 つの transaction で、見える条件は [`get_page`] と同じ)。
/// 書いたページ数。自 tenant に見えなければ何も書かずに `None`
pub async fn set_base_date(
    pg: &mut PgClient,
    tenant_id: Uuid,
    id: Uuid,
    base_date: Option<NaiveDate>,
    visible_since: Option<DateTime<Utc>>,
) -> Result<Option<u64>, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            if tx
                .query_typed_opt(
                    &sql::page_by_id(),
                    &[
                        (&tenant_id, Type::UUID),
                        (&id, Type::UUID),
                        (&visible_since, Type::TIMESTAMPTZ),
                    ],
                )
                .await?
                .is_none()
            {
                return Ok(None);
            }
            let n = tx
                .execute_typed(
                    sql::BASE_DATE_UPDATE,
                    &[
                        (&tenant_id, Type::UUID),
                        (&id, Type::UUID),
                        (&base_date, Type::DATE),
                    ],
                )
                .await?;
            Ok(Some(n))
        })
    })
    .await
}

/// 紐づけた一番星の行のうち日が `from`〜`to` のもの (ページ, 行)
pub async fn link_rows(
    pg: &mut PgClient,
    tenant_id: Uuid,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<(Uuid, LinkRow)>, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let rows = tx
                .query_typed(
                    sql::LINK_ROWS_IN_WINDOW,
                    &[
                        (&tenant_id, Type::UUID),
                        (&from, Type::DATE),
                        (&to, Type::DATE),
                    ],
                )
                .await?;
            Ok(rows
                .iter()
                .map(|r| {
                    (
                        r.get(0),
                        LinkRow {
                            row_id: r.get(1),
                            employee_cd: r.get(2),
                            day: r.get(3),
                            item_name: r.get(4),
                            last_updated: r.get(5),
                        },
                    )
                })
                .collect())
        })
    })
    .await
}

/// ページ 1 枚の紐づけを外す (1 つの transaction で: 期間と一番星の行を消し、未紐づけに戻す。乗務員C と備考は残す)。
/// 外したら 1 (紐づけ済みでなければ 0)
pub async fn unlink_page(pg: &mut PgClient, tenant_id: Uuid, id: Uuid) -> Result<u64, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let ids = [
                (
                    &tenant_id as &(dyn tokio_postgres::types::ToSql + Sync),
                    Type::UUID,
                ),
                (&id, Type::UUID),
            ];
            tx.execute_typed(sql::PERIODS_DELETE, &ids).await?;
            tx.execute_typed(sql::LINK_ROWS_DELETE, &ids).await?;
            tx.execute_typed(sql::PAGE_UNLINK, &ids).await
        })
    })
    .await
}

/// ページの PDF の key。自 tenant に見えなければ `None` (他 tenant の key を推測で読ませない)
pub async fn page_r2_key(
    pg: &mut PgClient,
    tenant_id: Uuid,
    id: Uuid,
    visible_since: Option<DateTime<Utc>>,
) -> Result<Option<String>, Error> {
    pg.tenant_tx(tenant_id, move |tx| {
        Box::pin(async move {
            let row = tx
                .query_typed_opt(
                    &sql::page_r2_key(),
                    &[
                        (&tenant_id, Type::UUID),
                        (&id, Type::UUID),
                        (&visible_since, Type::TIMESTAMPTZ),
                    ],
                )
                .await?;
            Ok(row.map(|r| r.get(0)))
        })
    })
    .await
}

/// 取り込んだ PDF のページ (1..=`pages`) を未紐づけで入れる。1 文 = 全ページ (途中までは入らない)。
/// `mail_from` / `subject` は列として残す (取り込み経路 = スキャンの inbox は NULL)
pub struct NewPages {
    pub r2_key: String,
    pub mail_from: Option<String>,
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
