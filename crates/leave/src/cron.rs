//! 毎朝の突き合わせ (worker の `#[event(scheduled)]` から呼ぶ。本番だけ — staging には一番星の binding が無い)。
//!
//! 有効な tenant ([`pg::enabled_tenants`]) ごとに:
//! 1. 一番星の社員を tenant の `dept_codes` で絞る。休暇は「今日 − 62 日 〜 今日 − 1 日」を 1 回だけ取り、乗務員で絞って正規化する
//! 2. 紐づけ済みの期間を同じ窓で日ごとに展開する
//! 3. (乗務員, 日, 区分) の差を取る (`今日 − grace_days` より後の日は待つ)
//! 4. `leave_alerts` に書き、窓の中で今回ずれていない未解決の行を解決にする
//! 5. 未解消 (未解決かつ窓の中) の警告を、通知済みかを問わず毎朝全部、乗務員ごとにまとめて LINE WORKS (グループか個人)
//!    へ送り、送れた分に最後に送った時刻を付ける (宛先が無い・個人の宛先が無効なら送らない。翌朝また送る)
//! 6. 有給の法定付与を全乗務員ぶん足す (在る付与日は飛ばす)
//!
//! 1 tenant の失敗で他の tenant を止めない。結果は 1 行のログ (tenant は順番の番号で出し、識別子・氏名は出さない。
//! 表に無い品名N の語は出してよい)。

use std::collections::{BTreeMap, BTreeSet};

use alc_worker_db::PgClient;
use chrono::{Duration, NaiveDate};
use futures_util::future::BoxFuture;
use uuid::Uuid;

use crate::grants::statutory;
use crate::ichiban::{IchibanApi, EMPLOYEES_PATH};
use crate::pg;
use crate::reconcile::{
    days_paths, diff, expand_periods, messages, parse_days, parse_employees, IchibanDays,
};

/// 窓の長さ (今日 − 62 日 〜 今日 − 1 日)
pub const WINDOW_DAYS: i64 = 62;

/// 突き合わせの窓 (今日 − [`WINDOW_DAYS`] 日, 今日 − 1 日)。毎朝の通知と未解消の一覧 (`GET /api/leave/alerts`) が同じこれを使う
pub fn window(today: NaiveDate) -> (NaiveDate, NaiveDate) {
    (
        today - Duration::days(WINDOW_DAYS),
        today - Duration::days(1),
    )
}

/// auth-worker の内部 proxy 経由の LINE WORKS 送信の口 (host は名目。path が `/alc-internal-proxy/` で始まる必要がある。
/// nuxt-dtako-admin の dtako-scraper-relay の `lineworks-notify.ts` と同じ規約)
pub const LINEWORKS_SEND_URL: &str =
    "https://auth-worker.internal/alc-internal-proxy/api/internal/lineworks/send";

/// 警告の宛先 (`leave_settings` のグループか個人のどちらか 1 つ。rust-alc-api の `SendTarget` と同じ形)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NotifyTarget {
    /// `lineworks_channels.id` (LINE WORKS の channelId ではない)
    Channel(Uuid),
    /// `notify_recipients.id` (LINE WORKS の有効な行) と、その送信に使う bot (`bot_configs.id`)。
    /// bot が `None` なら送信側の既定 (設定が無い従来の形)。tenant に bot が複数あるとき、bot を指定しないと
    /// 別用途の bot から届くので、設定した bot が使えないときは宛先にせず送らない (`pg::notify_target`)
    Recipient { id: Uuid, bot: Option<Uuid> },
}

impl alc_worker_db::TxOutput for NotifyTarget {}

/// 送信の本文 (`{"channel_id": "<id>", "text": "..."}` か `{"recipient_id": "<id>", "text": "..."}` の排他。
/// 個人宛てで bot があれば `"bot_config_id"` も載せる)。宛先は `Uuid` の型で受けるので形は常に正しい
pub fn send_body(target: NotifyTarget, text: &str) -> String {
    match target {
        NotifyTarget::Channel(id) => serde_json::json!({ "channel_id": id, "text": text }),
        NotifyTarget::Recipient { id, bot: None } => {
            serde_json::json!({ "recipient_id": id, "text": text })
        }
        NotifyTarget::Recipient { id, bot: Some(bot) } => {
            serde_json::json!({ "recipient_id": id, "bot_config_id": bot, "text": text })
        }
    }
    .to_string()
}

/// 送信の失敗 (中身は持たない — 応答の本文やチャネルの識別子をログに出さない)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NotifyError(pub &'static str);

/// LINE WORKS のトークルームか個人へ 1 通送る (worker では auth-worker の Service Binding)
pub trait Notifier: Send + Sync {
    /// 送れる環境か (binding か secret が無い環境は `false`)。cron は見ない (送れなければ警告が残るだけ)。
    /// テスト送信の口が「送信に失敗した」(502) と「送れない環境」(503) を分けるのに使う
    fn available(&self) -> bool {
        true
    }

