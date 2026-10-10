//! 一番星の休暇入力と、申請書 (紐づけ済みの期間) の突き合わせ (純粋関数)。
//!
//! - 一番星の `GET /api/leave/days` の本文を読み、乗務員で絞って区分に正規化する ([`parse_days`])
//! - 期間 (開始〜終了) を日ごとに展開する ([`expand_periods`])
//! - (乗務員, 日, 区分) の差を取る ([`diff`]。半日の am / pm は比べない)
//! - カレンダーの日ごとの状態 ([`day_status`]) と、通知の文面 ([`messages`])

use std::collections::{BTreeMap, BTreeSet, HashSet};

use alc_worker_db::TxOutput;
use chrono::{Datelike, Duration, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::ichiban::filter_employees;
use crate::kinds::{label, normalize, DayKind, Normalized};

/// 一番星の休暇の口 (`from` / `to` は両端を含み 92 日まで)
pub const DAYS_PATH: &str = "/api/leave/days";
pub const DAYS_MAX_SPAN: i64 = 92;

/// `from`〜`to` (両端含む) を 92 日以内の区間に分けた、一番星の口の path の列 (古い順)
pub fn days_paths(from: NaiveDate, to: NaiveDate) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = from;
    while start <= to {
        let end = (start + Duration::days(DAYS_MAX_SPAN - 1)).min(to);
        out.push(format!("{DAYS_PATH}?from={start}&to={end}"));
        start = end + Duration::days(1);
    }
    out
}

/// 一番星の社員 1 人 (自 tenant の部門に入るものだけ)
#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct Employee {
    pub employee_code: String,
    #[serde(default)]
    pub employee_name: Option<String>,
    #[serde(default)]
    pub hire_date: Option<NaiveDate>,
    #[serde(default)]
    pub retire_date: Option<NaiveDate>,
}

/// `GET /api/leave/employees` の本文を `dept_codes` で絞って読む。形が違えば `Err`
pub fn parse_employees(body: &[u8], dept_codes: &[String]) -> Result<Vec<Employee>, &'static str> {
    let filtered = filter_employees(body, dept_codes)?;
    let rows = filtered["data"].as_array().cloned().unwrap_or_default();
    rows.into_iter()
        .map(|r| serde_json::from_value::<Employee>(r).map_err(|_| "ichiban_bad_response"))
        .map(|r| {
            r.map(|mut e| {
                e.employee_code = e.employee_code.trim().to_owned();
                e
            })
        })
        .collect()
}

/// 一番星の休暇 1 件 (正規化済み)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IchibanDay<'a> {
    pub employee_cd: &'a str,
    pub date: NaiveDate,
    pub kind: DayKind,
}

/// 一番星の休暇の読み取り結果 (乗務員コードは `codes` の側の文字列を借りる)
#[derive(Debug, Default, Clone, PartialEq)]
pub struct IchibanDays<'a> {
    pub rows: Vec<IchibanDay<'a>>,
    /// 表に無い語と件数
    pub unknown: BTreeMap<String, usize>,
}

#[derive(Deserialize)]
struct DayRow {
    employee_code: Value,
    date: NaiveDate,
    item_name: Option<String>,
}

#[derive(Deserialize)]
struct DaysBody {
    data: Vec<DayRow>,
}

fn code_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.trim().to_owned()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `GET /api/leave/days` の本文 (`{"data": [{"employee_code", "date", "item_name"}]}`) を読み、`codes` に入る乗務員の行だけを
/// 正規化して `out` に足す。対象外の語は捨て、表に無い語は数える。形が違えば `Err`
pub fn parse_days<'a>(
    body: &[u8],
    codes: &'a BTreeSet<String>,
    out: &mut IchibanDays<'a>,
) -> Result<(), &'static str> {
    let parsed: DaysBody = serde_json::from_slice(body).map_err(|_| "ichiban_bad_response")?;
    for row in parsed.data {
        let Some(code) = code_of(&row.employee_code).and_then(|c| codes.get(&c)) else {
            continue;
        };
        match normalize(row.item_name.as_deref().unwrap_or("")) {
            Normalized::Leave(kind) => out.rows.push(IchibanDay {
                employee_cd: code,
                date: row.date,
                kind,
            }),
            Normalized::Ignored => {}
            Normalized::Unknown(word) => *out.unknown.entry(word).or_default() += 1,
        }
    }
    Ok(())
}

/// 申請書の期間 1 行 (紐づけ済みのページのもの)
#[derive(Debug, Clone, PartialEq)]
pub struct LinkedPeriod {
    pub employee_cd: String,
    pub page_id: Uuid,
    pub kind: String,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub half: Option<String>,
}

