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
    add_days, admin, app_client, ichiban_with, lineworks_channel, notify_recipient, tenant,
    FakeIchiban, FakeNotifier,
};
use tokio::sync::{Mutex, MutexGuard};
use uuid::Uuid;

const TODAY: &str = "2026-10-10";
/// 窓 (今日 − 62 日 〜 今日 − 1 日)
const FROM: &str = "2026-08-09";
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
    let s = run(&mut app_client().await, f, n, today()).await;
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
    assert_eq!(sent, vec![(NotifyTarget::Channel(ch.unwrap()), "【勤怠申請】一番星と申請書のずれ 3 件\n1021 山田太郎: 10/03 有休 — 申請書なし\n1021 山田太郎: 10/06 労災 — 一番星に入力なし\n1022 佐藤花子: 09/20 欠勤 — 申請書なし".to_owned())]);
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

    // 2 回目: 何も増えず、送らない
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
        (3, 0, 0, 0, 0, 0, 0)
    );
    assert!(n2.sent.lock().unwrap().is_empty());
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
    // 紐づけを外したら再発: 未解決・未通知に戻り、もう一度送る
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
    assert_eq!(n.sent.lock().unwrap()[0].0, NotifyTarget::Channel(ch));
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
            NotifyTarget::Recipient(r),
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
        NotifyTarget::Recipient(_)
    ));
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
    // 次の回は残りだけを 1 通で送る
    let n2 = FakeNotifier::default();
    let s = run_once(&f, &n2).await;
    assert_eq!(
        (
            s.tenants[0].pending,
            s.tenants[0].sent_messages,
            s.tenants[0].notified as usize
        ),
        (120 - notified, 1, 120 - notified)
    );
    assert!(n2.sent.lock().unwrap()[0].1.starts_with(&format!(
        "【勤怠申請】一番星と申請書のずれ {} 件\n",
        120 - notified
    )));
    assert!(alerts(t).await.iter().all(|x| x.4));
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

#[tokio::test]
async fn broken_db_is_reported() {
    let _g = serial().await;
    // 切れた接続で回す (一覧の段で止まる)
    let mut pg = support::broken_client().await;
    let s = run(
        &mut pg,
        &FakeIchiban::default(),
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
