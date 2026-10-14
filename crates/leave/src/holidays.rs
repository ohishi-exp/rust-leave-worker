//! 日本の祝日 (労災の紐づけを延ばすとき、行が無くても続きとみなす日。[`crate::reconcile::rousai_extension`])。
//!
//! 社内の既存の取り方 (社内 PHP の HolidaysTrait、ohishi-exp/rust-ichibanboshi#119) に揃えて、holidays-jp の
//! `GET {year}/date.json` (`{"YYYY-MM-DD": "祝日名", …}`) を年ごとに取る。Worker での実装 (外向きの `fetch`) は直下の
//! `src/bindings.rs`。テストは偽物を差す。
//!
//! **取れなかった年は祝日なし** (= 日曜だけ飛ばす。延長が短くなる側に倒す) で、ログに固定語 [`UNAVAILABLE`] を出す。
//! 年末年始は holidays-jp に無いので平日のまま。

use std::collections::BTreeSet;

use chrono::{Datelike, NaiveDate};
use futures_util::future::BoxFuture;
use serde_json::Value;

/// holidays-jp の年ごとの口 (`{year}/date.json` を付ける)
pub const HOLIDAYS_BASE_URL: &str = "https://holidays-jp.github.io/api/v1/";

/// 取得に失敗したときのログの固定語
pub const UNAVAILABLE: &str = "holidays_unavailable";

/// 年 `year` の口の URL
pub fn holidays_url(year: i32) -> String {
    format!("{HOLIDAYS_BASE_URL}{year}/date.json")
}

/// 応答 (状態コードと本文のバイト列)
#[derive(Debug, Clone, PartialEq)]
pub struct HolidaysResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// 呼び出しそのものの失敗。中身は持たない (生のエラー文をログに出さない)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HolidaysError;

/// 年 `year` の祝日の口を 1 回打つ
pub trait HolidaysApi: Send + Sync {
    fn year(&self, year: i32) -> BoxFuture<'_, Result<HolidaysResponse, HolidaysError>>;
}

/// `{"YYYY-MM-DD": "祝日名", …}` の本文を日付の集合にする。形が違えば `Err` (日付と読めないキーも)
pub fn parse_holidays(body: &[u8]) -> Result<BTreeSet<NaiveDate>, &'static str> {
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(body) else {
        return Err("holidays_bad_response");
    };
    map.keys()
        .map(|k| NaiveDate::parse_from_str(k, "%Y-%m-%d").map_err(|_| "holidays_bad_response"))
        .collect()
}

/// `from`〜`to` (両端含む) に掛かる年の祝日を取る (`from` > `to` なら何も呼ばない)。
/// 1 年でも取れなければ (呼び出しの失敗・2xx 以外・形が違う) その年は祝日なしで、戻り値の 2 つ目が `true`
pub async fn load(
    api: &dyn HolidaysApi,
    from: NaiveDate,
    to: NaiveDate,
) -> (BTreeSet<NaiveDate>, bool) {
    let mut out = BTreeSet::new();
    let mut unavailable = false;
    let last = if from <= to {
        to.year()
    } else {
        from.year() - 1
    };
    for year in from.year()..=last {
        let got = match api.year(year).await {
            Ok(r) if (200..300).contains(&r.status) => parse_holidays(&r.body).ok(),
            _ => None,
        };
        match got {
            Some(days) => out.extend(days),
            None => unavailable = true,
        }
    }
    (out, unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn url_of_year() {
        assert_eq!(
            holidays_url(2026),
            "https://holidays-jp.github.io/api/v1/2026/date.json"
        );
    }

    #[test]
    fn parse_keys_as_dates() {
        let body = r#"{"2026-09-21":"敬老の日","2026-09-22":"国民の休日","2026-09-23":"秋分の日"}"#;
        assert_eq!(
            parse_holidays(body.as_bytes()).unwrap(),
            BTreeSet::from([d("2026-09-21"), d("2026-09-22"), d("2026-09-23")])
        );
        assert_eq!(parse_holidays(b"{}").unwrap(), BTreeSet::new());
        assert_eq!(parse_holidays(b"[]"), Err("holidays_bad_response"));
        assert_eq!(parse_holidays(b"not json"), Err("holidays_bad_response"));
        assert_eq!(
            parse_holidays(r#"{"2026/09/21":"敬老の日"}"#.as_bytes()),
            Err("holidays_bad_response")
        );
    }

    struct Fake {
        years: HashMap<i32, Result<HolidaysResponse, HolidaysError>>,
        calls: Mutex<Vec<i32>>,
    }

    impl HolidaysApi for Fake {
        fn year(&self, year: i32) -> BoxFuture<'_, Result<HolidaysResponse, HolidaysError>> {
            self.calls.lock().unwrap().push(year);
            let got = self.years.get(&year).cloned().unwrap_or(Err(HolidaysError));
            Box::pin(async move { got })
        }
    }

    fn ok(status: u16, body: &str) -> Result<HolidaysResponse, HolidaysError> {
        Ok(HolidaysResponse {
            status,
            body: body.as_bytes().to_vec(),
        })
    }

    #[tokio::test]
    async fn load_each_year_and_mark_failures() {
        let fake = Fake {
            years: HashMap::from([
                (2025, ok(200, r#"{"2025-11-03":"文化の日"}"#)),
                (2026, ok(200, r#"{"2026-01-01":"元日"}"#)),
            ]),
            calls: Mutex::new(Vec::new()),
        };
        let (days, unavailable) = load(&fake, d("2025-12-01"), d("2026-02-01")).await;
        assert_eq!(days, BTreeSet::from([d("2025-11-03"), d("2026-01-01")]));
        assert!(!unavailable);
        assert_eq!(*fake.calls.lock().unwrap(), vec![2025, 2026]);

        // 呼び出しの失敗・2xx 以外・形が違う年は祝日なしで、取れた年は使う
        let fake = Fake {
            years: HashMap::from([
                (2024, ok(200, r#"{"2024-01-01":"元日"}"#)),
                (2025, ok(503, "{}")),
                (2026, ok(200, "[]")),
            ]),
            calls: Mutex::new(Vec::new()),
        };
        let (days, unavailable) = load(&fake, d("2023-12-31"), d("2026-01-01")).await;
        assert_eq!(days, BTreeSet::from([d("2024-01-01")]));
        assert!(unavailable);

        // 空の窓は呼ばない
        let (days, unavailable) = load(&fake, d("2026-02-01"), d("2026-01-01")).await;
        assert!(days.is_empty() && !unavailable);
    }
}
