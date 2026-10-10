//! 休暇申請書 (PDF の 1 ページ = 申請書 1 枚) を乗務員と期間に紐づける管理 API の route の crate (Refs #1)。
//! 接続も R2・一番星 Worker の実装も持たない (worker が [`routes::LeaveState`] に差す)。
//!
//! - [`auth`] — 管理者の判定 (1 か所) と操作した人の識別子
//! - [`settings`] / [`pages`] — 入力の検証と応答の形 (純粋関数)
//! - [`ingest`] / [`internal`] — メール取り込みの口 `/internal/ingest` (管理 API の外。共有 secret で守る) と入力の検証
//! - [`ichiban`] — 一番星 Worker の口の抽象と応答の解釈
//! - [`store`] — PDF の置き場の抽象
//! - [`pg`] — SQL を流す 1 か所 (`tenant_tx` + `query_typed` 系だけ)
//! - [`kinds`] / [`grants`] / [`reconcile`] — 品名N の正規化・有給の付与と残・突き合わせ (純粋関数)
//! - [`ledger`] — カレンダーと付与台帳の口
//! - [`cron`] — 毎朝の突き合わせと通知 (worker の scheduled から呼ぶ)
//! - [`routes`] — 口

pub mod auth;
pub mod cron;
pub mod grants;
pub mod ichiban;
pub mod ingest;
pub mod internal;
pub mod kinds;
pub mod ledger;
pub mod pages;
pub mod pg;
pub mod reconcile;
pub mod routes;
pub mod settings;
pub mod store;

pub use internal::{ingest_router, IngestState, INGEST_PATH};
pub use routes::{tenant_router, Clock, LeaveState, LogSink, TEST_NOTIFY_PATH, TEST_NOTIFY_TEXT};
