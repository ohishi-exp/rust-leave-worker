//! 管理者の判定と、操作した人の識別子。
//!
//! 休暇申請 (労災・欠勤を含む) は機微なので、**全部の口を管理者だけに開ける**。判定はこのファイルの
//! [`is_admin`] の 1 か所で、[`require_admin`] が router の全 route に掛かる (`routes::tenant_router`)。
//!
//! ヘッダーは auth-worker の `/alc-proxy` が JWT を検証したうえで付け直したもの (`X-User-Role` = JWT の `role`、
//! `X-User-Email` = `email`、`X-User-ID` = `sub`)。管理者の値は rust-alc-api の tenant_users の role 集合
//! (`admin` / `viewer` / `payroll`) の `admin` で、完全一致だけを通す。

use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;

/// 管理者の role の値 (完全一致)
pub const ADMIN_ROLE: &str = "admin";

/// `X-User-Role` が [`ADMIN_ROLE`] と完全一致するときだけ真
pub fn is_admin(headers: &HeaderMap) -> bool {
    headers
        .get("X-User-Role")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == ADMIN_ROLE)
}

/// 紐づけた人 (`leave_pages.linked_by`)。`X-User-Email`、無ければ `X-User-ID`、どちらも無ければ `None`
pub fn actor(headers: &HeaderMap) -> Option<String> {
    ["X-User-Email", "X-User-ID"].iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    })
}

/// 管理者でなければ 403 `{"error":"forbidden"}` で止める (handler にも DB にも届かない)
pub async fn require_admin(req: Request, next: Next) -> Response {
    if is_admin(req.headers()) {
        next.run(req).await
    } else {
        (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "forbidden" })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn admin_only_exact_match() {
        assert!(is_admin(&headers(&[("X-User-Role", "admin")])));
        for v in [
            "Admin", "ADMIN", " admin", "admin ", "viewer", "payroll", "",
        ] {
            let mut h = HeaderMap::new();
            h.insert("X-User-Role", HeaderValue::from_str(v).unwrap());
            assert!(!is_admin(&h), "{v:?}");
        }
        assert!(!is_admin(&HeaderMap::new()));
    }

    #[test]
    fn non_utf8_role_is_not_admin() {
        let mut h = HeaderMap::new();
        h.insert(
            "X-User-Role",
            HeaderValue::from_bytes(b"\xffadmin").unwrap(),
        );
        assert!(!is_admin(&h));
    }

    #[test]
    fn actor_prefers_email_then_user_id() {
        assert_eq!(
            actor(&headers(&[
                ("X-User-Email", "a@example.com"),
                ("X-User-ID", "u1")
            ])),
            Some("a@example.com".to_owned())
        );
        assert_eq!(
            actor(&headers(&[("X-User-Email", " "), ("X-User-ID", "u1")])),
            Some("u1".to_owned())
        );
        assert_eq!(
            actor(&headers(&[("X-User-ID", "u1")])),
            Some("u1".to_owned())
        );
        assert_eq!(actor(&HeaderMap::new()), None);
    }
}
