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
        assert_forbidden("PUT", "/api/leave/settings", Some(json!({"grace_days": 3}))).await;
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
    async fn list_alerts() {
        assert_forbidden("GET", "/api/leave/alerts", None).await;
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
    async fn list_documents() {
        assert_forbidden("GET", "/api/leave/documents", None).await;
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
        json!({"dept_codes": [], "lineworks_channel_id": null,
               "notify_recipient_id": null, "notify_bot_config_id": null, "grace_days": 7, "enabled": false, "updated_at": null})
    );

    let got = put_settings(
        &a,
        t,
        json!({"dept_codes": ["10", "30"], "grace_days": 3, "enabled": true}),
    )
    .await;
    assert_eq!(got.status, 200);
    let v = got.json();
    assert_eq!(v["dept_codes"], json!(["10", "30"]));
    assert_eq!(v["grace_days"], 3);
    assert_eq!(v["enabled"], true);
    assert!(v["updated_at"].is_string());

    let got = call(&a, "GET", "/api/leave/settings", Some(t), ADMIN, None).await;
    assert_eq!(got.json(), v);

    // PUT は全体の置き換え (省いた項目は既定値に戻る)
    let got = put_settings(&a, t, json!({})).await;
    assert_eq!(got.json()["dept_codes"], json!([]));
    assert_eq!(got.json()["enabled"], false);
    assert_eq!(got.json()["grace_days"], 7);
}

