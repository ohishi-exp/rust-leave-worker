//! スキャンの inbox の取り込み (Queue `leave-inbox` の consumer。worker の `#[event(queue)]` が 1 メッセージずつ呼ぶ)。
//!
//! 複合機 → nuxt-leave の WebDAV が R2 bucket `leave-inbox` に PDF を置き、`customMetadata.ticket` に auth-worker の
//! 署名付きチケットを載せる。R2 のイベント通知 (object-create) が Queue に積み、ここへ届く。
//!
//! 流れ: 通知の JSON ([`InboxEvent`]) → 対象の action か・大きさ → inbox の object を読む → チケットを auth-worker の
//! RPC `verifyTicket` で検証 ([`TicketVerifier`]。**この Worker は鍵を持たない**) → 本文の sha256 がチケットと一致 →
//! [`check_pdf_bytes`] → [`ingest_pdf`] (key は inbox の object key から決定的に作る = 再配送でも二重にならない) →
//! inbox の object を消して ack。
//!
//! - **tenant はチケットの検証結果だけから取る** (inbox の key・メタデータからは読まない)。送信元の許可リストは見ない
//!   (メール経路だけのもの。スキャンは資格情報で会社が確定している)
//! - 取り込めない object (チケットが無い・不正・sha256 の不一致・PDF でない・読めない) は消して ack (再試行しない)
//! - R2・DB・RPC の一時的な失敗は retry (Queue の max_retries の後は dead letter queue)。object が既に無ければ ack
//! - ログは段の名前と固定の語だけ (チケット・PDF の中身・object key は出さない。tenant は取り込み後のログに出す)

use alc_worker_db::PgClient;
use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;
use futures_util::lock::Mutex;
use serde::Deserialize;
use uuid::Uuid;

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
    /// 受信時刻 (`leave_pages.received_at`)。再配送でも同じ値なので、決定的な key の年月もぶれない
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

/// inbox の bucket (binding `LEAVE_INBOX`)。Worker での実装は直下の `src/bindings.rs`
pub trait Inbox: Send + Sync {
    /// 無ければ `Ok(None)`
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<InboxObject>, StoreError>>;
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), StoreError>>;
}

/// RPC の失敗 (一時的なもの。retry)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VerifyError;

/// auth-worker の `verifyTicket(ticket, purpose)`。戻り値は JSON のまま (`null` か `{tenantId, username, sha256}`)。
/// 形の解釈は [`parse_ticket`] (形が違えば不正なチケットとして扱う)
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

/// `verifyTicket` の戻りの解釈。`null`・形の違うものは `None`
pub fn parse_ticket(v: serde_json::Value) -> Option<Ticket> {
    serde_json::from_value(v).ok()
}

/// メッセージの扱い
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    Ack,
    Retry,
}

pub struct InboxDeps<'a> {
    pub pg: &'a Mutex<PgClient>,
    /// 本保存の bucket (`LEAVE_R2`)
    pub pdfs: &'a dyn PdfStore,
    pub inbox: &'a dyn Inbox,
    pub verifier: &'a dyn TicketVerifier,
    pub log: &'a LogSink,
}

/// 取り込めない object を消して ack (消せなければ retry。次の配送でもう一度消す)
async fn discard(d: &InboxDeps<'_>, key: &str, reason: &str) -> Outcome {
    (d.log)(&format!("inbox dropped: {reason}"));
    settle(d, key).await
}

/// inbox の object を消して ack。消せなければ retry (取り込みは冪等なので、もう一度通してよい)
async fn settle(d: &InboxDeps<'_>, key: &str) -> Outcome {
    match d.inbox.delete(key).await {
        Ok(()) => Outcome::Ack,
        Err(e) => {
            (d.log)(&format!("inbox: delete {}", e.0));
            Outcome::Retry
        }
    }
}

/// 1 メッセージを処理する
pub async fn handle(d: &InboxDeps<'_>, body: serde_json::Value) -> Outcome {
    let Ok(event) = serde_json::from_value::<InboxEvent>(body) else {
        (d.log)("inbox: invalid message");
        return Outcome::Ack;
    };
    if !STORE_ACTIONS.contains(&event.action.as_str()) || event.object.size == Some(0) {
        (d.log)("inbox: skipped");
        return Outcome::Ack;
    }
    let key = event.object.key.as_str();
    if event.object.size.is_some_and(|s| s > MAX_PDF_BYTES as u64) {
        return discard(d, key, "pdf_too_large").await;
    }
    let object = match d.inbox.get(key).await {
        Ok(Some(o)) => o,
        Ok(None) => {
            (d.log)("inbox: object gone");
            return Outcome::Ack;
        }
        Err(e) => {
            (d.log)(&format!("inbox: {}", e.0));
            return Outcome::Retry;
        }
    };
    let Some(ticket) = object.ticket.as_deref() else {
        return discard(d, key, "ticket_missing").await;
    };
    let ticket = match d.verifier.verify(ticket, TICKET_PURPOSE).await {
        Ok(v) => parse_ticket(v),
        Err(VerifyError) => {
            (d.log)("inbox: verify failed");
            return Outcome::Retry;
        }
    };
    let Some(ticket) = ticket else {
        return discard(d, key, "ticket_invalid").await;
    };
    if sha256_hex(&object.bytes) != ticket.sha256 {
        return discard(d, key, "sha256_mismatch").await;
    }
    if let Err(reason) = check_pdf_bytes(&object.bytes) {
        return discard(d, key, reason).await;
    }
    let tenant_id = ticket.tenant_id;
    let pdf = NewPdf {
        tenant_id,
        pdf: object.bytes,
        received_at: event.event_time,
        mail_from: None,
        subject: None,
        key_seed: Some(key.to_owned()),
    };
    match ingest_pdf(d.pg, d.pdfs, d.log, pdf).await {
        Ok(pages) => {
            (d.log)(&format!("inbox stored: tenant={tenant_id} pages={pages}"));
            settle(d, key).await
        }
        Err(IngestFailure::Drop(reason)) => discard(d, key, reason).await,
        Err(IngestFailure::Failed) => Outcome::Retry,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ticket_is_parsed_or_none() {
        let t = Uuid::from_u128(7);
        let ok = json!({ "tenantId": t, "username": "scan", "sha256": "ab" });
        assert_eq!(
            parse_ticket(ok),
            Some(Ticket {
                tenant_id: t,
                sha256: "ab".to_owned()
            })
        );
        assert_eq!(parse_ticket(serde_json::Value::Null), None);
        assert_eq!(
            parse_ticket(json!({ "tenantId": "x", "sha256": "ab" })),
            None
        );
        assert_eq!(parse_ticket(json!({ "tenantId": t })), None);
    }
}
