# rust-leave-worker

休暇申請書 PDF (1 ページ = 1 枚) を乗務員 (一番星の社員C) と期間に紐づける管理 API の Worker (Refs #1)。
型は ippoan/alc-vein-worker・alc-dtako-worker と同じ。直下 = Worker (wasm 専用)、`crates/leave/` = 口と検証。

## コマンド
```bash
bash scripts/check-exposure.sh && bash scripts/check-exposure-test.sh
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --target wasm32-unknown-unknown --release -- -D warnings
bash scripts/fetch-migrations.sh   # テストの準備の SQL (版は scripts/ALC_MIGRATIONS_REV)
LEAVE_TEST_ADMIN_DATABASE_URL=postgresql://postgres:<pw>@127.0.0.1:<port>/postgres cargo test --workspace
worker-build --release && npx wrangler@4.144.0 deploy --dry-run
```
実 DB のテストは使い捨ての postgres:17 に向ける (全 migration を流す)。URL が無ければ失敗する (skip しない)。

## 規範
- 口は `/api/leave/…` だけ。**全部の口が管理者限定** (`auth::is_admin` の 1 か所)。口を足したら 403 のテストも足す。
- 公開範囲を弱めない: 本番は Service Binding だけ (`workers_dev`/`preview_urls` = false、route 無し)。
  `ICHIBAN_DB` (本番の社員マスタ)・Hyperdrive は本番だけ、staging の R2 は staging の bucket。
  `wrangler.toml` の表の順を変えない (新しい表は placement より後)。
- DB は `PgClient::tenant_tx` + `query_typed` 系だけ。SQL は `crates/leave/src/pg.rs` の 1 か所。
  名前付き prepared statement を呼ばない。WHERE に tenant_id も明示する。
- 応答・ログに識別子・入力値・生のエラー文を出さない (固定の語だけ)。
- `coverage_100.toml` の登録を外さない。CI の本数 (lib 24・api_db 23) はテストを足したら上げる。
- public repo: ホスト名・tenant UUID・個人情報を書かない。タグ `v*` = 本番 (Tag Release を手動)。
- kit (alc-core-wasm / alc-worker-db) の rev は直下 `Cargo.toml` の 1 か所、vein と同じ rev。
