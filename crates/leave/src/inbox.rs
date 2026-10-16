//! スキャンの inbox の取り込み (一覧の口を開いたときに [`sweep`] が拾う)。
//!
//! 複合機 → nuxt-leave の WebDAV が R2 bucket `leave-inbox` に PDF を置き、`customMetadata.ticket` に auth-worker の
//! 署名付きチケットを載せる。一覧の口 (`GET /api/leave/documents`) を開いたとき、list の結果から [`InboxEvent`] の形の本文を組んでここへ通す。
//!
//! 流れ: 本文の JSON ([`InboxEvent`]) → 対象の action か・大きさ → inbox の object を読む → チケットを auth-worker の
//! RPC `verifyTicket` で検証 ([`TicketVerifier`]。**この Worker は鍵を持たない**) → 本文の sha256 がチケットと一致 →
//! [`check_pdf_bytes`] → [`ingest_pdf`] (key は inbox の object key から決定的に作る = 同じ object を sweep で 2 回拾っても二重にならない) →
//! inbox の object を消して ack (inbox から外す)。
//!
//! - **tenant はチケットの検証結果だけから取る** (inbox の key・メタデータからは読まない)。会社は機械用資格情報で
//!   確定している (送信元の照合は無い)
//! - 取り込めない object (チケットが無い・不正・sha256 の不一致・PDF でない・読めない) は消して ack (再試行しない)
//! - **DB の接続は検証の後**: batch の全件の検証 (R2・RPC) を済ませ、通ったものがあるときだけ 1 本張る
//!   ([`handle_batch`]。RPC の先が同じ DB に繋ぎにいっても、こちらが先に握っていないので詰まらない)
//! - R2・DB・RPC の一時的な失敗 (`verifyTicket` の `{error: "unavailable"}` を含む) は retry (inbox に残して次に一覧を開いたときにやり直す)。object が既に無ければ ack
//! - ログは段の名前と固定の語だけ (チケット・PDF の中身・object key は出さない。tenant は取り込み後のログに出す)
//!
//! 拾う口は [`sweep`] (list の結果の本文を [`handle_batch`] に通す)。判定は [`may_sweep`]

use std::fmt::Display;
use std::future::Future;

use alc_worker_db::PgClient;
use axum::http::{HeaderMap, Method};
use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;
use futures_util::lock::Mutex;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::can_use_leave;
use crate::ingest::{check_pdf_bytes, sha256_hex, MAX_PDF_BYTES};
use crate::internal::{ingest_pdf, IngestFailure, NewPdf};
use crate::routes::LogSink;
use crate::store::{PdfStore, StoreError};

/// auth-worker の `verifyTicket` に渡す用途 (nuxt-leave が `issueTicket` に渡すものと同じ)
pub const TICKET_PURPOSE: &str = "leave-scan";

/// 取り込む action (R2 のイベント通知。ほかは何もせず ack)
const STORE_ACTIONS: [&str; 2] = ["PutObject", "CompleteMultipartUpload"];

/// R2 のイベント通知の本文 (使う項目だけ。`account`・`bucket` などは読まない)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxEvent {
    pub action: String,
    pub object: EventObject,
    /// 受信時刻 (`leave_pages.received_at`)。sweep で何度拾っても同じ値なので、決定的な key の年月もぶれない
    pub event_time: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct EventObject {
    pub key: String,
    pub size: Option<u64>,
}

/// inbox の object (中身とチケット)
pub struct InboxObject {
    pub bytes: Vec<u8>,
    /// `customMetadata.ticket`
    pub ticket: Option<String>,
}

/// inbox の list の 1 件 (メタデータだけ)
#[derive(Debug, Clone, PartialEq)]
pub struct InboxListed {
    pub key: String,
    pub size: u64,
    /// 置かれた時刻 (通知の `eventTime` の代わりに `leave_pages.received_at` にする)
    pub uploaded: DateTime<Utc>,
}

/// inbox の bucket (binding `LEAVE_INBOX`)。Worker での実装は直下の `src/bindings.rs`
pub trait Inbox: Send + Sync {
    /// 先頭から `limit` 件まで (key の辞書順。続きは読まない)
    fn list(&self, limit: u32) -> BoxFuture<'_, Result<Vec<InboxListed>, StoreError>>;
    /// 無ければ `Ok(None)`
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<InboxObject>, StoreError>>;
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), StoreError>>;
}

/// RPC の失敗 (一時的なもの。retry)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VerifyError;

