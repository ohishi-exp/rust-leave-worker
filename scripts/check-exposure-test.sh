#!/usr/bin/env bash
# check-exposure.sh の陰性対照 (ippoan/alc-dtako-worker の同名の script の写し、元は ippoan/alc-vein-worker)。wrangler.toml を 1 か所ずつ崩したコピーで
# exit 1 になること、元のままなら exit 0 になることを確かめる。CI で check-exposure.sh の直後に走る。
set -euo pipefail
cd "$(dirname "$0")/.."

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
fail=0

expect() {
  local want="$1" label="$2" toml="$3"
  if bash scripts/check-exposure.sh "$toml" >"$tmp/out" 2>&1; then got=0; else got=1; fi
  if [ "$got" = "$want" ]; then
    echo "ok   ${label} (exit ${got})"
  else
    echo "FAIL ${label}: exit ${got}, want ${want}"; sed 's/^/     /' "$tmp/out"; fail=1
  fi
}

mutate() { # $1 = label, $2 = python の置換式 (s を書き換える)
  python3 - wrangler.toml "$tmp/w.toml" "$2" <<'PY'
import re
import sys
s = open(sys.argv[1]).read()
before = s
exec(sys.argv[3])
assert s != before, "mutation did not change wrangler.toml"
open(sys.argv[2], "w").write(s)
PY
  expect 1 "$1" "$tmp/w.toml"
}

expect 0 "wrangler.toml そのまま" wrangler.toml
mutate "staging で workers_dev = true" \
  's = s.replace("name = \"rust-leave-worker-staging\"\nworkers_dev = false", "name = \"rust-leave-worker-staging\"\nworkers_dev = true", 1)'
mutate "staging で preview_urls = true" \
  's = s.replace("preview_urls = false\n\n[env.staging.observability]", "preview_urls = true\n\n[env.staging.observability]", 1)'
mutate "staging 以外の env で workers_dev = true" \
  's += "\n[env.dev]\nname = \"leave-dev\"\nworkers_dev = true\n"'
mutate "staging 以外の env で preview_urls = true" \
  's += "\n[env.preview]\nname = \"leave-preview\"\npreview_urls = true\n"'
mutate "staging 以外の env がトップレベルの true を継承" \
  's = re.sub(r"^workers_dev = false$", "workers_dev = true", s, count=1, flags=re.M) + "\n[env.other]\nname = \"x\"\n"'
mutate "トップレベルの vars に ALLOW_INSECURE_DB" \
  's = s.replace("[build]", "[vars]\nALLOW_INSECURE_DB = \"1\"\n\n[build]", 1)'
mutate "staging の vars に ALLOW_INSECURE_DB" \
  's = s.replace("[env.staging.vars]\n", "[env.staging.vars]\nALLOW_INSECURE_DB = \"1\"\n", 1)'
mutate "トップレベルの workers_dev を true に" \
  's = re.sub(r"^workers_dev = false$", "workers_dev = true", s, count=1, flags=re.M)'
mutate "トップレベルの preview_urls を消す" \
  's = re.sub(r"^preview_urls = false\n", "", s, count=1, flags=re.M)'
mutate "本番に routes を足す" \
  's = s.replace("[build]", "routes = [{ pattern = \"leave.example.com\", custom_domain = true }]\n\n[build]", 1)'
mutate "staging に route を足す" \
  's = s.replace("[env.staging.observability]", "route = \"leave.example.com/*\"\n\n[env.staging.observability]", 1)'
mutate "本番に vpc_services (平文の DB 経路) を足す" \
  's = s.replace("[build]", "[[vpc_services]]\nbinding = \"LEAVE_DB_VPC\"\nservice_id = \"x\"\nremote = true\n\n[build]", 1)'
# DO の binding は env.staging の STAGING_DB 1 件 (name・class_name・script_name の完全一致) だけ許す。
# 元の wrangler.toml が持つその 1 件を崩す / 別の形を足す
mutate "staging の DO の script_name が別名" \
  's = s.replace("script_name = \"alc-staging-db-staging\"", "script_name = \"alc-staging-db\"", 1)'
