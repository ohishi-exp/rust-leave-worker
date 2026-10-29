//! 毎朝の突き合わせ (`leave::cron::run`) を実 DB (RLS が効く `alc_api_app`)・偽の一番星・偽の LINE WORKS 送信で通すテスト。
//! 接続先は `LEAVE_TEST_ADMIN_DATABASE_URL` (未設定なら失敗する。tests/support/mod.rs)。
//!
//! `leave_enabled_tenants()` は DB の全 tenant を返すので、テストは直列に走らせ ([`serial`])、始めに他の tenant を無効にする。

mod support;

use chrono::NaiveDate;
use leave::cron::{run, NotifyTarget, Summary};
use leave::ichiban::EMPLOYEES_PATH;
use serde_json::{json, Value};
use support::{
    add_days, admin, app_client, channel_target, ichiban_with, lineworks_channel, notify_recipient,
    tenant, FakeHolidays, FakeIchiban, FakeNotifier,
};
use tokio::sync::{Mutex, MutexGuard};
use uuid::Uuid;

const TODAY: &str = "2026-10-10";
/// 窓 (ALERTS_FROM 〜 今日 − 1 日)
const FROM: &str = "2026-08-01";
const TO: &str = "2026-10-09";

static SERIAL: Mutex<()> = Mutex::const_new(());

/// 直列にし、DB に残る他の tenant の設定を無効にする
async fn serial() -> MutexGuard<'static, ()> {
    let guard = SERIAL.lock().await;
    support::prepare().await;
    admin()
        .await
        .batch_execute("UPDATE alc_api.leave_settings SET enabled = false")
        .await
        .unwrap();
    guard
}

fn today() -> NaiveDate {
    TODAY.parse().unwrap()
}

/// 有効な tenant を 1 つ作る (部門 10・grace 3 日。`channel` なら LINE WORKS のチャネルも)
async fn enabled_tenant(dept: &str, channel: bool) -> (Uuid, Option<Uuid>) {
    let t = tenant().await;
    let ch = if channel {
        Some(lineworks_channel(t).await)
    } else {
        None
    };
    admin()
        .await
        .execute(
            "INSERT INTO alc_api.leave_settings (tenant_id, dept_codes, lineworks_channel_id, grace_days, enabled) VALUES ($1, $2, $3, 3, true)",
            &[&t, &vec![dept.to_owned()], &ch],
        )
        .await
        .unwrap();
    (t, ch)
}

/// ページを 1 枚と期間を入れる (superuser で。紐づけの口の代わり)
async fn page_with(
    t: Uuid,
    emp: &str,
    status: &str,
    periods: &[(&str, &str, &str, Option<&str>)],
) -> Uuid {
    let su = admin().await;
    let p: Uuid = su
        .query_one(
            "INSERT INTO alc_api.leave_pages (tenant_id, r2_key, page_no, received_at, employee_cd, status) VALUES ($1, $2, 1, now(), $3, $4) RETURNING id",
            &[&t, &format!("{t}/{}.pdf", Uuid::new_v4()), &emp, &status],
        )
        .await
        .unwrap()
        .get(0);
    for (kind, from, to, half) in periods {
        let (from, to): (NaiveDate, NaiveDate) = (from.parse().unwrap(), to.parse().unwrap());
        su.execute(
            "INSERT INTO alc_api.leave_periods (tenant_id, page_id, kind, start_date, end_date, half) VALUES ($1, $2, $3, $4, $5, $6)",
            &[&t, &p, kind, &from, &to, half],
        )
        .await
        .unwrap();
    }
    p
}

fn employee(code: &str, name: &str, dept: &str, hire: Option<&str>) -> Value {
    json!({"employee_code": code, "employee_name": name, "dept_code": dept, "hire_date": hire, "retire_date": null})
}

fn day(code: &str, date: &str, item: &str) -> Value {
    json!({"employee_code": code, "date": date, "item_name": item})
}

fn ichiban(employees: Vec<Value>, days: &[Value]) -> FakeIchiban {
    let mut f = ichiban_with(&[(EMPLOYEES_PATH, 200, json!({"data": employees}))]);
    add_days(&mut f, FROM, TO, days);
    f
}

async fn run_once(f: &FakeIchiban, n: &FakeNotifier) -> Summary {
    let s = run(
        &mut app_client().await,
        f,
        &FakeHolidays::japan_2026(),
        n,
        today(),
    )
    .await;
    eprintln!("[leave-db] {}", s.line());
    s
}

/// tenant の警告 (乗務員, 日, 区分, 向き, 通知済み, 解決済み)
async fn alerts(t: Uuid) -> Vec<(String, String, String, String, bool, bool)> {
    admin()
        .await
        .query(
            "SELECT employee_cd, day::text, kind, direction, notified_at IS NOT NULL, resolved_at IS NOT NULL FROM alc_api.leave_alerts WHERE tenant_id = $1 ORDER BY employee_cd, day, kind, direction",
            &[&t],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5)))
        .collect()
}