#[tokio::test]
async fn settings_rejects_bad_input() {
    let t = tenant().await;
    let a = app(FakePdfs::default(), None).await;
    for (body, code) in [
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

/// `r2_key` の絞り (完全一致・並びは page_no, id・status と併用・長さ超過は固定の語の 400)
#[tokio::test]
async fn list_pages_filters_by_r2_key() {
    let (t, other) = (tenant().await, tenant().await);
    let key = format!("{t}/mail.pdf");
    // 他の key の方が新しい。同じ key のページは挿入順と page_no 順が逆
    let p3 = page(t, &key, 3, "2026-10-01T00:00:00Z").await;
    let p1 = page(t, &key, 1, "2026-10-01T00:00:00Z").await;
    let p2 = page(t, &key, 2, "2026-10-01T00:00:00Z").await;
    page(t, &format!("{t}/mail.pdf.bak"), 1, "2026-10-05T00:00:00Z").await;
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
    let get = |q: String| {
        let a = &a;
        async move {
            call(
                a,
                "GET",
                &format!("/api/leave/pages?{q}"),
                Some(t),
                ADMIN,
                None,
            )
            .await
        }
    };

    let got = get(format!("r2_key={key}")).await;
    assert_eq!(got.status, 200);
    assert_eq!(
        ids(got.json()),
        vec![p1.to_string(), p2.to_string(), p3.to_string()]
    );
    // 他の tenant の key は 0 件 (在ることも漏れない)
    let got = get(format!("r2_key={other}/x.pdf")).await;
    assert_eq!(got.status, 200);
    assert!(ids(got.json()).is_empty());
    // status と併用
    let body = json!({"status": "ignored"});
    let put = call(
        &a,
        "PUT",
        &format!("/api/leave/pages/{p2}"),
        Some(t),
        ADMIN,
        Some(body),
    )
    .await;
    assert_eq!(put.status, 200);
    let got = get(format!("r2_key={key}&status=ignored")).await;
    assert_eq!(ids(got.json()), vec![p2.to_string()]);
    let got = get(format!("r2_key={key}&limit=1&offset=1")).await;
    assert_eq!(ids(got.json()), vec![p2.to_string()]);

    // 長さ超過は固定の語。入力の値は応答に載らない
    let long = "k".repeat(leave::pages::MAX_R2_KEY_LEN + 1);
    let got = get(format!("r2_key={long}")).await;
    assert_eq!(got.status, 400);
    assert_eq!(got.json(), json!({"error": "invalid_r2_key"}));
    assert!(!String::from_utf8_lossy(&got.body).contains(&long));
}

/// PDF 単位の一覧: state 3 種・集約の値・tenant 分離・limit/offset・不正値の 400
#[tokio::test]
async fn list_documents_states_counts_and_isolation() {
    let (t, other) = (tenant().await, tenant().await);
    let (ka, kb, kc) = (
        format!("{t}/a.pdf"),
        format!("{t}/b.pdf"),
        format!("{t}/c.pdf"),
    );
    // A: 3 ページとも未紐づけ (挿入は page_no 順でない) / B: linked + ignored (全部済み) / C: linked + unlinked
    page(t, &ka, 2, "2026-10-01T00:00:00Z").await;
    page(t, &ka, 3, "2026-10-01T00:00:00Z").await;
    let a1 = page(t, &ka, 1, "2026-10-01T00:00:00Z").await;
    let b1 = page(t, &kb, 1, "2026-10-02T00:00:00Z").await;
    let b2 = page(t, &kb, 2, "2026-10-02T00:00:00Z").await;
    let c1 = page(t, &kc, 1, "2026-09-01T00:00:00Z").await;
    page(t, &kc, 2, "2026-09-01T00:00:00Z").await;
    // 他の tenant の PDF (未紐づけ) は一覧に出ない
    page(other, &format!("{other}/x.pdf"), 1, "2026-10-09T00:00:00Z").await;
    let app = app(FakePdfs::default(), None).await;
    let put = |id: Uuid, body: Value| {
        let app = &app;
        async move {
            let got = call(
                app,
                "PUT",
                &format!("/api/leave/pages/{id}"),
                Some(t),
                ADMIN,
                Some(body),
            )
            .await;
            assert_eq!(got.status, 200);
        }
    };
    let link = json!({"employee_cd": "1021", "status": "linked",
                      "periods": [{"kind": "yukyu", "start_date": "2026-10-02", "end_date": "2026-10-02"}]});
    put(b1, link.clone()).await;
    put(b2, json!({"status": "ignored"})).await;
    put(c1, link).await;

    let get = |q: &str| {
        let (app, q) = (&app, q.to_owned());
        async move {
            call(
                app,
                "GET",
                &format!("/api/leave/documents{q}"),
                Some(t),
                ADMIN,
                None,
            )
            .await
        }
    };
    let keys = |v: Value| -> Vec<String> {
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["r2_key"].as_str().unwrap().to_owned())
            .collect()
    };

    // 既定は open。受信の新しい順
    let got = get("").await;
    assert_eq!(got.status, 200);
    assert_eq!(keys(got.json()), vec![ka.clone(), kc.clone()]);
    let got = get("?state=done").await;
    assert_eq!(keys(got.json()), vec![kb.clone()]);
    let got = get("?state=all").await;
    assert_eq!(keys(got.json()), vec![kb.clone(), ka.clone(), kc.clone()]);

    // 集約の値
    let data = got.json()["data"].clone();
    assert_eq!(
        data[1],
        json!({"r2_key": ka, "first_page_id": a1, "received_at": "2026-10-01T00:00:00Z",
               "mail_from": "sender@example.com", "subject": "休暇申請",
               "page_count": 3, "unlinked_count": 3, "linked_count": 0, "ignored_count": 0})
    );
    assert_eq!(data[0]["first_page_id"], json!(b1));
    assert_eq!(
        [
            &data[0]["page_count"],
            &data[0]["unlinked_count"],
            &data[0]["linked_count"],
            &data[0]["ignored_count"]
        ],
        [&json!(2), &json!(0), &json!(1), &json!(1)]
    );
    assert_eq!(
        [
            &data[2]["page_count"],
            &data[2]["unlinked_count"],
            &data[2]["linked_count"]
        ],
        [&json!(2), &json!(1), &json!(1)]
    );

    // limit / offset (上限を超える指定は 400 でなく上限に寄る)
    let got = get("?state=all&limit=1&offset=1").await;
    assert_eq!(keys(got.json()), vec![ka.clone()]);
    let got = get("?state=all&limit=100000").await;
    assert_eq!(got.status, 200);
    assert_eq!(keys(got.json()).len(), 3);

    // 他の tenant からは自分の PDF だけ
    let got = call(
        &app,
        "GET",
        "/api/leave/documents?state=all",
        Some(other),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(keys(got.json()), vec![format!("{other}/x.pdf")]);

    for (q, code) in [
        ("?state=linked", "invalid_state"),
        ("?limit=0", "invalid_limit"),
        ("?offset=-1", "invalid_offset"),
        ("?state=open&state=done", "invalid_query"),
    ] {
        let got = get(q).await;
        assert_eq!(got.status, 400, "{q}");
        assert_eq!(got.json(), json!({"error": code}));
    }
}

/// staging の表示制限は PDF 単位の一覧にも掛かる (見えないページは数えず、全部見えない PDF は出ない)
#[tokio::test]
async fn list_documents_respects_visible_minutes() {
    let t = tenant().await;
    // NOW = 2026-10-10T00:00:00Z。10 分前ちょうどまでは見える
    let (kmix, kold) = (format!("{t}/mix.pdf"), format!("{t}/old.pdf"));
    page(t, &kmix, 1, "2026-10-09T23:50:00Z").await;
    page(t, &kmix, 2, "2026-10-09T23:49:59Z").await;
    page(t, &kold, 1, "2026-10-09T23:49:00Z").await;
    let staging = support::app_with_visible_minutes(FakePdfs::default(), "10").await;
    let got = call(
        &staging,
        "GET",
        "/api/leave/documents?state=all",
        Some(t),
        ADMIN,
        None,
    )
    .await;
    let v = got.json();
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"][0]["r2_key"], kmix);
    assert_eq!(v["data"][0]["page_count"], 1);

    let prod = app(FakePdfs::default(), None).await;
    let got = call(
        &prod,
        "GET",
        "/api/leave/documents?state=all",
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(got.json()["data"].as_array().unwrap().len(), 2);
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

/// 警告を 1 行入れる (superuser で。毎朝の突き合わせの代わり)
async fn alert_row(
    t: Uuid,
    emp: &str,
    day: &str,
    (kind, direction): (&str, &str),
    notified_at: Option<&str>,
    resolved: bool,
) {
    let day: chrono::NaiveDate = day.parse().unwrap();
    let notified_at: Option<chrono::DateTime<chrono::Utc>> =
        notified_at.map(|v| v.parse().unwrap());
    support::admin()
        .await
        .execute(
            "INSERT INTO alc_api.leave_alerts (tenant_id, employee_cd, day, kind, direction, first_seen_at, notified_at, resolved_at) \
             VALUES ($1, $2, $3, $4, $5, '2026-10-01T00:00:00Z', $6, CASE WHEN $7 THEN now() END)",
            &[&t, &emp, &day, &kind, &direction, &notified_at, &resolved],
        )
        .await
        .unwrap();
}

/// 未解消の警告の一覧: 未解決かつ窓 (今日 2026-10-10 (JST) − 62 日 〜 昨日) の中だけを乗務員・日の順で。
/// 解決済み・窓の外 (未解決のまま表に残る行)・他の tenant の行は出ない
#[tokio::test]
async fn list_alerts_open_in_window_only() {
    let (t, other) = (tenant().await, tenant().await);
    let a = app(FakePdfs::default(), None).await;
    let get = |t: Uuid| {
        let a = &a;
        async move { call(a, "GET", "/api/leave/alerts", Some(t), ADMIN, None).await }
    };
    // 1 件も無ければ空の配列
    let got = get(t).await;
    assert_eq!((got.status, got.json()), (200, json!({"alerts": []})));

    let ichiban_only = ("yukyu", "ichiban_only");
    alert_row(t, "1022", "2026-10-03", ichiban_only, None, false).await;
    alert_row(
        t,
        "1021",
        "2026-10-09",
        ("rousai", "pdf_only"),
        Some("2026-10-09T23:00:00Z"),
        false,
    )
    .await;
    alert_row(t, "1021", "2026-08-09", ichiban_only, None, false).await;
    // 窓の外 (前日・今日) と解決済み
    alert_row(t, "1021", "2026-08-08", ichiban_only, None, false).await;
    alert_row(t, "1021", "2026-10-10", ichiban_only, None, false).await;
    alert_row(
        t,
        "1021",
        "2026-10-01",
        ichiban_only,
        Some("2026-10-02T00:00:00Z"),
        true,
    )
    .await;
    // 他の tenant
    alert_row(other, "1021", "2026-10-03", ichiban_only, None, false).await;

    let got = get(t).await;
    assert_eq!(got.status, 200);
    assert_eq!(
        got.json(),
        json!({"alerts": [
            {"employee_cd": "1021", "day": "2026-08-09", "kind": "yukyu", "direction": "ichiban_only",
             "first_seen_at": "2026-10-01T00:00:00Z", "notified_at": null},
            {"employee_cd": "1021", "day": "2026-10-09", "kind": "rousai", "direction": "pdf_only",
             "first_seen_at": "2026-10-01T00:00:00Z", "notified_at": "2026-10-09T23:00:00Z"},
            {"employee_cd": "1022", "day": "2026-10-03", "kind": "yukyu", "direction": "ichiban_only",
             "first_seen_at": "2026-10-01T00:00:00Z", "notified_at": null}
        ]})
    );
    let got = get(other).await;
    assert_eq!(got.json()["alerts"].as_array().unwrap().len(), 1);
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
        ("GET", "/api/leave/alerts".to_owned(), None),
        ("GET", "/api/leave/employees".to_owned(), None),
        ("GET", "/api/leave/pages".to_owned(), None),
        ("GET", "/api/leave/documents".to_owned(), None),
        ("GET", format!("/api/leave/pages/{p}"), None),
        ("PUT", format!("/api/leave/pages/{p}"), Some(link)),
        ("GET", format!("/api/leave/pages/{p}/pdf"), None),
    ] {
        let got = call(&a, method, &uri, Some(t), ADMIN, body).await;
        assert_eq!(got.status, 500, "{method} {uri}");
        assert_eq!(got.json(), json!({"error": "internal_error"}));
    }
    let logs = a.logs.lock().unwrap();
    assert_eq!(logs.len(), 10);
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

/// staging の表示制限 (vars `STAGING_VISIBLE_MINUTES`): 受信から N 分を過ぎたページは一覧・詳細・更新・PDF で 404。
/// vars が無い (本番) なら全部見える
#[tokio::test]
async fn staging_hides_pages_older_than_visible_minutes() {
    let t = tenant().await;
    // NOW = 2026-10-10T00:00:00Z。10 分前ちょうどまでは見える
    let fresh_key = format!("{t}/fresh.pdf");
    let old_key = format!("{t}/old.pdf");
    let fresh = page(t, &fresh_key, 1, "2026-10-09T23:55:00Z").await;
    let edge = page(t, &format!("{t}/edge.pdf"), 1, "2026-10-09T23:50:00Z").await;
    let old = page(t, &old_key, 1, "2026-10-09T23:49:59Z").await;
    let pdfs = || FakePdfs {
        objects: HashMap::from([
            (fresh_key.clone(), b"%PDF-fresh".to_vec()),
            (old_key.clone(), b"%PDF-old".to_vec()),
        ]),
        ..Default::default()
    };
    let ids = |v: &Value| -> Vec<String> {
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap().to_owned())
            .collect()
    };

    let staging = support::app_with_visible_minutes(pdfs(), "10").await;
    let got = call(&staging, "GET", "/api/leave/pages", Some(t), ADMIN, None).await;
    assert_eq!(got.status, 200);
    assert_eq!(ids(&got.json()), vec![fresh.to_string(), edge.to_string()]);
    for (method, path, body) in [
        ("GET", format!("/api/leave/pages/{old}"), None),
        (
            "PUT",
            format!("/api/leave/pages/{old}"),
            Some(json!({"status": "ignored"})),
        ),
        ("GET", format!("/api/leave/pages/{old}/pdf"), None),
    ] {
        let got = call(&staging, method, &path, Some(t), ADMIN, body).await;
        assert_eq!(got.status, 404, "{method} {path}");
        assert_eq!(got.json(), json!({"error": "not_found"}));
    }
    assert!(staging.pdfs.reads.lock().unwrap().is_empty());
    let got = call(
        &staging,
        "GET",
        &format!("/api/leave/pages/{fresh}/pdf"),
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(got.body, b"%PDF-fresh");
    let path = format!("/api/leave/pages/{edge}");
    let got = call(&staging, "GET", &path, Some(t), ADMIN, None).await;
    assert_eq!(got.status, 200);
    let body = Some(json!({"status": "ignored"}));
    let got = call(&staging, "PUT", &path, Some(t), ADMIN, body).await;
    assert_eq!(got.status, 200);
    assert_eq!(got.json()["status"], "ignored");

    // 本番 (vars が無い) は全部見える
    let prod = app(pdfs(), None).await;
    let got = call(&prod, "GET", "/api/leave/pages", Some(t), ADMIN, None).await;
    assert_eq!(ids(&got.json()).len(), 3);
    let got = call(
        &prod,
        "GET",
        &format!("/api/leave/pages/{old}/pdf"),
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(got.body, b"%PDF-old");
}

/// スキャンの inbox の取り込み (Queue の consumer。worker の `#[event(queue)]` が呼ぶ `inbox::handle`)
mod inbox {
    use super::*;
    use std::sync::Mutex as StdMutex;

    use alc_worker_db::PgClient;
    use futures_util::future::BoxFuture;
    use leave::inbox::{
        handle_batch, InboxDeps, Outcome, TicketVerifier, VerifyError, TICKET_PURPOSE,
    };
    use leave::ingest::{minimal_pdf, sha256_hex, MAX_PAGES, MAX_PDF_BYTES};
    use leave::LogSink;
    use support::{app_client, FakeInbox, FakeVerifier};

    const KEY: &str = "0b7c-申請.pdf";
    const KEY_2: &str = "9d41-休暇.pdf";
    const TICKET: &str = "test-ticket";

    struct Run {
        pdfs: FakePdfs,
        inbox: FakeInbox,
        verifier: FakeVerifier,
        logs: Arc<StdMutex<Vec<String>>>,
        log: LogSink,
    }

    async fn setup(pdfs: FakePdfs, inbox: FakeInbox, verifier: FakeVerifier) -> Run {
        let logs = Arc::new(StdMutex::new(Vec::new()));
        let sink = logs.clone();
        Run {
            pdfs,
            inbox,
            verifier,
            logs,
            log: Arc::new(move |m: &str| sink.lock().unwrap().push(m.to_owned())),
        }
    }

    impl Run {
        async fn handle(&self, body: Value) -> Outcome {
            self.batch(vec![body]).await.remove(0)
        }

        async fn batch(&self, bodies: Vec<Value>) -> Vec<Outcome> {
            let deps = InboxDeps {
                pdfs: &self.pdfs,
                inbox: &self.inbox,
                verifier: &self.verifier,
                log: &self.log,
            };
            let connect = || async { Ok::<_, String>(app_client().await) };
            handle_batch(&deps, bodies, connect).await
        }

        fn logs(&self) -> Vec<String> {
            self.logs.lock().unwrap().clone()
        }

        fn deleted(&self) -> Vec<String> {
            self.inbox.deleted.lock().unwrap().clone()
        }
    }

    fn event(action: &str, size: Option<u64>) -> Value {
        let mut object = json!({ "key": KEY, "eTag": "e" });
        if let Some(s) = size {
            object["size"] = json!(s);
        }
        json!({
            "account": "acc",
            "action": action,
            "bucket": "leave-inbox",
            "object": object,
            "eventTime": "2026-10-09T08:30:00Z",
        })
    }

    fn put_event() -> Value {
        event("PutObject", Some(100))
    }

    fn ticket_for(t: Uuid, pdf: &[u8]) -> Result<Value, VerifyError> {
        Ok(json!({ "tenantId": t, "username": "scan", "sha256": sha256_hex(pdf) }))
    }

    async fn pages_of(t: Uuid) -> Vec<Value> {
        let a = app(FakePdfs::default(), None).await;
        let got = call(&a, "GET", "/api/leave/pages", Some(t), ADMIN, None).await;
        assert_eq!(got.status, 200);
        got.json()["data"].as_array().unwrap().clone()
    }

    /// チケットの tenant に入る (mail_from / subject は NULL)。再配送しても同じ key・同じ行で、二重にならない
    #[tokio::test]
    async fn stores_for_the_ticket_tenant_once() {
        let t = tenant().await;
        let other = tenant().await;
        let pdf = minimal_pdf(2);
        let r = setup(
            FakePdfs::default(),
            FakeInbox::with(KEY, pdf.clone(), Some(TICKET)),
            FakeVerifier::returning(ticket_for(t, &pdf)),
        )
        .await;
        assert_eq!(r.handle(put_event()).await, Outcome::Ack);
        let calls = r.verifier.calls.lock().unwrap().clone();
        assert_eq!(calls, vec![(TICKET.to_owned(), TICKET_PURPOSE.to_owned())]);
        assert_eq!(r.deleted(), vec![KEY]);
        let stored: Vec<String> = r.pdfs.stored.lock().unwrap().keys().cloned().collect();
        assert_eq!(stored.len(), 1);
        assert!(stored[0].starts_with(&format!("{t}/2026/10/")));
        assert!(!stored[0].contains("申請"));
        let pages = pages_of(t).await;
        assert_eq!(pages.len(), 2);
        for p in &pages {
            assert_eq!(p["mail_from"], Value::Null);
            assert_eq!(p["subject"], Value::Null);
            assert_eq!(p["r2_key"], stored[0].as_str());
            assert_eq!(p["received_at"], "2026-10-09T08:30:00Z");
        }
        assert!(pages_of(other).await.is_empty());
        assert_eq!(r.logs(), vec![format!("inbox stored: tenant={t} pages=2")]);

        // 2 回目の配送 (前回の削除の後に届いた): object が無いので ack
        assert_eq!(r.handle(put_event()).await, Outcome::Ack);
        assert_eq!(r.logs().last().unwrap(), "inbox: object gone");

        // 削除に失敗して再配送された: 同じ key に置き直し、行は増えない
        r.inbox
            .objects
            .lock()
            .unwrap()
            .insert(KEY.to_owned(), (pdf.clone(), Some(TICKET.to_owned())));
        assert_eq!(r.handle(put_event()).await, Outcome::Ack);
        assert_eq!(r.pdfs.stored.lock().unwrap().len(), 1);
        assert_eq!(pages_of(t).await.len(), 2);
    }

    /// 対象外の action・大きさ 0・読めない本文は何もせず ack (inbox の object には触らない)
    #[tokio::test]
    async fn ignores_other_events() {
        let r = setup(
            FakePdfs::default(),
            FakeInbox::default(),
            FakeVerifier::returning(Ok(Value::Null)),
        )
        .await;
        for body in [
            event("DeleteObject", Some(10)),
            event("CopyObject", Some(10)),
            event("PutObject", Some(0)),
        ] {
            assert_eq!(r.handle(body).await, Outcome::Ack);
        }
        assert_eq!(r.handle(json!({"action": "PutObject"})).await, Outcome::Ack);
        assert_eq!(r.handle(json!("text")).await, Outcome::Ack);
        assert_eq!(
            r.logs(),
            vec![
                "inbox: skipped",
                "inbox: skipped",
                "inbox: skipped",
                "inbox: invalid message",
                "inbox: invalid message"
            ]
        );
        assert!(r.deleted().is_empty());
        assert!(r.verifier.calls.lock().unwrap().is_empty());
    }

    /// 取り込めない object は消して ack (再試行しない)。DB にも R2 の本保存にも入らない
    #[tokio::test]
    async fn discards_what_it_cannot_store() {
        let t = tenant().await;
        let pdf = minimal_pdf(1);
        let not_pdf = b"GIF89a".to_vec();
        let broken = b"%PDF-1.4\nbroken".to_vec();
        let many = minimal_pdf(MAX_PAGES + 1);
        type Case<'a> = (
            Value,
            Vec<u8>,
            Option<&'a str>,
            Result<Value, VerifyError>,
            &'a str,
        );
        let cases: Vec<Case> = vec![
            (
                event("PutObject", Some(MAX_PDF_BYTES as u64 + 1)),
                pdf.clone(),
                Some(TICKET),
                ticket_for(t, &pdf),
                "pdf_too_large",
            ),
            (
                put_event(),
                pdf.clone(),
                None,
                ticket_for(t, &pdf),
                "ticket_missing",
            ),
            (
                put_event(),
                pdf.clone(),
                Some(TICKET),
                Ok(Value::Null),
                "ticket_invalid",
            ),
            (
                put_event(),
                pdf.clone(),
                Some(TICKET),
                Ok(json!({"tenantId": "not-a-uuid", "sha256": sha256_hex(&pdf)})),
                "ticket_invalid",
            ),
            (
                put_event(),
                pdf.clone(),
                Some(TICKET),
                ticket_for(t, b"another pdf"),
                "sha256_mismatch",
            ),
            (
                put_event(),
                not_pdf.clone(),
                Some(TICKET),
                ticket_for(t, &not_pdf),
                "not_pdf",
            ),
            (
                put_event(),
                broken.clone(),
                Some(TICKET),
                ticket_for(t, &broken),
                "unreadable_pdf",
            ),
            (
                put_event(),
                many.clone(),
                Some(TICKET),
                ticket_for(t, &many),
                "too_many_pages",
            ),
        ];
        for (body, bytes, ticket, verified, reason) in cases {
            let r = setup(
                FakePdfs::default(),
                FakeInbox::with(KEY, bytes, ticket),
                FakeVerifier::returning(verified),
            )
            .await;
            assert_eq!(r.handle(body).await, Outcome::Ack, "{reason}");
            assert_eq!(r.logs(), vec![format!("inbox dropped: {reason}")]);
            assert_eq!(r.deleted(), vec![KEY], "{reason}");
            assert!(r.pdfs.stored.lock().unwrap().is_empty(), "{reason}");
        }
        assert!(pages_of(t).await.is_empty());
    }

    /// 一時的な失敗 (inbox の読み出し・RPC・R2 の put・inbox の削除) は retry
    #[tokio::test]
    async fn temporary_failures_are_retried() {
        let t = tenant().await;
        let pdf = minimal_pdf(1);
        let inbox = || FakeInbox::with(KEY, pdf.clone(), Some(TICKET));

        let r = setup(
            FakePdfs::default(),
            FakeInbox {
                fail_get: true,
                ..inbox()
            },
            FakeVerifier::returning(ticket_for(t, &pdf)),
        )
        .await;
        assert_eq!(r.handle(put_event()).await, Outcome::Retry);
        assert_eq!(r.logs(), vec!["inbox: get"]);

        let r = setup(
            FakePdfs::default(),
            inbox(),
            FakeVerifier::returning(Err(VerifyError)),
        )
        .await;
        assert_eq!(r.handle(put_event()).await, Outcome::Retry);
        assert_eq!(r.logs(), vec!["inbox: verify failed"]);
        assert!(r.deleted().is_empty());

        let fail_put = FakePdfs {
            fail_put: true,
            ..Default::default()
        };
        let verifier = FakeVerifier::returning(ticket_for(t, &pdf));
        let r = setup(fail_put, inbox(), verifier).await;
        assert_eq!(r.handle(put_event()).await, Outcome::Retry);
        assert_eq!(r.logs(), vec!["ingest: r2 put failed"]);
        assert!(r.deleted().is_empty());

        // 取り込めたが inbox を消せなかった: retry (次の配送は同じ key・同じ行で冪等)
        let r = setup(
            FakePdfs::default(),
            FakeInbox {
                fail_delete: true,
                ..inbox()
            },
            FakeVerifier::returning(ticket_for(t, &pdf)),
        )
        .await;
        assert_eq!(r.handle(put_event()).await, Outcome::Retry);
        let stored = format!("inbox stored: tenant={t} pages=1");
        assert_eq!(r.logs(), vec![stored, "inbox: delete delete".to_owned()]);
        assert_eq!(pages_of(t).await.len(), 1);

        // 取り込めない object を消せなかった: retry
        let r = setup(
            FakePdfs::default(),
            FakeInbox {
                fail_delete: true,
                ..FakeInbox::with(KEY, pdf.clone(), None)
            },
            FakeVerifier::returning(ticket_for(t, &pdf)),
        )
        .await;
        assert_eq!(r.handle(put_event()).await, Outcome::Retry);
        assert_eq!(
            r.logs(),
            vec!["inbox dropped: ticket_missing", "inbox: delete delete"]
        );
    }

    /// auth-worker が `{error: "unavailable"}` (資格情報の DB が一時的に引けない) を返したら retry。
    /// チケット不正として消さない (object は inbox に残り、DB にも本保存にも入らない)
    #[tokio::test]
    async fn unavailable_verification_is_retried_and_keeps_the_object() {
        let t = tenant().await;
        let pdf = minimal_pdf(1);
        let r = setup(
            FakePdfs::default(),
            FakeInbox::with(KEY, pdf, Some(TICKET)),
            FakeVerifier::returning(Ok(json!({ "error": "unavailable" }))),
        )
        .await;
        assert_eq!(r.handle(put_event()).await, Outcome::Retry);
        assert_eq!(r.logs(), vec!["inbox: verify unavailable"]);
        assert!(r.deleted().is_empty());
        assert!(r.inbox.objects.lock().unwrap().contains_key(KEY));
        assert!(r.pdfs.stored.lock().unwrap().is_empty());
        assert!(pages_of(t).await.is_empty());
    }

    /// verify の呼び出しと connect の呼び出しの順を残す verifier (どの呼び出しも同じ戻り)
    struct OrderedVerifier {
        events: Arc<StdMutex<Vec<&'static str>>>,
        result: Value,
    }

    impl TicketVerifier for OrderedVerifier {
        fn verify<'a>(
            &'a self,
            _ticket: &'a str,
            _purpose: &'a str,
        ) -> BoxFuture<'a, Result<Value, VerifyError>> {
            Box::pin(async move {
                self.events.lock().unwrap().push("verify");
                Ok(self.result.clone())
            })
        }
    }

    fn event_for(key: &str) -> Value {
        let mut e = put_event();
        e["object"]["key"] = json!(key);
        e
    }

    /// batch の全メッセージの検証が終わるまで DB に繋がない (verify の間は接続が無い)。繋ぐのは 1 回で、使い回す
    #[tokio::test]
    async fn connects_once_after_every_ticket_is_verified() {
        let t = tenant().await;
        let pdf = minimal_pdf(1);
        let events = Arc::new(StdMutex::new(Vec::new()));
        let verifier = OrderedVerifier {
            events: events.clone(),
            result: ticket_for(t, &pdf).unwrap(),
        };
        let inbox = FakeInbox::with(KEY, pdf.clone(), Some(TICKET));
        inbox
            .objects
            .lock()
            .unwrap()
            .insert(KEY_2.to_owned(), (pdf.clone(), Some(TICKET.to_owned())));
        let pdfs = FakePdfs::default();
        let log: LogSink = Arc::new(|_: &str| {});
        let deps = InboxDeps {
            pdfs: &pdfs,
            inbox: &inbox,
            verifier: &verifier,
            log: &log,
        };
        let sink = events.clone();
        let connect = || async move {
            sink.lock().unwrap().push("connect");
            Ok::<_, String>(app_client().await)
        };
        let bodies = vec![event_for(KEY), event_for(KEY_2)];
        let got = handle_batch(&deps, bodies, connect).await;
        assert_eq!(got, vec![Outcome::Ack, Outcome::Ack]);
        assert_eq!(*events.lock().unwrap(), vec!["verify", "verify", "connect"]);
        assert_eq!(pdfs.stored.lock().unwrap().len(), 2);
        assert_eq!(pages_of(t).await.len(), 2);
    }

    /// 検証を通ったものが 1 件も無ければ DB に繋がない (対象外・不正・一時失敗だけの batch)
    #[tokio::test]
    async fn does_not_connect_without_a_verified_message() {
        let t = tenant().await;
        let pdf = minimal_pdf(1);
        let r = setup(
            FakePdfs::default(),
            FakeInbox::with(KEY, pdf.clone(), Some(TICKET)),
            FakeVerifier::returning(Ok(Value::Null)),
        )
        .await;
        let deps = InboxDeps {
            pdfs: &r.pdfs,
            inbox: &r.inbox,
            verifier: &r.verifier,
            log: &r.log,
        };
        let connected = Arc::new(StdMutex::new(0));
        let count = connected.clone();
        let connect = || async move {
            *count.lock().unwrap() += 1;
            Err::<PgClient, String>("must not be called".to_owned())
        };
        // 対象外の action・チケットが不正・object が無い
        let bodies = vec![
            event("DeleteObject", Some(10)),
            event_for(KEY),
            event_for(KEY_2),
        ];
        let got = handle_batch(&deps, bodies, connect).await;
        assert_eq!(got, vec![Outcome::Ack, Outcome::Ack, Outcome::Ack]);
        assert_eq!(*connected.lock().unwrap(), 0);
        assert_eq!(r.deleted(), vec![KEY]);
        assert!(pages_of(t).await.is_empty());
    }

    /// 繋がらなければ、検証を通ったものだけ retry (取り込めないものは今までどおり消して ack)。DB の文面は固定の語で残す
    #[tokio::test]
    async fn connect_failure_retries_only_the_verified() {
        let t = tenant().await;
        let pdf = minimal_pdf(1);
        let r = setup(
            FakePdfs::default(),
            FakeInbox::with(KEY, pdf.clone(), Some(TICKET)),
            FakeVerifier::returning(ticket_for(t, &pdf)),
        )
        .await;
        r.inbox
            .objects
            .lock()
            .unwrap()
            .insert(KEY_2.to_owned(), (pdf.clone(), None));
        let deps = InboxDeps {
            pdfs: &r.pdfs,
            inbox: &r.inbox,
            verifier: &r.verifier,
            log: &r.log,
        };
        let connect = || async { Err::<PgClient, String>("connect failed".to_owned()) };
        let bodies = vec![event_for(KEY), event_for(KEY_2)];
        let got = handle_batch(&deps, bodies, connect).await;
        assert_eq!(got, vec![Outcome::Retry, Outcome::Ack]);
        assert_eq!(
            r.logs(),
            vec!["inbox dropped: ticket_missing", "inbox: connect failed"]
        );
        assert_eq!(r.deleted(), vec![KEY_2]);
        assert!(r.pdfs.stored.lock().unwrap().is_empty());
        assert!(pages_of(t).await.is_empty());
    }

    /// INSERT が失敗しても、決定的な key の本保存の object は消さない (前の配送の行が指していることがある)
    #[tokio::test]
    async fn failed_insert_keeps_the_seeded_object() {
        let t = tenant().await;
        let pdf = minimal_pdf(1);
        let r = setup(
            FakePdfs {
                drop_tenant_on_put: Some(t),
                ..Default::default()
            },
            FakeInbox::with(KEY, pdf.clone(), Some(TICKET)),
            FakeVerifier::returning(ticket_for(t, &pdf)),
        )
        .await;
        assert_eq!(r.handle(put_event()).await, Outcome::Retry);
        assert_eq!(r.logs(), vec!["ingest insert: 23503"]);
        assert_eq!(r.pdfs.stored.lock().unwrap().len(), 1);
        assert!(r.deleted().is_empty());
    }
}