mutate "staging の DO に script_name が無い (この worker 自身の class を指す)" \
  's = s.replace("script_name = \"alc-staging-db-staging\"\n", "", 1)'
mutate "staging の DO の class_name が違う" \
  's = s.replace("class_name = \"StagingDb\"", "class_name = \"LeaveDb\"", 1)'
mutate "staging の DO の binding 名が違う" \
  's = s.replace("name = \"STAGING_DB\"", "name = \"LEAVE_DB\"", 1)'
mutate "staging の DO を 2 件にする" \
  's = s.replace("[env.staging.version_metadata]", "[[env.staging.durable_objects.bindings]]\nname = \"OTHER_DB\"\nclass_name = \"OtherDb\"\nscript_name = \"alc-staging-db-staging\"\n\n[env.staging.version_metadata]", 1)'
mutate "トップレベル (本番) に DO の binding を足す" \
  's = s.replace("[build]", "[[durable_objects.bindings]]\nname = \"STAGING_DB\"\nclass_name = \"StagingDb\"\nscript_name = \"alc-staging-db-staging\"\n\n[build]", 1)'
mutate "staging 以外の env (local) に DO の binding を足す" \
  's += "\n[[env.local.durable_objects.bindings]]\nname = \"STAGING_DB\"\nclass_name = \"StagingDb\"\nscript_name = \"alc-staging-db-staging\"\n"'
mutate "トップレベルに migrations (DO の class の定義) を足す" \
  's = s.replace("[build]", "[[migrations]]\ntag = \"v1\"\nnew_sqlite_classes = [\"LeaveDb\"]\n\n[build]", 1)'
mutate "staging に migrations を足す" \
  's = s.replace("[env.staging.version_metadata]", "[[env.staging.migrations]]\ntag = \"v1\"\nnew_sqlite_classes = [\"LeaveDb\"]\n\n[env.staging.version_metadata]", 1)'
mutate "本番に containers を足す" \
  's = s.replace("[build]", "[[containers]]\nclass_name = \"X\"\nimage = \"./x\"\n\n[build]", 1)'
mutate "staging に containers を足す" \
  's = s.replace("[env.staging.version_metadata]", "[[env.staging.containers]]\nclass_name = \"X\"\nimage = \"./x\"\n\n[env.staging.version_metadata]", 1)'
mutate "staging に hyperdrive (本番の DB へ届く binding) を足す" \
  's = s.replace("[env.staging.observability]", "[[env.staging.hyperdrive]]\nbinding = \"LEAVE_HYPERDRIVE\"\nid = \"x\"\n\n[env.staging.observability]", 1)'

mutate "staging の r2_buckets が本番の bucket leave-pdfs を指す" \
  's = s.replace("bucket_name = \"leave-pdfs-staging\"", "bucket_name = \"leave-pdfs\"", 1)'
mutate "staging に services (一番星 Worker = 本番の社員マスタ) を足す" \
  's = s.replace("[env.staging.observability]", "[[env.staging.services]]\nbinding = \"ICHIBAN_DB\"\nservice = \"ichibanboshi-ichiban\"\n\n[env.staging.observability]", 1)'
mutate "staging に一番星の binding を -staging の service 名で足す" \
  's = s.replace("[env.staging.observability]", "[[env.staging.services]]\nbinding = \"ICHIBAN_DB\"\nservice = \"ichibanboshi-ichiban-staging\"\n\n[env.staging.observability]", 1)'
mutate "staging の services が本番の auth-worker を指す" \
  's = s.replace("service = \"auth-worker-staging\"", "service = \"auth-worker\"", 1)'
mutate "staging の r2_buckets が本番の bucket leave-inbox を指す" \
  's = s.replace("bucket_name = \"leave-inbox-staging\"", "bucket_name = \"leave-inbox\"", 1)'

exit "$fail"