fn a(
    emp: &str,
    day: &str,
    kind: &str,
    dir: &str,
    notified: bool,
    resolved: bool,
) -> (String, String, String, String, bool, bool) {
    (
        emp.into(),
        day.into(),
        kind.into(),
        dir.into(),
        notified,
        resolved,
    )
}

#[tokio::test]
async fn detects_notifies_and_grants() {
    let _g = serial().await;
    let (t, ch) = enabled_tenant("10", true).await;
    let f = ichiban(
        vec![
            employee("1021", "山田太郎", "10", Some("2024-04-01")),
            employee("1022", "佐藤花子", "10", None),
            employee("2001", "他部門", "20", Some("2020-04-01")),
        ],
        &[
            day("1021", "2026-10-03", "有休"),
            day("1021", "2026-10-05", "午前休"),
            // grace (3 日) の内側は待つ
            day("1021", "2026-10-09", "有休"),
            day("1021", "2026-10-04", "謎"),
            day("1022", "2026-09-20", "欠勤"),
            day("2001", "2026-10-03", "有休"),
        ],
    );
    page_with(
        t,
        "1021",
        "linked",
        &[
            ("yukyu", "2026-10-05", "2026-10-05", Some("pm")),
            ("rousai", "2026-10-06", "2026-10-06", None),
        ],
    )
    .await;
    page_with(
        t,
        "2001",
        "linked",
        &[("yukyu", "2026-10-01", "2026-10-01", None)],
    )
    .await;
    page_with(
        t,
        "1022",
        "ignored",
        &[("kekkin", "2026-09-20", "2026-09-20", None)],
    )
    .await;
    let n = FakeNotifier::default();
    let s = run_once(&f, &n).await;
    assert_eq!(s.tenants.len(), 1);
    let r = &s.tenants[0];
    assert_eq!(
        (
            r.mismatches,
            r.upserted,
            r.resolved,
            r.pending,
            r.sent_messages,
            r.notified,
            r.grants,
            r.failed.as_deref()
        ),
        (3, 3, 0, 3, 1, 3, 3, None)
    );
    assert_eq!(s.unknown.get("謎"), Some(&1));
    let sent = n.sent.lock().unwrap().clone();
    assert_eq!(sent, vec![(channel_target(t, ch.unwrap()).await, "【勤怠申請】一番星と申請書のずれ 3 件\n1021 山田太郎: 10/03 有休 — 申請書なし\n1021 山田太郎: 10/06 労災 — 一番星に入力なし\n1022 佐藤花子: 09/20 欠勤 — 申請書なし".to_owned())]);
    assert_eq!(
        alerts(t).await,
        vec![
            a("1021", "2026-10-03", "yukyu", "ichiban_only", true, false),
            a("1021", "2026-10-06", "rousai", "pdf_only", true, false),
            a("1022", "2026-09-20", "kekkin", "ichiban_only", true, false)
        ]
    );
    // 法定付与 (2024-10-01・2025-10-01・2026-10-01) が auto で入る
    let grants: Vec<(String, String)> = admin().await.query("SELECT grant_date::text, source FROM alc_api.leave_grants WHERE tenant_id = $1 ORDER BY grant_date", &[&t]).await.unwrap().iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(
        grants,
        vec![
            ("2024-10-01".into(), "auto".into()),
            ("2025-10-01".into(), "auto".into()),
            ("2026-10-01".into(), "auto".into())
        ]
    );
    // ログに tenant の識別子・氏名を出さない
    assert!(!s.line().contains(&t.to_string()) && !s.line().contains("山田"));

    // 2 回目: 何も増えないが、未解消は毎朝全部 (新規と継続を区別せず) 同じ文面で送り、最後に送った時刻を進める
    let before = notified_at(t).await;
    let n2 = FakeNotifier::default();
    let s = run_once(&f, &n2).await;
    let r = &s.tenants[0];
    assert_eq!(
        (
            r.mismatches,
            r.upserted,
            r.resolved,
            r.pending,
            r.sent_messages,
            r.notified,
            r.grants
        ),
        (3, 0, 0, 3, 1, 3, 0)
    );
    assert_eq!(*n2.sent.lock().unwrap(), sent);
    let after = notified_at(t).await;
    assert_eq!(after.len(), 3);
    assert!(
        before.iter().zip(&after).all(|(b, a)| a > b),
        "{before:?} {after:?}"
    );
}