/// auth-worker の `verifyTicket(ticket, purpose)`。戻り値は JSON のまま (`null` か `{tenantId, username, sha256}`)。
/// 形の解釈は [`parse_ticket`] (`{error: "unavailable"}` は一時的な失敗、形が違えば不正なチケット)
pub trait TicketVerifier: Send + Sync {
    fn verify<'a>(
        &'a self,
        ticket: &'a str,
        purpose: &'a str,
    ) -> BoxFuture<'a, Result<serde_json::Value, VerifyError>>;
}

/// 検証を通ったチケット (使う項目だけ)
#[derive(Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ticket {
    pub tenant_id: Uuid,
    /// PDF の sha256 (hex 小文字)
    pub sha256: String,
}

/// `verifyTicket` の戻りの解釈
#[derive(Debug, PartialEq)]
pub enum TicketCheck {
    Valid(Ticket),
    /// `null`・形の違うもの (消して ack)
    Invalid,
    /// `{ "error": "unavailable" }` (auth-worker が資格情報を引けなかった一時的な失敗。retry)
    Unavailable,
}

/// `verifyTicket` の戻りの解釈。`{error: "unavailable"}` は一時的な失敗、`null`・ほかの形は不正
pub fn parse_ticket(v: serde_json::Value) -> TicketCheck {
    if v.get("error").and_then(serde_json::Value::as_str) == Some("unavailable") {
        return TicketCheck::Unavailable;
    }
    serde_json::from_value(v).map_or(TicketCheck::Invalid, TicketCheck::Valid)
}

/// メッセージの扱い
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    Ack,
    Retry,
}

pub struct InboxDeps<'a> {
    /// 本保存の bucket (`LEAVE_R2`)
    pub pdfs: &'a dyn PdfStore,
    pub inbox: &'a dyn Inbox,
    pub verifier: &'a dyn TicketVerifier,
    pub log: &'a LogSink,
}

/// 検証を通った object (DB に取り込む前のもの)
pub struct Verified {
    key: String,
    tenant_id: Uuid,
    bytes: Vec<u8>,
    received_at: DateTime<Utc>,
}

/// 検証段の結果。`Done` はもう決まったもの (ack か retry)、`Ready` は DB へ取り込むもの
pub enum Verdict {
    Done(Outcome),
    Ready(Verified),
}

/// 取り込めない object を消して ack (消せなければ retry。次に拾うときもう一度消す)
async fn discard(d: &InboxDeps<'_>, key: &str, reason: &str) -> Verdict {
    (d.log)(&format!("inbox dropped: {reason}"));
    Verdict::Done(settle(d, key).await)
}

/// inbox の object を消して ack (inbox から外す)。消せなければ retry (取り込みは冪等なので、もう一度通してよい)
async fn settle(d: &InboxDeps<'_>, key: &str) -> Outcome {
    match d.inbox.delete(key).await {
        Ok(()) => Outcome::Ack,
        Err(e) => {
            (d.log)(&format!("inbox: delete {}", e.0));
            Outcome::Retry
        }
    }
}

/// 1 メッセージの検証段 (**DB を使わない**): R2 の object の取得・チケットの検証・sha256 の照合・PDF の検査。
/// チケットの検証 (RPC) は別の Worker 経由で同じ DB に繋ぎにいくことがある。同時に 1 本しか受けない DB では、
/// こちらが先に接続を握っていると後の接続が閉じられるので、DB の接続を張る前に済ませる
pub async fn verify(d: &InboxDeps<'_>, body: serde_json::Value) -> Verdict {
    let Ok(event) = serde_json::from_value::<InboxEvent>(body) else {
        (d.log)("inbox: invalid message");
        return Verdict::Done(Outcome::Ack);
    };
    if !STORE_ACTIONS.contains(&event.action.as_str()) || event.object.size == Some(0) {
        (d.log)("inbox: skipped");
        return Verdict::Done(Outcome::Ack);
    }
    let key = event.object.key.as_str();
    if event.object.size.is_some_and(|s| s > MAX_PDF_BYTES as u64) {
        return discard(d, key, "pdf_too_large").await;
    }
    let object = match d.inbox.get(key).await {
        Ok(Some(o)) => o,
        Ok(None) => {
            (d.log)("inbox: object gone");
            return Verdict::Done(Outcome::Ack);
        }
        Err(e) => {
            (d.log)(&format!("inbox: {}", e.0));
            return Verdict::Done(Outcome::Retry);
        }
    };
    let Some(ticket) = object.ticket.as_deref() else {
        return discard(d, key, "ticket_missing").await;
    };
    let ticket = match d.verifier.verify(ticket, TICKET_PURPOSE).await {
        Ok(v) => parse_ticket(v),
        Err(VerifyError) => {
            (d.log)("inbox: verify failed");
            return Verdict::Done(Outcome::Retry);
        }
    };
    let ticket = match ticket {
        TicketCheck::Valid(t) => t,
        TicketCheck::Invalid => return discard(d, key, "ticket_invalid").await,
        TicketCheck::Unavailable => {
            (d.log)("inbox: verify unavailable");
            return Verdict::Done(Outcome::Retry);
        }
    };
    if sha256_hex(&object.bytes) != ticket.sha256 {
        return discard(d, key, "sha256_mismatch").await;
    }
    if let Err(reason) = check_pdf_bytes(&object.bytes) {
        return discard(d, key, reason).await;
    }
    Verdict::Ready(Verified {
        key: key.to_owned(),
        tenant_id: ticket.tenant_id,
        bytes: object.bytes,
        received_at: event.event_time,
    })
}

