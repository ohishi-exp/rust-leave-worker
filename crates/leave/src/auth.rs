//! 口を使える人の判定 (2 層) と、操作した人の識別子。
//!
//! 休暇申請 (労災・欠勤を含む) は機微なので、口は 2 層に分けて閉じる。判定はこのファイルの 1 か所で、
//! layer を掛けるのは `routes::tenant_router` だけ:
//! - **admin 層** ([`require_admin`]、[`can_admin`]): 設定・テスト送信・付与台帳の書き込み・期首残の保存。
//!   管理者か、全権限 `*` を持つ人。既定はこちらで、`tenant_router` に足した新しい口は明示的に移さない限り
//!   この層。不一致は 403 `admin_only`
//! - **leave.use 層** ([`require_leave_use`]、[`can_use_leave`]): 申請書の一覧・紐づけ・PDF・カレンダー・台帳・漏れ。
//!   管理者か、権限 [`LEAVE_USE`] (または全権限 `*`) を持つ人。不一致は 403 `forbidden`
//!
//! ヘッダーは auth-worker の `/alc-proxy` が JWT を検証したうえで付け直したもの (`X-User-Role` = JWT の `role`、
//! `X-User-Email` = `email`、`X-User-ID` = `sub`、[`PERMISSIONS_HEADER`] = 解決済みの権限名の空白区切り)。
//! 管理者の値は rust-alc-api の tenant_users の role 集合 (`admin` / `viewer` / `payroll`) の `admin` で、
//! 権限名とともに完全一致 (大文字小文字も区別) だけを通す。権限のヘッダーが来ない間は admin だけが通る。

use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;

/// 管理者の role の値 (完全一致)
pub const ADMIN_ROLE: &str = "admin";

/// 休暇申請の画面を使う権限の名前 (完全一致)
pub const LEAVE_USE: &str = "leave.use";

/// 全権限の印 (auth-worker が管理者に付ける)
pub const ALL_PERMISSIONS: &str = "*";

/// 解決済みの権限名 (空白区切り) を運ぶヘッダー
pub const PERMISSIONS_HEADER: &str = "X-User-Permissions";

/// `X-User-Role` が [`ADMIN_ROLE`] と完全一致するときだけ真
pub fn is_admin(headers: &HeaderMap) -> bool {
    headers
        .get("X-User-Role")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == ADMIN_ROLE)
}

/// 空白区切りの権限名 `permissions` に `name` か [`ALL_PERMISSIONS`] が (完全一致で) 含まれるときだけ真
pub fn grants(permissions: &str, name: &str) -> bool {
    permissions
        .split_ascii_whitespace()
        .any(|p| p == name || p == ALL_PERMISSIONS)
}

fn header_grants(headers: &HeaderMap, name: &str) -> bool {
    headers
        .get(PERMISSIONS_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|p| grants(p, name))
}

/// admin 層: 管理者か、全権限 [`ALL_PERMISSIONS`] を持つときだけ真
pub fn can_admin(headers: &HeaderMap) -> bool {
    is_admin(headers) || header_grants(headers, ALL_PERMISSIONS)
}

/// leave.use 層: 管理者か、権限 [`LEAVE_USE`] (または [`ALL_PERMISSIONS`]) を持つときだけ真
pub fn can_use_leave(headers: &HeaderMap) -> bool {
    is_admin(headers) || header_grants(headers, LEAVE_USE)
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

fn forbidden(code: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({ "error": code })),
    )
        .into_response()
}

/// [`can_admin`] でなければ 403 `{"error":"admin_only"}` で止める (handler にも DB にも届かない)
pub async fn require_admin(req: Request, next: Next) -> Response {
    if can_admin(req.headers()) {
        next.run(req).await
    } else {
        forbidden("admin_only")
    }
}

/// [`can_use_leave`] でなければ 403 `{"error":"forbidden"}` で止める (handler にも DB にも届かない)
pub async fn require_leave_use(req: Request, next: Next) -> Response {
    if can_use_leave(req.headers()) {
        next.run(req).await
    } else {
        forbidden("forbidden")
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
    fn leave_use_is_an_exact_whitespace_separated_name() {
        for v in [
            "leave.use",
            "*",
            "a leave.use b",
            "leave.use leave.use",
            "  leave.use  ",
            "x\tleave.use\ny",
            "a * b",
        ] {
            assert!(grants(v, LEAVE_USE), "{v:?}");
        }
        for v in [
            "",
            " ",
            "Leave.use",
            "LEAVE.USE",
            "leave.usex",
            "xleave.use",
            "leave.use,a",
            "leave",
            "**",
            "admin",
        ] {
            assert!(!grants(v, LEAVE_USE), "{v:?}");
        }
        // 全権限は `*` そのものだけ
        assert!(grants("a * b", ALL_PERMISSIONS));
        for v in ["", "leave.use", "**", "admin"] {
            assert!(!grants(v, ALL_PERMISSIONS), "{v:?}");
        }
    }

    #[test]
    fn can_admin_is_admin_or_all_permissions() {
        assert!(can_admin(&headers(&[("X-User-Role", "admin")])));
        assert!(can_admin(&headers(&[
            ("X-User-Role", "viewer"),
            ("X-User-Permissions", "*")
        ])));
        assert!(!can_admin(&headers(&[
            ("X-User-Role", "viewer"),
            ("X-User-Permissions", "leave.use")
        ])));
        assert!(!can_admin(&headers(&[("X-User-Role", "viewer")])));
        assert!(!can_admin(&HeaderMap::new()));
    }

    #[test]
    fn can_use_leave_is_admin_or_the_permission() {
        assert!(can_use_leave(&headers(&[("X-User-Role", "admin")])));
        assert!(can_use_leave(&headers(&[
            ("X-User-Role", "viewer"),
            ("X-User-Permissions", "leave.use")
        ])));
        assert!(can_use_leave(&headers(&[("X-User-Permissions", "*")])));
        assert!(!can_use_leave(&headers(&[("X-User-Role", "viewer")])));
        assert!(!can_use_leave(&headers(&[
            ("X-User-Role", "viewer"),
            ("X-User-Permissions", "Leave.use")
        ])));
        assert!(!can_use_leave(&HeaderMap::new()));
        let mut h = HeaderMap::new();
        h.insert(
            "X-User-Permissions",
            HeaderValue::from_bytes(b"\xffleave.use").unwrap(),
        );
        assert!(!can_use_leave(&h));
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