/// tenant の警告の最後に送った時刻 ([`alerts`] と同じ並び)
async fn notified_at(t: Uuid) -> Vec<chrono::DateTime<chrono::Utc>> {
    admin()
        .await
        .query(
            "SELECT notified_at FROM alc_api.leave_alerts WHERE tenant_id = $1 ORDER BY employee_cd, day, kind, direction",
            &[&t],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

#[tokio::test]
async fn resolved_then_reopened_is_notified_again() {
    let _g = serial().await;
    let (t, _) = enabled_tenant("10", true).await;
    let f = ichiban(
        vec![employee("1021", "山田太郎", "10", None)],
        &[day("1021", "2026-10-03", "有休")],
    );
    let n = FakeNotifier::default();
    run_once(&f, &n).await;
    assert_eq!(
        alerts(t).await,
        vec![a(
            "1021",
            "2026-10-03",
            "yukyu",
            "ichiban_only",
            true,
            false
        )]
    );
    // 申請書を紐づけたら解決になり、送らない
    let p = page_with(
        t,
        "1021",
        "linked",
        &[("yukyu", "2026-10-03", "2026-10-03", None)],
    )
    .await;
    let s = run_once(&f, &n).await;
    assert_eq!((s.tenants[0].resolved, s.tenants[0].pending), (1, 0));
    assert_eq!(
        alerts(t).await,
        vec![a("1021", "2026-10-03", "yukyu", "ichiban_only", true, true)]
    );
    assert_eq!(n.sent.lock().unwrap().len(), 1);
    // 紐づけを外したら再発: 未解決に戻り、もう一度送る
    admin()
        .await
        .execute(
            "UPDATE alc_api.leave_pages SET status = 'ignored' WHERE id = $1",
            &[&p],
        )
        .await
        .unwrap();
    let s = run_once(&f, &n).await;
    assert_eq!(
        (
            s.tenants[0].upserted,
            s.tenants[0].sent_messages,
            s.tenants[0].notified
        ),
        (1, 1, 1)
    );
    assert_eq!(
        alerts(t).await,
        vec![a(
            "1021",
            "2026-10-03",
            "yukyu",
            "ichiban_only",
            true,
            false
        )]
    );
    assert_eq!(n.sent.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn no_channel_keeps_alerts_pending() {
    let _g = serial().await;
    let (t, _) = enabled_tenant("10", false).await;
    let f = ichiban(
        vec![employee("1021", "山田太郎", "10", None)],
        &[day("1021", "2026-10-03", "有休")],
    );
    let n = FakeNotifier::default();
    let s = run_once(&f, &n).await;
    assert_eq!((s.tenants[0].pending, s.tenants[0].sent_messages), (1, 0));
    assert!(n.sent.lock().unwrap().is_empty());
    assert_eq!(
        alerts(t).await,
        vec![a(
            "1021",
            "2026-10-03",
            "yukyu",
            "ichiban_only",
            false,
            false
        )]
    );
    // チャネルを設定したら次の回に送る
    let ch = lineworks_channel(t).await;
    admin()
        .await
        .execute(
            "UPDATE alc_api.leave_settings SET lineworks_channel_id = $2 WHERE tenant_id = $1",
            &[&t, &ch],
        )
        .await
        .unwrap();
    let s = run_once(&f, &n).await;
    assert_eq!((s.tenants[0].sent_messages, s.tenants[0].notified), (1, 1));
    let want = channel_target(t, ch).await;
    assert_eq!(n.sent.lock().unwrap()[0].0, want);
}

/// 有効な tenant の宛先を個人にする
async fn set_recipient(t: Uuid, r: Uuid) {
    admin()
        .await
        .execute(
            "UPDATE alc_api.leave_settings SET lineworks_channel_id = NULL, notify_recipient_id = $2 WHERE tenant_id = $1",
            &[&t, &r],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn recipient_target_is_sent() {
    let _g = serial().await;
    let (t, _) = enabled_tenant("10", false).await;
    let r = notify_recipient(t, "lineworks", true).await;
    set_recipient(t, r).await;
    let f = ichiban(
        vec![employee("1021", "山田太郎", "10", None)],
        &[day("1021", "2026-10-03", "有休")],
    );
    let n = FakeNotifier::default();
    let s = run_once(&f, &n).await;
    assert_eq!((s.tenants[0].sent_messages, s.tenants[0].notified), (1, 1));
    assert_eq!(
        *n.sent.lock().unwrap(),
        vec![(
            NotifyTarget::Recipient { id: r, bot: None },
            "【勤怠申請】一番星と申請書のずれ 1 件\n1021 山田太郎: 10/03 有休 — 申請書なし"
                .to_owned()
        )]
    );
    assert!(alerts(t).await.iter().all(|x| x.4));
}

#[tokio::test]
async fn unusable_recipient_keeps_alerts_pending() {
    let _g = serial().await;
    let (t, _) = enabled_tenant("10", false).await;
    let f = ichiban(
        vec![employee("1021", "山田太郎", "10", None)],
        &[day("1021", "2026-10-03", "有休")],
    );
    // 無効になった個人・LINE の行には送らない (警告は未通知のまま残る)
    for r in [
        notify_recipient(t, "lineworks", false).await,
        notify_recipient(t, "line", true).await,
    ] {
        set_recipient(t, r).await;
        let n = FakeNotifier::default();
        let s = run_once(&f, &n).await;
        assert_eq!((s.tenants[0].pending, s.tenants[0].sent_messages), (1, 0));
        assert!(n.sent.lock().unwrap().is_empty());
        assert_eq!(*n.calls.lock().unwrap(), 0);
    }
    assert!(alerts(t).await.iter().all(|x| !x.4));
    // 有効に戻せば次の回に送る
    admin()
        .await
        .execute(
            "UPDATE alc_api.notify_recipients SET enabled = true, provider = 'lineworks', lineworks_user_id = line_user_id, line_user_id = NULL WHERE tenant_id = $1 AND provider = 'line'",
            &[&t],
        )
        .await
        .unwrap();
    let n = FakeNotifier::default();
    let s = run_once(&f, &n).await;
    assert_eq!(s.tenants[0].sent_messages, 1);
    assert!(matches!(
        n.sent.lock().unwrap()[0].0,
        NotifyTarget::Recipient { .. }
    ));
}

/// 個人宛ての bot を設定する (個人は `set_recipient` で先に入れる)
async fn set_bot(t: Uuid, bot: Uuid) {
    admin()
        .await
        .execute(
            "UPDATE alc_api.leave_settings SET notify_bot_config_id = $2 WHERE tenant_id = $1",
            &[&t, &bot],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn recipient_with_bot_is_sent_through_that_bot() {
    let _g = serial().await;
    let (t, _) = enabled_tenant("10", false).await;
    let r = notify_recipient(t, "lineworks", true).await;
    let bot = support::bot_config(t, "lineworks", true).await;
    set_recipient(t, r).await;
    set_bot(t, bot).await;
    let f = ichiban(
        vec![employee("1021", "山田太郎", "10", None)],
        &[day("1021", "2026-10-03", "有休")],
    );
    let n = FakeNotifier::default();
    let s = run_once(&f, &n).await;
    assert_eq!((s.tenants[0].sent_messages, s.tenants[0].notified), (1, 1));
    assert_eq!(
        n.sent.lock().unwrap()[0].0,
        NotifyTarget::Recipient {
            id: r,
            bot: Some(bot)
        }
    );
}

#[tokio::test]
async fn unusable_bot_keeps_alerts_pending_without_falling_back() {
    let _g = serial().await;
    let (t, _) = enabled_tenant("10", false).await;
    let r = notify_recipient(t, "lineworks", true).await;
    set_recipient(t, r).await;
    let f = ichiban(
        vec![employee("1021", "山田太郎", "10", None)],
        &[day("1021", "2026-10-03", "有休")],
    );
    // 無効の bot・LINE の bot には送らない (bot 無しの Recipient に戻って別の bot から届くことも無い)
    for bot in [
        support::bot_config(t, "lineworks", false).await,
        support::bot_config(t, "line", true).await,
    ] {
        set_bot(t, bot).await;
        let n = FakeNotifier::default();
        let s = run_once(&f, &n).await;
        assert_eq!((s.tenants[0].pending, s.tenants[0].sent_messages), (1, 0));
        assert!(n.sent.lock().unwrap().is_empty());
        assert_eq!(*n.calls.lock().unwrap(), 0);
    }
    assert!(alerts(t).await.iter().all(|x| !x.4));
    // 使える bot にすれば次の回に送る
    let ok = support::bot_config(t, "lineworks", true).await;
    set_bot(t, ok).await;
    let n = FakeNotifier::default();
    let s = run_once(&f, &n).await;
    assert_eq!(s.tenants[0].sent_messages, 1);
    assert_eq!(
        n.sent.lock().unwrap()[0].0,
        NotifyTarget::Recipient {
            id: r,
            bot: Some(ok)
        }
    );
}

#[tokio::test]
async fn long_lists_are_split_and_failed_parts_resent() {
    let _g = serial().await;
    let (t, _) = enabled_tenant("10", true).await;
    let codes: Vec<String> = (0..120).map(|i| format!("{:04}", 5000 + i)).collect();
    let f = ichiban(
        codes
            .iter()
            .map(|c| employee(c, "氏名", "10", None))
            .collect(),
        &codes
            .iter()
            .map(|c| day(c, "2026-10-01", "特別休暇"))
            .collect::<Vec<_>>(),
    );
    // 3 通に分かれ、2 通目の送信が失敗する (1・3 通目の分だけが通知済みになる)
    let n = FakeNotifier {
        fail_calls: vec![1],
        ..Default::default()
    };
    let s = run_once(&f, &n).await;
    let r = &s.tenants[0];
    assert_eq!((r.pending, r.sent_messages), (120, 2));
    let first = n.sent.lock().unwrap()[0].1.clone();
    assert!(
        first.starts_with(
            "【勤怠申請】一番星と申請書のずれ 120 件 (1/3)\n5000 氏名: 10/01 特別休暇 — 申請書なし"
        ),
        "{first}"
    );
    let notified = r.notified as usize;
    let lines: usize = n
        .sent
        .lock()
        .unwrap()
        .iter()
        .map(|m| m.1.lines().count() - 1)
        .sum();
    assert_eq!(notified, lines);
    assert_eq!(alerts(t).await.iter().filter(|x| x.4).count(), notified);
    // 次の回は (前の回に送れたものも含めて) 未解消の 120 件を全部、同じ 3 通に分けて送り直す
    let n2 = FakeNotifier::default();
    let s = run_once(&f, &n2).await;
    assert_eq!(
        (
            s.tenants[0].pending,
            s.tenants[0].sent_messages,
            s.tenants[0].notified
        ),
        (120, 3, 120)
    );
    assert_eq!(n2.sent.lock().unwrap()[0].1, first);
    assert!(alerts(t).await.iter().all(|x| x.4));
}

#[tokio::test]
async fn unresolved_rows_outside_the_window_are_not_sent() {
    let _g = serial().await;
    let (t, _) = enabled_tenant("10", true).await;
    // 窓 (FROM〜TO) の外へ出た未解決の行: 解決にもならず表に残るが、送らない (窓の 1 日前と今日)
    for d in ["2026-07-31", TODAY] {
        let d: NaiveDate = d.parse().unwrap();
        admin()
            .await
            .execute(
                "INSERT INTO alc_api.leave_alerts (tenant_id, employee_cd, day, kind, direction) VALUES ($1, '1021', $2, 'yukyu', 'ichiban_only')",
                &[&t, &d],
            )
            .await
            .unwrap();
    }
    let f = ichiban(vec![employee("1021", "山田太郎", "10", None)], &[]);
    let n = FakeNotifier::default();
    let s = run_once(&f, &n).await;
    let r = &s.tenants[0];
    assert_eq!(
        (r.mismatches, r.resolved, r.pending, r.sent_messages),
        (0, 0, 0, 0)
    );
    assert!(n.sent.lock().unwrap().is_empty());
    assert_eq!(
        alerts(t).await,
        vec![
            a("1021", "2026-07-31", "yukyu", "ichiban_only", false, false),
            a("1021", TODAY, "yukyu", "ichiban_only", false, false)
        ]
    );
}

#[tokio::test]
async fn failures_do_not_stop_other_tenants() {
    let _g = serial().await;
    let n = FakeNotifier::default();
    // 有効な tenant が無ければ一番星も呼ばない
    let f = FakeIchiban::default();
    let s = run_once(&f, &n).await;
    assert_eq!((s.tenants.len(), s.failed.as_deref()), (0, None));
    assert!(f.calls.lock().unwrap().is_empty());

    // 部門 99 の社員の行だけ形が壊れている: 部門 99 の tenant だけが失敗し、部門 10 の tenant は進む
    let (bad, _) = enabled_tenant("99", true).await;
    let (good, _) = enabled_tenant("10", true).await;
    let mut employees = vec![employee("1021", "山田太郎", "10", None)];
    employees.push(json!({"employee_code": "9901", "dept_code": "99", "hire_date": "x"}));
    let f = ichiban(employees, &[day("1021", "2026-10-03", "有休")]);
    let s = run_once(&f, &n).await;
    let order: Vec<Uuid> = admin()
        .await
        .query("SELECT tenant_id FROM alc_api.leave_enabled_tenants()", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    let i_bad = order.iter().position(|x| *x == bad).unwrap();
    let i_good = order.iter().position(|x| *x == good).unwrap();
    assert_eq!(
        s.tenants[i_bad].failed.as_deref(),
        Some("ichiban_bad_response")
    );
    assert_eq!(
        (
            s.tenants[i_good].failed.as_deref(),
            s.tenants[i_good].sent_messages
        ),
        (None, 1)
    );
    assert!(s
        .line()
        .contains(&format!("#{} failed=ichiban_bad_response", i_bad + 1)));

    // 一番星の失敗は tenant に入る前に止まる (呼び出しの失敗・非 2xx・休暇の口の形)
    let s = run_once(&ichiban_with(&[]), &n).await;
    assert_eq!(
        (s.tenants.len(), s.failed.as_deref()),
        (0, Some("ichiban_unavailable"))
    );
    let mut f = ichiban(vec![], &[]);
    let path = leave::reconcile::days_paths(FROM.parse().unwrap(), TO.parse().unwrap()).remove(0);
    f.responses.insert(
        path.clone(),
        leave::ichiban::IchibanResponse {
            status: 500,
            body: vec![],
        },
    );
    let s = run_once(&f, &n).await;
    assert_eq!(s.failed.as_deref(), Some("ichiban_status:500"));
    f.responses.insert(
        path,
        leave::ichiban::IchibanResponse {
            status: 200,
            body: b"[]".to_vec(),
        },
    );
    let s = run_once(&f, &n).await;
    assert!(s
        .tenants
        .iter()
        .all(|t| t.failed.as_deref() == Some("ichiban_bad_response")));
}

// ---- 労災の紐づけの延長と窓の始め ----

async fn run_with_holidays(f: &FakeIchiban, h: &FakeHolidays) -> Summary {
    let s = run(
        &mut app_client().await,
        f,
        h,
        &FakeNotifier::default(),
        today(),
    )
    .await;
    eprintln!("[leave-db] {}", s.line());
    s
}

/// 労災のページのあと一番星の労災が続く日 (土曜は行が在る日、日曜は飛ばす) は漏れにならない。
/// 行の無い平日で止まり、その後の労災は漏れになる
#[tokio::test]
async fn rousai_continuing_after_the_page_is_not_a_leak() {
    let _g = serial().await;
    let (t, _) = enabled_tenant("10", false).await;
    page_with(
        t,
        "1021",
        "linked",
        &[("rousai", "2026-09-28", "2026-09-30", None)],
    )
    .await;
    let days: Vec<Value> = [
        "2026-09-28",
        "2026-09-29",
        "2026-09-30",
        "2026-10-01",
        "2026-10-02",
        "2026-10-03",
        "2026-10-05",
        "2026-10-07",
    ]
    .iter()
    .map(|d| day("1021", d, "労災"))
    .collect();
    let f = ichiban(vec![employee("1021", "山田太郎", "10", None)], &days);
    let h = FakeHolidays::japan_2026();
    let s = run_with_holidays(&f, &h).await;
    assert_eq!(s.tenants[0].mismatches, 1);
    assert!(!s.line().contains("holidays_unavailable"));
    // 10/4 (日) は飛ばし、10/6 (火) で止まる。10/7 は申請書なし
    assert_eq!(
        alerts(t).await,
        vec![a(
            "1021",
            "2026-10-07",
            "rousai",
            "ichiban_only",
            false,
            false
        )]
    );
    // 祝日は窓のうち延長を数える年だけ取る
    assert_eq!(*h.calls.lock().unwrap(), vec![2026]);
}

/// 祝日が取れなければ日曜だけ飛ばす (祝日で止まり、その後は漏れになる)。ログに固定語
#[tokio::test]
async fn holidays_unavailable_skips_only_sundays() {
    let _g = serial().await;
    let (t, _) = enabled_tenant("10", false).await;
    page_with(
        t,
        "1021",
        "linked",
        &[("rousai", "2026-09-14", "2026-09-18", None)],
    )
    .await;
    let days: Vec<Value> = [
        "2026-09-14",
        "2026-09-15",
        "2026-09-16",
        "2026-09-17",
        "2026-09-18",
        "2026-09-19",
        "2026-09-24",
    ]
    .iter()
    .map(|d| day("1021", d, "労災"))
    .collect();
    let f = ichiban(vec![employee("1021", "山田太郎", "10", None)], &days);
    // 取れれば 9/21〜23 (祝) を飛ばして 9/24 まで続く
    let s = run_with_holidays(&f, &FakeHolidays::japan_2026()).await;
    assert_eq!(s.tenants[0].mismatches, 0);
    // 取れなければ 9/21 (月) で止まり、9/24 は申請書なし
    let s = run_with_holidays(&f, &FakeHolidays::default()).await;
    assert_eq!(
        (s.tenants[0].mismatches, s.tenants[0].holidays_unavailable),
        (1, true)
    );
    assert!(
        s.line()
            .ends_with("grants=0 holidays_unavailable] unknown_items=0 []"),
        "{}",
        s.line()
    );
    assert_eq!(
        alerts(t).await,
        vec![a(
            "1021",
            "2026-09-24",
            "rousai",
            "ichiban_only",
            false,
            false
        )]
    );
}

/// 窓の始め (ALERTS_FROM = 2026-08-01) より前の日は数えない (一番星もそこから読む)。
/// 窓より前に終わった労災は窓の始めから延ばす
#[tokio::test]
async fn days_before_alerts_from_are_not_counted() {
    let _g = serial().await;
    let (t, _) = enabled_tenant("10", false).await;
    page_with(
        t,
        "1021",
        "linked",
        &[("yukyu", "2026-07-30", "2026-08-03", None)],
    )
    .await;
    page_with(
        t,
        "1022",
        "linked",
        &[("rousai", "2026-07-01", "2026-07-20", None)],
    )
    .await;
    let days = [
        day("1021", "2026-08-03", "有休"),
        day("1022", "2026-08-01", "労災"),
        day("1022", "2026-08-03", "労災"),
    ];
    let f = ichiban(
        vec![
            employee("1021", "山田太郎", "10", None),
            employee("1022", "佐藤花子", "10", None),
        ],
        &days,
    );
    run_once(&f, &FakeNotifier::default()).await;
    // 1021 の 7/30・7/31 は数えず、窓の中の 8/1 (土)・8/2 (日) だけが申請書だけ。1022 の 8/1・8/3 は延長で覆う
    assert_eq!(
        alerts(t).await,
        vec![
            a("1021", "2026-08-01", "yukyu", "pdf_only", false, false),
            a("1021", "2026-08-02", "yukyu", "pdf_only", false, false),
        ]
    );
    let calls = f.calls.lock().unwrap();
    assert_eq!(
        calls[1..],
        ["/api/leave/days?from=2026-08-01&to=2026-10-09".to_owned()]
    );
}

#[tokio::test]
async fn broken_db_is_reported() {
    let _g = serial().await;
    // 切れた接続で回す (一覧の段で止まる)
    let mut pg = support::broken_client().await;
    let s = run(
        &mut pg,
        &FakeIchiban::default(),
        &FakeHolidays::default(),
        &FakeNotifier::default(),
        today(),
    )
    .await;
    assert!(
        s.failed
            .as_deref()
            .is_some_and(|f| f.starts_with("enabled_tenants:")),
        "{:?}",
        s.failed
    );
    assert!(s.tenants.is_empty());
}

// ---- 紐づけた一番星の行の毎朝の比べ ----

/// 紐づけ済みのページ (`yukyu` の 1 日) と、そのとき保存した一番星の行 1 つ (superuser で直接入れる)
async fn linked_with_row(
    t: Uuid,
    emp: &str,
    day: &str,
    (row_id, item, last_updated): (&str, &str, Option<&str>),
) -> Uuid {
    let p = page_with(t, emp, "linked", &[("yukyu", day, day, None)]).await;
    let d: NaiveDate = day.parse().unwrap();
    let su = admin().await;
    su.execute(
        "UPDATE alc_api.leave_pages SET linked_by = 'admin@example.com', linked_at = now(), note = 'メモ' WHERE id = $1",
        &[&p],
    )
    .await
    .unwrap();
    su.execute(
        "INSERT INTO alc_api.leave_link_rows (tenant_id, page_id, row_id, employee_cd, day, item_name, last_updated) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
        &[&t, &p, &row_id, &emp, &d, &item, &last_updated],
    )
    .await
    .unwrap();
    p
}

/// ページの (状態, 乗務員, 備考, linked_by が在るか, linked_at が在るか, 期間の数, 保存した行の数)
async fn page_state(p: Uuid) -> (String, Option<String>, Option<String>, bool, bool, i64, i64) {
    let r = admin()
        .await
        .query_one(
            "SELECT status, employee_cd, note, linked_by IS NOT NULL, linked_at IS NOT NULL, \
             (SELECT count(*) FROM alc_api.leave_periods WHERE page_id = $1), \
             (SELECT count(*) FROM alc_api.leave_link_rows WHERE page_id = $1) \
             FROM alc_api.leave_pages WHERE id = $1",
            &[&p],
        )
        .await
        .unwrap();
    (
        r.get(0),
        r.get(1),
        r.get(2),
        r.get(3),
        r.get(4),
        r.get(5),
        r.get(6),
    )
}

fn still_linked(emp: &str) -> (String, Option<String>, Option<String>, bool, bool, i64, i64) {
    (
        "linked".into(),
        Some(emp.into()),
        Some("メモ".into()),
        true,
        true,
        1,
        1,
    )
}

fn unlinked(emp: &str) -> (String, Option<String>, Option<String>, bool, bool, i64, i64) {
    (
        "unlinked".into(),
        Some(emp.into()),
        Some("メモ".into()),
        false,
        false,
        0,
        0,
    )
}

#[tokio::test]
async fn changed_ichiban_rows_unlink_pages() {
    let _g = serial().await;
    let (t, ch) = enabled_tenant("10", true).await;
    use support::day_row;
    let f = ichiban(
        vec![
            employee("1021", "山田太郎", "10", None),
            employee("2001", "他部門", "20", None),
        ],
        &[
            day_row("1021", "2026-09-01", "有休", Some("a"), Some("t1")),
            day_row("1021", "2026-09-02", "有休", Some("b"), Some("t2")),
            day_row("1021", "2026-09-03", "午前休", Some("c"), None),
            day_row("1021", "2026-09-04", "有休", Some("d"), Some("t9")),
        ],
    );
    // 最終更新が同じ → 残す
    let pa = linked_with_row(t, "1021", "2026-09-01", ("a", "有休", Some("t1"))).await;
    // 最終更新が変わった → 外す
    let pb = linked_with_row(t, "1021", "2026-09-02", ("b", "有休", Some("t1"))).await;
    // 最終更新が NULL で品名が変わった → 外す
    let pc = linked_with_row(t, "1021", "2026-09-03", ("c", "有休", None)).await;
    // 最終更新が NULL で中身が同じ → 残す (一番星の側に最終更新が付いても)
    let pd = linked_with_row(t, "1021", "2026-09-04", ("d", "有休", None)).await;
    // 行が消えた → 外す
    let pe = linked_with_row(t, "1021", "2026-09-05", ("e", "有休", None)).await;
    // 窓の外の行は比べない
    let pf = linked_with_row(t, "1021", "2026-07-31", ("f", "有休", None)).await;
    // 部門から外れた乗務員の行は比べない
    let pg = linked_with_row(t, "2001", "2026-09-06", ("g", "有休", None)).await;

    let n = FakeNotifier::default();
    let s = run_once(&f, &n).await;
    let r = &s.tenants[0];
    assert_eq!((r.unlinked, r.failed.as_deref()), (3, None));
    assert!(s.line().contains("#1 unlinked=3 mismatches=2"));
    for p in [pa, pd, pf] {
        assert_eq!(page_state(p).await, still_linked("1021"), "{p}");
    }
    assert_eq!(page_state(pg).await, still_linked("2001"));
    for p in [pb, pc, pe] {
        assert_eq!(page_state(p).await, unlinked("1021"), "{p}");
    }
    // 外したページは突き合わせに入らない (一番星だけに在る 2 日がずれになる)
    assert_eq!(
        alerts(t).await,
        vec![
            a("1021", "2026-09-02", "yukyu", "ichiban_only", true, false),
            a("1021", "2026-09-03", "yukyu", "ichiban_only", true, false)
        ]
    );
    let want = channel_target(t, ch.unwrap()).await;
    let sent = n.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, want);
    assert!(sent[0].1.starts_with(
        "【勤怠申請】一番星と申請書のずれ 2 件\n一番星が変わったため紐づけを解除: 3 件\n1021 山田太郎: 09/02"
    ));
    // 行・品名の値は文面にもログにも出さない
    assert!(!sent[0].1.contains("t1") && !s.line().contains("t1"));
}

#[tokio::test]
async fn unlinking_alone_sends_one_message() {
    let _g = serial().await;
    let (t, _) = enabled_tenant("10", true).await;
    // 有休が「休み」に直された (区分の対象外になったので、ずれも出ない)
    let f = ichiban(
        vec![employee("1021", "山田太郎", "10", None)],
        &[support::day_row(
            "1021",
            "2026-09-02",
            "休み",
            Some("b"),
            Some("t1"),
        )],
    );
    let p = linked_with_row(t, "1021", "2026-09-02", ("b", "有休", Some("t1"))).await;
    let n = FakeNotifier::default();
    let s = run_once(&f, &n).await;
    let r = &s.tenants[0];
    assert_eq!(
        (r.unlinked, r.mismatches, r.sent_messages, r.notified),
        (1, 0, 1, 0)
    );
    assert_eq!(page_state(p).await, unlinked("1021"));
    assert!(alerts(t).await.is_empty());
    assert_eq!(
        n.sent.lock().unwrap()[0].1,
        "【勤怠申請】一番星が変わったため紐づけを解除: 1 件"
    );
}

#[tokio::test]
async fn failed_ichiban_runs_do_not_unlink() {
    let _g = serial().await;
    let (t, _) = enabled_tenant("10", true).await;
    let p = linked_with_row(t, "1021", "2026-09-02", ("b", "有休", Some("t1"))).await;
    let n = FakeNotifier::default();
    // 一番星の取得の失敗 (tenant に入る前に止まる)
    let s = run_once(&ichiban_with(&[]), &n).await;
    assert_eq!(s.failed.as_deref(), Some("ichiban_unavailable"));
    assert_eq!(page_state(p).await, still_linked("1021"));
    // tenant の社員の読みの失敗
    let bad = ichiban(
        vec![json!({"employee_code": "9901", "dept_code": "10", "hire_date": "x"})],
        &[],
    );
    let s = run_once(&bad, &n).await;
    assert_eq!(s.tenants[0].failed.as_deref(), Some("ichiban_bad_response"));
    assert_eq!(page_state(p).await, still_linked("1021"));
    assert!(n.sent.lock().unwrap().is_empty());
    // 取れた回は外す (同じ行が一番星から消えている)
    let ok = ichiban(vec![employee("1021", "山田太郎", "10", None)], &[]);
    let s = run_once(&ok, &n).await;
    assert_eq!(s.tenants[0].unlinked, 1);
    assert_eq!(page_state(p).await, unlinked("1021"));
}
