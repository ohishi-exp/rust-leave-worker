//! 口から実 DB (RLS が効く `alc_api_app`)・偽の PDF 置き場・偽の一番星までを通すテスト。
//! 接続先は `LEAVE_TEST_ADMIN_DATABASE_URL` (未設定なら失敗する。tests/support/mod.rs)。

mod support;

use std::collections::HashMap;

use leave::ichiban::{IchibanResponse, DEPARTMENTS_PATH, EMPLOYEES_PATH};
use serde_json::{json, Value};
use support::{app, call, lineworks_channel, page, tenant, App, FakeIchiban, FakePdfs};
use uuid::Uuid;

const ADMIN: Option<&str> = Some("admin");

fn ichiban_with(pairs: &[(&str, u16, Value)]) -> FakeIchiban {
    FakeIchiban {
        responses: pairs
            .iter()
            .map(|(p, s, v)| {
                (
                    (*p).to_owned(),
                    IchibanResponse {
                        status: *s,
                        body: serde_json::to_vec(v).unwrap(),
                    },
                )
            })
            .collect(),
        ..Default::default()
    }
}

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
               "grace_days": 7, "enabled": false, "updated_at": null})
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
    assert_eq!(logs.len(), 7);
    for line in logs.iter() {
        assert!(
            !line.contains(&t.to_string()) && !line.contains(&p.to_string()),
            "{line}"
        );
    }
}
