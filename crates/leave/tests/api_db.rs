//! 口から実 DB (RLS が効く `alc_api_app`)・偽の PDF 置き場・偽の一番星までを通すテスト。
//! 接続先は `LEAVE_TEST_ADMIN_DATABASE_URL` (未設定なら失敗する。tests/support/mod.rs)。

mod support;

use std::collections::HashMap;

use std::sync::Arc;

use leave::cron::NotifyTarget;
use leave::ichiban::{DEPARTMENTS_PATH, EMPLOYEES_PATH};
use leave::TEST_NOTIFY_TEXT;
use serde_json::{json, Value};
use support::{
    app, app_with_notifier, call, ichiban_with, lineworks_channel, notify_recipient, page, tenant,
    App, FakeIchiban, FakeNotifier, FakePdfs, UnavailableNotifier,
};
use uuid::Uuid;

const ADMIN: Option<&str> = Some("admin");

fn employees_body() -> Value {
    json!({
        "source_table": "社員ﾏｽﾀ",
        "data": [
            {"employee_code": "1021", "employee_name": "甲", "dept_code": "10", "hire_date": null, "retire_date": null},
            {"employee_code": "2001", "employee_name": "乙", "dept_code": "20", "hire_date": null, "retire_date": null}
        ]
    })
}

async fn put_settings(app: &App, t: Uuid, body: Value) -> support::Got {
    call(
        app,
        "PUT",
        "/api/leave/settings",
        Some(t),
        ADMIN,
        Some(body),
    )
    .await
}

/// 7 本の口 (method 違いを含めて 8 組) すべてについて、管理者でなければ 403 で、DB にも置き場にも一番星にも届かない
mod forbidden {
    use super::*;

    async fn assert_forbidden(method: &str, path: &str, body: Option<Value>) {
        let t = tenant().await;
        let p = page(t, &format!("{t}/a.pdf"), 1, "2026-10-01T00:00:00Z").await;
        let path = path.replace("{id}", &p.to_string());
        let a = app(
            FakePdfs::default(),
            Some(ichiban_with(&[(DEPARTMENTS_PATH, 200, json!([]))])),
        )
        .await;
        for role in [
            None,
            Some("viewer"),
            Some("payroll"),
            Some("Admin"),
            Some(""),
        ] {
            let got = call(&a, method, &path, Some(t), role, body.clone()).await;
            assert_eq!(got.status, 403, "{method} {path} role={role:?}");
            assert_eq!(got.json(), json!({"error": "forbidden"}));
        }
        assert!(a.pdfs.reads.lock().unwrap().is_empty());
        assert!(a.ichiban.unwrap().calls.lock().unwrap().is_empty());
        // ページも設定も変わっていない
        let detail = call(
            &app(FakePdfs::default(), None).await,
            "GET",
            &format!("/api/leave/pages/{p}"),
            Some(t),
            ADMIN,
            None,
        )
        .await;
        assert_eq!(detail.json()["status"], "unlinked");
        let s = call(
            &app(FakePdfs::default(), None).await,
            "GET",
            "/api/leave/settings",
            Some(t),
            ADMIN,
            None,
        )
        .await;
        assert_eq!(s.json()["updated_at"], Value::Null);
    }

    #[tokio::test]
    async fn get_settings() {
        assert_forbidden("GET", "/api/leave/settings", None).await;
    }

    #[tokio::test]
    async fn put_settings() {
        assert_forbidden(
            "PUT",
            "/api/leave/settings",
            Some(json!({"mailbox": "abc"})),
        )
        .await;
    }