    fn send<'a>(
        &'a self,
        target: NotifyTarget,
        text: &'a str,
    ) -> BoxFuture<'a, Result<(), NotifyError>>;
}

/// tenant 1 つの結果
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TenantResult {
    pub mismatches: usize,
    pub upserted: u64,
    pub resolved: u64,
    pub pending: usize,
    pub sent_messages: usize,
    pub notified: u64,
    pub grants: u64,
    /// 失敗した段 (途中で止まった)
    pub failed: Option<String>,
}

/// cron 全体の結果
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub tenants: Vec<TenantResult>,
    pub unknown: BTreeMap<String, usize>,
    /// tenant に入る前に止まった段 (一覧・一番星)
    pub failed: Option<String>,
}

impl Summary {
    /// 1 行のログ
    pub fn line(&self) -> String {
        let tenants: Vec<String> = self
            .tenants
            .iter()
            .enumerate()
            .map(|(i, t)| match &t.failed {
                Some(stage) => format!("#{} failed={stage}", i + 1),
                None => format!(
                    "#{} mismatches={} upserted={} resolved={} pending={} sent={} notified={} grants={}",
                    i + 1,
                    t.mismatches,
                    t.upserted,
                    t.resolved,
                    t.pending,
                    t.sent_messages,
                    t.notified,
                    t.grants
                ),
            })
            .collect();
        let unknown: Vec<String> = self
            .unknown
            .iter()
            .map(|(w, n)| format!("{w:?}x{n}"))
            .collect();
        format!(
            "cron: tenants={} [{}] unknown_items={} [{}]{}",
            self.tenants.len(),
            tenants.join("; "),
            self.unknown.values().sum::<usize>(),
            unknown.join(", "),
            self.failed
                .as_ref()
                .map(|f| format!(" failed={f}"))
                .unwrap_or_default()
        )
    }
}

fn db_stage(stage: &str, e: &tokio_postgres::Error) -> String {
    format!("{stage}:{}", alc_worker_db::kind(e))
}

async fn get_ok(api: &dyn IchibanApi, path: &str) -> Result<Vec<u8>, String> {
    match api.get(path).await {
        Ok(r) if (200..300).contains(&r.status) => Ok(r.body),
        Ok(r) => Err(format!("ichiban_status:{}", r.status)),
        Err(_) => Err("ichiban_unavailable".into()),
    }
}

