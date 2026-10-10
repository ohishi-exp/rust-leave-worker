//! PDF の置き場 (R2 の binding `LEAVE_R2`) の抽象。Worker での実装は直下の `src/bindings.rs`、テストは偽物を差す。
//! 読むのは管理 API、書く・消すのはメール取り込みの口 ([`crate::internal`])。

use futures_util::future::BoxFuture;

/// 読み出しの失敗。段の名前だけを持つ (key もランタイムの生のエラー文も持たない)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StoreError(pub &'static str);

/// `Send` な future を返す (axum の state と handler が要求する)
pub trait PdfStore: Send + Sync {
    /// key の object の中身。無ければ `Ok(None)`
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>, StoreError>>;
    /// key に中身を置く (同じ key があれば上書き。key はサーバ側で作った一意なもの)
    fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), StoreError>>;
    /// key の object を消す (DB に入れられなかった object の取り消し)
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), StoreError>>;
}