/// 検証を通った 1 件の取り込み段 (DB を使う)
async fn ingest(d: &InboxDeps<'_>, pg: &Mutex<PgClient>, v: Verified) -> Outcome {
    let Verified {
        key,
        tenant_id,
        bytes,
        received_at,
    } = v;
    let pdf = NewPdf {
        tenant_id,
        pdf: bytes,
        received_at,
        mail_from: None,
        subject: None,
        key_seed: key.clone(),
    };
    match ingest_pdf(pg, d.pdfs, d.log, pdf).await {
        Ok(pages) => {
            (d.log)(&format!("inbox stored: tenant={tenant_id} pages={pages}"));
            settle(d, &key).await
        }
        Err(IngestFailure::Drop(reason)) => {
            (d.log)(&format!("inbox dropped: {reason}"));
            settle(d, &key).await
        }
        Err(IngestFailure::Failed) => Outcome::Retry,
    }
}

/// 1 batch を 2 段で処理して、メッセージごとの扱い (入力と同じ順) を返す。
/// 1. 全メッセージの検証段 ([`verify`])。**この間は DB に繋がない**
/// 2. 検証を通ったものがあるときだけ `connect` を 1 回呼び、その接続を使い回して取り込む。
///    繋がらなければ検証を通ったものを全部 retry (inbox に残して次の回)
pub async fn handle_batch<F, Fut, E>(
    d: &InboxDeps<'_>,
    bodies: Vec<serde_json::Value>,
    connect: F,
) -> Vec<Outcome>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<PgClient, E>>,
    E: Display,
{
    let mut outcomes = vec![Outcome::Retry; bodies.len()];
    let mut ready = Vec::new();
    for (i, body) in bodies.into_iter().enumerate() {
        match verify(d, body).await {
            Verdict::Done(o) => outcomes[i] = o,
            Verdict::Ready(v) => ready.push((i, v)),
        }
    }
    if ready.is_empty() {
        return outcomes;
    }
    match connect().await {
        Ok(pg) => {
            let pg = Mutex::new(pg);
            for (i, v) in ready {
                outcomes[i] = ingest(d, &pg, v).await;
            }
        }
        // `outcomes` の初期値が Retry なので、検証を通ったものはそのまま retry になる
        Err(e) => (d.log)(&format!("inbox: {e}")),
    }
    outcomes
}

/// [`sweep`] で list する件数 (R2 の 1 回の上限。key は UUID 前置なので、超えるとそれより後ろは見えない)
pub const SWEEP_LIST_LIMIT: u32 = 1000;
/// [`sweep`] 1 回で取り込む件数 (置かれた時刻の新しい順。残りは次に開いたとき)
pub const SWEEP_BATCH: usize = 10;

/// 一覧の口 (worker から見た path)
const SWEEP_PATH: &str = "/api/leave/documents";

/// 一覧の口 `GET /api/leave/documents` で、router が通す人 (tenant ヘッダーが uuid として読める、かつ leave.use 層) の
/// ときだけ真。tenant の条件は `alc_core_wasm::require_tenant_header` と同じ (`to_str` → `Uuid::parse_str`、trim しない)。
/// 偽なら sweep せず router に任せる (401 / 403)
pub fn may_sweep(method: &Method, path: &str, headers: &HeaderMap) -> bool {
    method == Method::GET
        && path == SWEEP_PATH
        && headers
            .get("X-Tenant-ID")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| Uuid::parse_str(v).is_ok())
        && can_use_leave(headers)
}