/// 一番星の社員と休暇の本文 (全 tenant で 1 回だけ取る)
async fn fetch_ichiban(
    api: &dyn IchibanApi,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<(Vec<u8>, Vec<Vec<u8>>), String> {
    let employees = get_ok(api, EMPLOYEES_PATH).await?;
    let mut days = Vec::new();
    for path in days_paths(from, to) {
        days.push(get_ok(api, &path).await?);
    }
    Ok((employees, days))
}

/// 1 回分を流す。`today` は日本時間の今日
pub async fn run(
    pg: &mut PgClient,
    ichiban: &dyn IchibanApi,
    notifier: &dyn Notifier,
    today: NaiveDate,
) -> Summary {
    let mut summary = Summary::default();
    let tenants = match pg::enabled_tenants(pg).await {
        Ok(t) => t,
        Err(e) => {
            summary.failed = Some(db_stage("enabled_tenants", &e));
            return summary;
        }
    };
    if tenants.is_empty() {
        return summary;
    }
    let (from, to) = window(today);
    let (employees_body, day_bodies) = match fetch_ichiban(ichiban, from, to).await {
        Ok(b) => b,
        Err(stage) => {
            summary.failed = Some(stage);
            return summary;
        }
    };
    for t in tenants {
        let mut result = TenantResult::default();
        if let Err(stage) = run_tenant(
            pg,
            notifier,
            &t,
            &employees_body,
            &day_bodies,
            (from, to, today),
            &mut result,
            &mut summary.unknown,
        )
        .await
        {
            result.failed = Some(stage);
        }
        summary.tenants.push(result);
    }
    summary
}

#[allow(clippy::too_many_arguments)]
async fn run_tenant(
    pg: &mut PgClient,
    notifier: &dyn Notifier,
    t: &pg::EnabledTenant,
    employees_body: &[u8],
    day_bodies: &[Vec<u8>],
    (from, to, today): (NaiveDate, NaiveDate, NaiveDate),
    result: &mut TenantResult,
    unknown: &mut BTreeMap<String, usize>,
) -> Result<(), String> {
    let employees = parse_employees(employees_body, &t.dept_codes).map_err(str::to_owned)?;
    let codes: BTreeSet<String> = employees.iter().map(|e| e.employee_code.clone()).collect();
    let mut days = IchibanDays::default();
    for body in day_bodies {
        parse_days(body, &codes, &mut days).map_err(str::to_owned)?;
    }
    for (w, n) in &days.unknown {
        *unknown.entry(w.clone()).or_default() += n;
    }
    let periods = pg::linked_periods(pg, t.tenant_id, None, from, to)
        .await
        .map_err(|e| db_stage("linked_periods", &e))?;
    // 自 tenant の部門から外れた乗務員の紐づけは比べない (一番星の側に行が無く、pdf_only が出続けるため)
    let pdf: Vec<_> = expand_periods(&periods, from, to)
        .into_iter()
        .filter(|p| codes.contains(&p.employee_cd))
        .collect();
    let cutoff = today - Duration::days(i64::from(t.grace_days));
    let mismatches = diff(&days.rows, &pdf, cutoff);
    result.mismatches = mismatches.len();
    let (upserted, resolved, pending) = pg::record_alerts(pg, t.tenant_id, mismatches, from, to)
        .await
        .map_err(|e| db_stage("record_alerts", &e))?;
    result.upserted = upserted;
    result.resolved = resolved;
    result.pending = pending.len();
    let target = pg::notify_target(pg, t.tenant_id)
        .await
        .map_err(|e| db_stage("notify_target", &e))?;
    if let Some(target) = target {
        let names: BTreeMap<String, String> = employees
            .iter()
            .filter_map(|e| Some((e.employee_code.clone(), e.employee_name.clone()?)))
            .collect();
        let mut sent = Vec::new();
        for m in messages(&pending, &names) {
            if notifier.send(target, &m.text).await.is_ok() {
                result.sent_messages += 1;
                sent.extend(m.alerts.iter().map(|&i| pending[i].id));
            }
        }
        if !sent.is_empty() {
            result.notified = pg::mark_notified(pg, t.tenant_id, sent)
                .await
                .map_err(|e| db_stage("mark_notified", &e))?;
        }
    }
    let grants: Vec<_> = employees
        .iter()
        .flat_map(|e| {
            e.hire_date
                .map(|h| statutory(h, e.retire_date, today))
                .unwrap_or_default()
                .into_iter()
                .map(|g| (e.employee_code.clone(), g))
        })
        .collect();
    result.grants = pg::insert_auto_grants(pg, t.tenant_id, grants)
        .await
        .map_err(|e| db_stage("auto_grants", &e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_line_has_no_identifiers() {
        let s = Summary {
            tenants: vec![
                TenantResult {
                    mismatches: 2,
                    upserted: 2,
                    pending: 2,
                    sent_messages: 1,
                    notified: 2,
                    grants: 3,
                    ..Default::default()
                },
                TenantResult {
                    failed: Some("record_alerts:08006".into()),
                    ..Default::default()
                },
            ],
            unknown: BTreeMap::from([("謎".to_owned(), 2)]),
            failed: None,
        };
        assert_eq!(s.line(), "cron: tenants=2 [#1 mismatches=2 upserted=2 resolved=0 pending=2 sent=1 notified=2 grants=3; #2 failed=record_alerts:08006] unknown_items=2 [\"謎\"x2]");
        let f = Summary {
            failed: Some("ichiban_unavailable".into()),
            ..Default::default()
        };
        assert_eq!(
            f.line(),
            "cron: tenants=0 [] unknown_items=0 [] failed=ichiban_unavailable"
        );
    }

    #[test]
    fn send_body_shape() {
        let body: serde_json::Value = serde_json::from_str(&send_body(
            NotifyTarget::Channel(Uuid::from_u128(1)),
            "あ\n\"い\"",
        ))
        .unwrap();
        assert_eq!(
            body,
            serde_json::json!({"channel_id": Uuid::from_u128(1).to_string(), "text": "あ\n\"い\""})
        );
    }

    #[test]
    fn send_body_recipient_shape() {
        let body: serde_json::Value = serde_json::from_str(&send_body(
            NotifyTarget::Recipient {
                id: Uuid::from_u128(2),
                bot: None,
            },
            "う",
        ))
        .unwrap();
        assert_eq!(
            body,
            serde_json::json!({"recipient_id": Uuid::from_u128(2).to_string(), "text": "う"})
        );
    }

    #[test]
    fn send_body_recipient_with_bot_shape() {
        let body: serde_json::Value = serde_json::from_str(&send_body(
            NotifyTarget::Recipient {
                id: Uuid::from_u128(2),
                bot: Some(Uuid::from_u128(3)),
            },
            "え",
        ))
        .unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "recipient_id": Uuid::from_u128(2).to_string(),
                "bot_config_id": Uuid::from_u128(3).to_string(),
                "text": "え"
            })
        );
    }
}
