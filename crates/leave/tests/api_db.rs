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

/// 口 23 本 (method ごと) すべてについて、2 層の判定 (crates/leave/src/auth.rs) を確かめる。
/// 通らない人は 403 で、DB にも置き場にも一番星にも届かない (admin 層は `admin_only`、leave.use 層は `forbidden`)。
/// 管理者と全権限 `*` は全部通り、権限 `leave.use` だけの人は leave.use 層だけ通る
mod forbidden {
    use super::*;
    use leave::ingest::minimal_pdf;
    use support::{call_as, CallBody};

    #[derive(Clone, Copy, Debug)]
    enum Layer {
        LeaveUse,
        Admin,
    }

    /// (`X-User-Role`, `X-User-Permissions`) の組
    type Who = (Option<&'static str>, Option<&'static str>);

    /// どちらの層も通らない (大文字・前後の付け足し・別の名前は権限にならない)
    const NOBODY: [Who; 9] = [
        (None, None),
        (Some("viewer"), None),
        (Some("payroll"), None),
        (Some("Admin"), None),
        (Some(""), None),
        (Some("viewer"), Some("")),
        (Some("viewer"), Some("leave.usex")),
        (Some("viewer"), Some("Leave.use")),
        (Some("viewer"), Some("admin")),
    ];

    /// 権限 `leave.use` だけを持つ (leave.use 層だけ通る)
    const LEAVE_USERS: [Who; 3] = [
        (Some("viewer"), Some("leave.use")),
        (Some("payroll"), Some("a leave.use b")),
        (None, Some("leave.use")),
    ];

    /// どちらの層も通る (管理者、または role に関わらず全権限 `*`)
    const ALL_LAYERS: [Who; 3] = [
        (Some("admin"), None),
        (Some("viewer"), Some("*")),
        (None, Some("*")),
    ];

    fn denied_and_allowed(layer: Layer) -> (Vec<Who>, &'static str, Vec<Who>) {
        match layer {
            Layer::LeaveUse => (
                NOBODY.to_vec(),
                "forbidden",
                [ALL_LAYERS, LEAVE_USERS].concat(),
            ),
            Layer::Admin => (
                [NOBODY.to_vec(), LEAVE_USERS.to_vec()].concat(),
                "admin_only",
                ALL_LAYERS.to_vec(),
            ),
        }
    }

    async fn assert_layer(method: &str, path: &str, body: CallBody, layer: Layer) {
        let t = tenant().await;
        let p = page(t, &format!("{t}/a.pdf"), 1, "2026-10-01T00:00:00Z").await;
        let path = path.replace("{id}", &p.to_string());
        let a = app(
            FakePdfs::default(),
            Some(ichiban_with(&[(DEPARTMENTS_PATH, 200, json!([]))])),
        )
        .await;
        let (denied, code, allowed) = denied_and_allowed(layer);
        for (role, perms) in denied {
            let got = call_as(&a, method, &path, Some(t), role, perms, body.clone()).await;
            assert_eq!(
                got.status, 403,
                "{method} {path} {layer:?} role={role:?} permissions={perms:?}"
            );
            assert_eq!(got.json(), json!({"error": code}));
        }
        assert!(a.pdfs.reads.lock().unwrap().is_empty());
        assert!(a.pdfs.stored.lock().unwrap().is_empty());
        assert!(a.ichiban.as_ref().unwrap().calls.lock().unwrap().is_empty());
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
        assert_eq!(detail.json()["base_date"], Value::Null);
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
        // 通る人は handler に届く (中身は各口のテストで見る。handler は 403 を返さない)
        for (role, perms) in allowed {
            let got = call_as(&a, method, &path, Some(t), role, perms, body.clone()).await;
            assert_ne!(
                got.status, 403,
                "{method} {path} {layer:?} role={role:?} permissions={perms:?}"
            );
        }
    }

    #[tokio::test]
    async fn get_settings() {
        assert_layer("GET", "/api/leave/settings", CallBody::Empty, Layer::Admin).await;
    }

    #[tokio::test]
    async fn put_settings() {
        let body = CallBody::Json(json!({"grace_days": 3}));
        assert_layer("PUT", "/api/leave/settings", body, Layer::Admin).await;
    }

    /// 管理者だけ。通らない人では送信の口にも届かない
    #[tokio::test]
    async fn test_notify() {
        let t = tenant().await;
        let r = notify_recipient(t, "lineworks", true).await;
        let n = Arc::new(FakeNotifier::default());
        let a = app_with_notifier(n.clone()).await;
        let got = super::put_settings(&a, t, json!({"notify_recipient_id": r})).await;
        assert_eq!(got.status, 200);
        let (denied, code, allowed) = denied_and_allowed(Layer::Admin);
        for (role, perms) in denied {
            let got = call_as(
                &a,
                "POST",
                "/api/leave/settings/test-notify",
                Some(t),
                role,
                perms,
                CallBody::Empty,
            )
            .await;
            assert_eq!(got.status, 403, "role={role:?} permissions={perms:?}");
            assert_eq!(got.json(), json!({"error": code}));
        }
        assert_eq!(*n.calls.lock().unwrap(), 0);
        for (role, perms) in allowed {
            let got = call_as(
                &a,
                "POST",
                "/api/leave/settings/test-notify",
                Some(t),
                role,
                perms,
                CallBody::Empty,
            )
            .await;
            assert_eq!(got.status, 200, "role={role:?} permissions={perms:?}");
        }
        assert_eq!(*n.calls.lock().unwrap(), ALL_LAYERS.len());
    }

    #[tokio::test]
    async fn list_alerts() {
        assert_layer("GET", "/api/leave/alerts", CallBody::Empty, Layer::LeaveUse).await;
    }

    /// PDF の取り込み。通らない人では置き場にも DB にも届かない
    #[tokio::test]
    async fn uploads() {
        let body = CallBody::Pdf(minimal_pdf(1));
        assert_layer("POST", "/api/leave/uploads", body, Layer::LeaveUse).await;
    }

    #[tokio::test]
    async fn departments() {
        assert_layer(
            "GET",
            "/api/leave/departments",
            CallBody::Empty,
            Layer::LeaveUse,
        )
        .await;
    }

    #[tokio::test]
    async fn employees() {
        assert_layer(
            "GET",
            "/api/leave/employees",
            CallBody::Empty,
            Layer::LeaveUse,
        )
        .await;
    }

    #[tokio::test]
    async fn list_pages() {
        assert_layer("GET", "/api/leave/pages", CallBody::Empty, Layer::LeaveUse).await;
    }

    #[tokio::test]
    async fn list_documents() {
        assert_layer(
            "GET",
            "/api/leave/documents",
            CallBody::Empty,
            Layer::LeaveUse,
        )
        .await;
    }

    #[tokio::test]
    async fn get_page() {
        assert_layer(
            "GET",
            "/api/leave/pages/{id}",
            CallBody::Empty,
            Layer::LeaveUse,
        )
        .await;
    }

    #[tokio::test]
    async fn put_page() {
        let body = CallBody::Json(json!({"status": "ignored"}));
        assert_layer("PUT", "/api/leave/pages/{id}", body, Layer::LeaveUse).await;
    }

    #[tokio::test]
    async fn get_pdf() {
        assert_layer(
            "GET",
            "/api/leave/pages/{id}/pdf",
            CallBody::Empty,
            Layer::LeaveUse,
        )
        .await;
    }

    #[tokio::test]
    async fn candidates() {
        assert_layer(
            "GET",
            "/api/leave/pages/{id}/candidates?employee_cd=1021",
            CallBody::Empty,
            Layer::LeaveUse,
        )
        .await;
    }

    /// 通らない人では基準日も変わらない
    #[tokio::test]
    async fn put_base_date() {
        let body = CallBody::Json(json!({"base_date": "2026-10-20"}));
        assert_layer(
            "PUT",
            "/api/leave/pages/{id}/base-date",
            body,
            Layer::LeaveUse,
        )
        .await;
    }

    #[tokio::test]
    async fn calendar() {
        assert_layer(
            "GET",
            "/api/leave/calendar?employee_cd=1021&month=2026-10",
            CallBody::Empty,
            Layer::LeaveUse,
        )
        .await;
    }