    /// 管理者でなければ 403 で、送信の口にも DB にも届かない
    #[tokio::test]
    async fn test_notify() {
        let t = tenant().await;
        let r = notify_recipient(t, "lineworks", true).await;
        let n = Arc::new(FakeNotifier::default());
        let a = app_with_notifier(n.clone()).await;
        let got = super::put_settings(&a, t, json!({"notify_recipient_id": r})).await;
        assert_eq!(got.status, 200);
        for role in [
            None,
            Some("viewer"),
            Some("payroll"),
            Some("Admin"),
            Some(""),
        ] {
            let got = call(
                &a,
                "POST",
                "/api/leave/settings/test-notify",
                Some(t),
                role,
                None,
            )
            .await;
            assert_eq!(got.status, 403, "role={role:?}");
            assert_eq!(got.json(), json!({"error": "forbidden"}));
        }
        assert_eq!(*n.calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn departments() {
        assert_forbidden("GET", "/api/leave/departments", None).await;
    }

    #[tokio::test]
    async fn employees() {
        assert_forbidden("GET", "/api/leave/employees", None).await;
    }

    #[tokio::test]
    async fn list_pages() {
        assert_forbidden("GET", "/api/leave/pages", None).await;
    }

    #[tokio::test]
    async fn get_page() {
        assert_forbidden("GET", "/api/leave/pages/{id}", None).await;
    }

    #[tokio::test]
    async fn put_page() {
        assert_forbidden(
            "PUT",
            "/api/leave/pages/{id}",
            Some(json!({"status": "ignored"})),
        )
        .await;
    }

    #[tokio::test]
    async fn get_pdf() {
        assert_forbidden("GET", "/api/leave/pages/{id}/pdf", None).await;
    }

    #[tokio::test]
    async fn calendar() {
        assert_forbidden(
            "GET",
            "/api/leave/calendar?employee_cd=1021&month=2026-10",
            None,
        )
        .await;
    }

    #[tokio::test]
    async fn list_grants() {
        assert_forbidden("GET", "/api/leave/grants?employee_cd=1021", None).await;
    }

    #[tokio::test]
    async fn create_grant() {
        let body = json!({"employee_cd": "1021", "grant_date": "2026-04-01", "days": 1});
        assert_forbidden("POST", "/api/leave/grants", Some(body)).await;
    }

    #[tokio::test]
    async fn auto_grants() {
        assert_forbidden("POST", "/api/leave/grants/auto?employee_cd=1021", None).await;
    }

    #[tokio::test]
    async fn update_grant() {
        let body = json!({"days": 1, "expires_on": "2028-01-01"});
        assert_forbidden("PUT", "/api/leave/grants/{id}", Some(body)).await;
    }

    #[tokio::test]
    async fn delete_grant() {
        assert_forbidden("DELETE", "/api/leave/grants/{id}", None).await;
    }

    #[tokio::test]
    async fn list_openings() {
        assert_forbidden("GET", "/api/leave/opening", None).await;
    }

    #[tokio::test]
    async fn put_openings() {
        let body = json!({"as_of": "2026-09-30", "rows": [{"employee_cd": "1021", "carryover": 1, "current": 2}]});
        assert_forbidden("PUT", "/api/leave/opening", Some(body)).await;
    }
}

#[tokio::test]
async fn missing_tenant_header_is_401() {
    let a = app(FakePdfs::default(), None).await;
    for path in ["/api/leave/settings", "/api/leave/pages"] {
        let got = call(&a, "GET", path, None, ADMIN, None).await;
        assert_eq!(got.status, 401, "{path}");
    }
    // 口の無い path は 404 (中身を返さない)
    let got = call(&a, "GET", "/api/leave/nope", None, ADMIN, None).await;
    assert_eq!(got.status, 404);
}

#[tokio::test]
async fn settings_default_then_upsert() {
    let t = tenant().await;
    let a = app(FakePdfs::default(), None).await;
    let got = call(&a, "GET", "/api/leave/settings", Some(t), ADMIN, None).await;
    assert_eq!(got.status, 200);
    assert_eq!(
        got.json(),
        json!({"mailbox": null, "allowed_senders": [], "dept_codes": [], "lineworks_channel_id": null,
               "notify_recipient_id": null, "notify_bot_config_id": null, "grace_days": 7, "enabled": false, "updated_at": null})
    );

    let mailbox = format!("Leave-{}", &t.simple().to_string()[..12]);
    let got = put_settings(
        &a,
        t,
        json!({"mailbox": mailbox, "allowed_senders": ["Boss@Example.com"], "dept_codes": ["10", "30"],
               "grace_days": 3, "enabled": true}),
    )
    .await;
    assert_eq!(got.status, 200);
    let v = got.json();
    assert_eq!(v["mailbox"], mailbox.to_lowercase());
    assert_eq!(v["allowed_senders"], json!(["boss@example.com"]));
    assert_eq!(v["dept_codes"], json!(["10", "30"]));
    assert_eq!(v["grace_days"], 3);
    assert_eq!(v["enabled"], true);
    assert!(v["updated_at"].is_string());

    let got = call(&a, "GET", "/api/leave/settings", Some(t), ADMIN, None).await;
    assert_eq!(got.json(), v);

    // PUT は全体の置き換え (省いた項目は既定値に戻る)
    let got = put_settings(&a, t, json!({})).await;
    assert_eq!(got.json()["mailbox"], Value::Null);
    assert_eq!(got.json()["dept_codes"], json!([]));
    assert_eq!(got.json()["grace_days"], 7);
}

#[tokio::test]
async fn settings_rejects_bad_input() {
    let t = tenant().await;
    let a = app(FakePdfs::default(), None).await;
    for (body, code) in [
        (json!({"mailbox": "ab"}), "invalid_mailbox"),
        (json!({"mailbox": "a.b.c"}), "invalid_mailbox"),
        (json!({"grace_days": 61}), "invalid_grace_days"),
        (json!({"tenant_id": t}), "invalid_body"),
        (json!({"lineworks_channel_id": "x"}), "invalid_body"),
    ] {
        let got = put_settings(&a, t, body.clone()).await;
        assert_eq!(got.status, 400, "{body}");
        assert_eq!(got.json(), json!({"error": code}), "{body}");
    }
}

#[tokio::test]
async fn mailbox_taken_by_another_tenant_is_409() {
    let (t1, t2) = (tenant().await, tenant().await);
    let a = app(FakePdfs::default(), None).await;
    let mailbox = format!("dup-{}", &t1.simple().to_string()[..12]);
    assert_eq!(
        put_settings(&a, t1, json!({"mailbox": mailbox}))
            .await
            .status,
        200
    );
    let got = put_settings(&a, t2, json!({"mailbox": mailbox.to_uppercase()})).await;
    assert_eq!(got.status, 409);
    assert_eq!(got.json(), json!({"error": "mailbox_taken"}));
}

#[tokio::test]
async fn lineworks_channel_must_belong_to_tenant() {
    let (t1, t2) = (tenant().await, tenant().await);
    let own = lineworks_channel(t1).await;
    let other = lineworks_channel(t2).await;
    let a = app(FakePdfs::default(), None).await;

    let got = put_settings(&a, t1, json!({"lineworks_channel_id": own})).await;
    assert_eq!(got.status, 200);
    assert_eq!(got.json()["lineworks_channel_id"], own.to_string());

    for ch in [other, Uuid::new_v4()] {
        let got = put_settings(&a, t1, json!({"lineworks_channel_id": ch, "grace_days": 1})).await;
        assert_eq!(got.status, 400);
        assert_eq!(got.json(), json!({"error": "unknown_lineworks_channel"}));
    }
    // 拒んだ PUT は何も書いていない
    let got = call(&a, "GET", "/api/leave/settings", Some(t1), ADMIN, None).await;
    assert_eq!(got.json()["lineworks_channel_id"], own.to_string());
    assert_eq!(got.json()["grace_days"], 7);
}

#[tokio::test]
async fn notify_recipient_must_be_own_enabled_lineworks() {
    let (t1, t2) = (tenant().await, tenant().await);
    let own = notify_recipient(t1, "lineworks", true).await;
    let other = notify_recipient(t2, "lineworks", true).await;
    let line = notify_recipient(t1, "line", true).await;
    let disabled = notify_recipient(t1, "lineworks", false).await;
    let a = app(FakePdfs::default(), None).await;

    let got = put_settings(&a, t1, json!({"notify_recipient_id": own})).await;
    assert_eq!(got.status, 200);
    assert_eq!(got.json()["notify_recipient_id"], own.to_string());
    assert_eq!(got.json()["lineworks_channel_id"], Value::Null);

    for r in [other, line, disabled, Uuid::new_v4()] {
        let got = put_settings(&a, t1, json!({"notify_recipient_id": r, "grace_days": 1})).await;
        assert_eq!(got.status, 400);
        assert_eq!(got.json(), json!({"error": "unknown_notify_recipient"}));
    }
    // 拒んだ PUT は何も書いていない
    let got = call(&a, "GET", "/api/leave/settings", Some(t1), ADMIN, None).await;
    assert_eq!(got.json()["notify_recipient_id"], own.to_string());
    assert_eq!(got.json()["grace_days"], 7);
}

#[tokio::test]
async fn notify_targets_are_exclusive() {
    let t = tenant().await;
    let ch = lineworks_channel(t).await;
    let r = notify_recipient(t, "lineworks", true).await;
    let a = app(FakePdfs::default(), None).await;

    let got = put_settings(
        &a,
        t,
        json!({"lineworks_channel_id": ch, "notify_recipient_id": r}),
    )
    .await;
    assert_eq!(got.status, 400);
    assert_eq!(got.json(), json!({"error": "both_notify_targets"}));

    // グループから個人へ置き換えられる (PUT は全体の置き換え)
    let got = put_settings(&a, t, json!({"lineworks_channel_id": ch})).await;
    assert_eq!(got.status, 200);
    let got = put_settings(&a, t, json!({"notify_recipient_id": r})).await;
    assert_eq!(got.status, 200);
    let v = got.json();
    assert_eq!(
        (
            v["lineworks_channel_id"].clone(),
            v["notify_recipient_id"].clone()
        ),
        (Value::Null, json!(r.to_string()))
    );
}

// ---- テスト送信 (POST /leave/settings/test-notify) ----

async fn test_notify(a: &App, t: Uuid) -> support::Got {
    call(
        a,
        "POST",
        "/api/leave/settings/test-notify",
        Some(t),
        ADMIN,
        None,
    )
    .await
}

/// 保存済みの宛先 (個人もグループも) へ固定の文面を 1 通だけ送る。ほかの tenant の宛先へは送らない
#[tokio::test]
async fn test_notify_sends_the_fixed_text_to_the_saved_target() {
    let t = tenant().await;
    let other = tenant().await;
    let other_r = notify_recipient(other, "lineworks", true).await;
    let n = Arc::new(FakeNotifier::default());
    let a = app_with_notifier(n.clone()).await;
    put_settings(&a, other, json!({"notify_recipient_id": other_r})).await;

    let r = notify_recipient(t, "lineworks", true).await;
    assert_eq!(
        put_settings(&a, t, json!({"notify_recipient_id": r}))
            .await
            .status,
        200
    );
    let got = test_notify(&a, t).await;
    assert_eq!(got.status, 200);
    assert_eq!(got.json(), json!({"ok": true}));
    assert_eq!(
        *n.sent.lock().unwrap(),
        vec![(
            NotifyTarget::Recipient { id: r, bot: None },
            TEST_NOTIFY_TEXT.to_owned()
        )]
    );
    assert_eq!(
        TEST_NOTIFY_TEXT,
        "勤怠申請システムからのテスト通知です。この通知が届けば毎朝の警告も届きます。"
    );

    let ch = lineworks_channel(t).await;
    put_settings(&a, t, json!({"lineworks_channel_id": ch})).await;
    let got = test_notify(&a, t).await;
    assert_eq!(got.status, 200);
    let sent = n.sent.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[1],
        (NotifyTarget::Channel(ch), TEST_NOTIFY_TEXT.to_owned())
    );
}

/// 宛先が無い (設定が無い・個人が LINE か無効) なら 400 `no_notify_target` で、送らない
#[tokio::test]
async fn test_notify_without_a_target_is_400() {
    let t = tenant().await;
    let n = Arc::new(FakeNotifier::default());
    let a = app_with_notifier(n.clone()).await;
    let got = test_notify(&a, t).await;
    assert_eq!(got.status, 400);
    assert_eq!(got.json(), json!({"error": "no_notify_target"}));

    // 個人が送れない (LINE の個人・無効) 行は宛先にならない (設定に入るのは自 tenant の有効な LINE WORKS だけなので、
    // 保存後に無効にして確かめる)
    let r = notify_recipient(t, "lineworks", true).await;
    put_settings(&a, t, json!({"notify_recipient_id": r})).await;
    support::admin()
        .await
        .execute(
            "UPDATE alc_api.notify_recipients SET enabled = false WHERE id = $1",
            &[&r],
        )
        .await
        .unwrap();
    let got = test_notify(&a, t).await;
    assert_eq!(got.status, 400);
    assert_eq!(got.json(), json!({"error": "no_notify_target"}));
    assert_eq!(*n.calls.lock().unwrap(), 0);
}

/// 送信に失敗したら 502 `notify_failed`。ログも応答も固定の語だけ
#[tokio::test]
async fn test_notify_send_failure_is_502() {
    let t = tenant().await;
    let r = notify_recipient(t, "lineworks", true).await;
    let n = Arc::new(FakeNotifier {
        fail_calls: vec![0],
        ..Default::default()
    });
    let a = app_with_notifier(n.clone()).await;
    put_settings(&a, t, json!({"notify_recipient_id": r})).await;
    let got = test_notify(&a, t).await;
    assert_eq!(got.status, 502);
    assert_eq!(got.json(), json!({"error": "notify_failed"}));
    assert!(n.sent.lock().unwrap().is_empty());
    let logs = a.logs.lock().unwrap();
    assert_eq!(*logs, vec!["test_notify: send status".to_owned()]);
}

/// 送れない環境 (staging・local) は 503 `notify_unavailable`
#[tokio::test]
async fn test_notify_unavailable_is_503() {
    let t = tenant().await;
    let r = notify_recipient(t, "lineworks", true).await;
    let a = app_with_notifier(Arc::new(UnavailableNotifier)).await;
    put_settings(&a, t, json!({"notify_recipient_id": r})).await;
    let got = test_notify(&a, t).await;
    assert_eq!(got.status, 503);
    assert_eq!(got.json(), json!({"error": "notify_unavailable"}));
    assert!(a.logs.lock().unwrap().is_empty());
}

/// 個人宛てに使う bot は自 tenant の有効な LINE WORKS だけ。個人なしの bot は 400。拒んだ PUT は何も書かない
#[tokio::test]
async fn notify_bot_must_be_own_enabled_lineworks_with_recipient() {
    let (t1, t2) = (tenant().await, tenant().await);
    let r = notify_recipient(t1, "lineworks", true).await;
    let own = support::bot_config(t1, "lineworks", true).await;
    let other = support::bot_config(t2, "lineworks", true).await;
    let disabled = support::bot_config(t1, "lineworks", false).await;
    let line = support::bot_config(t1, "line", true).await;
    let a = app(FakePdfs::default(), None).await;

    let got = put_settings(
        &a,
        t1,
        json!({"notify_recipient_id": r, "notify_bot_config_id": own}),
    )
    .await;
    assert_eq!(got.status, 200);
    assert_eq!(got.json()["notify_bot_config_id"], own.to_string());
    let got = call(&a, "GET", "/api/leave/settings", Some(t1), ADMIN, None).await;
    assert_eq!(got.json()["notify_bot_config_id"], own.to_string());

    for bot in [other, disabled, line, Uuid::new_v4()] {
        let got = put_settings(
            &a,
            t1,
            json!({"notify_recipient_id": r, "notify_bot_config_id": bot, "grace_days": 1}),
        )
        .await;
        assert_eq!(got.status, 400);
        assert_eq!(got.json(), json!({"error": "unknown_notify_bot"}));
    }
    // 個人を選んでいないのに bot だけ指定
    let got = put_settings(&a, t1, json!({"notify_bot_config_id": own})).await;
    assert_eq!(got.status, 400);
    assert_eq!(got.json(), json!({"error": "notify_bot_without_recipient"}));

    // 拒んだ PUT は何も書いていない
    let got = call(&a, "GET", "/api/leave/settings", Some(t1), ADMIN, None).await;
    assert_eq!(got.json()["notify_bot_config_id"], own.to_string());
    assert_eq!(got.json()["grace_days"], 7);

    // bot を外せば (個人のまま) bot 無しに戻る
    let got = put_settings(&a, t1, json!({"notify_recipient_id": r})).await;
    assert_eq!(got.status, 200);
    assert_eq!(got.json()["notify_bot_config_id"], Value::Null);
}

/// テスト送信は設定した bot を載せて送る。bot が使えなくなったら (無効にした) 送らない — 別の bot へ戻らない
#[tokio::test]
async fn test_notify_uses_the_configured_bot_and_never_falls_back() {
    let t = tenant().await;
    let r = notify_recipient(t, "lineworks", true).await;
    let bot = support::bot_config(t, "lineworks", true).await;
    let n = Arc::new(FakeNotifier::default());
    let a = app_with_notifier(n.clone()).await;
    put_settings(
        &a,
        t,
        json!({"notify_recipient_id": r, "notify_bot_config_id": bot}),
    )
    .await;

    let got = test_notify(&a, t).await;
    assert_eq!(got.status, 200);
    assert_eq!(
        *n.sent.lock().unwrap(),
        vec![(
            NotifyTarget::Recipient {
                id: r,
                bot: Some(bot)
            },
            TEST_NOTIFY_TEXT.to_owned()
        )]
    );

    support::admin()
        .await
        .execute(
            "UPDATE alc_api.bot_configs SET enabled = false WHERE id = $1",
            &[&bot],
        )
        .await
        .unwrap();
    let got = test_notify(&a, t).await;
    assert_eq!(got.status, 400);
    assert_eq!(got.json(), json!({"error": "no_notify_target"}));
    assert_eq!(n.sent.lock().unwrap().len(), 1);
    assert_eq!(*n.calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn departments_relay() {
    let t = tenant().await;
    let body = json!({"source_table": "部門ﾏｽﾀ", "data": [{"department_code": "10", "department_name": "本社"}]});
    let a = app(
        FakePdfs::default(),
        Some(ichiban_with(&[(DEPARTMENTS_PATH, 200, body.clone())])),
    )
    .await;
    let got = call(&a, "GET", "/api/leave/departments", Some(t), ADMIN, None).await;
    assert_eq!(got.status, 200);
    assert_eq!(got.content_type.as_deref(), Some("application/json"));
    assert_eq!(got.json(), body);
    assert_eq!(
        *a.ichiban.unwrap().calls.lock().unwrap(),
        vec![DEPARTMENTS_PATH]
    );
}

#[tokio::test]
async fn ichiban_not_configured_is_503() {
    let t = tenant().await;
    let a = app(FakePdfs::default(), None).await;
    for path in ["/api/leave/departments", "/api/leave/employees"] {
        let got = call(&a, "GET", path, Some(t), ADMIN, None).await;
        assert_eq!(got.status, 503, "{path}");
        assert_eq!(got.json(), json!({"error": "ichiban_not_configured"}));
    }
}

#[tokio::test]
async fn ichiban_failures_are_502() {
    let t = tenant().await;
    let a = app(FakePdfs::default(), None).await;
    put_settings(&a, t, json!({"dept_codes": ["10"]})).await;
    // 上流の非 2xx
    let a = app(
        FakePdfs::default(),
        Some(ichiban_with(&[
            (DEPARTMENTS_PATH, 500, json!({"error": "x"})),
            (EMPLOYEES_PATH, 404, json!({})),
        ])),
    )
    .await;
    for path in ["/api/leave/departments", "/api/leave/employees"] {
        let got = call(&a, "GET", path, Some(t), ADMIN, None).await;
        assert_eq!(got.status, 502, "{path}");
        assert_eq!(got.json(), json!({"error": "ichiban_upstream"}));
    }
    // 呼び出しそのものの失敗 (登録の無い path)
    let a = app(FakePdfs::default(), Some(FakeIchiban::default())).await;
    for path in ["/api/leave/departments", "/api/leave/employees"] {
        let got = call(&a, "GET", path, Some(t), ADMIN, None).await;
        assert_eq!(got.status, 502, "{path}");
        assert_eq!(got.json(), json!({"error": "ichiban_unavailable"}));
    }
    // 形の違う応答
    let a = app(
        FakePdfs::default(),
        Some(ichiban_with(&[(EMPLOYEES_PATH, 200, json!({"rows": []}))])),
    )
    .await;
    let got = call(&a, "GET", "/api/leave/employees", Some(t), ADMIN, None).await;
    assert_eq!(got.status, 502);
    assert_eq!(got.json(), json!({"error": "ichiban_bad_response"}));
    // ログは固定の語と path だけ
    for line in a.logs.lock().unwrap().iter() {
        assert!(!line.contains(&t.to_string()), "{line}");
    }
}

#[tokio::test]
async fn employees_filtered_by_tenant_departments() {
    let t = tenant().await;
    let a = app(
        FakePdfs::default(),
        Some(ichiban_with(&[(EMPLOYEES_PATH, 200, employees_body())])),
    )
    .await;
    // 部門を選んでいない → 空 (一番星も呼ばない)
    let got = call(&a, "GET", "/api/leave/employees", Some(t), ADMIN, None).await;
    assert_eq!(got.status, 200);
    assert_eq!(got.json(), json!({"data": []}));
    assert!(a.ichiban.as_ref().unwrap().calls.lock().unwrap().is_empty());

    put_settings(&a, t, json!({"dept_codes": ["10"]})).await;
    let got = call(&a, "GET", "/api/leave/employees", Some(t), ADMIN, None).await;
    assert_eq!(got.status, 200);
    let v = got.json();
    assert_eq!(v["source_table"], "社員ﾏｽﾀ");
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"][0]["employee_code"], "1021");
}

#[tokio::test]
async fn list_pages_order_filters_and_isolation() {
    let (t, other) = (tenant().await, tenant().await);
    let key = format!("{t}/mail.pdf");
    let p_old = page(t, &format!("{t}/old.pdf"), 1, "2026-09-01T00:00:00Z").await;
    let p2 = page(t, &key, 2, "2026-10-01T00:00:00Z").await;
    let p1 = page(t, &key, 1, "2026-10-01T00:00:00Z").await;
    page(other, &format!("{other}/x.pdf"), 1, "2026-10-05T00:00:00Z").await;
    let a = app(FakePdfs::default(), None).await;

    let ids = |v: Value| -> Vec<String> {
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let got = call(&a, "GET", "/api/leave/pages", Some(t), ADMIN, None).await;
    assert_eq!(got.status, 200);
    assert_eq!(
        ids(got.json()),
        vec![p1.to_string(), p2.to_string(), p_old.to_string()]
    );
    assert_eq!(got.json()["data"][0]["r2_key"], key);

    let got = call(
        &a,
        "GET",
        "/api/leave/pages?limit=1&offset=1",
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(ids(got.json()), vec![p2.to_string()]);

    // 紐づけて、status / employee_cd で絞る
    let body = json!({"employee_cd": "1021", "status": "linked",
                      "periods": [{"kind": "yukyu", "start_date": "2026-10-02", "end_date": "2026-10-02"}]});
    assert_eq!(
        call(
            &a,
            "PUT",
            &format!("/api/leave/pages/{p2}"),
            Some(t),
            ADMIN,
            Some(body)
        )
        .await
        .status,
        200
    );
    let got = call(
        &a,
        "GET",
        "/api/leave/pages?status=linked",
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(ids(got.json()), vec![p2.to_string()]);
    let got = call(
        &a,
        "GET",
        "/api/leave/pages?employee_cd=1021&status=",
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(ids(got.json()), vec![p2.to_string()]);
    let got = call(
        &a,
        "GET",
        "/api/leave/pages?status=unlinked",
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(ids(got.json()), vec![p1.to_string(), p_old.to_string()]);

    for (q, code) in [
        ("status=done", "invalid_status"),
        ("limit=0", "invalid_limit"),
        ("offset=-1", "invalid_offset"),
    ] {
        let got = call(
            &a,
            "GET",
            &format!("/api/leave/pages?{q}"),
            Some(t),
            ADMIN,
            None,
        )
        .await;
        assert_eq!(got.status, 400, "{q}");
        assert_eq!(got.json(), json!({"error": code}));
    }
    let got = call(
        &a,
        "GET",
        "/api/leave/pages?status=linked&status=ignored",
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(got.status, 400);
    assert_eq!(got.json(), json!({"error": "invalid_query"}));
}

#[tokio::test]
async fn link_page_replaces_periods_in_one_tx() {
    let t = tenant().await;
    let p = page(t, &format!("{t}/a.pdf"), 1, "2026-10-01T00:00:00Z").await;
    let a = app(FakePdfs::default(), None).await;
    let uri = format!("/api/leave/pages/{p}");

    let body = json!({"employee_cd": "1021", "status": "linked", "note": " 要確認 ",
    "periods": [
        {"kind": "rousai", "start_date": "2026-10-05", "end_date": "2026-10-07"},
        {"kind": "yukyu", "start_date": "2026-10-01", "end_date": "2026-10-01", "half": "pm"}
    ]});
    let got = call(&a, "PUT", &uri, Some(t), ADMIN, Some(body)).await;
    assert_eq!(got.status, 200);
    let v = got.json();
    assert_eq!(v["id"], p.to_string());
    assert_eq!(v["status"], "linked");
    assert_eq!(v["employee_cd"], "1021");
    assert_eq!(v["note"], "要確認");
    assert_eq!(v["linked_by"], "admin@example.com");
    assert!(v["linked_at"].is_string());
    assert_eq!(
        v["periods"],
        json!([
            {"kind": "yukyu", "start_date": "2026-10-01", "end_date": "2026-10-01", "half": "pm"},
            {"kind": "rousai", "start_date": "2026-10-05", "end_date": "2026-10-07", "half": null}
        ])
    );
    assert_eq!(call(&a, "GET", &uri, Some(t), ADMIN, None).await.json(), v);

    // 2 回目は全削除 → 挿入
    let body = json!({"employee_cd": "1021", "status": "linked",
        "periods": [{"kind": "kekkin", "start_date": "2026-10-09", "end_date": "2026-10-09"}]});
    let got = call(&a, "PUT", &uri, Some(t), ADMIN, Some(body)).await;
    assert_eq!(got.json()["periods"].as_array().unwrap().len(), 1);
    assert_eq!(got.json()["periods"][0]["kind"], "kekkin");

    // linked でなくなれば linked_by / linked_at は消える
    let got = call(
        &a,
        "PUT",
        &uri,
        Some(t),
        ADMIN,
        Some(json!({"status": "ignored", "note": "重複"})),
    )
    .await;
    assert_eq!(got.status, 200);
    let v = got.json();
    assert_eq!(v["status"], "ignored");
    assert_eq!(v["employee_cd"], Value::Null);
    assert_eq!(v["linked_by"], Value::Null);
    assert_eq!(v["linked_at"], Value::Null);
    assert_eq!(v["periods"], json!([]));
}

#[tokio::test]
async fn link_page_rejects_bad_input_without_writing() {
    let t = tenant().await;
    let p = page(t, &format!("{t}/a.pdf"), 1, "2026-10-01T00:00:00Z").await;
    let a = app(FakePdfs::default(), None).await;
    let uri = format!("/api/leave/pages/{p}");
    let period = |s: &str, e: &str, half: Value| json!({"kind": "yukyu", "start_date": s, "end_date": e, "half": half});
    for (body, code) in [
        (
            json!({"employee_cd": "1021", "status": "linked", "periods": [period("2026-10-03", "2026-10-01", Value::Null)]}),
            "period_end_before_start",
        ),
        (
            json!({"employee_cd": "1021", "status": "linked", "periods": [period("2026-10-01", "2026-10-02", json!("am"))]}),
            "half_day_must_be_single_day",
        ),
        (
            json!({"employee_cd": "1021", "status": "linked", "periods": []}),
            "linked_requires_periods",
        ),
        (
            json!({"status": "linked", "periods": [period("2026-10-01", "2026-10-01", Value::Null)]}),
            "linked_requires_employee_cd",
        ),
        (json!({"status": "done"}), "invalid_status"),
        (
            json!({"status": "linked", "periods": [{"kind": "yukyu", "start_date": "10/01", "end_date": "10/01"}]}),
            "invalid_body",
        ),
        (json!({"employee_cd": "1021"}), "invalid_body"),
    ] {
        let got = call(&a, "PUT", &uri, Some(t), ADMIN, Some(body.clone())).await;
        assert_eq!(got.status, 400, "{body}");
        assert_eq!(got.json(), json!({"error": code}), "{body}");
    }
    let got = call(&a, "GET", &uri, Some(t), ADMIN, None).await;
    assert_eq!(got.json()["status"], "unlinked");
    assert_eq!(got.json()["periods"], json!([]));
}

#[tokio::test]
async fn other_tenant_page_is_404_everywhere() {
    let (t, other) = (tenant().await, tenant().await);
    let key = format!("{other}/secret.pdf");
    let p = page(other, &key, 1, "2026-10-01T00:00:00Z").await;
    let pdfs = FakePdfs {
        objects: HashMap::from([(key, b"%PDF-1.7 other".to_vec())]),
        ..Default::default()
    };
    let a = app(pdfs, None).await;
    let body = json!({"status": "ignored"});
    for (method, uri, body) in [
        ("GET", format!("/api/leave/pages/{p}"), None),
        ("PUT", format!("/api/leave/pages/{p}"), Some(body.clone())),
        ("GET", format!("/api/leave/pages/{p}/pdf"), None),
        ("GET", format!("/api/leave/pages/{}", Uuid::new_v4()), None),
        (
            "PUT",
            format!("/api/leave/pages/{}", Uuid::new_v4()),
            Some(body.clone()),
        ),
        (
            "GET",
            format!("/api/leave/pages/{}/pdf", Uuid::new_v4()),
            None,
        ),
        ("GET", "/api/leave/pages/not-a-uuid".to_owned(), None),
        (
            "PUT",
            "/api/leave/pages/not-a-uuid".to_owned(),
            Some(body.clone()),
        ),
        ("GET", "/api/leave/pages/not-a-uuid/pdf".to_owned(), None),
    ] {
        let got = call(&a, method, &uri, Some(t), ADMIN, body).await;
        assert_eq!(got.status, 404, "{method} {uri}");
        assert_eq!(got.json(), json!({"error": "not_found"}));
    }
    // 他 tenant の key では置き場を読んでいない
    assert!(a.pdfs.reads.lock().unwrap().is_empty());
    // 他 tenant のページは変わっていない
    let mine = app(FakePdfs::default(), None).await;
    let got = call(
        &mine,
        "GET",
        &format!("/api/leave/pages/{p}"),
        Some(other),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(got.json()["status"], "unlinked");
}

#[tokio::test]
async fn pdf_served_from_store() {
    let t = tenant().await;
    let key = format!("{t}/a.pdf");
    let p = page(t, &key, 3, "2026-10-01T00:00:00Z").await;
    let missing = page(t, &format!("{t}/gone.pdf"), 1, "2026-10-01T00:00:00Z").await;
    let pdfs = FakePdfs {
        objects: HashMap::from([(key.clone(), b"%PDF-1.7 mine".to_vec())]),
        ..Default::default()
    };
    let a = app(pdfs, None).await;
    let got = call(
        &a,
        "GET",
        &format!("/api/leave/pages/{p}/pdf"),
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(got.status, 200);
    assert_eq!(got.content_type.as_deref(), Some("application/pdf"));
    assert_eq!(got.body, b"%PDF-1.7 mine");
    assert_eq!(*a.pdfs.reads.lock().unwrap(), vec![key]);

    let got = call(
        &a,
        "GET",
        &format!("/api/leave/pages/{missing}/pdf"),
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(got.status, 404);
    assert_eq!(got.json(), json!({"error": "pdf_not_found"}));

    let a = app(
        FakePdfs {
            fail: true,
            ..Default::default()
        },
        None,
    )
    .await;
    let got = call(
        &a,
        "GET",
        &format!("/api/leave/pages/{p}/pdf"),
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(got.status, 500);
    assert_eq!(got.json(), json!({"error": "internal_error"}));
    assert_eq!(*a.logs.lock().unwrap(), vec!["get_pdf: store get"]);
}

/// 接続が切れた DB では、どの口も 500 の固定の語で、ログは段の名前と kind だけ
#[tokio::test]
async fn db_errors_are_500() {
    let t = tenant().await;
    let p = page(t, &format!("{t}/a.pdf"), 1, "2026-10-01T00:00:00Z").await;
    let a = support::app_with_broken_db(Some(ichiban_with(&[]))).await;
    let link = json!({"employee_cd": "1", "status": "linked",
                      "periods": [{"kind": "yukyu", "start_date": "2026-10-01", "end_date": "2026-10-01"}]});
    for (method, uri, body) in [
        ("GET", "/api/leave/settings".to_owned(), None),
        ("PUT", "/api/leave/settings".to_owned(), Some(json!({}))),
        ("POST", "/api/leave/settings/test-notify".to_owned(), None),
        ("GET", "/api/leave/employees".to_owned(), None),
        ("GET", "/api/leave/pages".to_owned(), None),
        ("GET", format!("/api/leave/pages/{p}"), None),
        ("PUT", format!("/api/leave/pages/{p}"), Some(link)),
        ("GET", format!("/api/leave/pages/{p}/pdf"), None),
    ] {
        let got = call(&a, method, &uri, Some(t), ADMIN, body).await;
        assert_eq!(got.status, 500, "{method} {uri}");
        assert_eq!(got.json(), json!({"error": "internal_error"}));
    }
    let logs = a.logs.lock().unwrap();
    assert_eq!(logs.len(), 8);
    for line in logs.iter() {
        assert!(
            !line.contains(&t.to_string()) && !line.contains(&p.to_string()),
            "{line}"
        );
    }
}

// ---- カレンダーと有給の付与台帳 (ledger) ----

fn ledger_employees() -> Value {
    json!({"data": [
        {"employee_code": "1021", "employee_name": "山田太郎", "dept_code": "10", "hire_date": "2024-04-01", "retire_date": null},
        {"employee_code": "1030", "employee_name": "甲", "dept_code": "10", "hire_date": null, "retire_date": null},
        {"employee_code": "2001", "employee_name": "乙", "dept_code": "20", "hire_date": "2020-04-01", "retire_date": null}
    ]})
}

fn day(code: &str, date: &str, item: &str) -> Value {
    json!({"employee_code": code, "date": date, "item_name": item})
}

/// 1021 の一番星の休暇 (付与台帳の今日 = 2026-10-10 とカレンダーの 2026-10 の両方の区間で登録する)
fn ledger_ichiban(rows: &[Value]) -> FakeIchiban {
    let mut f = ichiban_with(&[(EMPLOYEES_PATH, 200, ledger_employees())]);
    support::add_days(&mut f, "2024-04-01", "2026-10-10", rows);
    support::add_days(&mut f, "2024-04-01", "2026-10-31", rows);
    f
}

fn ledger_rows() -> Vec<Value> {
    vec![
        day("1021", "2025-01-10", "有休"),
        day("1021", "2025-11-01", "午前休"),
        day("1021", "2026-03-02", "労災"),
        day("1021", "2026-10-01", "有休（積置）"),
        day("1021", "2026-10-02", "有休"),
        day("1021", "2026-10-05", "午前休"),
        day("1021", "2026-10-06", "労災"),
        day("1021", "2026-10-07", "謎"),
        day("1021", "2026-10-09", "休み"),
        day("2001", "2026-10-01", "有休"),
    ]
}

async fn ledger_app(t: Uuid) -> App {
    let a = app(FakePdfs::default(), Some(ledger_ichiban(&ledger_rows()))).await;
    assert_eq!(
        put_settings(&a, t, json!({"dept_codes": ["10"]}))
            .await
            .status,
        200
    );
    a
}

#[tokio::test]
async fn grants_auto_manual_edit_delete_and_balance() {
    let t = tenant().await;
    let a = ledger_app(t).await;
    // 法定付与: 入社 2024-04-01 → 2024-10-01 (10 日)・2025-10-01 (11 日)・2026-10-01 (12 日)
    let got = call(
        &a,
        "POST",
        "/api/leave/grants/auto?employee_cd=1021",
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(got.status, 200);
    assert_eq!(
        got.json(),
        json!({"employee_cd": "1021", "hire_date": "2024-04-01", "inserted": 3, "computed": [
        {"grant_date": "2024-10-01", "days": 10.0, "expires_on": "2026-10-01"},
        {"grant_date": "2025-10-01", "days": 11.0, "expires_on": "2027-10-01"},
        {"grant_date": "2026-10-01", "days": 12.0, "expires_on": "2028-10-01"}]})
    );
    let again = call(
        &a,
        "POST",
        "/api/leave/grants/auto?employee_cd=1021",
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(again.json()["inserted"], 0);
    // 入社日の無い乗務員は付与しない
    let none = call(
        &a,
        "POST",
        "/api/leave/grants/auto?employee_cd=1030",
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(
        none.json(),
        json!({"employee_cd": "1030", "hire_date": null, "inserted": 0, "computed": []})
    );

    // 手で足す (manual)。同じ付与日は 409
    let body =
        json!({"employee_cd": "1021", "grant_date": "2026-04-01", "days": 3.5, "note": " 前職 "});
    let got = call(
        &a,
        "POST",
        "/api/leave/grants",
        Some(t),
        ADMIN,
        Some(body.clone()),
    )
    .await;
    assert_eq!(got.status, 201);
    let manual = got.json();
    assert_eq!(
        (
            manual["source"].as_str(),
            manual["days"].as_f64(),
            manual["expires_on"].as_str(),
            manual["note"].as_str()
        ),
        (Some("manual"), Some(3.5), Some("2028-04-01"), Some("前職"))
    );
    let dup = call(&a, "POST", "/api/leave/grants", Some(t), ADMIN, Some(body)).await;
    assert_eq!(
        (dup.status, dup.json()),
        (409, json!({"error": "grant_exists"}))
    );

    // 一覧と残 (今日 = 2026-10-10)。2024-10-01 の付与は 2025-01-10 (1)・2025-11-01 (0.5) を消化して 8.5 日が失効、
    // 2026-10-01・02・05 (2.5) は 2025-10-01 の付与から
    let got = call(
        &a,
        "GET",
        "/api/leave/grants?employee_cd=1021",
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(got.status, 200);
    let v = got.json();
    assert_eq!(
        v["balance"],
        json!({"as_of": "2026-10-10", "remaining": 24.0, "expired": 8.5, "unallocated": 0.0, "consumption_from": "2024-04-01"})
    );
    let rows: Vec<(String, String, f64, f64, f64)> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            (
                g["grant_date"].as_str().unwrap().to_owned(),
                g["source"].as_str().unwrap().to_owned(),
                g["used"].as_f64().unwrap(),
                g["remaining"].as_f64().unwrap(),
                g["expired"].as_f64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            ("2024-10-01".into(), "auto".into(), 1.5, 0.0, 8.5),
            ("2025-10-01".into(), "auto".into(), 2.5, 8.5, 0.0),
            ("2026-04-01".into(), "manual".into(), 0.0, 3.5, 0.0),
            ("2026-10-01".into(), "auto".into(), 0.0, 12.0, 0.0)
        ]
    );
    assert_eq!(
        (v["employee_name"].as_str(), v["hire_date"].as_str()),
        (Some("山田太郎"), Some("2024-04-01"))
    );
    // 期首残が無ければ従来どおり (置き換えた行も無い)
    assert_eq!(v["opening"], Value::Null);
    assert!(v["data"]
        .as_array()
        .unwrap()
        .iter()
        .all(|g| g["superseded"] == false));

    // auto を手で直すと manual になり、auto の再計算で上書きされない
    let auto_id = v["data"][1]["id"].as_str().unwrap().to_owned();
    let got = call(
        &a,
        "PUT",
        &format!("/api/leave/grants/{auto_id}"),
        Some(t),
        ADMIN,
        Some(json!({"days": 12, "expires_on": "2027-10-01", "note": "修正"})),
    )
    .await;
    assert_eq!(got.status, 200);
    assert_eq!(
        (
            got.json()["source"].as_str(),
            got.json()["days"].as_f64(),
            got.json()["note"].as_str()
        ),
        (Some("manual"), Some(12.0), Some("修正"))
    );
    let again = call(
        &a,
        "POST",
        "/api/leave/grants/auto?employee_cd=1021",
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(again.json()["inserted"], 0);
    let list = call(
        &a,
        "GET",
        "/api/leave/grants?employee_cd=1021",
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(list.json()["data"][1]["days"], 12.0);

    // 直すときの検証
    for (body, code) in [
        (
            json!({"days": 1, "expires_on": "2025-01-01"}),
            "expires_before_grant",
        ),
        (
            json!({"days": 0.25, "expires_on": "2028-01-01"}),
            "invalid_days",
        ),
        (json!({"days": 1}), "invalid_body"),
    ] {
        let got = call(
            &a,
            "PUT",
            &format!("/api/leave/grants/{auto_id}"),
            Some(t),
            ADMIN,
            Some(body.clone()),
        )
        .await;
        assert_eq!(
            (got.status, got.json()),
            (400, json!({"error": code})),
            "{body}"
        );
    }
    for id in [Uuid::new_v4().to_string(), "x".into()] {
        let got = call(
            &a,
            "PUT",
            &format!("/api/leave/grants/{id}"),
            Some(t),
            ADMIN,
            Some(json!({"days": 1, "expires_on": "2028-01-01"})),
        )
        .await;
        assert_eq!(got.status, 404);
        let got = call(
            &a,
            "DELETE",
            &format!("/api/leave/grants/{id}"),
            Some(t),
            ADMIN,
            None,
        )
        .await;
        assert_eq!(got.status, 404);
    }

    // 消す
    let id = manual["id"].as_str().unwrap();
    let got = call(
        &a,
        "DELETE",
        &format!("/api/leave/grants/{id}"),
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!((got.status, got.body.len()), (204, 0));
    let got = call(
        &a,
        "DELETE",
        &format!("/api/leave/grants/{id}"),
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(got.status, 404);
}

#[tokio::test]
async fn calendar_matches_days_and_counts_rousai() {
    let t = tenant().await;
    let a = ledger_app(t).await;
    call(
        &a,
        "POST",
        "/api/leave/grants/auto?employee_cd=1021",
        Some(t),
        ADMIN,
        None,
    )
    .await;
    let p = page(t, &format!("{t}/c.pdf"), 1, "2026-10-08T00:00:00Z").await;
    let link = json!({"employee_cd": "1021", "status": "linked", "periods": [
        {"kind": "yukyu", "start_date": "2026-10-01", "end_date": "2026-10-01"},
        {"kind": "yukyu", "start_date": "2026-10-05", "end_date": "2026-10-05", "half": "pm"},
        {"kind": "kekkin", "start_date": "2026-10-06", "end_date": "2026-10-06"},
        {"kind": "tokukyu", "start_date": "2026-10-08", "end_date": "2026-10-08"},
        {"kind": "tokukyu", "start_date": "2026-11-02", "end_date": "2026-11-02"}]});
    assert_eq!(
        call(
            &a,
            "PUT",
            &format!("/api/leave/pages/{p}"),
            Some(t),
            ADMIN,
            Some(link)
        )
        .await
        .status,
        200
    );
    // 紐づけていないページの期間は出ない
    let other = page(t, &format!("{t}/d.pdf"), 1, "2026-10-08T00:00:00Z").await;
    let ignored = json!({"employee_cd": "1021", "status": "ignored", "periods": [{"kind": "kekkin", "start_date": "2026-10-09", "end_date": "2026-10-09"}]});
    call(
        &a,
        "PUT",
        &format!("/api/leave/pages/{other}"),
        Some(t),
        ADMIN,
        Some(ignored),
    )
    .await;

    let got = call(
        &a,
        "GET",
        "/api/leave/calendar?employee_cd=1021&month=2026-10",
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(got.status, 200);
    let v = got.json();
    let pid = p.to_string();
    assert_eq!(
        v["days"],
        json!([
            {"date": "2026-10-01", "ichiban": [{"kind": "yukyu", "half": null}], "pdf": [{"kind": "yukyu", "half": null, "page_id": pid}], "status": "match"},
            {"date": "2026-10-02", "ichiban": [{"kind": "yukyu", "half": null}], "pdf": [], "status": "ichiban_only"},
            {"date": "2026-10-05", "ichiban": [{"kind": "yukyu", "half": "am"}], "pdf": [{"kind": "yukyu", "half": "pm", "page_id": pid}], "status": "match"},
            {"date": "2026-10-06", "ichiban": [{"kind": "rousai", "half": null}], "pdf": [{"kind": "kekkin", "half": null, "page_id": pid}], "status": "mismatch"},
            {"date": "2026-10-08", "ichiban": [], "pdf": [{"kind": "tokukyu", "half": null, "page_id": pid}], "status": "pdf_only"}
        ])
    );
    assert_eq!(
        v["yukyu"],
        json!({"as_of": "2026-10-31", "remaining": 20.5, "expired": 8.5, "unallocated": 0.0, "consumption_from": "2024-04-01"})
    );
    assert_eq!(v["rousai"], json!({"year": 2, "month": 1}));
    assert_eq!(
        (
            v["unknown_items"].as_u64(),
            v["employee_name"].as_str(),
            v["month"].as_str()
        ),
        (Some(1), Some("山田太郎"), Some("2026-10"))
    );
}

#[tokio::test]
async fn ledger_is_tenant_and_department_scoped() {
    let (t1, t2) = (tenant().await, tenant().await);
    let a = ledger_app(t1).await;
    assert_eq!(
        put_settings(&a, t2, json!({"dept_codes": ["10"]}))
            .await
            .status,
        200
    );
    let body = json!({"employee_cd": "1021", "grant_date": "2026-04-01", "days": 1});
    let g = call(&a, "POST", "/api/leave/grants", Some(t1), ADMIN, Some(body))
        .await
        .json();
    let id = g["id"].as_str().unwrap();
    // 他 tenant から付与は見えず、直せず、消せない
    let list = call(
        &a,
        "GET",
        "/api/leave/grants?employee_cd=1021",
        Some(t2),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(list.json()["data"], json!([]));
    let put = call(
        &a,
        "PUT",
        &format!("/api/leave/grants/{id}"),
        Some(t2),
        ADMIN,
        Some(json!({"days": 9, "expires_on": "2028-01-01"})),
    )
    .await;
    assert_eq!(put.status, 404);
    assert_eq!(
        call(
            &a,
            "DELETE",
            &format!("/api/leave/grants/{id}"),
            Some(t2),
            ADMIN,
            None
        )
        .await
        .status,
        404
    );
    let list = call(
        &a,
        "GET",
        "/api/leave/grants?employee_cd=1021",
        Some(t1),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(list.json()["data"][0]["days"], 1.0);
    // 自 tenant の部門に入らない乗務員・一番星に居ない乗務員は 404
    for code in ["2001", "9999"] {
        for (method, uri, body) in [
            ("GET", format!("/api/leave/grants?employee_cd={code}"), None),
            (
                "GET",
                format!("/api/leave/calendar?employee_cd={code}&month=2026-10"),
                None,
            ),
            (
                "POST",
                format!("/api/leave/grants/auto?employee_cd={code}"),
                None,
            ),
            (
                "POST",
                "/api/leave/grants".to_owned(),
                Some(json!({"employee_cd": code, "grant_date": "2026-04-01", "days": 1})),
            ),
        ] {
            let got = call(&a, method, &uri, Some(t1), ADMIN, body).await;
            assert_eq!(
                (got.status, got.json()),
                (404, json!({"error": "not_found"})),
                "{method} {uri}"
            );
        }
    }
    // 部門を選んでいない tenant には誰も居ない (一番星も呼ばない)
    let t3 = tenant().await;
    let before = a.ichiban.as_ref().unwrap().calls.lock().unwrap().len();
    let got = call(
        &a,
        "GET",
        "/api/leave/grants?employee_cd=1021",
        Some(t3),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(got.status, 404);
    assert_eq!(
        a.ichiban.as_ref().unwrap().calls.lock().unwrap().len(),
        before
    );
}

#[tokio::test]
async fn ledger_rejects_bad_input_and_needs_ichiban() {
    let t = tenant().await;
    let a = ledger_app(t).await;
    for (method, uri, body, code) in [
        ("GET", "/api/leave/grants", None, "invalid_employee_cd"),
        (
            "GET",
            "/api/leave/grants?employee_cd=1&employee_cd=2",
            None,
            "invalid_query",
        ),
        (
            "POST",
            "/api/leave/grants/auto?employee_cd=%20",
            None,
            "invalid_employee_cd",
        ),
        (
            "GET",
            "/api/leave/calendar?employee_cd=1021",
            None,
            "invalid_month",
        ),
        (
            "GET",
            "/api/leave/calendar?employee_cd=1021&month=2026-13",
            None,
            "invalid_month",
        ),
        (
            "GET",
            "/api/leave/calendar?month=2026-10",
            None,
            "invalid_employee_cd",
        ),
        (
            "POST",
            "/api/leave/grants",
            Some(json!({"employee_cd": "1021"})),
            "invalid_body",
        ),
        (
            "POST",
            "/api/leave/grants",
            Some(json!({"employee_cd": "1021", "grant_date": "2026-04-01", "days": -1})),
            "invalid_days",
        ),
    ] {
        let got = call(&a, method, uri, Some(t), ADMIN, body).await;
        assert_eq!(
            (got.status, got.json()),
            (400, json!({"error": code})),
            "{method} {uri}"
        );
    }
    // 一番星の binding が無い環境 (staging) では 503
    let none = app(FakePdfs::default(), None).await;
    for (method, uri, body) in [
        ("GET", "/api/leave/grants?employee_cd=1021", None),
        (
            "GET",
            "/api/leave/calendar?employee_cd=1021&month=2026-10",
            None,
        ),
        ("POST", "/api/leave/grants/auto?employee_cd=1021", None),
        (
            "POST",
            "/api/leave/grants",
            Some(json!({"employee_cd": "1021", "grant_date": "2026-04-01", "days": 1})),
        ),
    ] {
        let got = call(&none, method, uri, Some(t), ADMIN, body).await;
        assert_eq!(
            (got.status, got.json()),
            (503, json!({"error": "ichiban_not_configured"})),
            "{method} {uri}"
        );
    }
}

#[tokio::test]
async fn ledger_ichiban_failures_are_502() {
    let t = tenant().await;
    let a = ledger_app(t).await;
    let cases: [(FakeIchiban, &str); 4] = [
        (
            ichiban_with(&[(EMPLOYEES_PATH, 500, json!({}))]),
            "ichiban_upstream",
        ),
        (
            ichiban_with(&[(EMPLOYEES_PATH, 200, json!({"rows": []}))]),
            "ichiban_bad_response",
        ),
        // 休暇の口が無い (呼び出しの失敗)
        (
            ichiban_with(&[(EMPLOYEES_PATH, 200, ledger_employees())]),
            "ichiban_unavailable",
        ),
        // 休暇の口の形が違う
        (
            {
                let mut f = ichiban_with(&[(EMPLOYEES_PATH, 200, ledger_employees())]);
                for p in leave::reconcile::days_paths(
                    "2024-04-01".parse().unwrap(),
                    "2026-10-31".parse().unwrap(),
                )
                .into_iter()
                .chain(leave::reconcile::days_paths(
                    "2024-04-01".parse().unwrap(),
                    "2026-10-10".parse().unwrap(),
                )) {
                    f.responses.insert(
                        p,
                        leave::ichiban::IchibanResponse {
                            status: 200,
                            body: b"[]".to_vec(),
                        },
                    );
                }
                f
            },
            "ichiban_bad_response",
        ),
    ];
    for (fake, code) in cases {
        let b = app(FakePdfs::default(), Some(fake)).await;
        for uri in [
            "/api/leave/grants?employee_cd=1021",
            "/api/leave/calendar?employee_cd=1021&month=2026-10",
        ] {
            let got = call(&b, "GET", uri, Some(t), ADMIN, None).await;
            assert_eq!(
                (got.status, got.json()),
                (502, json!({"error": code})),
                "{uri}"
            );
        }
        for line in b.logs.lock().unwrap().iter() {
            assert!(
                !line.contains(&t.to_string()) && !line.contains("山田"),
                "{line}"
            );
        }
    }
    drop(a);
}

#[tokio::test]
async fn ledger_db_errors_are_500() {
    let t = tenant().await;
    let a = support::app_with_broken_db(Some(ledger_ichiban(&[]))).await;
    let id = Uuid::new_v4();
    for (method, uri, body) in [
        ("GET", "/api/leave/grants?employee_cd=1021".to_owned(), None),
        (
            "GET",
            "/api/leave/calendar?employee_cd=1021&month=2026-10".to_owned(),
            None,
        ),
        (
            "POST",
            "/api/leave/grants/auto?employee_cd=1021".to_owned(),
            None,
        ),
        (
            "POST",
            "/api/leave/grants".to_owned(),
            Some(json!({"employee_cd": "1021", "grant_date": "2026-04-01", "days": 1})),
        ),
        (
            "PUT",
            format!("/api/leave/grants/{id}"),
            Some(json!({"days": 1, "expires_on": "2028-01-01"})),
        ),
        ("DELETE", format!("/api/leave/grants/{id}"), None),
    ] {
        let got = call(&a, method, &uri, Some(t), ADMIN, body).await;
        assert_eq!(
            (got.status, got.json()),
            (500, json!({"error": "internal_error"})),
            "{method} {uri}"
        );
    }
    for line in a.logs.lock().unwrap().iter() {
        assert!(
            !line.contains(&t.to_string()) && !line.contains(&id.to_string()),
            "{line}"
        );
    }
}

async fn put_opening(app: &App, t: Uuid, body: Value) -> support::Got {
    call(app, "PUT", "/api/leave/opening", Some(t), ADMIN, Some(body)).await
}

async fn openings(app: &App, t: Uuid) -> Value {
    let got = call(app, "GET", "/api/leave/opening", Some(t), ADMIN, None).await;
    assert_eq!(got.status, 200);
    got.json()
}

/// 1021 (入社 2024-04-01) と 1030 (入社日なし) の期首残
fn opening_of(v: &Value, code: &str) -> Value {
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["employee_cd"] == code)
        .unwrap()["opening"]
        .clone()
}

#[tokio::test]
async fn opening_list_put_and_delete() {
    let t = tenant().await;
    let a = ledger_app(t).await;
    let calls = |a: &App| a.ichiban.as_ref().unwrap().calls.lock().unwrap().len();
    let before = calls(&a);
    // 自 tenant の部門の全員 (保存前は null)。一番星は 1 回だけ
    let v = openings(&a, t).await;
    assert_eq!(
        v,
        json!({"data": [
            {"employee_cd": "1021", "employee_name": "山田太郎", "dept_code": "10", "hire_date": "2024-04-01", "retire_date": null, "opening": null},
            {"employee_cd": "1030", "employee_name": "甲", "dept_code": "10", "hire_date": null, "retire_date": null, "opening": null}
        ]})
    );
    assert_eq!(calls(&a), before + 1);

    // 片方 null は 0。入社日の無い乗務員も 0 日なら入る
    let got = put_opening(
        &a,
        t,
        json!({"as_of": "2026-09-30", "rows": [
            {"employee_cd": " 1021 ", "carryover": 1.5, "current": 8.5},
            {"employee_cd": "1030", "carryover": 0, "current": null}]}),
    )
    .await;
    assert_eq!(
        (got.status, got.json()),
        (200, json!({"saved": 2, "deleted": 0}))
    );
    let v = openings(&a, t).await;
    assert_eq!(
        opening_of(&v, "1021"),
        json!({"as_of": "2026-09-30", "carryover": 1.5, "current": 8.5})
    );
    assert_eq!(
        opening_of(&v, "1030"),
        json!({"as_of": "2026-09-30", "carryover": 0.0, "current": 0.0})
    );

    // 置き換え (期首日も変わる) と、両方 null の行は削除 (無い行の削除は数えない)
    let got = put_opening(
        &a,
        t,
        json!({"as_of": "2026-10-01", "rows": [
            {"employee_cd": "1021", "carryover": null, "current": 2},
            {"employee_cd": "1030", "carryover": null, "current": null}]}),
    )
    .await;
    assert_eq!(
        (got.status, got.json()),
        (200, json!({"saved": 1, "deleted": 1}))
    );
    let v = openings(&a, t).await;
    assert_eq!(
        opening_of(&v, "1021"),
        json!({"as_of": "2026-10-01", "carryover": 0.0, "current": 2.0})
    );
    assert_eq!(opening_of(&v, "1030"), Value::Null);
    let got = put_opening(
        &a,
        t,
        json!({"as_of": "2026-10-01", "rows": [{"employee_cd": "1021"}, {"employee_cd": "1030"}]}),
    )
    .await;
    assert_eq!(
        (got.status, got.json()),
        (200, json!({"saved": 0, "deleted": 1}))
    );
    assert_eq!(opening_of(&openings(&a, t).await, "1021"), Value::Null);
    // 空の rows
    let got = put_opening(&a, t, json!({"as_of": "2026-10-01", "rows": []})).await;
    assert_eq!(
        (got.status, got.json()),
        (200, json!({"saved": 0, "deleted": 0}))
    );
}

#[tokio::test]
async fn opening_put_rejects_whole_body() {
    let t = tenant().await;
    let a = ledger_app(t).await;
    let row =
        |code: &str, c: Value, r: Value| json!({"employee_cd": code, "carryover": c, "current": r});
    let ok = row("1021", json!(1), json!(2));
    let many: Vec<Value> = (0..1001)
        .map(|i| row(&i.to_string(), json!(1), json!(1)))
        .collect();
    for (body, code) in [
        (json!({"as_of": "2026-09-30"}), "invalid_body"),
        (
            json!({"as_of": "2026-09-30", "rows": [], "x": 1}),
            "invalid_body",
        ),
        (
            json!({"as_of": "2026-09-30", "rows": [{"employee_cd": "1021", "days": 1}]}),
            "invalid_body",
        ),
        (
            json!({"as_of": "2026-09-30", "rows": [ok, row("1021", json!(1), Value::Null)]}),
            "invalid_body",
        ),
        (json!({"as_of": "2026-9-30", "rows": [ok]}), "invalid_as_of"),
        (
            json!({"as_of": "2026-09-31", "rows": [ok]}),
            "invalid_as_of",
        ),
        (
            json!({"as_of": "2026-09-30", "rows": many}),
            "too_many_rows",
        ),
        (
            json!({"as_of": "2026-09-30", "rows": [ok, row("1030", json!(-1), json!(0))]}),
            "invalid_days",
        ),
        (
            json!({"as_of": "2026-09-30", "rows": [ok, row("1030", json!(0), json!(1000))]}),
            "invalid_days",
        ),
        (
            json!({"as_of": "2026-09-30", "rows": [ok, row("1030", json!(0.25), json!(0))]}),
            "invalid_days",
        ),
        // 自 tenant の部門に入らない・一番星に居ない・空 (削除の行も同じ)
        (
            json!({"as_of": "2026-09-30", "rows": [ok, row("2001", json!(1), json!(1))]}),
            "unknown_employee",
        ),
        (
            json!({"as_of": "2026-09-30", "rows": [ok, row("9999", Value::Null, Value::Null)]}),
            "unknown_employee",
        ),
        (
            json!({"as_of": "2026-09-30", "rows": [ok, row(" ", json!(1), json!(1))]}),
            "unknown_employee",
        ),
        // 入社日の無い乗務員 (G0 も G1 も無い)
        (
            json!({"as_of": "2026-09-30", "rows": [ok, row("1030", json!(1), json!(0))]}),
            "opening_no_prior_grant",
        ),
        (
            json!({"as_of": "2026-09-30", "rows": [ok, row("1030", json!(0), json!(0.5))]}),
            "opening_no_current_grant",
        ),
        // 入社 2024-04-01: 2025-09-30 時点の付与は 2024-10-01 だけ (G1 だけ)。2024-09-30 時点は無い
        (
            json!({"as_of": "2025-09-30", "rows": [row("1021", json!(1), json!(5))]}),
            "opening_no_prior_grant",
        ),
        (
            json!({"as_of": "2024-09-30", "rows": [row("1021", json!(0), json!(1))]}),
            "opening_no_current_grant",
        ),
    ] {
        let got = put_opening(&a, t, body.clone()).await;
        assert_eq!(
            (got.status, got.json()),
            (400, json!({"error": code})),
            "{body}"
        );
    }
    // どれも保存されていない (一部だけ保存しない)
    let v = openings(&a, t).await;
    assert!(v["data"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["opening"].is_null()));
    // G1 だけの時点でも、繰越 0 なら入る
    let got = put_opening(
        &a,
        t,
        json!({"as_of": "2025-09-30", "rows": [row("1021", json!(0), json!(5))]}),
    )
    .await;
    assert_eq!(got.status, 200);
}

#[tokio::test]
async fn opening_is_tenant_scoped() {
    let (t1, t2, t3) = (tenant().await, tenant().await, tenant().await);
    let a = ledger_app(t1).await;
    assert_eq!(
        put_settings(&a, t2, json!({"dept_codes": ["10"]}))
            .await
            .status,
        200
    );
    assert_eq!(
        put_settings(&a, t3, json!({"dept_codes": ["20"]}))
            .await
            .status,
        200
    );
    let body = json!({"as_of": "2026-09-30", "rows": [{"employee_cd": "1021", "carryover": 1, "current": 2}]});
    assert_eq!(put_opening(&a, t1, body.clone()).await.status, 200);
    // 他 tenant から見えず、消しても t1 の行は消えない
    assert_eq!(opening_of(&openings(&a, t2).await, "1021"), Value::Null);
    let del = put_opening(
        &a,
        t2,
        json!({"as_of": "2026-09-30", "rows": [{"employee_cd": "1021"}]}),
    )
    .await;
    assert_eq!(del.json(), json!({"saved": 0, "deleted": 0}));
    assert_eq!(
        opening_of(&openings(&a, t1).await, "1021"),
        json!({"as_of": "2026-09-30", "carryover": 1.0, "current": 2.0})
    );
    // 部門の違う tenant には 1021 が居ない
    let got = put_opening(&a, t3, body.clone()).await;
    assert_eq!(
        (got.status, got.json()),
        (400, json!({"error": "unknown_employee"}))
    );
    let v = openings(&a, t3).await;
    let codes: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["employee_cd"].as_str().unwrap())
        .collect();
    assert_eq!(codes, vec!["2001"]);
    // 部門を選んでいない tenant は空で、一番星も呼ばない
    let t4 = tenant().await;
    let before = a.ichiban.as_ref().unwrap().calls.lock().unwrap().len();
    assert_eq!(openings(&a, t4).await, json!({"data": []}));
    let got = put_opening(&a, t4, body).await;
    assert_eq!(
        (got.status, got.json()),
        (400, json!({"error": "unknown_employee"}))
    );
    assert_eq!(
        a.ichiban.as_ref().unwrap().calls.lock().unwrap().len(),
        before
    );
}

#[tokio::test]
async fn opening_needs_ichiban_and_db() {
    let t = tenant().await;
    let body = json!({"as_of": "2026-09-30", "rows": [{"employee_cd": "1021", "carryover": 1, "current": 2}]});
    // 一番星の binding が無い環境は 503、一番星の失敗は 502
    let none = app(FakePdfs::default(), None).await;
    assert_eq!(
        put_settings(&none, t, json!({"dept_codes": ["10"]}))
            .await
            .status,
        200
    );
    let bad = app(
        FakePdfs::default(),
        Some(ichiban_with(&[(EMPLOYEES_PATH, 500, json!({}))])),
    )
    .await;
    let broken = support::app_with_broken_db(Some(ledger_ichiban(&[]))).await;
    for (a, status, code) in [
        (&none, 503, "ichiban_not_configured"),
        (&bad, 502, "ichiban_upstream"),
        (&broken, 500, "internal_error"),
    ] {
        for (method, b) in [("GET", None), ("PUT", Some(body.clone()))] {
            let got = call(a, method, "/api/leave/opening", Some(t), ADMIN, b).await;
            assert_eq!(
                (got.status, got.json()),
                (status, json!({"error": code})),
                "{method} {code}"
            );
        }
    }
    for line in broken.logs.lock().unwrap().iter() {
        assert!(!line.contains(&t.to_string()), "{line}");
    }
}

/// 期首残を使う一番星 (期首日 2026-09-30 の翌日から・2026 年の月のカレンダー・期首日より前の月の区間も登録する)
fn opening_ichiban() -> FakeIchiban {
    let rows = ledger_rows();
    let mut f = ledger_ichiban(&rows);
    support::add_days(&mut f, "2026-10-01", "2026-10-10", &rows);
    support::add_days(&mut f, "2026-01-01", "2026-10-31", &rows);
    support::add_days(&mut f, "2024-04-01", "2026-08-31", &rows);
    f
}

#[tokio::test]
async fn grants_and_calendar_use_opening() {
    let t = tenant().await;
    let a = app(FakePdfs::default(), Some(opening_ichiban())).await;
    assert_eq!(
        put_settings(&a, t, json!({"dept_codes": ["10"]}))
            .await
            .status,
        200
    );
    call(
        &a,
        "POST",
        "/api/leave/grants/auto?employee_cd=1021",
        Some(t),
        ADMIN,
        None,
    )
    .await;
    // 期首日 2026-09-30: 繰越 2 日 (G0 = 2024-10-01、2026-10-01 に失効)・今年度 9 日 (G1 = 2025-10-01)
    let got = put_opening(
        &a,
        t,
        json!({"as_of": "2026-09-30", "rows": [{"employee_cd": "1021", "carryover": 2, "current": 9}]}),
    )
    .await;
    assert_eq!(got.status, 200);
    let get = |uri: &'static str| {
        let a = &a;
        async move {
            let got = call(a, "GET", uri, Some(t), ADMIN, None).await;
            assert_eq!(got.status, 200, "{uri}");
            got.json()
        }
    };
    // 今日 = 2026-10-10。期首日の翌日以降の消化 (10/1・10/2・10/5 午前 = 2.5 日) は今年度から (繰越は 10/1 に失効)。
    // 期首日以前の付与 (2024-10-01・2025-10-01 の auto) は置き換え、2026-10-01 の付与 (12 日) は従来どおり
    let v = get("/api/leave/grants?employee_cd=1021").await;
    assert_eq!(
        v["opening"],
        json!({"as_of": "2026-09-30", "carryover": 2.0, "current": 9.0,
               "carryover_grant_date": "2024-10-01", "carryover_expires_on": "2026-10-01",
               "current_grant_date": "2025-10-01", "current_expires_on": "2027-10-01"})
    );
    assert_eq!(
        v["balance"],
        json!({"as_of": "2026-10-10", "remaining": 18.5, "expired": 2.0, "unallocated": 0.0, "consumption_from": "2026-10-01"})
    );
    let rows: Vec<(String, bool, f64, f64, f64)> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            (
                g["grant_date"].as_str().unwrap().to_owned(),
                g["superseded"].as_bool().unwrap(),
                g["used"].as_f64().unwrap(),
                g["remaining"].as_f64().unwrap(),
                g["expired"].as_f64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            ("2024-10-01".into(), true, 0.0, 0.0, 0.0),
            ("2025-10-01".into(), true, 0.0, 0.0, 0.0),
            ("2026-10-01".into(), false, 0.0, 12.0, 0.0)
        ]
    );
    // カレンダーの有給残も同じ規則
    let v = get("/api/leave/calendar?employee_cd=1021&month=2026-10").await;
    assert_eq!(
        v["yukyu"],
        json!({"as_of": "2026-10-31", "remaining": 18.5, "expired": 2.0, "unallocated": 0.0, "consumption_from": "2026-10-01"})
    );
    // 期首日より前の月は従来の計算 (2024-10-01 の付与 10 日から 1.5 日、2025-10-01 の 11 日)
    let v = get("/api/leave/calendar?employee_cd=1021&month=2026-08").await;
    assert_eq!(
        v["yukyu"],
        json!({"as_of": "2026-08-31", "remaining": 19.5, "expired": 0.0, "unallocated": 0.0, "consumption_from": "2024-04-01"})
    );

    // 未来の期首日 (今日より後) は拒まない。今日の残は従来の計算で、置き換えも無い
    let got = put_opening(
        &a,
        t,
        json!({"as_of": "2026-10-20", "rows": [{"employee_cd": "1021", "carryover": 5, "current": 12}]}),
    )
    .await;
    assert_eq!(got.status, 200);
    let v = get("/api/leave/grants?employee_cd=1021").await;
    assert_eq!(
        (
            v["opening"]["carryover_grant_date"].as_str(),
            v["opening"]["current_grant_date"].as_str()
        ),
        (Some("2025-10-01"), Some("2026-10-01"))
    );
    assert_eq!(
        v["balance"],
        json!({"as_of": "2026-10-10", "remaining": 20.5, "expired": 8.5, "unallocated": 0.0, "consumption_from": "2024-04-01"})
    );
    assert!(v["data"]
        .as_array()
        .unwrap()
        .iter()
        .all(|g| g["superseded"] == false));
}

/// メール取り込みの口 `POST /internal/ingest` (管理 API の外。共有 secret で守る)
mod ingest {
    use super::*;
    use leave::ingest::{minimal_pdf, MAX_PAGES};
    use support::{ingest, INGEST_SECRET};

    const SENDER: &str = "Boss@Example.com";

    /// tenant を作り、mailbox と許可リストで受信を有効にする
    async fn enabled_tenant(allowed: &[&str]) -> (Uuid, String) {
        let t = tenant().await;
        let mailbox = format!("m-{}", &Uuid::new_v4().simple().to_string()[..12]);
        let a = app(FakePdfs::default(), None).await;
        let got = put_settings(
            &a,
            t,
            json!({"mailbox": mailbox, "allowed_senders": allowed, "enabled": true}),
        )
        .await;
        assert_eq!(got.status, 200);
        (t, mailbox)
    }

    fn body(mailbox: &str, from: &str, pdf: &[u8]) -> Vec<u8> {
        use base64::Engine;
        serde_json::to_vec(&json!({
            "mailbox": mailbox,
            "header_from": from,
            "envelope_from": from,
            "subject": "休暇申請",
            "message_id": "<m1@example.com>",
            "received_at": "2026-10-09T08:30:00Z",
            "filename": "leave.pdf",
            "pdf_base64": base64::engine::general_purpose::STANDARD.encode(pdf),
        }))
        .unwrap()
    }

    async fn page_count(t: Uuid) -> usize {
        let a = app(FakePdfs::default(), None).await;
        let got = call(&a, "GET", "/api/leave/pages", Some(t), ADMIN, None).await;
        assert_eq!(got.status, 200);
        got.json()["data"].as_array().unwrap().len()
    }

    #[tokio::test]
    async fn rejects_missing_or_wrong_secret() {
        let (t, mailbox) = enabled_tenant(&[SENDER]).await;
        let a = app(FakePdfs::default(), None).await;
        let b = body(&mailbox, SENDER, &minimal_pdf(1));
        for secret in [
            None,
            Some(""),
            Some("wrong"),
            Some("leave-test-ingest-secre"),
        ] {
            let got = ingest(&a, secret, Some(t), b.clone()).await;
            assert_eq!(got.status, 401, "secret={secret:?}");
            assert_eq!(got.json(), json!({"error": "unauthorized"}));
        }
        assert!(a.pdfs.stored.lock().unwrap().is_empty());
        assert_eq!(page_count(t).await, 0);
    }

    #[tokio::test]
    async fn stores_pages_for_the_mailbox_tenant_only() {
        let (t, mailbox) = enabled_tenant(&[SENDER]).await;
        let (other, _) = enabled_tenant(&[SENDER]).await;
        let a = app(FakePdfs::default(), None).await;
        // 他の tenant の X-Tenant-ID を付けても、body の mailbox の tenant に入る。mailbox は大文字でもよい
        let pdf = minimal_pdf(3);
        let b = body(&mailbox.to_uppercase(), SENDER, &pdf);
        let got = ingest(&a, Some(INGEST_SECRET), Some(other), b).await;
        assert_eq!(got.status, 200);
        assert_eq!(got.json(), json!({"pages": 3}));

        // R2 には tenant の下の key で 1 つだけ。中身は送った PDF
        let stored = a.pdfs.stored.lock().unwrap().clone();
        assert_eq!(stored.len(), 1);
        let (key, bytes) = stored.iter().next().unwrap();
        assert_eq!(bytes, &pdf);
        let prefix = format!("{t}/2026/10/");
        assert!(key.starts_with(&prefix) && key.ends_with(".pdf"), "{key}");

        // 自 tenant には 3 ページ (未紐づけ、page_no 1..3、同じ key)。他 tenant には見えない
        let a2 = app(FakePdfs::default(), None).await;
        let got = call(&a2, "GET", "/api/leave/pages", Some(t), ADMIN, None).await;
        let rows = got.json()["data"].clone();
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), 3);
        let mut nos: Vec<i64> = rows
            .iter()
            .map(|r| r["page_no"].as_i64().unwrap())
            .collect();
        nos.sort();
        assert_eq!(nos, vec![1, 2, 3]);
        for r in rows {
            assert_eq!(r["status"], "unlinked");
            assert_eq!(r["r2_key"], key.as_str());
            assert_eq!(r["mail_from"], SENDER);
            assert_eq!(r["subject"], "休暇申請");
            assert_eq!(r["received_at"], "2026-10-09T08:30:00Z");
        }
        assert_eq!(page_count(other).await, 0);
        let uri = format!("/api/leave/pages/{}", rows[0]["id"].as_str().unwrap());
        let got = call(&a2, "GET", &uri, Some(other), ADMIN, None).await;
        assert_eq!(got.status, 404);

        // ログは tenant と枚数だけ (差出人のアドレスも PDF も出ない)
        let logs = a.logs.lock().unwrap().clone();
        assert_eq!(logs, vec![format!("ingest stored: tenant={t} pages=3")]);
    }

    #[tokio::test]
    async fn drops_what_it_should_not_store() {
        let (t, mailbox) = enabled_tenant(&["boss@example.com"]).await;
        let (empty, empty_box) = enabled_tenant(&[]).await;
        let off = tenant().await;
        let off_box = format!("o-{}", &Uuid::new_v4().simple().to_string()[..12]);
        let a = app(FakePdfs::default(), None).await;
        let s = json!({"mailbox": off_box, "allowed_senders": [SENDER]});
        assert_eq!(put_settings(&a, off, s).await.status, 200);
        let pdf = minimal_pdf(1);
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (body("no-such-box", SENDER, &pdf), "unknown_mailbox"),
            (body(&off_box, SENDER, &pdf), "unknown_mailbox"),
            (
                body(&mailbox, "other@example.com", &pdf),
                "sender_not_allowed",
            ),
            (body(&empty_box, SENDER, &pdf), "sender_not_allowed"),
            // 許可されない送信元の PDF は parser に掛けない (壊れていても、ページ数の理由ではなく送信元で捨てる)
            (
                body(&mailbox, "other@example.com", b"%PDF-1.4\nbroken"),
                "sender_not_allowed",
            ),
            (body(&mailbox, SENDER, b"GIF89a"), "not_pdf"),
            (
                body(&mailbox, SENDER, b"%PDF-1.4\nbroken"),
                "unreadable_pdf",
            ),
            (body(&mailbox, SENDER, &minimal_pdf(0)), "unreadable_pdf"),
            (
                body(&mailbox, SENDER, &minimal_pdf(MAX_PAGES + 1)),
                "too_many_pages",
            ),
        ];
        for (b, reason) in cases {
            let got = ingest(&a, Some(INGEST_SECRET), None, b).await;
            assert_eq!(got.status, 202, "{reason}");
            assert_eq!(got.json(), json!({"dropped": reason}));
        }
        // 大文字小文字だけが違う送信元は通る
        let b = body(&mailbox, "BOSS@example.COM", &pdf);
        let got = ingest(&a, Some(INGEST_SECRET), None, b).await;
        assert_eq!(got.status, 200);
        assert_eq!(a.pdfs.stored.lock().unwrap().len(), 1);
        assert_eq!(page_count(t).await, 1);
        assert_eq!(page_count(empty).await, 0);
        assert_eq!(page_count(off).await, 0);
        let logs = a.logs.lock().unwrap().clone();
        let denied = format!("ingest dropped: sender_not_allowed tenant={t}");
        assert!(logs.contains(&denied), "{logs:?}");
        assert!(logs.iter().all(|l| !l.contains("example.com")), "{logs:?}");
    }

    #[tokio::test]
    async fn rejects_malformed_input_with_400() {
        let (t, mailbox) = enabled_tenant(&[SENDER]).await;
        let a = app(FakePdfs::default(), None).await;
        let ok = serde_json::from_slice::<Value>(&body(&mailbox, SENDER, b"x")).unwrap();
        let mut not_base64 = ok.clone();
        not_base64["pdf_base64"] = json!("***");
        let mut no_received_at = ok.clone();
        no_received_at
            .as_object_mut()
            .unwrap()
            .remove("received_at");
        let mut too_large = ok.clone();
        too_large["pdf_base64"] = json!("A".repeat(14_000_000));
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (b"not json".to_vec(), "invalid_body"),
            (serde_json::to_vec(&no_received_at).unwrap(), "invalid_body"),
            (body("ab", SENDER, &minimal_pdf(1)), "invalid_mailbox"),
            (body("a_b_c", SENDER, &minimal_pdf(1)), "invalid_mailbox"),
            (serde_json::to_vec(&not_base64).unwrap(), "invalid_base64"),
            (serde_json::to_vec(&too_large).unwrap(), "pdf_too_large"),
            // body の大きさの上限 (16 MiB) を超える
            (vec![b' '; 17 * 1024 * 1024], "invalid_body"),
        ];
        for (b, code) in cases {
            let got = ingest(&a, Some(INGEST_SECRET), None, b).await;
            assert_eq!(got.status, 400, "{code}");
            assert_eq!(got.json(), json!({"error": code}));
        }
        assert!(a.pdfs.stored.lock().unwrap().is_empty());
        assert_eq!(page_count(t).await, 0);
    }

    /// R2 に置いた後の INSERT が失敗したら、置いた object を消してから 500 (消すのも失敗しても 500)
    #[tokio::test]
    async fn failed_insert_removes_the_object() {
        for fail_delete in [false, true] {
            let (t, mailbox) = enabled_tenant(&[SENDER]).await;
            // put の途中で tenant を消す → leave_pages の INSERT が外部キーで失敗する
            let a = app(
                FakePdfs {
                    drop_tenant_on_put: Some(t),
                    fail_delete,
                    ..Default::default()
                },
                None,
            )
            .await;
            let b = body(&mailbox, SENDER, &minimal_pdf(2));
            let got = ingest(&a, Some(INGEST_SECRET), None, b).await;
            assert_eq!(got.status, 500);
            assert_eq!(got.json(), json!({"error": "internal_error"}));
            let deleted = a.pdfs.deleted.lock().unwrap().clone();
            assert_eq!(deleted.len(), 1);
            assert!(deleted[0].starts_with(&format!("{t}/")));
            // 消せたときは R2 に残らない (消せなかった回は残るが、それを log に出す)
            assert_eq!(
                a.pdfs.stored.lock().unwrap().len(),
                usize::from(fail_delete)
            );
            let logs = a.logs.lock().unwrap().clone();
            assert_eq!(logs[0], "ingest insert: 23503");
            assert_eq!(logs.len(), if fail_delete { 2 } else { 1 });
            assert!(logs.iter().all(|l| !l.contains(&t.to_string())), "{logs:?}");
        }
    }

    #[tokio::test]
    async fn store_and_db_errors_are_500() {
        let (t, mailbox) = enabled_tenant(&[SENDER]).await;
        let fail_put = FakePdfs {
            fail_put: true,
            ..Default::default()
        };
        let a = app(fail_put, None).await;
        let b = body(&mailbox, SENDER, &minimal_pdf(1));
        let got = ingest(&a, Some(INGEST_SECRET), None, b.clone()).await;
        assert_eq!(got.status, 500);
        assert_eq!(got.json(), json!({"error": "internal_error"}));
        assert_eq!(*a.logs.lock().unwrap(), vec!["ingest: r2 put failed"]);
        assert_eq!(page_count(t).await, 0);

        let a = support::app_with_broken_db(None).await;
        let got = ingest(&a, Some(INGEST_SECRET), None, b).await;
        assert_eq!(got.status, 500);
        assert_eq!(got.json(), json!({"error": "internal_error"}));
        assert_eq!(a.logs.lock().unwrap().len(), 1);
        assert!(a.pdfs.stored.lock().unwrap().is_empty());
    }
}