/// inbox を list して、大きさ 0 を除き、置かれた時刻の新しい順に `batch` 件を [`handle_batch`] に通す
/// (通知と同じ形の本文を組む。失敗し続ける古い object が新しいものを塞がない)。Retry に終わったものは inbox に残り、
/// 次の回にやり直す。結果は固定の語と件数で 1 行ログに出し、扱い (処理した順) を返す。list の失敗はログだけ
pub async fn sweep<F, Fut, E>(d: &InboxDeps<'_>, batch: usize, connect: F) -> Vec<Outcome>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<PgClient, E>>,
    E: Display,
{
    let mut listed = match d.inbox.list(SWEEP_LIST_LIMIT).await {
        Ok(l) => l,
        Err(e) => {
            (d.log)(&format!("inbox sweep: {}", e.0));
            return Vec::new();
        }
    };
    listed.retain(|o| o.size > 0);
    listed.sort_by_key(|o| std::cmp::Reverse(o.uploaded));
    let bodies = listed
        .into_iter()
        .take(batch)
        .map(|o| {
            serde_json::json!({
                "action": STORE_ACTIONS[0],
                "object": { "key": o.key, "size": o.size },
                "eventTime": o.uploaded,
            })
        })
        .collect();
    let outcomes = handle_batch(d, bodies, connect).await;
    let retry = outcomes.iter().filter(|o| **o == Outcome::Retry).count();
    (d.log)(&format!(
        "inbox sweep: done={} retry={retry}",
        outcomes.len() - retry
    ));
    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use serde_json::json;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn sweeps_only_the_list_for_leave_users() {
        let tenant = Uuid::from_u128(1).to_string();
        let get = Method::GET;
        let user = headers(&[
            ("X-Tenant-ID", tenant.as_str()),
            ("X-User-Permissions", "leave.use"),
        ]);
        let admin = headers(&[("X-Tenant-ID", tenant.as_str()), ("X-User-Role", "admin")]);
        assert!(may_sweep(&get, SWEEP_PATH, &user));
        assert!(may_sweep(&get, SWEEP_PATH, &admin));
        // 別の method・別の path
        assert!(!may_sweep(&Method::POST, SWEEP_PATH, &admin));
        assert!(!may_sweep(&get, "/api/leave/pages", &admin));
        assert!(!may_sweep(&get, "/api/leave/documents/", &admin));
        // tenant ヘッダーが無い・uuid でない・前後に空白 (require_tenant_header は trim しない)・文字列にならない
        assert!(!may_sweep(
            &get,
            SWEEP_PATH,
            &headers(&[("X-User-Role", "admin")])
        ));
        for bad in ["x".to_owned(), format!(" {tenant}"), format!("{tenant} ")] {
            let h = headers(&[("X-Tenant-ID", bad.as_str()), ("X-User-Role", "admin")]);
            assert!(!may_sweep(&get, SWEEP_PATH, &h));
        }
        let mut opaque = admin.clone();
        opaque.insert("X-Tenant-ID", HeaderValue::from_bytes(b"\xff").unwrap());
        assert!(!may_sweep(&get, SWEEP_PATH, &opaque));
        // leave.use も admin も無い
        let viewer = headers(&[("X-Tenant-ID", tenant.as_str()), ("X-User-Role", "viewer")]);
        assert!(!may_sweep(&get, SWEEP_PATH, &viewer));
    }

    #[test]
    fn ticket_is_parsed_or_none() {
        let t = Uuid::from_u128(7);
        let ok = json!({ "tenantId": t, "username": "scan", "sha256": "ab" });
        assert_eq!(
            parse_ticket(ok),
            TicketCheck::Valid(Ticket {
                tenant_id: t,
                sha256: "ab".to_owned()
            })
        );
        let invalid = TicketCheck::Invalid;
        assert_eq!(parse_ticket(serde_json::Value::Null), invalid);
        assert_eq!(
            parse_ticket(json!({ "tenantId": "x", "sha256": "ab" })),
            invalid
        );
        assert_eq!(parse_ticket(json!({ "tenantId": t })), invalid);
        // 一時的な失敗だけが Unavailable (ほかの error・形は不正)
        let unavailable = json!({ "error": "unavailable" });
        assert_eq!(parse_ticket(unavailable), TicketCheck::Unavailable);
        assert_eq!(parse_ticket(json!({ "error": "other" })), invalid);
        assert_eq!(parse_ticket(json!({ "error": 1 })), invalid);
    }
}