impl TxOutput for LinkedPeriod {}

/// 期間を日ごとに展開した 1 日
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PdfDay {
    pub employee_cd: String,
    pub date: NaiveDate,
    pub kind: String,
    pub half: Option<String>,
    pub page_id: Uuid,
}

/// 期間を `from`〜`to` (両端含む) の中で日ごとに展開する
pub fn expand_periods(periods: &[LinkedPeriod], from: NaiveDate, to: NaiveDate) -> Vec<PdfDay> {
    let mut out = Vec::new();
    for p in periods {
        let mut d = p.start_date.max(from);
        while d <= p.end_date.min(to) {
            out.push(PdfDay {
                employee_cd: p.employee_cd.clone(),
                date: d,
                kind: p.kind.clone(),
                half: p.half.clone(),
                page_id: p.page_id,
            });
            d += Duration::days(1);
        }
    }
    out.sort();
    out
}

pub const ICHIBAN_ONLY: &str = "ichiban_only";
pub const PDF_ONLY: &str = "pdf_only";

/// ずれ 1 件 (`leave_alerts` の 1 行の鍵)
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Mismatch {
    pub employee_cd: String,
    pub day: NaiveDate,
    pub kind: String,
    pub direction: &'static str,
}

/// (乗務員, 日, 区分) の差。`cutoff` (= 今日 − grace_days) より後の日は比べない (申請書の到着・一番星の入力を待つ)。
/// 半日の am / pm は比べない
pub fn diff(ichiban: &[IchibanDay<'_>], pdf: &[PdfDay], cutoff: NaiveDate) -> Vec<Mismatch> {
    let a: BTreeSet<(&str, NaiveDate, &str)> = ichiban
        .iter()
        .filter(|r| r.date <= cutoff)
        .map(|r| (r.employee_cd, r.date, r.kind.kind))
        .collect();
    let b: BTreeSet<(&str, NaiveDate, &str)> = pdf
        .iter()
        .filter(|r| r.date <= cutoff)
        .map(|r| (r.employee_cd.as_str(), r.date, r.kind.as_str()))
        .collect();
    let one = |(e, d, k): &(&str, NaiveDate, &str), direction| Mismatch {
        employee_cd: (*e).to_owned(),
        day: *d,
        kind: (*k).to_owned(),
        direction,
    };
    let mut out: Vec<Mismatch> = a
        .difference(&b)
        .map(|x| one(x, ICHIBAN_ONLY))
        .chain(b.difference(&a).map(|x| one(x, PDF_ONLY)))
        .collect();
    out.sort();
    out
}

/// カレンダーの 1 日の状態: 区分の集合が同じなら `match`、片方だけなら `ichiban_only` / `pdf_only`、
/// 両方あって区分が違えば `mismatch`
pub fn day_status<'a>(
    ichiban: impl IntoIterator<Item = &'a str>,
    pdf: impl IntoIterator<Item = &'a str>,
) -> &'static str {
    let a: BTreeSet<&str> = ichiban.into_iter().collect();
    let b: BTreeSet<&str> = pdf.into_iter().collect();
    match (a.is_empty(), b.is_empty()) {
        _ if a == b => "match",
        (false, true) => ICHIBAN_ONLY,
        (true, false) => PDF_ONLY,
        _ => "mismatch",
    }
}

/// カレンダーの 1 日の一番星の側
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct IchibanEntry {
    pub kind: &'static str,
    pub half: Option<&'static str>,
}

