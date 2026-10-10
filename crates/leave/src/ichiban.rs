//! 一番星 Worker (Service Binding `ICHIBAN_DB`、認可なし・binding 専用) への口の抽象と、応答の解釈。
//!
//! Worker での実装 (binding の `fetch`) は直下の `src/bindings.rs`。テストは偽物を差す。
//! 呼ぶ口は 2 本だけ: [`DEPARTMENTS_PATH`] (そのまま中継) と [`EMPLOYEES_PATH`] (tenant の部門で絞る)。

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

pub const DEPARTMENTS_PATH: &str = "/api/sales/departments";
pub const EMPLOYEES_PATH: &str = "/api/leave/employees";

/// 一番星 Worker の応答 (状態コードと本文のバイト列)
#[derive(Debug, Clone, PartialEq)]
pub struct IchibanResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// 呼び出しそのものの失敗 (binding の例外など)。中身は持たない (ログに識別子を出さない)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IchibanError;

/// `GET <path>` を 1 回打つ。`Send` な future を返す (axum の state と handler が要求する)
pub trait IchibanApi: Send + Sync {
    fn get<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<IchibanResponse, IchibanError>>;
}

/// 応答の値を文字列にする (部門C が数値で来ても文字列で比べる)
fn as_code(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.trim().to_owned()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `GET /api/leave/employees` の本文 (`{"source_table": "...", "data": [{"employee_code", "employee_name",
/// "dept_code", "hire_date", "retire_date"}, ...]}`) を読み、`dept_code` が `dept_codes` に入る行だけを残す。
/// **`dept_codes` が空なら空** (部門を選んでいない tenant に他社の社員を見せない)。
/// 形が違えば `Err` (502 にする)。行の項目はそのまま返す
pub fn filter_employees(body: &[u8], dept_codes: &[String]) -> Result<Value, &'static str> {
    let parsed: Value = serde_json::from_slice(body).map_err(|_| "ichiban_bad_response")?;
    let Value::Object(mut top) = parsed else {
        return Err("ichiban_bad_response");
    };
    let Some(Value::Array(rows)) = top.remove("data") else {
        return Err("ichiban_bad_response");
    };
    let data: Vec<Value> = rows
        .into_iter()
        .filter(|row| {
            row.get("dept_code")
                .and_then(as_code)
                .is_some_and(|c| dept_codes.iter().any(|d| d == &c))
        })
        .collect();
    let mut out = Map::new();
    if let Some(source) = top.remove("source_table") {
        out.insert("source_table".into(), source);
    }
    out.insert("data".into(), Value::Array(data));
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(v: Value) -> Vec<u8> {
        serde_json::to_vec(&v).unwrap()
    }

    fn sample() -> Vec<u8> {
        body(json!({
            "source_table": "社員ﾏｽﾀ",
            "data": [
                {"employee_code": "1021", "employee_name": "甲", "dept_code": "10", "hire_date": "2020-04-01", "retire_date": null},
                {"employee_code": "1022", "employee_name": "乙", "dept_code": "20", "hire_date": null, "retire_date": null},
                {"employee_code": "1023", "employee_name": "丙", "dept_code": 30, "hire_date": null, "retire_date": null},
                {"employee_code": "1024", "employee_name": "丁", "dept_code": null},
                {"employee_code": "1025", "employee_name": "戊"}
            ]
        }))
    }

    fn codes(v: &Value) -> Vec<&str> {
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["employee_code"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn keeps_only_tenant_departments() {
        let out = filter_employees(&sample(), &["10".into(), "30".into()]).unwrap();
        assert_eq!(codes(&out), vec!["1021", "1023"]);
        assert_eq!(out["source_table"], "社員ﾏｽﾀ");
        // 行の項目はそのまま
        assert_eq!(out["data"][0]["hire_date"], "2020-04-01");
    }

    #[test]
    fn empty_dept_codes_is_empty() {
        let out = filter_employees(&sample(), &[]).unwrap();
        assert!(codes(&out).is_empty());
    }

    #[test]
    fn source_table_is_optional() {
        let out = filter_employees(&body(json!({"data": []})), &["10".into()]).unwrap();
        assert_eq!(out, json!({"data": []}));
    }

    #[test]
    fn bad_shapes_are_errors() {
        for bad in [
            b"not json".to_vec(),
            body(json!([1, 2])),
            body(json!({"source_table": "x"})),
            body(json!({"data": {"a": 1}})),
        ] {
            assert_eq!(
                filter_employees(&bad, &["10".into()]),
                Err("ichiban_bad_response")
            );
        }
    }
}
