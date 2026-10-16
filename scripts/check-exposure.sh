#!/usr/bin/env bash
# rust-leave-worker が外から素で届かないことを wrangler.toml で検査する (ippoan/alc-dtako-worker の同名の script の写し、元は ippoan/alc-vein-worker。
# Refs #1)。
#
# この Worker は JWT を検証せず X-Tenant-ID を信頼するので、本番の到達経路は auth-worker からの
# Service Binding だけでなければならない (ippoan/rust-alc-api#556 と同じ穴を開けない)。
#   - トップレベル (本番): workers_dev = false と preview_urls = false が明示されている
#   - どこにも (トップレベル・env 配下とも) route / routes が無い (custom_domain も routes の中に書く)
#   - どの env (staging を含む) にも workers_dev = true / preview_urls = true が無い
#     (入口は Service Binding だけ。トップレベルの true を env が継承するのも不可)
#   - どの vars にも ALLOW_INSECURE_DB (DB への平文接続を許すローカル専用フラグ) が無い
#   - トップレベル (本番) に平文 (NoTls + trust) で繋ぐ DB の binding が無い: vpc_services /
#     vpc_networks (Workers VPC)。src/db.rs はこの binding を Hyperdrive の binding LEAVE_HYPERDRIVE →
#     文字列 DATABASE_URL より先に見るので、本番に紛れると TLS を強制する経路を飛ばして平文に落ちる。
#     env.staging にだけ置く
#   - containers はどの階層にも無い。migrations (Durable Object の class の定義) もどの階層にも無い
#     (class は別 Worker のもの。この worker は Durable Object の class も Container も持たない)
#   - durable_objects はトップレベルに無い。env 配下で許すのは env.staging の binding 1 件だけで、
#     name = STAGING_DB・class_name = StagingDb・script_name = alc-staging-db-staging の完全一致
#     (staging の DB = ippoan/alc-staging-db の DO。本番がテスト用の DB に繋がらないよう staging にだけ置く)
#   - env 配下 (env.staging を含む) に hyperdrive が無い (ippoan/rust-alc-api#723)。Hyperdrive の binding は本番の DB へ
#     届くので、トップレベル (本番) にだけ置く (staging に置かない)
#   - env 配下の r2_buckets に本番の bucket (bucket_name = "leave-pdfs" / "leave-inbox" 完全一致) が無い
#     (staging が本番の bucket を指すと、本番の申請書を読み書きする・本番の inbox を取り込む)
#   - env 配下 (env.staging を含む) の services は、service 名が `-staging` で終わり、かつ binding が一番星用 (ICHIBAN_DB)
#     でないものだけ。一番星 Worker は本番の社員マスタを返すので staging から引かせない (一番星 Worker に staging は無い)。
#     ほかの service も本番の Worker (auth-worker 等) を staging から呼ばない
# 違えば exit 1。CI で毎回走らせる。陰性対照は scripts/check-exposure-test.sh。
#
#   bash scripts/check-exposure.sh [wrangler.toml]
set -euo pipefail

TOML="${1:-$(dirname "$0")/../wrangler.toml}"

python3 - "$TOML" <<'PY'
import sys
import tomllib

path = sys.argv[1]
with open(path, "rb") as f:
    cfg = tomllib.load(f)

errors = []

def err(msg):
    errors.append(msg)
    print(f"::error file={path}::{msg}")

# 本番 (トップレベル): 明示的に false
for key in ("workers_dev", "preview_urls"):
    if cfg.get(key) is not False:
        err(f"トップレベルに {key} = false がない (本番が外から直接届く)")

envs = cfg.get("env", {})

for scope, table in [("トップレベル", cfg)] + [(f"env.{n}", e) for n, e in envs.items()]:
    for key in ("route", "routes"):
        if key in table:
            err(f"{scope} に {key} がある (外から直接届く)")

