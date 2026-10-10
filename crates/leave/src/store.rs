//! PDF の置き場 (R2 の binding `LEAVE_R2`) の抽象。Worker での実装は直下の `src/bindings.rs`、テストは偽物を差す。
//! 読むだけ (書くのはメール受信の口 = 別タスク)。

use futures_util::future::BoxFuture;

/// 読み出しの失敗。段の名前だけを持つ (key もランタイムの生のエラー文も持たない)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StoreError(pub &'static str);

/// `Send` な future を返す (axum の state と handler が要求する)
pub trait PdfStore: Send + Sync {
    /// key の object の中身。無ければ `Ok(None)`
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>, StoreError>>;
}