/// カレンダーの 1 日の申請書の側
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct PdfEntry {
    pub kind: String,
    pub half: Option<String>,
    pub page_id: Uuid,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct CalendarDay {
    pub date: NaiveDate,
    pub ichiban: Vec<IchibanEntry>,
    pub pdf: Vec<PdfEntry>,
    pub status: &'static str,
}

/// 1 人の日ごとの突き合わせ (どちらかに何かある日だけ、日付順)
pub fn calendar_days(ichiban: &[IchibanDay<'_>], pdf: &[PdfDay]) -> Vec<CalendarDay> {
    let mut days: BTreeMap<NaiveDate, (Vec<IchibanEntry>, Vec<PdfEntry>)> = BTreeMap::new();
    let mut ichiban = ichiban.to_vec();
    ichiban.sort();
    for r in ichiban {
        days.entry(r.date).or_default().0.push(IchibanEntry {
            kind: r.kind.kind,
            half: r.kind.half,
        });
    }
    for r in pdf {
        days.entry(r.date).or_default().1.push(PdfEntry {
            kind: r.kind.clone(),
            half: r.half.clone(),
            page_id: r.page_id,
        });
    }
    days.into_iter()
        .map(|(date, (ichiban, pdf))| {
            let status = day_status(
                ichiban.iter().map(|e| e.kind),
                pdf.iter().map(|e| e.kind.as_str()),
            );
            CalendarDay {
                date,
                ichiban,
                pdf,
                status,
            }
        })
        .collect()
}

/// 1 通の文字数の目安 (超えたら次の通へ)
pub const MESSAGE_CHARS: usize = 1500;

/// 通知 1 通 (本文と、その通に載せた警告の位置)
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub text: String,
    pub alerts: Vec<usize>,
}

/// 未通知の警告 1 件 (通知の文面の材料)
#[derive(Debug, Clone, PartialEq)]
pub struct PendingAlert {
    pub id: Uuid,
    pub employee_cd: String,
    pub day: NaiveDate,
    pub kind: String,
    pub direction: String,
}

impl TxOutput for PendingAlert {}

/// 警告を乗務員ごとに並べた文面。長ければ [`MESSAGE_CHARS`] 字程度で分ける (各通の先頭に見出し)。
/// `names` は乗務員コード → 氏名 (無ければコードだけ)
pub fn messages(alerts: &[PendingAlert], names: &BTreeMap<String, String>) -> Vec<Message> {
    let mut order: Vec<usize> = (0..alerts.len()).collect();
    order.sort_by_key(|&i| {
        let a = &alerts[i];
        (&a.employee_cd, a.day, &a.kind, &a.direction)
    });
    let mut chunks: Vec<(Vec<String>, Vec<usize>, usize)> = Vec::new();
    for i in order {
        let a = &alerts[i];
        let who = match names.get(&a.employee_cd) {
            Some(n) => format!("{} {}", a.employee_cd, n),
            None => a.employee_cd.clone(),
        };
        let what = if a.direction == ICHIBAN_ONLY {
            "申請書なし"
        } else {
            "一番星に入力なし"
        };
        let line = format!(
            "{who}: {:02}/{:02} {} — {what}",
            a.day.month(),
            a.day.day(),
            label(&a.kind)
        );
        let len = line.chars().count() + 1;
        match chunks.last_mut() {
            Some((lines, idx, size)) if *size + len <= MESSAGE_CHARS => {
                lines.push(line);
                idx.push(i);
                *size += len;
            }
            _ => chunks.push((vec![line], vec![i], len)),
        }
    }
    let total = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(n, (lines, idx, _))| {
            let part = if total > 1 {
                format!(" ({}/{total})", n + 1)
            } else {
                String::new()
            };
            Message {
                text: format!(
                    "【勤怠申請】一番星と申請書のずれ {} 件{part}\n{}",
                    alerts.len(),
                    lines.join("\n")
                ),
                alerts: idx,
            }
        })
        .collect()
}

/// 一番星の行のうち `employee_cd` のものだけ (カレンダー)
pub fn rows_of<'a>(rows: &[IchibanDay<'a>], employee_cd: &str) -> Vec<IchibanDay<'a>> {
    rows.iter()
        .copied()
        .filter(|r| r.employee_cd == employee_cd)
        .collect()
}