    #[tokio::test]
    async fn list_grants() {
        assert_layer(
            "GET",
            "/api/leave/grants?employee_cd=1021",
            CallBody::Empty,
            Layer::LeaveUse,
        )
        .await;
    }

    #[tokio::test]
    async fn create_grant() {
        let body =
            CallBody::Json(json!({"employee_cd": "1021", "grant_date": "2026-04-01", "days": 1}));
        assert_layer("POST", "/api/leave/grants", body, Layer::Admin).await;
    }

    #[tokio::test]
    async fn auto_grants() {
        assert_layer(
            "POST",
            "/api/leave/grants/auto?employee_cd=1021",
            CallBody::Empty,
            Layer::Admin,
        )
        .await;
    }

    #[tokio::test]
    async fn update_grant() {
        let body = CallBody::Json(json!({"days": 1, "expires_on": "2028-01-01"}));
        assert_layer("PUT", "/api/leave/grants/{id}", body, Layer::Admin).await;
    }

    #[tokio::test]
    async fn delete_grant() {
        assert_layer(
            "DELETE",
            "/api/leave/grants/{id}",
            CallBody::Empty,
            Layer::Admin,
        )
        .await;
    }

    #[tokio::test]
    async fn list_openings() {
        assert_layer(
            "GET",
            "/api/leave/opening",
            CallBody::Empty,
            Layer::LeaveUse,
        )
        .await;
    }

    #[tokio::test]
    async fn ledger() {
        assert_layer(
            "GET",
            "/api/leave/ledger?as_of=2026-04-01",
            CallBody::Empty,
            Layer::LeaveUse,
        )
        .await;
    }

    #[tokio::test]
    async fn put_openings() {
        let body = CallBody::Json(
            json!({"as_of": "2026-09-30", "rows": [{"employee_cd": "1021", "carryover": 1, "current": 2}]}),
        );
        assert_layer("PUT", "/api/leave/opening", body, Layer::Admin).await;
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
               "page_count": 3, "unlinked_count": 3, "linked_count": 0, "ignored_count": 0,
               "base_date": null})
    );
    assert_eq!(data[0]["first_page_id"], json!(b1));
    // 基準日は PDF 単位 (全ページに入れた後は集約にも出る。無い PDF は null のまま)
    assert_eq!(
        put_base(&app, t, &b1.to_string(), json!({"base_date": "2026-10-20"}))
            .await
            .status,
        200
    );
    let got = get("?state=all").await;
    let data = got.json()["data"].clone();
    assert_eq!(
        [
            &data[0]["base_date"],
            &data[1]["base_date"],
            &data[2]["base_date"]
        ],
        [&json!("2026-10-20"), &Value::Null, &Value::Null]
    );
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

/// 未解消の警告の一覧: 未解決かつ窓 (2026-08-01 〜 昨日 = 今日 2026-10-10 (JST) − 1 日) の中だけを乗務員・日の順で。
/// 応答に窓 (`from` / `to`) も載せる。
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
    assert_eq!(
        (got.status, got.json()),
        (
            200,
            json!({"alerts": [], "from": "2026-08-01", "to": "2026-10-09"})
        )
    );

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
    alert_row(t, "1021", "2026-07-31", ichiban_only, None, false).await;
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
        ], "from": "2026-08-01", "to": "2026-10-09"})
    );
    let got = get(other).await;
    assert_eq!(got.json()["alerts"].as_array().unwrap().len(), 1);
}

/// 紐づけて保存したら、その期間の日の未解決の `ichiban_only` を同じ PUT ですぐ閉じる (翌朝の突き合わせを待たない)。
/// 期間が覆わない日・別の区分・`pdf_only`・別 tenant の同じ鍵は残る。linked でない保存は閉じない。
/// 窓の外の日も鍵が合えば閉じる (毎朝の解決は窓の中だけだが、窓の外の行は一覧にも通知にも出ないので意図した差)
#[tokio::test]
async fn link_page_resolves_ichiban_only_alerts() {
    let (t, other) = (tenant().await, tenant().await);
    let p = page(t, &format!("{t}/a.pdf"), 1, "2026-10-01T00:00:00Z").await;
    let a = app(FakePdfs::default(), None).await;
    let uri = format!("/api/leave/pages/{p}");
    let yukyu = ("yukyu", "ichiban_only");
    alert_row(t, "1380", "2026-10-03", yukyu, None, false).await;
    alert_row(t, "1380", "2026-10-04", yukyu, None, false).await;
    alert_row(t, "1380", "2026-10-05", yukyu, None, false).await;
    alert_row(
        t,
        "1380",
        "2026-10-03",
        ("kekkin", "ichiban_only"),
        None,
        false,
    )
    .await;
    alert_row(t, "1380", "2026-10-03", ("yukyu", "pdf_only"), None, false).await;
    alert_row(t, "1381", "2026-10-03", yukyu, None, false).await;
    alert_row(t, "1380", "2026-07-20", yukyu, None, false).await;
    alert_row(other, "1380", "2026-10-03", yukyu, None, false).await;
    let open = |t: Uuid| {
        let a = &a;
        async move {
            let v = call(a, "GET", "/api/leave/alerts", Some(t), ADMIN, None)
                .await
                .json();
            v["alerts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| {
                    format!(
                        "{} {} {} {}",
                        x["employee_cd"], x["day"], x["kind"], x["direction"]
                    )
                    .replace('"', "")
                })
                .collect::<Vec<String>>()
        }
    };
    let before = open(t).await;
    assert_eq!(before.len(), 6);
    let resolved_july = || async {
        support::admin()
            .await
            .query_one(
                "SELECT resolved_at IS NOT NULL FROM alc_api.leave_alerts WHERE tenant_id = $1 AND day = '2026-07-20'",
                &[&t],
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    };

    // linked でない保存は閉じない
    let periods = json!([
        {"kind": "yukyu", "start_date": "2026-10-03", "end_date": "2026-10-03", "half": "pm"},
        {"kind": "yukyu", "start_date": "2026-10-04", "end_date": "2026-10-04"},
        {"kind": "yukyu", "start_date": "2026-07-20", "end_date": "2026-07-20"}
    ]);
    let body = json!({"employee_cd": "1380", "status": "unlinked", "periods": periods});
    let got = call(&a, "PUT", &uri, Some(t), ADMIN, Some(body)).await;
    assert_eq!(got.status, 200);
    assert_eq!(open(t).await, before);
    assert!(!resolved_july().await);

    let body = json!({"employee_cd": "1380", "status": "linked", "periods": periods});
    let got = call(&a, "PUT", &uri, Some(t), ADMIN, Some(body)).await;
    assert_eq!(got.status, 200);
    assert_eq!(
        open(t).await,
        vec![
            "1380 2026-10-03 kekkin ichiban_only",
            "1380 2026-10-03 yukyu pdf_only",
            "1380 2026-10-05 yukyu ichiban_only",
            "1381 2026-10-03 yukyu ichiban_only",
        ]
    );
    assert!(resolved_july().await);
    assert_eq!(
        open(other).await,
        vec!["1380 2026-10-03 yukyu ichiban_only"]
    );
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
        (
            "PUT",
            format!("/api/leave/pages/{p}/base-date"),
            Some(json!({"base_date": null})),
        ),
        (
            "GET",
            format!("/api/leave/pages/{p}/candidates?employee_cd=1"),
            None,
        ),
    ] {
        let got = call(&a, method, &uri, Some(t), ADMIN, body).await;
        assert_eq!(got.status, 500, "{method} {uri}");
        assert_eq!(got.json(), json!({"error": "internal_error"}));
    }
    let logs = a.logs.lock().unwrap();
    assert_eq!(logs.len(), 12);
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

