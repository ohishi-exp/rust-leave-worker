# rust-leave-worker

休暇申請書 PDF を乗務員と期間に紐づける管理 API の Worker (Refs #1)。型は alc-dtako-worker と同じ。
直下 = Worker (wasm 専用)、`crates/leave/` = 口と検証。

## コマンド
```bash
bash scripts/check-exposure.sh && bash scripts/check-exposure-test.sh
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --target wasm32-unknown-unknown --release -- -D warnings
bash scripts/fetch-migrations.sh   # 版は scripts/ALC_MIGRATIONS_REV
LEAVE_TEST_ADMIN_DATABASE_URL=postgresql://postgres:<pw>@127.0.0.1:<port>/postgres cargo test --workspace
worker-build --release && npx wrangler@4.144.0 deploy --dry-run
```
DB テストは使い捨ての postgres:17 へ。URL が無ければ失敗 (skip しない)。

## 規範
- 管理 API の口は `/api/leave/…` だけ。**全部管理者限定** (`auth::is_admin` の 1 か所)。403 のテストも足す。
  例外は `POST /internal/ingest` と `POST /internal/scan-check` (受信アドレスと tenant の照合。204 / 403 `mailbox_mismatch`。
  どちらも共有 secret。`internal.rs` を見る) と定時実行 (`#[event(scheduled)]`、本番だけ・
  `crates/leave/src/cron.rs`。tenant をまたぐのは `leave_enabled_tenants()` だけ)と、スキャンの inbox の Queue
  (`#[event(queue)]`、`crates/leave/src/inbox.rs`。tenant は auth-worker の `verifyTicket` の結果だけから取る)。
- 公開範囲を弱めない: 本番・staging とも Service Binding だけ (どの env も `workers_dev`/`preview_urls` = false、route 無し)。
- DB の経路 (`src/db.rs`、上から見て最初にあるもの): staging は **DO の binding `STAGING_DB`** (別 Worker
  `alc-staging-db-staging` の `StagingDb` = PGlite。user は本番と同じ `alc_api_rt`) が最優先 → `LEAVE_DB_VPC` (docker の
  PgBouncer。**切り戻し用**) → 本番 Hyperdrive → ローカル `DATABASE_URL`。**切り戻しは `STAGING_DB` の binding を外すだけ**。
  binding が在るのに繋がらないときは次の段へ落とさず 500。DO の binding は **env.staging に 1 件だけ**
  (name/class_name/script_name 完全一致。トップレベル・他の env・migrations は禁止。`check-exposure.sh`)。
  `ICHIBAN_DB`・`AUTH_WORKER` (LINE WORKS 送信)・Hyperdrive・`[triggers]` (cron) は本番だけ (staging / local の crons は空)、
  staging の R2・Queue は staging のもの、env 下の services は `-staging` の service だけ (一番星用は置かない)。
  `wrangler.toml` の表の順を変えない (新しい表は placement より後)。
- DB は `PgClient::tenant_tx` + `query_typed` 系だけ。SQL は `crates/leave/src/pg.rs` の 1 か所。
  名前付き prepared statement 禁止。WHERE に tenant_id も明示する。
- 応答・ログには固定の語だけ (識別子・入力値・生のエラー文を出さない)。
- `coverage_100.toml` の登録を外さない。CI の本数 (lib 82・api_db 72・cron_db 10) はテストを足したら上げる。
- public repo: ホスト名・tenant UUID・個人情報を書かない。`v*` タグ = 本番 (Tag Release 手動)。
- kit の rev は直下 `Cargo.toml` の 1 か所 (alc-vein-worker と同じ rev)。
