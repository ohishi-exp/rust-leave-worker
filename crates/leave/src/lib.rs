//! 休暇申請書 (PDF の 1 ページ = 申請書 1 枚) を乗務員と期間に紐づける管理 API の route の crate (Refs #1)。
//! 接続も R2・一番星 Worker の実装も持たない (worker が [`routes::LeaveState`] に差す)。
//!
//! - [`auth`] — 管理者の判定 (1 か所) と操作した人の識別子
//! - [`settings`] / [`pages`] — 入力の検証と応答の形 (純粋関数)
//! - [`ichiban`] — 一番星 Worker の口の抽象と応答の解釈
//! - [`store`] — PDF の置き場の抽象
//! - [`pg`] — SQL を流す 1 か所 (`tenant_tx` + `query_typed` 系だけ)
//! - [`routes`] — 口

pub mod auth;
pub mod ichiban;
pub mod pages;
pub mod pg;
pub mod routes;
pub mod settings;
pub mod store;

pub use routes::{tenant_router, LeaveState, LogSink};