# どの env も公開しない (workers_dev / preview_urls は env へ継承される)
for name, e in envs.items():
    for key in ("workers_dev", "preview_urls"):
        if e.get(key, cfg.get(key)) is True:
            err(f"env.{name} の {key} が true (どの env も workers.dev / preview URL を開けない)")

# 平文の DB 接続を許すフラグはローカル (wrangler dev --var / .dev.vars) にだけ置く
for scope, table in [("トップレベル", cfg)] + [(f"env.{n}", e) for n, e in envs.items()]:
    if "ALLOW_INSECURE_DB" in table.get("vars", {}):
        err(f"{scope} の vars に ALLOW_INSECURE_DB がある (DB 接続が平文に落ちうる。ローカル専用)")

# 平文の DB 経路の binding は staging 専用 (db.rs が secret より先に見るので、本番にあると TLS を飛ばす)
for key in ("vpc_services", "vpc_networks"):
    if cfg.get(key):
        err(f"トップレベルに {key} がある (本番の DB 接続が平文の VPC 経路に落ちる。env.staging にだけ置く)")

# この worker は Container も DO の class (migrations) も持たない (どの階層にも置かない)
for scope, table in [("トップレベル", cfg)] + [(f"env.{n}", e) for n, e in envs.items()]:
    for key in ("containers", "migrations"):
        if key in table:
            err(f"{scope} に {key} がある (この worker は Container も Durable Object の class も持たない)")

# Durable Object の binding は env.staging の STAGING_DB 1 件だけ (別 Worker alc-staging-db-staging の class StagingDb)。
# トップレベル (本番) に置くと本番がテスト用の DB に繋がる。env.staging 以外 (local 等) にも置かない
ALLOWED_DO = {"name": "STAGING_DB", "class_name": "StagingDb", "script_name": "alc-staging-db-staging"}
if "durable_objects" in cfg:
    err("トップレベルに durable_objects がある (本番の DB 接続が staging の DB に落ちる。env.staging にだけ置く)")
for name, e in envs.items():
    if "durable_objects" not in e:
        continue
    do = e["durable_objects"]
    if name != "staging":
        err(f"env.{name} に durable_objects がある (env.staging の STAGING_DB 1 件だけを許す)")
    elif set(do) != {"bindings"} or do["bindings"] != [ALLOWED_DO]:
        err("env.staging の durable_objects が STAGING_DB (class_name = StagingDb・script_name = alc-staging-db-staging) の 1 件だけでない")

# Hyperdrive の binding は本番の DB へ届く。staging を含め、env 配下には置かない
for name, e in envs.items():
    if e.get("hyperdrive"):
        err(f"env.{name} に hyperdrive がある (本番の DB へ届く binding。トップレベルにだけ置く)")

# 本番の R2 の bucket を env 配下 (staging を含む) から指さない (本番の object を上書きする・本番の inbox を取り込む)
for name, e in envs.items():
    for b in e.get("r2_buckets", []):
        if b.get("bucket_name") in ("leave-pdfs", "leave-inbox"):
            err(f"env.{name} の r2_buckets が本番の bucket {b['bucket_name']} を指している (env 用の bucket を指すこと)")

# env 配下の Service Binding は `-staging` の service だけ (本番の auth-worker を staging から呼ばない)。
# 一番星 Worker (ICHIBAN_DB) は本番の社員マスタを返すので、service 名に関わらず env 配下に置かない
for name, e in envs.items():
    for s in e.get("services", []):
        if s.get("binding") == "ICHIBAN_DB":
            err(f"env.{name} の services に ICHIBAN_DB がある (一番星 Worker は本番の社員マスタを返す。トップレベルにだけ置く)")
        elif not str(s.get("service", "")).endswith("-staging"):
            err(f"env.{name} の services が -staging でない service を指している (本番の Worker へ届く)")

if errors:
    sys.exit(1)
print(f"OK: {path} は本番が workers_dev / preview_urls = false・route 無しで、どの env も workers.dev を開けない")
PY
