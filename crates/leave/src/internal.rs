//! 取り込みの本体 [`ingest_pdf`] (スキャンの inbox の取り込み [`crate::inbox`] が呼ぶ)。
//!
//! 流れ: ページ数 ([`count_pages`]) → R2 に put → `tenant_tx` で全ページを INSERT
//! (R2 の await は transaction の外 = `tenant_tx` は INSERT の 1 回だけ)。
//!
//! ログは固定の語だけ。PDF の中身は出さない。

use alc_worker_db::PgClient;
use chrono::{DateTime, Utc};
use futures_util::lock::Mutex;
use uuid::Uuid;

use crate::ingest::{count_pages, seeded_object_key};
use crate::pg;
use crate::routes::LogSink;
use crate::store::PdfStore;

/// 取り込む PDF (tenant は呼び手が確定済み: auth-worker が検証したチケット)
pub struct NewPdf {
    pub tenant_id: Uuid,
    /// `%PDF-` で始まり、大きさの上限以内 (呼び手が確かめる)
    pub pdf: Vec<u8>,
    pub received_at: DateTime<Utc>,
    /// `leave_pages.mail_from` (スキャンは `None` = NULL)
    pub mail_from: Option<String>,
    pub subject: Option<String>,
    /// R2 の key をこれから決定的に作る (Queue の再配送で同じ key・同じ行になる)
    pub key_seed: String,
}

/// 取り込みの本体が返す失敗
#[derive(Debug, PartialEq)]
pub enum IngestFailure {
    /// 意図して捨てる (ページ数が読めない・多すぎる)。語は固定
    Drop(&'static str),
    /// R2 か DB の失敗 (一時的なもの。Queue は retry)。段の名前はログに出してある
    Failed,
}

/// 取り込みの本体 (Queue の consumer が呼ぶ): ページ数 ([`count_pages`]) → R2 に put → 全ページを INSERT。
/// INSERT が失敗しても R2 の object は消さない (key は決定的。前の配送で入った行がその object を指していることがあり、
/// 再試行は同じ key に put し直すため)。R2 の await は transaction の外 (`tenant_tx` は INSERT の 1 回だけ)
pub async fn ingest_pdf(
    pg: &Mutex<PgClient>,
    pdfs: &dyn PdfStore,
    log: &LogSink,
    p: NewPdf,
) -> Result<usize, IngestFailure> {
    let pages = count_pages(&p.pdf).map_err(IngestFailure::Drop)?;
    let key = seeded_object_key(p.tenant_id, p.received_at, &p.key_seed);
    if pdfs.put(&key, p.pdf).await.is_err() {
        log("ingest: r2 put failed");
        return Err(IngestFailure::Failed);
    }
    let new_pages = pg::NewPages {
        r2_key: key,
        mail_from: p.mail_from,
        subject: p.subject,
        received_at: p.received_at,
        pages: i32::try_from(pages).unwrap_or(i32::MAX),
    };
    let inserted = pg::insert_pages(&mut *pg.lock().await, p.tenant_id, new_pages).await;
    if let Err(e) = inserted {
        log(&format!("ingest insert: {}", alc_worker_db::kind(&e)));
        return Err(IngestFailure::Failed);
    }
    Ok(pages)
}