/// 有給の消化 (終日 10、半日 5。0.1 日単位)
pub fn yukyu_usages(rows: &[IchibanDay<'_>]) -> Vec<(NaiveDate, i32)> {
    rows.iter()
        .filter(|r| r.kind.kind == "yukyu")
        .map(|r| (r.date, if r.kind.half.is_some() { 5 } else { 10 }))
        .collect()
}

/// 労災の日数 (`from`〜`to`、同じ日は 1 日)
pub fn rousai_days(rows: &[IchibanDay<'_>], from: NaiveDate, to: NaiveDate) -> usize {
    rows.iter()
        .filter(|r| r.kind.kind == "rousai" && from <= r.date && r.date <= to)
        .map(|r| r.date)
        .collect::<HashSet<_>>()
        .len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn codes(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn days_paths_split_by_92_days() {
        assert_eq!(
            days_paths(d("2026-01-01"), d("2026-01-01")),
            vec!["/api/leave/days?from=2026-01-01&to=2026-01-01"]
        );
        let p = days_paths(d("2026-01-01"), d("2026-04-03"));
        assert_eq!(
            p,
            vec![
                "/api/leave/days?from=2026-01-01&to=2026-04-02",
                "/api/leave/days?from=2026-04-03&to=2026-04-03"
            ]
        );
        assert!(days_paths(d("2026-01-02"), d("2026-01-01")).is_empty());
    }

    #[test]
    fn employees_are_filtered_and_typed() {
        let body = serde_json::to_vec(&json!({"data": [
            {"employee_code": " 1021 ", "employee_name": "甲", "dept_code": "10", "hire_date": "2020-04-01", "retire_date": null},
            {"employee_code": "2001", "employee_name": "乙", "dept_code": "20", "hire_date": null, "retire_date": null}
        ]}))
        .unwrap();
        let got = parse_employees(&body, &["10".into()]).unwrap();
        assert_eq!(
            got,
            vec![Employee {
                employee_code: "1021".into(),
                employee_name: Some("甲".into()),
                hire_date: Some(d("2020-04-01")),
                retire_date: None
            }]
        );
        let bad = serde_json::to_vec(&json!({"data": [{"employee_code": 1, "dept_code": "10"}]}))
            .unwrap();
        assert_eq!(
            parse_employees(&bad, &["10".into()]),
            Err("ichiban_bad_response")
        );
        assert_eq!(
            parse_employees(b"x", &["10".into()]),
            Err("ichiban_bad_response")
        );
    }

    #[test]
    fn days_are_filtered_normalized_and_unknown_counted() {
        let c = codes(&["1021", "1022"]);
        let body = serde_json::to_vec(&json!({"data": [
            {"employee_code": "1021", "date": "2026-10-01", "item_name": "有休"},
            {"employee_code": 1022, "date": "2026-10-02", "item_name": "午前休"},
            {"employee_code": "9999", "date": "2026-10-02", "item_name": "有休"},
            {"employee_code": null, "date": "2026-10-02", "item_name": "有休"},
            {"employee_code": "1021", "date": "2026-10-03", "item_name": "休み"},
            {"employee_code": "1021", "date": "2026-10-04", "item_name": "謎"},
            {"employee_code": "1022", "date": "2026-10-05", "item_name": "謎"},
            {"employee_code": "1022", "date": "2026-10-06", "item_name": null}
        ]}))
        .unwrap();
        let mut out = IchibanDays::default();
        parse_days(&body, &c, &mut out).unwrap();
        assert_eq!(out.rows.len(), 2);
        assert_eq!(
            out.rows[1],
            IchibanDay {
                employee_cd: "1022",
                date: d("2026-10-02"),
                kind: DayKind {
                    kind: "yukyu",
                    half: Some("am")
                }
            }
        );
        assert_eq!(
            out.unknown,
            BTreeMap::from([("謎".to_owned(), 2), (String::new(), 1)])
        );
        assert_eq!(parse_days(b"{}", &c, &mut out), Err("ichiban_bad_response"));
    }

    fn period(emp: &str, kind: &str, from: &str, to: &str, half: Option<&str>) -> LinkedPeriod {
        LinkedPeriod {
            employee_cd: emp.into(),
            page_id: Uuid::from_u128(7),
            kind: kind.into(),
            start_date: d(from),
            end_date: d(to),
            half: half.map(Into::into),
        }
    }

    #[test]
    fn periods_expand_within_window() {
        let got = expand_periods(
            &[
                period("1021", "yukyu", "2026-09-29", "2026-10-02", None),
                period("1021", "rousai", "2026-11-01", "2026-11-03", None),
            ],
            d("2026-10-01"),
            d("2026-10-31"),
        );
        let dates: Vec<String> = got.iter().map(|p| p.date.to_string()).collect();
        assert_eq!(dates, vec!["2026-10-01", "2026-10-02"]);
    }

    fn ich<'a>(
        emp: &'a str,
        date: &str,
        kind: &'static str,
        half: Option<&'static str>,
    ) -> IchibanDay<'a> {
        IchibanDay {
            employee_cd: emp,
            date: d(date),
            kind: DayKind { kind, half },
        }
    }

    #[test]
    fn diff_respects_cutoff_and_ignores_half() {
        let ichiban = [
            ich("1021", "2026-10-01", "yukyu", Some("am")),
            ich("1021", "2026-10-02", "yukyu", None),
            ich("1021", "2026-10-09", "yukyu", None),
        ];
        let pdf = expand_periods(
            &[
                period("1021", "yukyu", "2026-10-01", "2026-10-01", Some("pm")),
                period("1021", "rousai", "2026-10-03", "2026-10-03", None),
                period("1021", "rousai", "2026-10-08", "2026-10-08", None),
            ],
            d("2026-09-01"),
            d("2026-10-31"),
        );
        let got = diff(&ichiban, &pdf, d("2026-10-05"));
        let m = |day: &str, kind: &str, direction| Mismatch {
            employee_cd: "1021".into(),
            day: d(day),
            kind: kind.into(),
            direction,
        };
        assert_eq!(
            got,
            vec![
                m("2026-10-02", "yukyu", ICHIBAN_ONLY),
                m("2026-10-03", "rousai", PDF_ONLY)
            ]
        );
    }

    #[test]
    fn statuses() {
        assert_eq!(day_status(["yukyu"], ["yukyu"]), "match");
        assert_eq!(day_status(["yukyu"], []), ICHIBAN_ONLY);
        assert_eq!(day_status([], ["yukyu"]), PDF_ONLY);
        assert_eq!(day_status(["yukyu"], ["rousai"]), "mismatch");
    }

    #[test]
    fn calendar_days_merge_both_sides() {
        let ichiban = [
            ich("1021", "2026-10-02", "yukyu", None),
            ich("1021", "2026-10-01", "yukyu", Some("am")),
        ];
        let pdf = expand_periods(
            &[
                period("1021", "yukyu", "2026-10-01", "2026-10-01", Some("am")),
                period("1021", "rousai", "2026-10-03", "2026-10-03", None),
            ],
            d("2026-10-01"),
            d("2026-10-31"),
        );
        let got = calendar_days(&ichiban, &pdf);
        let summary: Vec<(String, &str)> =
            got.iter().map(|c| (c.date.to_string(), c.status)).collect();
        assert_eq!(
            summary,
            vec![
                ("2026-10-01".into(), "match"),
                ("2026-10-02".into(), ICHIBAN_ONLY),
                ("2026-10-03".into(), PDF_ONLY)
            ]
        );
        assert_eq!(
            got[0].ichiban,
            vec![IchibanEntry {
                kind: "yukyu",
                half: Some("am")
            }]
        );
        assert_eq!(
            got[0].pdf,
            vec![PdfEntry {
                kind: "yukyu".into(),
                half: Some("am".into()),
                page_id: Uuid::from_u128(7)
            }]
        );
    }

    fn alert(emp: &str, day: &str, kind: &str, direction: &str) -> PendingAlert {
        PendingAlert {
            id: Uuid::nil(),
            employee_cd: emp.into(),
            day: d(day),
            kind: kind.into(),
            direction: direction.into(),
        }
    }

    #[test]
    fn message_is_grouped_by_employee() {
        let alerts = [
            alert("1022", "2026-10-05", "rousai", PDF_ONLY),
            alert("1021", "2026-10-03", "yukyu", ICHIBAN_ONLY),
        ];
        let names = BTreeMap::from([("1021".to_owned(), "山田太郎".to_owned())]);
        let got = messages(&alerts, &names);
        assert_eq!(got, vec![Message { text: "【勤怠申請】一番星と申請書のずれ 2 件\n1021 山田太郎: 10/03 有休 — 申請書なし\n1022: 10/05 労災 — 一番星に入力なし".into(), alerts: vec![1, 0] }]);
        assert!(messages(&[], &names).is_empty());
    }

    #[test]
    fn long_messages_are_split() {
        let alerts: Vec<PendingAlert> = (0..120)
            .map(|i| alert(&format!("{:04}", i), "2026-10-03", "tokukyu", ICHIBAN_ONLY))
            .collect();
        let got = messages(&alerts, &BTreeMap::new());
        assert_eq!(got.len(), 2);
        assert!(got[0].text.starts_with(
            "【勤怠申請】一番星と申請書のずれ 120 件 (1/2)\n0000: 10/03 特別休暇 — 申請書なし"
        ));
        assert!(got
            .iter()
            .all(|m| m.text.chars().count() <= MESSAGE_CHARS + 40));
        assert_eq!(got.iter().map(|m| m.alerts.len()).sum::<usize>(), 120);
    }

    #[test]
    fn usages_and_rousai() {
        let rows = [
            ich("1021", "2026-10-01", "yukyu", None),
            ich("1021", "2026-10-02", "yukyu", Some("pm")),
            ich("1021", "2026-10-03", "rousai", None),
            ich("1021", "2026-10-03", "rousai", None),
            ich("1022", "2026-10-04", "rousai", None),
            ich("1021", "2026-09-04", "rousai", None),
        ];
        assert_eq!(
            yukyu_usages(&rows),
            vec![(d("2026-10-01"), 10), (d("2026-10-02"), 5)]
        );
        let mine = rows_of(&rows, "1021");
        assert_eq!(mine.len(), 5);
        assert_eq!(rousai_days(&mine, d("2026-10-01"), d("2026-10-31")), 1);
        assert_eq!(rousai_days(&mine, d("2026-01-01"), d("2026-10-31")), 2);
    }
}