/// カレンダーの申請書の側: 労災のページは、そのあと一番星の労災が続く日 (日曜・祝日は行が無くても続き、行の無い平日・土曜で
/// 止まる) も覆う。祝日が取れない年があればログに固定語を 1 行
#[tokio::test]
async fn calendar_extends_rousai_pages() {
    let t = tenant().await;
    let rows: Vec<Value> = [
        "2026-09-14",
        "2026-09-18",
        "2026-09-19",
        "2026-09-24",
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
    let mut f = ledger_ichiban(&rows);
    support::add_days(&mut f, "2024-04-01", "2026-09-30", &rows);
    let a = app(FakePdfs::default(), Some(f)).await;
    put_settings(&a, t, json!({"dept_codes": ["10"]})).await;
    let link =
        |periods: Value| json!({"employee_cd": "1021", "status": "linked", "periods": periods});
    let p1 = page(t, &format!("{t}/r1.pdf"), 1, "2026-09-14T00:00:00Z").await;
    let p2 = page(t, &format!("{t}/r2.pdf"), 1, "2026-09-30T00:00:00Z").await;
    // 前の年に終わった労災 (後ろに行が無い。2025 年の祝日は取れない)
    let p3 = page(t, &format!("{t}/r3.pdf"), 1, "2025-12-29T00:00:00Z").await;
    for (p, periods) in [
        (
            p1,
            json!([{"kind": "rousai", "start_date": "2026-09-14", "end_date": "2026-09-18"}]),
        ),
        (
            p2,
            json!([{"kind": "rousai", "start_date": "2026-09-30", "end_date": "2026-09-30"}]),
        ),
        (
            p3,
            json!([{"kind": "rousai", "start_date": "2025-12-29", "end_date": "2025-12-30"}]),
        ),
    ] {
        let uri = format!("/api/leave/pages/{p}");
        let got = call(&a, "PUT", &uri, Some(t), ADMIN, Some(link(periods))).await;
        assert_eq!(got.status, 200);
    }
    let get = |month: &'static str| {
        let a = &a;
        async move {
            let uri = format!("/api/leave/calendar?employee_cd=1021&month={month}");
            call(a, "GET", &uri, Some(t), ADMIN, None).await.json()
        }
    };
    let name = |id: &Value| match id.as_str() {
        Some(id) if id == p1.to_string() => "p1",
        Some(id) if id == p2.to_string() => "p2",
        _ => "-",
    };
    let pdf_days = |v: &Value| -> Vec<(String, String, String)> {
        v["days"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| {
                (
                    d["date"].as_str().unwrap().to_owned(),
                    name(&d["pdf"][0]["page_id"]).to_owned(),
                    d["status"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let x = |d: &str, p: &str, s: &str| (d.to_owned(), p.to_owned(), s.to_owned());
    // 9 月: 9/19 (土) に行、9/20 (日)・9/21〜23 (祝) は飛ばし、9/24 (木) まで p1 が覆い、9/25 (金) で止まる
    let v = get("2026-09").await;
    assert_eq!(
        pdf_days(&v),
        vec![
            x("2026-09-14", "p1", "match"),
            x("2026-09-15", "p1", "pdf_only"),
            x("2026-09-16", "p1", "pdf_only"),
            x("2026-09-17", "p1", "pdf_only"),
            x("2026-09-18", "p1", "match"),
            x("2026-09-19", "p1", "match"),
            x("2026-09-24", "p1", "match"),
            x("2026-09-30", "p2", "match"),
        ]
    );
    // 10 月: p2 が月をまたいで 10/1〜10/3 (土)・10/5 (月、10/4 (日) は飛ばす) を覆い、10/6 (火) で止まる。10/7 は一番星だけ
    let v = get("2026-10").await;
    assert_eq!(
        pdf_days(&v),
        vec![
            x("2026-10-01", "p2", "match"),
            x("2026-10-02", "p2", "match"),
            x("2026-10-03", "p2", "match"),
            x("2026-10-05", "p2", "match"),
            x("2026-10-07", "-", "ichiban_only"),
        ]
    );
    assert_eq!(
        v["days"][0]["pdf"],
        json!([{"kind": "rousai", "half": null, "page_id": p2.to_string()}])
    );
    // 2025 年の祝日が取れなかった (固定語だけ。カレンダー 1 回に 1 行)
    let logs = a.logs.lock().unwrap();
    assert_eq!(*logs, vec!["holidays_unavailable"; 2]);
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
        ("GET", "/api/leave/ledger".to_owned(), None),
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

// ---- 全員分の有給の年度の一覧 (GET /api/leave/ledger) ----

/// 全員分の一覧が取る一番星の休暇の窓 (基準日ごとに、全員の消化の最初の日と年度の初日の早い方〜基準日)
const LEDGER_WINDOWS: [(&str, &str); 3] = [
    // 今日 (2026-10-10): 1030 (入社日なし) の 48 か月前の翌日から
    ("2022-10-11", "2026-10-10"),
    ("2022-04-01", "2026-03-31"),
    ("2022-10-01", "2026-09-30"),
];

fn year_ledger_ichiban() -> FakeIchiban {
    let mut rows = ledger_rows();
    rows.push(day("1030", "2026-05-10", "有休"));
    rows.push(day("1030", "2025-05-10", "午後休"));
    let mut f = ichiban_with(&[(EMPLOYEES_PATH, 200, ledger_employees())]);
    for (from, to) in LEDGER_WINDOWS {
        support::add_days(&mut f, from, to, &rows);
    }
    // 付与台帳の口 (1021 の 1 人分) の窓 (期首残の前と後)
    support::add_days(&mut f, "2024-04-01", "2026-10-10", &rows);
    support::add_days(&mut f, "2026-10-01", "2026-10-10", &rows);
    f
}

type LedgerLine = (String, Value, f64, f64, f64, f64, f64);

/// 一覧の行を (乗務員, 最新の付与, 年度の付与, 年度の取得, 残, 失効, 未割当) に
fn ledger_view(v: &Value) -> Vec<LedgerLine> {
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let n = |k: &str| r[k].as_f64().unwrap();
            (
                r["employee_cd"].as_str().unwrap().to_owned(),
                r["latest_grant"].clone(),
                n("granted_in_fy"),
                n("used_in_fy"),
                n("remaining"),
                n("expired"),
                n("unallocated"),
            )
        })
        .collect()
}

#[tokio::test]
async fn ledger_lists_everyone_by_fiscal_year() {
    let t = tenant().await;
    let a = app(FakePdfs::default(), Some(year_ledger_ichiban())).await;
    assert_eq!(
        put_settings(&a, t, json!({"dept_codes": ["10"]}))
            .await
            .status,
        200
    );
    // 1021: 法定付与 2024-10-01 (10)・2025-10-01 (11)・2026-10-01 (12)。1030: 手で 2025-06-01 (3)・2026-04-01 (5)
    call(
        &a,
        "POST",
        "/api/leave/grants/auto?employee_cd=1021",
        Some(t),
        ADMIN,
        None,
    )
    .await;
    for (date, days) in [("2025-06-01", 3), ("2026-04-01", 5)] {
        let body = json!({"employee_cd": "1030", "grant_date": date, "days": days});
        let got = call(&a, "POST", "/api/leave/grants", Some(t), ADMIN, Some(body)).await;
        assert_eq!(got.status, 201);
    }
    // 他 tenant の付与は混ざらない
    let other = tenant().await;
    put_settings(&a, other, json!({"dept_codes": ["10"]})).await;
    let body = json!({"employee_cd": "1021", "grant_date": "2026-04-01", "days": 7});
    let got = call(
        &a,
        "POST",
        "/api/leave/grants",
        Some(other),
        ADMIN,
        Some(body),
    )
    .await;
    assert_eq!(got.status, 201);

    let get = |uri: &'static str| {
        let a = &a;
        async move {
            let got = call(a, "GET", uri, Some(t), ADMIN, None).await;
            assert_eq!(got.status, 200, "{uri}");
            got.json()
        }
    };
    let calls = || a.ichiban.as_ref().unwrap().calls.lock().unwrap().len();

    // 基準日の省略は今日 (2026-10-10) → 2026 年度。一番星は社員 1 回 + 休暇の窓 1 つ (92 日ずつ 16 回) だけ
    let before = calls();
    let v = get("/api/leave/ledger").await;
    assert_eq!(calls() - before, 1 + 16);
    assert_eq!(
        (v["as_of"].clone(), v["fiscal_year"].clone()),
        (
            json!("2026-10-10"),
            json!({"start": "2026-04-01", "end": "2027-03-31", "label": 2026})
        )
    );
    // 1021 は付与台帳の口 (1 人分) と同じ残 (20.5・失効 8.5)。部門の外の 2001 は出ない
    assert_eq!(
        ledger_view(&v),
        vec![
            (
                "1021".into(),
                json!({"grant_date": "2026-10-01", "days": 12.0}),
                12.0,
                2.5,
                20.5,
                8.5,
                0.0
            ),
            (
                "1030".into(),
                json!({"grant_date": "2026-04-01", "days": 5.0}),
                5.0,
                1.0,
                7.0,
                0.0,
                0.5
            ),
        ]
    );
    assert_eq!(
        (
            v["data"][0]["employee_name"].as_str(),
            v["data"][0]["hire_date"].as_str(),
            v["data"][1]["hire_date"].clone()
        ),
        (Some("山田太郎"), Some("2024-04-01"), Value::Null)
    );
    let one = get("/api/leave/grants?employee_cd=1021").await;
    assert_eq!(
        (
            one["balance"]["remaining"].as_f64(),
            one["balance"]["expired"].as_f64()
        ),
        (Some(20.5), Some(8.5))
    );

    // 去年度の末日 → 2025 年度。2026-10-01 の付与はまだ無く、2025-01-10 の取得は 2024 年度。
    // 1030 の 2025-05-10 の半日は付与 (2025-06-01) より前なので未割当
    let v = get("/api/leave/ledger?as_of=2026-03-31").await;
    assert_eq!(
        v["fiscal_year"],
        json!({"start": "2025-04-01", "end": "2026-03-31", "label": 2025})
    );
    assert_eq!(
        ledger_view(&v),
        vec![
            (
                "1021".into(),
                json!({"grant_date": "2025-10-01", "days": 11.0}),
                11.0,
                0.5,
                19.5,
                0.0,
                0.0
            ),
            (
                "1030".into(),
                json!({"grant_date": "2025-06-01", "days": 3.0}),
                3.0,
                0.5,
                3.0,
                0.0,
                0.5
            ),
        ]
    );

    // 期首残 (2026-09-30: 繰越 2・今年度 9) を入れると、期首日以前の付与は置き換わる
    let got = put_opening(
        &a,
        t,
        json!({"as_of": "2026-09-30", "rows": [{"employee_cd": "1021", "carryover": 2, "current": 9}]}),
    )
    .await;
    assert_eq!(got.status, 200);
    // 今日: 付与台帳の口と同じ 18.5・失効 2 (年度の付与・取得は付与台帳と一番星のまま)
    let v = get("/api/leave/ledger?as_of=2026-10-10").await;
    assert_eq!(
        ledger_view(&v)[0],
        (
            "1021".into(),
            json!({"grant_date": "2026-10-01", "days": 12.0}),
            12.0,
            2.5,
            18.5,
            2.0,
            0.0
        )
    );
    let one = get("/api/leave/grants?employee_cd=1021").await;
    assert_eq!(
        (
            one["balance"]["remaining"].as_f64(),
            one["balance"]["expired"].as_f64()
        ),
        (Some(18.5), Some(2.0))
    );
    // 期首日: 基準日以前の付与は全部置き換わったので最新の付与は無く、残は期首残そのもの
    let v = get("/api/leave/ledger?as_of=2026-09-30").await;
    assert_eq!(
        ledger_view(&v)[0],
        ("1021".into(), Value::Null, 0.0, 0.0, 11.0, 0.0, 0.0)
    );
}

#[tokio::test]
async fn ledger_rejects_bad_as_of_and_needs_ichiban() {
    let t = tenant().await;
    let a = app(FakePdfs::default(), Some(year_ledger_ichiban())).await;
    // 部門を選んでいない tenant は空で、一番星も呼ばない
    let got = call(
        &a,
        "GET",
        "/api/leave/ledger?as_of=2026-03-31",
        Some(t),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(
        (got.status, got.json()),
        (
            200,
            json!({"as_of": "2026-03-31", "data": [],
                   "fiscal_year": {"start": "2025-04-01", "end": "2026-03-31", "label": 2025}})
        )
    );
    // 日付でない・今日の 1 年後 (2027-10-10) より後は 400
    for (q, code) in [
        ("as_of=2026-13-01", "invalid_as_of"),
        ("as_of=2026/10/10", "invalid_as_of"),
        ("as_of=", "invalid_as_of"),
        ("as_of=2027-10-11", "invalid_as_of"),
        ("as_of=1&as_of=2", "invalid_query"),
    ] {
        let got = call(
            &a,
            "GET",
            &format!("/api/leave/ledger?{q}"),
            Some(t),
            ADMIN,
            None,
        )
        .await;
        assert_eq!(
            (got.status, got.json()),
            (400, json!({"error": code})),
            "{q}"
        );
    }
    assert!(a.ichiban.as_ref().unwrap().calls.lock().unwrap().is_empty());
    // 一番星の binding が無い環境 (staging) では 503
    let none = app(FakePdfs::default(), None).await;
    let got = call(&none, "GET", "/api/leave/ledger", Some(t), ADMIN, None).await;
    assert_eq!(
        (got.status, got.json()),
        (503, json!({"error": "ichiban_not_configured"}))
    );
    // 一番星の失敗は 502 (社員の口・休暇の口)
    for (fake, code) in [
        (
            ichiban_with(&[(EMPLOYEES_PATH, 500, json!({}))]),
            "ichiban_upstream",
        ),
        (
            ichiban_with(&[(EMPLOYEES_PATH, 200, ledger_employees())]),
            "ichiban_unavailable",
        ),
    ] {
        let b = app(FakePdfs::default(), Some(fake)).await;
        put_settings(&b, t, json!({"dept_codes": ["10"]})).await;
        let got = call(&b, "GET", "/api/leave/ledger", Some(t), ADMIN, None).await;
        assert_eq!((got.status, got.json()), (502, json!({"error": code})));
        for line in b.logs.lock().unwrap().iter() {
            assert!(
                !line.contains(&t.to_string()) && !line.contains("山田"),
                "{line}"
            );
        }
    }
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

/// スキャンの inbox の取り込み (sweep と、それが通す `inbox::handle_batch`)
mod inbox {
    use super::*;
    use std::sync::Mutex as StdMutex;

    use alc_worker_db::PgClient;
    use futures_util::future::BoxFuture;
    use leave::inbox::{
        handle_batch, sweep, InboxDeps, Outcome, TicketVerifier, VerifyError, SWEEP_BATCH,
        SWEEP_LIST_LIMIT, TICKET_PURPOSE,
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

    /// チケットの tenant に入る (mail_from / subject は NULL)。sweep で 2 回拾っても同じ key・同じ行で、二重にならない
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

        // 削除に失敗して次の sweep でまた拾われた: 同じ key に置き直し、行は増えない
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

        // 取り込めたが inbox を消せなかった: retry (次の sweep は同じ key・同じ行で冪等)
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

    impl Run {
        async fn sweep(&self) -> Vec<Outcome> {
            let deps = InboxDeps {
                pdfs: &self.pdfs,
                inbox: &self.inbox,
                verifier: &self.verifier,
                log: &self.log,
            };
            let connect = || async { Ok::<_, String>(app_client().await) };
            sweep(&deps, SWEEP_BATCH, connect).await
        }

        /// inbox に置く (置かれた時刻つき)
        fn put(&self, key: &str, bytes: Vec<u8>, uploaded: &str) {
            self.inbox
                .objects
                .lock()
                .unwrap()
                .insert(key.to_owned(), (bytes, Some(TICKET.to_owned())));
            self.inbox
                .uploaded
                .lock()
                .unwrap()
                .insert(key.to_owned(), uploaded.parse().unwrap());
        }

        fn remaining(&self) -> Vec<String> {
            let mut keys: Vec<String> =
                self.inbox.objects.lock().unwrap().keys().cloned().collect();
            keys.sort();
            keys
        }
    }

    async fn documents_of(t: Uuid) -> Vec<Value> {
        let a = app(FakePdfs::default(), None).await;
        let got = call(
            &a,
            "GET",
            "/api/leave/documents?state=all",
            Some(t),
            ADMIN,
            None,
        )
        .await;
        assert_eq!(got.status, 200);
        got.json()["data"].as_array().unwrap().clone()
    }

    /// 一覧を開いたときの取り込み: list から通知と同じ形で通し、取り込んだものは inbox から消える。
    /// 置かれた時刻の新しい順に 10 件で打ち切り (残りは次の回)、大きさ 0 は除く。received_at は置かれた時刻
    #[tokio::test]
    async fn sweep_takes_the_newest_ten_and_leaves_the_rest() {
        let t = tenant().await;
        let pdf = minimal_pdf(1);
        let r = setup(
            FakePdfs::default(),
            FakeInbox::default(),
            FakeVerifier::returning(ticket_for(t, &pdf)),
        )
        .await;
        // key の順と置かれた時刻の順を逆にする (key の先頭が古い)
        for i in 0..12 {
            r.put(
                &format!("{i:02}-scan.pdf"),
                pdf.clone(),
                &format!("2026-10-16T01:{i:02}:00Z"),
            );
        }
        r.put("99-empty.pdf", Vec::new(), "2026-10-16T02:00:00Z");
        assert_eq!(r.sweep().await, vec![Outcome::Ack; 10]);
        assert_eq!(*r.inbox.list_limits.lock().unwrap(), vec![SWEEP_LIST_LIMIT]);
        assert_eq!(
            r.remaining(),
            vec!["00-scan.pdf", "01-scan.pdf", "99-empty.pdf"]
        );
        assert_eq!(r.logs().last().unwrap(), "inbox sweep: done=10 retry=0");
        let docs = documents_of(t).await;
        assert_eq!(docs.len(), 10);
        assert_eq!(docs[0]["received_at"], "2026-10-16T01:11:00Z");
        assert_eq!(docs[9]["received_at"], "2026-10-16T01:02:00Z");

        // 次の回に残りを取り込む (大きさ 0 は残ったまま)
        assert_eq!(r.sweep().await, vec![Outcome::Ack; 2]);
        assert_eq!(r.remaining(), vec!["99-empty.pdf"]);
        assert_eq!(documents_of(t).await.len(), 12);
        assert_eq!(r.logs().last().unwrap(), "inbox sweep: done=2 retry=0");
    }

    /// 会社はチケットの tenant で決まる (開いた人の会社とは無関係)。他社の PDF は自社の一覧に出ない
    #[tokio::test]
    async fn sweep_stores_for_the_ticket_tenant_only() {
        let (t, other) = (tenant().await, tenant().await);
        let pdf = minimal_pdf(2);
        let r = setup(
            FakePdfs::default(),
            FakeInbox::default(),
            FakeVerifier::returning(ticket_for(other, &pdf)),
        )
        .await;
        r.put(KEY, pdf, "2026-10-16T01:06:00Z");
        assert_eq!(r.sweep().await, vec![Outcome::Ack]);
        assert!(documents_of(t).await.is_empty());
        let docs = documents_of(other).await;
        assert_eq!(docs.len(), 1);
        assert_eq!(pages_of(other).await.len(), 2);
    }

    /// 同じ object を 2 回 (続けて・同時に) 拾っても 1 件 (本保存の key は inbox の key から決まり、pages は ON CONFLICT)
    #[tokio::test]
    async fn sweeping_the_same_object_twice_stores_it_once() {
        let t = tenant().await;
        let pdf = minimal_pdf(2);
        let r = setup(
            FakePdfs::default(),
            FakeInbox::default(),
            FakeVerifier::returning(ticket_for(t, &pdf)),
        )
        .await;
        // 同時: 両方が inbox から読んでから、別々の接続で INSERT する
        r.put(KEY, pdf.clone(), "2026-10-16T01:06:00Z");
        let (a, b) = tokio::join!(r.sweep(), r.sweep());
        assert_eq!((a, b), (vec![Outcome::Ack], vec![Outcome::Ack]));
        assert!(r.remaining().is_empty());
        assert_eq!(r.pdfs.stored.lock().unwrap().len(), 1);
        assert_eq!(documents_of(t).await.len(), 1);
        assert_eq!(pages_of(t).await.len(), 2);

        // 続けて: 消し損ねて残ったものをもう一度拾う
        r.put(KEY, pdf, "2026-10-16T01:06:00Z");
        assert_eq!(r.sweep().await, vec![Outcome::Ack]);
        assert_eq!(r.sweep().await, Vec::<Outcome>::new());
        assert_eq!(documents_of(t).await.len(), 1);
        assert_eq!(pages_of(t).await.len(), 2);
        assert_eq!(r.logs().last().unwrap(), "inbox sweep: done=0 retry=0");
    }

    /// 一時的な失敗は inbox に残して次の回にやり直す。list の失敗はログだけで、DB に繋がない
    #[tokio::test]
    async fn sweep_keeps_retried_objects_and_survives_list_failure() {
        let t = tenant().await;
        let pdf = minimal_pdf(1);
        let r = setup(
            FakePdfs::default(),
            FakeInbox::default(),
            FakeVerifier::returning(Err(VerifyError)),
        )
        .await;
        r.put(KEY, pdf, "2026-10-16T01:06:00Z");
        assert_eq!(r.sweep().await, vec![Outcome::Retry]);
        assert_eq!(r.remaining(), vec![KEY]);
        assert_eq!(
            r.logs(),
            vec!["inbox: verify failed", "inbox sweep: done=0 retry=1"]
        );
        assert!(pages_of(t).await.is_empty());

        let r = setup(
            FakePdfs::default(),
            FakeInbox {
                fail_list: true,
                ..Default::default()
            },
            FakeVerifier::returning(Ok(Value::Null)),
        )
        .await;
        let deps = InboxDeps {
            pdfs: &r.pdfs,
            inbox: &r.inbox,
            verifier: &r.verifier,
            log: &r.log,
        };
        let connect = || async { Err::<PgClient, String>("must not be called".to_owned()) };
        assert!(sweep(&deps, SWEEP_BATCH, connect).await.is_empty());
        assert_eq!(r.logs(), vec!["inbox sweep: list"]);
    }
}

/// 管理画面からの取り込みの口 `POST /api/leave/uploads` (本文 = PDF そのもの)
mod uploads {
    use super::*;
    use leave::ingest::{minimal_pdf, MAX_PAGES, MAX_PDF_BYTES};
    use support::call_bytes;

    const PATH: &str = "/api/leave/uploads";

    async fn pages_of(t: Uuid) -> Vec<Value> {
        let a = app(FakePdfs::default(), None).await;
        let got = call(&a, "GET", "/api/leave/pages", Some(t), ADMIN, None).await;
        assert_eq!(got.status, 200);
        got.json()["data"].as_array().unwrap().clone()
    }

    fn big(len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        v[..5].copy_from_slice(b"%PDF-");
        v
    }

    /// 自 tenant に全ページが入り (mail_from / subject は NULL)、置き場に 1 件
    #[tokio::test]
    async fn stores_the_pdf_for_the_header_tenant() {
        let t = tenant().await;
        let other = tenant().await;
        let a = app(FakePdfs::default(), None).await;
        let got = call_bytes(&a, "POST", PATH, Some(t), ADMIN, minimal_pdf(3)).await;
        assert_eq!(got.status, 201);
        assert_eq!(got.json(), json!({"pages": 3}));
        let stored: Vec<String> = a.pdfs.stored.lock().unwrap().keys().cloned().collect();
        assert_eq!(stored.len(), 1);
        assert!(stored[0].starts_with(&format!("{t}/")));
        let pages = pages_of(t).await;
        assert_eq!(pages.len(), 3);
        for p in &pages {
            assert_eq!(p["mail_from"], Value::Null);
            assert_eq!(p["subject"], Value::Null);
            assert_eq!(p["r2_key"], stored[0].as_str());
        }
        assert!(pages_of(other).await.is_empty());
    }

    /// 同じファイルを 2 回上げたら 2 件 (別 key・別の行)
    #[tokio::test]
    async fn the_same_body_twice_is_stored_twice() {
        let t = tenant().await;
        let a = app(FakePdfs::default(), None).await;
        for _ in 0..2 {
            let got = call_bytes(&a, "POST", PATH, Some(t), ADMIN, minimal_pdf(1)).await;
            assert_eq!(got.status, 201);
        }
        assert_eq!(a.pdfs.stored.lock().unwrap().len(), 2);
        assert_eq!(pages_of(t).await.len(), 2);
    }

    #[tokio::test]
    async fn rejects_a_body_that_is_not_a_pdf() {
        let t = tenant().await;
        let a = app(FakePdfs::default(), None).await;
        let got = call_bytes(&a, "POST", PATH, Some(t), ADMIN, b"GIF89a".to_vec()).await;
        assert_eq!(got.status, 400);
        assert_eq!(got.json(), json!({"error": "not_pdf"}));
        assert!(a.pdfs.stored.lock().unwrap().is_empty());
        assert!(pages_of(t).await.is_empty());
    }

    #[tokio::test]
    async fn rejects_too_many_pages() {
        let t = tenant().await;
        let a = app(FakePdfs::default(), None).await;
        let got = call_bytes(&a, "POST", PATH, Some(t), ADMIN, minimal_pdf(MAX_PAGES + 1)).await;
        assert_eq!(got.status, 422);
        assert_eq!(got.json(), json!({"error": "too_many_pages"}));
        assert!(a.pdfs.stored.lock().unwrap().is_empty());
        assert!(pages_of(t).await.is_empty());
    }

    /// 上限を 1 バイト超えたものは handler の 400、axum の上限 (+1024) を超えたものは 413
    #[tokio::test]
    async fn size_limits() {
        let t = tenant().await;
        let a = app(FakePdfs::default(), None).await;
        let got = call_bytes(&a, "POST", PATH, Some(t), ADMIN, big(MAX_PDF_BYTES + 1)).await;
        assert_eq!(got.status, 400);
        assert_eq!(got.json(), json!({"error": "pdf_too_large"}));
        let got = call_bytes(&a, "POST", PATH, Some(t), ADMIN, big(MAX_PDF_BYTES + 2048)).await;
        assert_eq!(got.status, 413);
        assert!(a.pdfs.stored.lock().unwrap().is_empty());
        assert!(pages_of(t).await.is_empty());
    }

    /// INSERT の失敗は 503 `ingest_failed`。ログは固定の語だけ
    #[tokio::test]
    async fn a_failed_insert_is_503_with_a_fixed_log() {
        let t = tenant().await;
        let a = app(
            FakePdfs {
                drop_tenant_on_put: Some(t),
                ..Default::default()
            },
            None,
        )
        .await;
        let got = call_bytes(&a, "POST", PATH, Some(t), ADMIN, minimal_pdf(1)).await;
        assert_eq!(got.status, 503);
        assert_eq!(got.json(), json!({"error": "ingest_failed"}));
        assert_eq!(*a.logs.lock().unwrap(), vec!["ingest insert: 23503"]);
    }
}

// ---- 一番星の候補・紐づけの保存・PDF の基準日 ----

/// 受信 (JST でも同じ日) と、その前後 7 日の窓
const RECEIVED: &str = "2026-10-01T00:00:00Z";
const WIN_FROM: &str = "2026-09-24";
const WIN_TO: &str = "2026-10-08";

fn link_day_rows() -> Vec<Value> {
    use support::day_row;
    vec![
        day_row(
            "1021",
            "2026-09-30",
            "有休",
            Some("20260930-1"),
            Some("2026-09-30 08:00:00.000"),
        ),
        day_row(
            "1021",
            "2026-10-01",
            " 有休（積置） ",
            Some("20261001-1"),
            None,
        ),
        day_row("1021", "2026-10-02", "休み", Some("20261002-1"), None),
        day_row("1021", "2026-10-03", "午前休", Some("20261003-1"), None),
        day_row("1021", "2026-10-04", "謎", Some("20261004-1"), None),
        day_row("1021", "2026-10-05", "有休", None, None),
        day_row("1021", "2026-10-06", "労災", Some("20261006-1"), None),
        day_row("2001", "2026-10-01", "有休", Some("20261001-2"), None),
        day_row("1030", "2026-10-01", "有休", Some("20261001-3"), None),
    ]
}

/// 社員と、`from`〜`to` の休暇の口 (worker が呼ぶのと同じ path)
fn link_ichiban(from: &str, to: &str, rows: &[Value]) -> FakeIchiban {
    let mut f = ichiban_with(&[(EMPLOYEES_PATH, 200, ledger_employees())]);
    support::add_days(&mut f, from, to, rows);
    f
}

async fn link_app(t: Uuid, f: FakeIchiban) -> App {
    let a = app(FakePdfs::default(), Some(f)).await;
    assert_eq!(
        put_settings(&a, t, json!({"dept_codes": ["10"]}))
            .await
            .status,
        200
    );
    a
}

fn ichiban_calls(a: &App) -> Vec<String> {
    a.ichiban.as_ref().unwrap().calls.lock().unwrap().clone()
}

#[tokio::test]
async fn candidates_group_rows_around_the_base_date() {
    let t = tenant().await;
    let p = page(t, &format!("{t}/a.pdf"), 1, RECEIVED).await;
    let a = link_app(t, link_ichiban(WIN_FROM, WIN_TO, &link_day_rows())).await;
    let uri = format!("/api/leave/pages/{p}/candidates?employee_cd=1021");
    let got = call(&a, "GET", &uri, Some(t), ADMIN, None).await;
    assert_eq!(got.status, 200);
    // 続く日の同じ区分は 1 期間・半日は単日・対象外・表に無い語・row_id 無し・別の乗務員は入れない
    assert_eq!(
        got.json(),
        json!({"periods": [
            {"kind": "yukyu", "start_date": "2026-09-30", "end_date": "2026-10-01", "half": null, "rows": [
                {"row_id": "20260930-1", "day": "2026-09-30", "item_name": "有休"},
                {"row_id": "20261001-1", "day": "2026-10-01", "item_name": " 有休（積置） "}
            ]},
            {"kind": "yukyu", "start_date": "2026-10-03", "end_date": "2026-10-03", "half": "am", "rows": [
                {"row_id": "20261003-1", "day": "2026-10-03", "item_name": "午前休"}
            ]},
            {"kind": "rousai", "start_date": "2026-10-06", "end_date": "2026-10-06", "half": null, "rows": [
                {"row_id": "20261006-1", "day": "2026-10-06", "item_name": "労災"}
            ]}
        ]})
    );
    // 基準日が無ければ受信日の前後 7 日を 1 回で取る
    let window = leave::reconcile::days_paths(WIN_FROM.parse().unwrap(), WIN_TO.parse().unwrap());
    assert_eq!(window.len(), 1);
    assert_eq!(
        ichiban_calls(&a),
        vec![EMPLOYEES_PATH.to_owned(), window[0].clone()]
    );

    // 基準日を置くと、その前後 7 日 (受信日の窓は呼ばない)
    let got = call(
        &a,
        "PUT",
        &format!("/api/leave/pages/{p}/base-date"),
        Some(t),
        ADMIN,
        Some(json!({"base_date": "2026-10-20"})),
    )
    .await;
    assert_eq!(got.status, 200);
    let b = app(
        FakePdfs::default(),
        Some(link_ichiban(
            "2026-10-13",
            "2026-10-27",
            &[support::day_row(
                "1021",
                "2026-10-20",
                "欠勤",
                Some("20261020-1"),
                None,
            )],
        )),
    )
    .await;
    let got = call(&b, "GET", &uri, Some(t), ADMIN, None).await;
    assert_eq!(got.status, 200);
    assert_eq!(
        got.json(),
        json!({"periods": [
            {"kind": "kekkin", "start_date": "2026-10-20", "end_date": "2026-10-20", "half": null, "rows": [
                {"row_id": "20261020-1", "day": "2026-10-20", "item_name": "欠勤"}
            ]}
        ]})
    );
}

/// 保存済みの期間が基準日の前後 7 日の外でも、窓をそこまで広げて候補に出し、紐づけ直せる
#[tokio::test]
async fn candidates_cover_saved_periods_outside_the_window() {
    let t = tenant().await;
    let p = page(t, &format!("{t}/a.pdf"), 1, RECEIVED).await;
    // 基準日 2026-10-17 (前後 7 日 = 10/10〜10/24)・保存済みの期間 2026-10-06 (午前) → 窓は 10/06〜10/24
    let rows = [
        support::day_row("1021", "2026-10-06", "午前休", Some("20261006-9"), None),
        support::day_row("1021", "2026-10-17", "有休", Some("20261017-1"), None),
    ];
    let a = link_app(t, link_ichiban("2026-10-06", "2026-10-24", &rows)).await;
    assert_eq!(
        put_base(&a, t, &p.to_string(), json!({"base_date": "2026-10-17"}))
            .await
            .status,
        200
    );
    let page_uri = format!("/api/leave/pages/{p}");
    let body = |ids: Value| {
        json!({"employee_cd": "1021", "status": "linked", "ichiban_row_ids": ids,
               "periods": [{"kind": "yukyu", "start_date": "2026-10-06", "end_date": "2026-10-06", "half": "am"}]})
    };
    // 行の指定の無い紐づけ (一番星は呼ばない) で期間だけ保存する
    let got = call(&a, "PUT", &page_uri, Some(t), ADMIN, Some(body(json!([])))).await;
    assert_eq!(got.status, 200);
    assert!(ichiban_calls(&a).is_empty());
    let uri = format!("/api/leave/pages/{p}/candidates?employee_cd=1021");
    let got = call(&a, "GET", &uri, Some(t), ADMIN, None).await;
    assert_eq!(got.status, 200);
    assert_eq!(
        got.json(),
        json!({"periods": [
            {"kind": "yukyu", "start_date": "2026-10-06", "end_date": "2026-10-06", "half": "am", "rows": [
                {"row_id": "20261006-9", "day": "2026-10-06", "item_name": "午前休"}
            ]},
            {"kind": "yukyu", "start_date": "2026-10-17", "end_date": "2026-10-17", "half": null, "rows": [
                {"row_id": "20261017-1", "day": "2026-10-17", "item_name": "有休"}
            ]}
        ]})
    );
    let window =
        leave::reconcile::days_paths("2026-10-06".parse().unwrap(), "2026-10-24".parse().unwrap());
    assert_eq!(
        ichiban_calls(&a),
        vec![EMPLOYEES_PATH.to_owned(), window[0].clone()]
    );
    // 紐づけの更新の引き直しも同じ窓 (保存済みの日の行を紐づけ直せる)
    let got = call(
        &a,
        "PUT",
        &page_uri,
        Some(t),
        ADMIN,
        Some(body(json!(["20261006-9"]))),
    )
    .await;
    assert_eq!(got.status, 200);
    assert_eq!(
        support::link_rows_of(p).await,
        vec![saved("20261006-9", "2026-10-06", "午前休", None)]
    );
}

#[tokio::test]
async fn candidates_reject_and_need_ichiban() {
    let (t, other) = (tenant().await, tenant().await);
    let p = page(t, &format!("{t}/a.pdf"), 1, RECEIVED).await;
    let po = page(other, &format!("{other}/a.pdf"), 1, RECEIVED).await;
    let a = link_app(t, link_ichiban(WIN_FROM, WIN_TO, &link_day_rows())).await;
    let uri = |id: &str, q: &str| format!("/api/leave/pages/{id}/candidates{q}");
    for (u, status, code) in [
        // 他部門の乗務員・見えないページ・UUID でない id
        (uri(&p.to_string(), "?employee_cd=2001"), 404, "not_found"),
        (uri(&po.to_string(), "?employee_cd=1021"), 404, "not_found"),
        (
            uri(&Uuid::new_v4().to_string(), "?employee_cd=1021"),
            404,
            "not_found",
        ),
        (uri("x", "?employee_cd=1021"), 404, "not_found"),
        (uri(&p.to_string(), ""), 400, "invalid_employee_cd"),
    ] {
        let got = call(&a, "GET", &u, Some(t), ADMIN, None).await;
        assert_eq!(
            (got.status, got.json()),
            (status, json!({"error": code})),
            "{u}"
        );
    }
    let ok = uri(&p.to_string(), "?employee_cd=1021");
    // 一番星の binding が無い環境 (staging) では 503
    let none = app(FakePdfs::default(), None).await;
    let got = call(&none, "GET", &ok, Some(t), ADMIN, None).await;
    assert_eq!(
        (got.status, got.json()),
        (503, json!({"error": "ichiban_not_configured"}))
    );
    // 一番星の失敗は 502 (休暇の口が無い・非 2xx・形が違う)
    let path =
        leave::reconcile::days_paths(WIN_FROM.parse().unwrap(), WIN_TO.parse().unwrap()).remove(0);
    let with_days = |status: u16, body: Value| {
        ichiban_with(&[
            (EMPLOYEES_PATH, 200, ledger_employees()),
            (path.as_str(), status, body),
        ])
    };
    for (fake, code) in [
        (
            ichiban_with(&[(EMPLOYEES_PATH, 200, ledger_employees())]),
            "ichiban_unavailable",
        ),
        (with_days(500, json!({})), "ichiban_upstream"),
        (with_days(200, json!([])), "ichiban_bad_response"),
    ] {
        let b = app(FakePdfs::default(), Some(fake)).await;
        let got = call(&b, "GET", &ok, Some(t), ADMIN, None).await;
        assert_eq!((got.status, got.json()), (502, json!({"error": code})));
        for line in b.logs.lock().unwrap().iter() {
            assert!(
                !line.contains(&t.to_string()) && !line.contains(&p.to_string()),
                "{line}"
            );
        }
    }
}

async fn put_base(a: &App, t: Uuid, id: &str, body: Value) -> support::Got {
    call(
        a,
        "PUT",
        &format!("/api/leave/pages/{id}/base-date"),
        Some(t),
        ADMIN,
        Some(body),
    )
    .await
}

async fn base_of(a: &App, t: Uuid, id: Uuid) -> Value {
    call(
        a,
        "GET",
        &format!("/api/leave/pages/{id}"),
        Some(t),
        ADMIN,
        None,
    )
    .await
    .json()["base_date"]
        .clone()
}

#[tokio::test]
async fn base_date_is_set_for_the_whole_pdf() {
    let (t, other) = (tenant().await, tenant().await);
    let key = format!("{t}/a.pdf");
    let p1 = page(t, &key, 1, RECEIVED).await;
    let p2 = page(t, &key, 2, RECEIVED).await;
    let p3 = page(t, &format!("{t}/b.pdf"), 1, RECEIVED).await;
    let po = page(other, &format!("{other}/a.pdf"), 1, RECEIVED).await;
    let a = app(FakePdfs::default(), None).await;
    let got = put_base(&a, t, &p2.to_string(), json!({"base_date": "2026-10-20"})).await;
    assert_eq!(
        (got.status, got.json()),
        (200, json!({"base_date": "2026-10-20", "updated": 2}))
    );
    assert_eq!(base_of(&a, t, p1).await, "2026-10-20");
    assert_eq!(base_of(&a, t, p2).await, "2026-10-20");
    // 別の PDF は変わらない
    assert_eq!(base_of(&a, t, p3).await, Value::Null);
    let list = call(
        &a,
        "GET",
        &format!("/api/leave/pages?r2_key={key}"),
        Some(t),
        ADMIN,
        None,
    )
    .await
    .json();
    assert_eq!(list["data"].as_array().unwrap().len(), 2);
    assert!(list["data"]
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["base_date"] == "2026-10-20"));
    // null で受信日に戻す
    let got = put_base(&a, t, &p1.to_string(), json!({"base_date": null})).await;
    assert_eq!(
        (got.status, got.json()),
        (200, json!({"base_date": null, "updated": 2}))
    );
    assert_eq!(base_of(&a, t, p2).await, Value::Null);
    // 他 tenant のページ・UUID でない id・壊れた本文
    for (id, body, status, code) in [
        (
            po.to_string(),
            json!({"base_date": "2026-10-20"}),
            404,
            "not_found",
        ),
        (
            "x".to_owned(),
            json!({"base_date": "2026-10-20"}),
            404,
            "not_found",
        ),
        (
            p1.to_string(),
            json!({"base_date": "10/20"}),
            400,
            "invalid_body",
        ),
        (
            p1.to_string(),
            json!({"base_date": null, "x": 1}),
            400,
            "invalid_body",
        ),
    ] {
        let got = put_base(&a, t, &id, body).await;
        assert_eq!(
            (got.status, got.json()),
            (status, json!({"error": code})),
            "{id}"
        );
    }
    assert_eq!(base_of(&a, other, po).await, Value::Null);
    assert_eq!(base_of(&a, t, p1).await, Value::Null);

    // staging の表示制限の外のページは 404 (中のページは書ける)
    let fresh = page(t, &format!("{t}/c.pdf"), 1, "2026-10-09T23:55:00Z").await;
    let s = support::app_with_visible_minutes(FakePdfs::default(), "10").await;
    for (id, status) in [(p1, 404), (fresh, 200)] {
        let got = put_base(&s, t, &id.to_string(), json!({"base_date": "2026-10-20"})).await;
        assert_eq!(got.status, status);
    }
    assert_eq!(base_of(&a, t, p1).await, Value::Null);
    assert_eq!(base_of(&a, t, fresh).await, "2026-10-20");
}

fn link_body(employee_cd: &str, kind: &str, ids: Value) -> Value {
    json!({"employee_cd": employee_cd, "status": "linked",
           "periods": [{"kind": kind, "start_date": "2026-09-30", "end_date": "2026-10-01"}],
           "ichiban_row_ids": ids})
}

fn saved(
    id: &str,
    day: &str,
    item: &str,
    lu: Option<&str>,
) -> (String, String, String, String, Option<String>) {
    (
        id.to_owned(),
        "1021".to_owned(),
        day.to_owned(),
        item.to_owned(),
        lu.map(str::to_owned),
    )
}

#[tokio::test]
async fn link_page_saves_ichiban_rows() {
    let t = tenant().await;
    let p = page(t, &format!("{t}/a.pdf"), 1, RECEIVED).await;
    let a = link_app(t, link_ichiban(WIN_FROM, WIN_TO, &link_day_rows())).await;
    let uri = format!("/api/leave/pages/{p}");

    // 一番星の生の値 (品名・最終更新) で入る (同じ row_id は 1 つ)
    let ids = json!(["20261001-1", "20260930-1", "20261001-1"]);
    let got = call(
        &a,
        "PUT",
        &uri,
        Some(t),
        ADMIN,
        Some(link_body("1021", "yukyu", ids)),
    )
    .await;
    assert_eq!(got.status, 200);
    assert_eq!(got.json()["status"], "linked");
    assert_eq!(
        support::link_rows_of(p).await,
        vec![
            saved(
                "20260930-1",
                "2026-09-30",
                "有休",
                Some("2026-09-30 08:00:00.000")
            ),
            saved("20261001-1", "2026-10-01", " 有休（積置） ", None)
        ]
    );
    // 2 回目は置き換え
    let ids = json!(["20261003-1"]);
    let got = call(
        &a,
        "PUT",
        &uri,
        Some(t),
        ADMIN,
        Some(link_body("1021", "yukyu", ids)),
    )
    .await;
    assert_eq!(got.status, 200);
    assert_eq!(
        support::link_rows_of(p).await,
        vec![saved("20261003-1", "2026-10-03", "午前休", None)]
    );
    // 行の指定が無い紐づけは手入力の期間だけ (一番星も部門も見ない。行は消える)
    let before = ichiban_calls(&a).len();
    let got = call(
        &a,
        "PUT",
        &uri,
        Some(t),
        ADMIN,
        Some(link_body("1021", "yukyu", json!([]))),
    )
    .await;
    assert_eq!(got.status, 200);
    assert!(support::link_rows_of(p).await.is_empty());
    assert_eq!(ichiban_calls(&a).len(), before);
    // linked 以外になれば行は消える (指定があっても一番星は呼ばない)
    let ids = json!(["20261006-1"]);
    let got = call(
        &a,
        "PUT",
        &uri,
        Some(t),
        ADMIN,
        Some(link_body("1021", "rousai", ids)),
    )
    .await;
    assert_eq!(got.status, 200);
    assert_eq!(support::link_rows_of(p).await.len(), 1);
    let before = ichiban_calls(&a).len();
    let body = json!({"status": "ignored", "ichiban_row_ids": ["20261006-1"]});
    let got = call(&a, "PUT", &uri, Some(t), ADMIN, Some(body)).await;
    assert_eq!(got.status, 200);
    assert_eq!(got.json()["status"], "ignored");
    assert!(support::link_rows_of(p).await.is_empty());
    assert_eq!(ichiban_calls(&a).len(), before);
}

#[tokio::test]
async fn link_page_rejects_unknown_rows_without_writing() {
    let t = tenant().await;
    let p = page(t, &format!("{t}/a.pdf"), 1, RECEIVED).await;
    let a = link_app(t, link_ichiban(WIN_FROM, WIN_TO, &link_day_rows())).await;
    let uri = format!("/api/leave/pages/{p}");
    let ids = json!(["20260930-1"]);
    let got = call(
        &a,
        "PUT",
        &uri,
        Some(t),
        ADMIN,
        Some(link_body("1021", "yukyu", ids)),
    )
    .await;
    assert_eq!(got.status, 200);
    let page_before = call(&a, "GET", &uri, Some(t), ADMIN, None).await.json();
    let rows_before = support::link_rows_of(p).await;
    assert_eq!(rows_before.len(), 1);

    let mut cases: Vec<(Value, u16, &str)> = vec![
        // 無い row_id・別の乗務員 (他部門 / 同じ部門) の row_id
        (
            link_body("1021", "kekkin", json!(["nope"])),
            400,
            "unknown_row",
        ),
        (
            link_body("1021", "kekkin", json!(["20260930-1", "20261001-2"])),
            400,
            "unknown_row",
        ),
        (
            link_body("1021", "kekkin", json!(["20261001-3"])),
            400,
            "unknown_row",
        ),
        // 本文の乗務員が自 tenant の部門の外
        (
            link_body("2001", "kekkin", json!(["20261001-2"])),
            404,
            "not_found",
        ),
    ];
    cases.push((
        link_body("1021", "kekkin", json!(["1".repeat(33)])),
        400,
        "invalid_row_id",
    ));
    for (body, status, code) in cases {
        let got = call(&a, "PUT", &uri, Some(t), ADMIN, Some(body.clone())).await;
        assert_eq!(
            (got.status, got.json()),
            (status, json!({"error": code})),
            "{body}"
        );
    }
    // 見えないページは 404
    let got = call(
        &a,
        "PUT",
        &format!("/api/leave/pages/{}", Uuid::new_v4()),
        Some(t),
        ADMIN,
        Some(link_body("1021", "kekkin", json!(["20260930-1"]))),
    )
    .await;
    assert_eq!(got.status, 404);
    // 一番星の失敗は 502・無い環境は 503
    for (fake, status, code) in [
        (
            Some(ichiban_with(&[(EMPLOYEES_PATH, 200, ledger_employees())])),
            502,
            "ichiban_unavailable",
        ),
        (None, 503, "ichiban_not_configured"),
    ] {
        let b = app(FakePdfs::default(), fake).await;
        let body = link_body("1021", "kekkin", json!(["20260930-1"]));
        let got = call(&b, "PUT", &uri, Some(t), ADMIN, Some(body)).await;
        assert_eq!((got.status, got.json()), (status, json!({"error": code})));
    }
    // どれもページも行も変えない (fail-closed)
    assert_eq!(
        call(&a, "GET", &uri, Some(t), ADMIN, None).await.json(),
        page_before
    );
    assert_eq!(support::link_rows_of(p).await, rows_before);
}
