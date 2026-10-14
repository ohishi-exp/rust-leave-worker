//! 一番星の休暇入力と、申請書 (紐づけ済みの期間) の突き合わせ (純粋関数)。
//!
//! - 一番星の `GET /api/leave/days` の本文を読み、乗務員で絞って区分に正規化する ([`parse_days`])
//! - 期間 (開始〜終了) を日ごとに展開する ([`expand_periods`])。労災は一番星の続く労災の日へ延ばす ([`rousai_extension`])
//! - (乗務員, 日, 区分) の差を取る ([`diff`]。半日の am / pm は比べない)
//! - カレンダーの日ごとの状態 ([`day_status`]) と、通知の文面 ([`messages`])

use std::collections::{BTreeMap, BTreeSet, HashSet};

use alc_worker_db::TxOutput;
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc, Weekday};
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
    /// 一番星の値のまま (文字列か数値)
    #[serde(default)]
    pub dept_code: Option<Value>,
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
    #[serde(default)]
    row_id: Option<String>,
    #[serde(default)]
    last_updated: Option<String>,
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

/// 一番星の休暇の行 1 件 (正規化前。乗務員コードは `codes` の側の文字列を借りる)
#[derive(Debug, Clone, PartialEq)]
pub struct IchibanRow<'a> {
    pub employee_cd: &'a str,
    pub date: NaiveDate,
    /// 品名N の生の値 (null は空文字)
    pub item_name: String,
    /// `管理年月日(YYYYMMDD)-管理C` (編集されても変わらない)
    pub row_id: Option<String>,
    /// 最終更新 (`YYYY-MM-DD HH:MI:SS.mmm` の文字列のまま)
    pub last_updated: Option<String>,
}

/// `GET /api/leave/days` の本文 (`{"data": [{"employee_code", "date", "item_name", "row_id", "last_updated"}]}`) を読み、
/// `codes` に入る乗務員の行だけを返す (正規化はしない)。形が違えば `Err`。一番星の休暇の本文を読むのはここだけ
pub fn read_day_rows<'a>(
    body: &[u8],
    codes: &'a BTreeSet<String>,
) -> Result<Vec<IchibanRow<'a>>, &'static str> {
    let parsed: DaysBody = serde_json::from_slice(body).map_err(|_| "ichiban_bad_response")?;
    Ok(parsed
        .data
        .into_iter()
        .filter_map(|row| {
            let code = code_of(&row.employee_code).and_then(|c| codes.get(&c))?;
            Some(IchibanRow {
                employee_cd: code,
                date: row.date,
                item_name: row.item_name.unwrap_or_default(),
                row_id: row.row_id,
                last_updated: row.last_updated,
            })
        })
        .collect())
}

/// 行を正規化して `out` に足す。対象外の語は捨て、表に無い語は数える
pub fn push_days<'a>(rows: &[IchibanRow<'a>], out: &mut IchibanDays<'a>) {
    for row in rows {
        match normalize(&row.item_name) {
            Normalized::Leave(kind) => out.rows.push(IchibanDay {
                employee_cd: row.employee_cd,
                date: row.date,
                kind,
            }),
            Normalized::Ignored => {}
            Normalized::Unknown(word) => *out.unknown.entry(word).or_default() += 1,
        }
    }
}

/// `GET /api/leave/days` の本文を読み ([`read_day_rows`])、`codes` に入る乗務員の行だけを
/// 正規化して `out` に足す ([`push_days`])。形が違えば `Err`
pub fn parse_days<'a>(
    body: &[u8],
    codes: &'a BTreeSet<String>,
    out: &mut IchibanDays<'a>,
) -> Result<(), &'static str> {
    push_days(&read_day_rows(body, codes)?, out);
    Ok(())
}

/// 候補の期間に載せる一番星の行 1 件 (応答の形)
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct CandidateRow {
    pub row_id: String,
    pub day: NaiveDate,
    pub item_name: String,
}

/// 候補の期間 1 件 (同じ区分・半日で暦日が続く一番星の行をまとめたもの。半日は単日)
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct CandidatePeriod {
    pub kind: &'static str,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub half: Option<&'static str>,
    pub rows: Vec<CandidateRow>,
}

/// 乗務員 `employee_cd` の行のうち休暇の区分に当たり `row_id` を持つものを、同じ (区分, 半日) で暦日が続くもの
/// ごとに 1 期間にまとめる (半日は同じ日だけ)。開始日・区分の順
pub fn candidate_periods(rows: &[IchibanRow<'_>], employee_cd: &str) -> Vec<CandidatePeriod> {
    let mut leaves: Vec<(DayKind, NaiveDate, &str, &str)> = rows
        .iter()
        .filter(|r| r.employee_cd == employee_cd)
        .filter_map(|r| match (normalize(&r.item_name), r.row_id.as_deref()) {
            (Normalized::Leave(kind), Some(id)) => Some((kind, r.date, id, r.item_name.as_str())),
            _ => None,
        })
        .collect();
    leaves.sort();
    let mut out: Vec<CandidatePeriod> = Vec::new();
    for (kind, date, row_id, item_name) in leaves {
        let row = CandidateRow {
            row_id: row_id.to_owned(),
            day: date,
            item_name: item_name.to_owned(),
        };
        let reach = Duration::days(if kind.half.is_some() { 0 } else { 1 });
        match out.last_mut() {
            Some(p) if p.kind == kind.kind && p.half == kind.half && date <= p.end_date + reach => {
                p.end_date = date;
                p.rows.push(row);
            }
            _ => out.push(CandidatePeriod {
                kind: kind.kind,
                start_date: date,
                end_date: date,
                half: kind.half,
                rows: vec![row],
            }),
        }
    }
    out.sort_by_key(|p| (p.start_date, p.kind, p.half));
    out
}

/// 紐づけたときの一番星の行 (`leave_link_rows` の 1 行。`item_name` は生の品名)
#[derive(Debug, Clone, PartialEq)]
pub struct LinkRow {
    pub row_id: String,
    pub employee_cd: String,
    pub day: NaiveDate,
    pub item_name: String,
    pub last_updated: Option<String>,
}

impl TxOutput for LinkRow {}

/// 受け取った `row_ids` を、一番星の行 (`rows`) のうち乗務員 `employee_cd` のものから引く。
/// 1 つでも無い (別の乗務員の行を含む) なら `None`
pub fn pick_link_rows(
    rows: &[IchibanRow<'_>],
    employee_cd: &str,
    row_ids: &[String],
) -> Option<Vec<LinkRow>> {
    row_ids
        .iter()
        .map(|id| {
            rows.iter()
                .find(|r| r.employee_cd == employee_cd && r.row_id.as_ref() == Some(id))
                .map(|r| LinkRow {
                    row_id: id.clone(),
                    employee_cd: r.employee_cd.to_owned(),
                    day: r.date,
                    item_name: r.item_name.clone(),
                    last_updated: r.last_updated.clone(),
                })
        })
        .collect()
}

/// 保存した (ページ, 行) を一番星の今の行と比べ、紐づけを解除するページを返す。`codes` に入らない乗務員の行は比べない。
/// 解除の条件 (文字列のまま比べる): 同じ `row_id` の行が無い / 乗務員・日・品名のどれかが違う /
/// 保存した最終更新が在り、今の最終更新と違う
pub fn stale_pages(
    saved: &[(Uuid, LinkRow)],
    rows: &[IchibanRow<'_>],
    codes: &BTreeSet<String>,
) -> BTreeSet<Uuid> {
    let now: BTreeMap<&str, &IchibanRow<'_>> = rows
        .iter()
        .filter_map(|r| Some((r.row_id.as_deref()?, r)))
        .collect();
    saved
        .iter()
        .filter(|(_, s)| codes.contains(&s.employee_cd))
        .filter(|(_, s)| {
            now.get(s.row_id.as_str()).is_none_or(|r| {
                r.employee_cd != s.employee_cd
                    || r.date != s.day
                    || r.item_name != s.item_name
                    || (s.last_updated.is_some() && s.last_updated != r.last_updated)
            })
        })
        .map(|(page, _)| *page)
        .collect()
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

/// 労災の延長を数え始める日〜窓の終わり (祝日を取る範囲)。労災の期間が無い・数える日が無いなら `None`
pub fn extension_span(
    periods: &[LinkedPeriod],
    from: NaiveDate,
    to: NaiveDate,
) -> Option<(NaiveDate, NaiveDate)> {
    periods
        .iter()
        .filter(|p| p.kind == ROUSAI)
        .map(|p| (p.end_date + Duration::days(1)).max(from))
        .min()
        .filter(|start| *start <= to)
        .map(|start| (start, to))
}

/// 労災の区分の語
pub const ROUSAI: &str = "rousai";

/// 紐づけ済みの労災の期間を、そのあと一番星に同じ乗務員の労災の行が続く日へ延ばす (保存しない。毎回計算する)。
///
/// 期間の終わりの翌日 (窓 `from` より前なら `from`) から 1 日ずつ見て、労災の行が在る日はそのページが覆う日にし、
/// 行が無くても日曜・祝日 (`holidays`) は飛ばして続け、行の無いそれ以外の日 (土曜を含む) で止まる。`to` で終わる。
/// 労災の期間そのものに入る日は出さない (その期間が覆う)。同じ日を複数のページが延長で覆うときは、起点 (期間の終わり)
/// が後のページ。労災以外の期間は延ばさない
pub fn rousai_extension(
    ichiban: &[IchibanDay<'_>],
    periods: &[LinkedPeriod],
    holidays: &BTreeSet<NaiveDate>,
    from: NaiveDate,
    to: NaiveDate,
) -> Vec<PdfDay> {
    let rows: HashSet<(&str, NaiveDate)> = ichiban
        .iter()
        .filter(|r| r.kind.kind == ROUSAI)
        .map(|r| (r.employee_cd, r.date))
        .collect();
    let rousai: Vec<&LinkedPeriod> = periods.iter().filter(|p| p.kind == ROUSAI).collect();
    let covered = |emp: &str, d: NaiveDate| {
        rousai
            .iter()
            .any(|p| p.employee_cd == emp && p.start_date <= d && d <= p.end_date)
    };
    let mut best: BTreeMap<(&str, NaiveDate), (NaiveDate, Uuid)> = BTreeMap::new();
    for p in &rousai {
        let emp = p.employee_cd.as_str();
        let mut d = (p.end_date + Duration::days(1)).max(from);
        while d <= to {
            if rows.contains(&(emp, d)) {
                if !covered(emp, d) {
                    let e = best.entry((emp, d)).or_insert((p.end_date, p.page_id));
                    *e = (*e).max((p.end_date, p.page_id));
                }
            } else if d.weekday() != Weekday::Sun && !holidays.contains(&d) {
                break;
            }
            d += Duration::days(1);
        }
    }
    best.into_iter()
        .map(|((emp, date), (_, page_id))| PdfDay {
            employee_cd: emp.to_owned(),
            date,
            kind: ROUSAI.to_owned(),
            half: None,
            page_id,
        })
        .collect()
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

/// 未解消の警告 1 件 (通知の文面の材料)
#[derive(Debug, Clone, PartialEq)]
pub struct PendingAlert {
    pub id: Uuid,
    pub employee_cd: String,
    pub day: NaiveDate,
    pub kind: String,
    pub direction: String,
}

impl TxOutput for PendingAlert {}

/// 未解消の警告 1 件 (`GET /api/leave/alerts` の 1 行。`kind` / `direction` は `leave_alerts` の列の値そのまま)
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct OpenAlert {
    pub employee_cd: String,
    pub day: NaiveDate,
    pub kind: String,
    pub direction: String,
    pub first_seen_at: DateTime<Utc>,
    /// 最後に通知を送った時刻 (まだ送れていなければ `null`)
    pub notified_at: Option<DateTime<Utc>>,
}

impl TxOutput for OpenAlert {}

/// 警告を乗務員ごとに並べた文面。長ければ [`MESSAGE_CHARS`] 字程度で分ける (各通の先頭に見出し)。
/// `names` は乗務員コード → 氏名 (無ければコードだけ)。`unlinked` (毎朝の解除の件数) が 1 以上なら 1 通目の見出しの
/// 次の行に載せる (警告が無くても 1 通出す)
pub fn messages(
    alerts: &[PendingAlert],
    names: &BTreeMap<String, String>,
    unlinked: u64,
) -> Vec<Message> {
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
    let unlinked_line = format!("一番星が変わったため紐づけを解除: {unlinked} 件");
    if total == 0 {
        return if unlinked > 0 {
            vec![Message {
                text: format!("【勤怠申請】{unlinked_line}"),
                alerts: Vec::new(),
            }]
        } else {
            Vec::new()
        };
    }
    chunks
        .into_iter()
        .enumerate()
        .map(|(n, (mut lines, idx, _))| {
            let part = if total > 1 {
                format!(" ({}/{total})", n + 1)
            } else {
                String::new()
            };
            if n == 0 && unlinked > 0 {
                lines.insert(0, unlinked_line.clone());
            }
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
        .filter(|r| r.kind.kind == ROUSAI && from <= r.date && r.date <= to)
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
                dept_code: Some(json!("10")),
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

    /// 延長した (乗務員, 日, ページ)
    fn ext(
        ichiban: &[IchibanDay<'_>],
        periods: &[LinkedPeriod],
        holidays: &[&str],
        from: &str,
        to: &str,
    ) -> Vec<(String, String, u128)> {
        let holidays: BTreeSet<NaiveDate> = holidays.iter().map(|h| d(h)).collect();
        rousai_extension(ichiban, periods, &holidays, d(from), d(to))
            .into_iter()
            .inspect(|p| assert_eq!((p.kind.as_str(), p.half.as_deref()), (ROUSAI, None)))
            .map(|p| (p.employee_cd, p.date.to_string(), p.page_id.as_u128()))
            .collect()
    }

    fn page_period(page: u128, kind: &str, from: &str, to: &str) -> LinkedPeriod {
        LinkedPeriod {
            page_id: Uuid::from_u128(page),
            ..period("1021", kind, from, to, None)
        }
    }

    fn e(day: &str, page: u128) -> (String, String, u128) {
        ("1021".into(), day.into(), page)
    }

    #[test]
    fn rousai_extends_over_sundays_until_a_day_without_a_row() {
        // 10/02 (金) で終わる期間。10/03 (土) は行が在る、10/04 (日) は行が無くても続き、10/07 (水) で止まる
        let rows = [
            ich("1021", "2026-10-03", "rousai", None),
            ich("1021", "2026-10-05", "rousai", None),
            ich("1021", "2026-10-06", "rousai", None),
            ich("1021", "2026-10-08", "rousai", None),
            ich("1022", "2026-10-07", "rousai", None),
            ich("1021", "2026-10-07", "yukyu", None),
        ];
        let periods = [page_period(7, ROUSAI, "2026-10-01", "2026-10-02")];
        assert_eq!(
            ext(&rows, &periods, &[], "2026-08-01", "2026-10-09"),
            vec![e("2026-10-03", 7), e("2026-10-05", 7), e("2026-10-06", 7)]
        );
        // 窓の終わりで止まる
        assert_eq!(
            ext(&rows, &periods, &[], "2026-08-01", "2026-10-05"),
            vec![e("2026-10-03", 7), e("2026-10-05", 7)]
        );
        // 土曜は飛ばさない (行が無ければ止まる)
        assert_eq!(
            ext(&rows[1..], &periods, &[], "2026-08-01", "2026-10-09"),
            vec![]
        );
        // 労災以外の期間は延ばさない
        let yukyu = [page_period(7, "yukyu", "2026-10-01", "2026-10-02")];
        assert_eq!(ext(&rows, &yukyu, &[], "2026-08-01", "2026-10-09"), vec![]);
    }

    #[test]
    fn rousai_extends_over_holidays() {
        // 9/18 (金) で終わる期間。9/20 (日)・9/21〜23 (祝) は行が無くても続き、9/25 (金) で止まる
        let rows = [
            ich("1021", "2026-09-19", "rousai", None),
            ich("1021", "2026-09-24", "rousai", None),
            ich("1021", "2026-09-28", "rousai", None),
        ];
        let periods = [page_period(7, ROUSAI, "2026-09-14", "2026-09-18")];
        let holidays = ["2026-09-21", "2026-09-22", "2026-09-23"];
        assert_eq!(
            ext(&rows, &periods, &holidays, "2026-08-01", "2026-10-09"),
            vec![e("2026-09-19", 7), e("2026-09-24", 7)]
        );
        // 祝日が取れなければ日曜だけ飛ばす (9/21 (月) で止まる)
        assert_eq!(
            ext(&rows, &periods, &[], "2026-08-01", "2026-10-09"),
            vec![e("2026-09-19", 7)]
        );
    }

    #[test]
    fn rousai_extension_starts_at_the_window() {
        // 窓より前に終わった期間は窓の始め (8/1 (土)) から数える
        let rows = [
            ich("1021", "2026-08-01", "rousai", None),
            ich("1021", "2026-08-03", "rousai", None),
        ];
        let periods = [page_period(7, ROUSAI, "2026-07-01", "2026-07-20")];
        assert_eq!(
            ext(&rows, &periods, &[], "2026-08-01", "2026-10-09"),
            vec![e("2026-08-01", 7), e("2026-08-03", 7)]
        );
        assert_eq!(
            extension_span(&periods, d("2026-08-01"), d("2026-10-09")),
            Some((d("2026-08-01"), d("2026-10-09")))
        );
    }

    #[test]
    fn overlapping_rousai_pages_prefer_the_later_start() {
        // A (10/01〜10/02) の延長は B (10/05〜10/06) の日を飛ばして続くが、B の後の日は B が覆う
        let days: Vec<String> = ["03", "05", "06", "07", "08"]
            .iter()
            .map(|day| format!("2026-10-{day}"))
            .collect();
        let rows: Vec<_> = days
            .iter()
            .map(|day| ich("1021", day, "rousai", None))
            .collect();
        let periods = [
            page_period(8, ROUSAI, "2026-10-05", "2026-10-06"),
            page_period(7, ROUSAI, "2026-10-01", "2026-10-02"),
        ];
        assert_eq!(
            ext(&rows, &periods, &[], "2026-08-01", "2026-10-09"),
            vec![e("2026-10-03", 7), e("2026-10-07", 8), e("2026-10-08", 8)]
        );
    }

    #[test]
    fn extension_span_is_from_the_earliest_start() {
        let w = (d("2026-08-01"), d("2026-10-09"));
        assert_eq!(extension_span(&[], w.0, w.1), None);
        let yukyu = [period("1021", "yukyu", "2026-09-01", "2026-09-02", None)];
        assert_eq!(extension_span(&yukyu, w.0, w.1), None);
        let two = [
            page_period(7, ROUSAI, "2026-09-01", "2026-09-30"),
            page_period(8, ROUSAI, "2026-09-01", "2026-09-10"),
        ];
        assert_eq!(
            extension_span(&two, w.0, w.1),
            Some((d("2026-09-11"), d("2026-10-09")))
        );
        // 窓の終わりまで期間が覆うなら数える日が無い
        let full = [page_period(7, ROUSAI, "2026-09-01", "2026-10-09")];
        assert_eq!(extension_span(&full, w.0, w.1), None);
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
        let got = messages(&alerts, &names, 0);
        assert_eq!(got, vec![Message { text: "【勤怠申請】一番星と申請書のずれ 2 件\n1021 山田太郎: 10/03 有休 — 申請書なし\n1022: 10/05 労災 — 一番星に入力なし".into(), alerts: vec![1, 0] }]);
        assert!(messages(&[], &names, 0).is_empty());
    }

    #[test]
    fn long_messages_are_split() {
        let alerts: Vec<PendingAlert> = (0..120)
            .map(|i| alert(&format!("{:04}", i), "2026-10-03", "tokukyu", ICHIBAN_ONLY))
            .collect();
        let got = messages(&alerts, &BTreeMap::new(), 0);
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
    fn messages_carry_the_unlinked_count() {
        let names = BTreeMap::new();
        // 警告が無くても解除があれば 1 通
        assert_eq!(
            messages(&[], &names, 2),
            vec![Message {
                text: "【勤怠申請】一番星が変わったため紐づけを解除: 2 件".into(),
                alerts: vec![]
            }]
        );
        let alerts = [alert("1021", "2026-10-03", "yukyu", ICHIBAN_ONLY)];
        assert_eq!(
            messages(&alerts, &names, 1),
            vec![Message {
                text: "【勤怠申請】一番星と申請書のずれ 1 件\n一番星が変わったため紐づけを解除: 1 件\n1021: 10/03 有休 — 申請書なし".into(),
                alerts: vec![0]
            }]
        );
        // 分けたときは 1 通目だけ
        let many: Vec<PendingAlert> = (0..120)
            .map(|i| alert(&format!("{:04}", i), "2026-10-03", "tokukyu", ICHIBAN_ONLY))
            .collect();
        let got = messages(&many, &names, 3);
        assert_eq!(got.len(), 2);
        assert!(got[0].text.starts_with(
            "【勤怠申請】一番星と申請書のずれ 120 件 (1/2)\n一番星が変わったため紐づけを解除: 3 件\n0000:"
        ));
        assert!(!got[1].text.contains("紐づけを解除"));
    }

    fn row<'a>(
        emp: &'a str,
        date: &str,
        item: &str,
        row_id: Option<&str>,
        last_updated: Option<&str>,
    ) -> IchibanRow<'a> {
        IchibanRow {
            employee_cd: emp,
            date: d(date),
            item_name: item.into(),
            row_id: row_id.map(Into::into),
            last_updated: last_updated.map(Into::into),
        }
    }

    #[test]
    fn day_rows_keep_raw_values() {
        let c = codes(&["1021"]);
        let body = serde_json::to_vec(&json!({"data": [
            {"employee_code": " 1021 ", "date": "2026-10-01", "item_name": " 有休 ", "row_id": "20261001-1", "last_updated": "2026-10-01 09:00:00.000"},
            {"employee_code": 1021, "date": "2026-10-02", "item_name": null},
            {"employee_code": "2001", "date": "2026-10-02", "item_name": "有休", "row_id": "20261002-9"}
        ]}))
        .unwrap();
        assert_eq!(
            read_day_rows(&body, &c).unwrap(),
            vec![
                row(
                    "1021",
                    "2026-10-01",
                    " 有休 ",
                    Some("20261001-1"),
                    Some("2026-10-01 09:00:00.000")
                ),
                row("1021", "2026-10-02", "", None, None)
            ]
        );
        assert_eq!(read_day_rows(b"[]", &c), Err("ichiban_bad_response"));
    }

    #[test]
    fn candidates_merge_consecutive_days_of_the_same_kind() {
        let rows = [
            row("1021", "2026-10-02", "有休", Some("b"), None),
            row("1021", "2026-10-01", "有休（積置）", Some("a"), None),
            // 1 日あくと分ける
            row("1021", "2026-10-04", "有休", Some("c"), None),
            // 区分が違えば分ける
            row("1021", "2026-10-05", "労災", Some("d"), None),
            // 半日は単日 (続く日でもまとめない)
            row("1021", "2026-10-06", "午前休", Some("e"), None),
            row("1021", "2026-10-07", "午前休", Some("f"), None),
            row("1021", "2026-10-07", "午後休", Some("g"), None),
            // 対象外・表に無い語・row_id 無し・別の乗務員は候補に入れない
            row("1021", "2026-10-03", "休み", Some("h"), None),
            row("1021", "2026-10-03", "謎", Some("i"), None),
            row("1021", "2026-10-03", "有休", None, None),
            row("1022", "2026-10-03", "有休", Some("j"), None),
        ];
        let got = candidate_periods(&rows, "1021");
        // (区分 開始..終了 半日 [row_id])
        let summary: Vec<String> = got
            .iter()
            .map(|p| {
                let ids: Vec<&str> = p.rows.iter().map(|r| r.row_id.as_str()).collect();
                format!(
                    "{} {}..{} {:?} {ids:?}",
                    p.kind, p.start_date, p.end_date, p.half
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                r#"yukyu 2026-10-01..2026-10-02 None ["a", "b"]"#,
                r#"yukyu 2026-10-04..2026-10-04 None ["c"]"#,
                r#"rousai 2026-10-05..2026-10-05 None ["d"]"#,
                r#"yukyu 2026-10-06..2026-10-06 Some("am") ["e"]"#,
                r#"yukyu 2026-10-07..2026-10-07 Some("am") ["f"]"#,
                r#"yukyu 2026-10-07..2026-10-07 Some("pm") ["g"]"#,
            ]
        );
        assert_eq!(
            got[0].rows[0],
            CandidateRow {
                row_id: "a".into(),
                day: d("2026-10-01"),
                item_name: "有休（積置）".into()
            }
        );
        // 同じ日の同じ区分の行は 1 期間に入れる
        let dup = [
            row("1021", "2026-10-01", "午前休", Some("x"), None),
            row("1021", "2026-10-01", "午前休", Some("y"), None),
        ];
        let got = candidate_periods(&dup, "1021");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].rows.len(), 2);
    }

    #[test]
    fn link_rows_must_exist_for_the_employee() {
        let rows = [
            row("1021", "2026-10-01", "有休", Some("a"), Some("t1")),
            row("1022", "2026-10-02", "有休", Some("b"), None),
        ];
        assert_eq!(
            pick_link_rows(&rows, "1021", &["a".into()]),
            Some(vec![LinkRow {
                row_id: "a".into(),
                employee_cd: "1021".into(),
                day: d("2026-10-01"),
                item_name: "有休".into(),
                last_updated: Some("t1".into())
            }])
        );
        assert_eq!(pick_link_rows(&rows, "1021", &["b".into()]), None);
        assert_eq!(
            pick_link_rows(&rows, "1021", &["a".into(), "z".into()]),
            None
        );
    }

    #[test]
    fn stale_pages_compare_rows_as_strings() {
        let saved_row =
            |row_id: &str, emp: &str, day: &str, item: &str, lu: Option<&str>| LinkRow {
                row_id: row_id.into(),
                employee_cd: emp.into(),
                day: d(day),
                item_name: item.into(),
                last_updated: lu.map(Into::into),
            };
        let p = Uuid::from_u128;
        let saved = [
            // 同じ (最終更新も同じ) → 残す
            (
                p(1),
                saved_row("a", "1021", "2026-10-01", "有休", Some("t1")),
            ),
            // 最終更新が変わった → 解除
            (
                p(2),
                saved_row("b", "1021", "2026-10-02", "有休", Some("t1")),
            ),
            // 最終更新を持たず、品名が変わった → 解除
            (p(3), saved_row("c", "1021", "2026-10-03", "有休", None)),
            // 最終更新を持たず、中身が同じ → 残す (一番星の側に最終更新が付いても)
            (p(4), saved_row("d", "1021", "2026-10-04", "有休", None)),
            // 行が消えた → 解除
            (p(5), saved_row("e", "1021", "2026-10-05", "有休", None)),
            // 日が変わった・乗務員が変わった → 解除
            (
                p(6),
                saved_row("f", "1021", "2026-10-06", "有休", Some("t1")),
            ),
            (
                p(7),
                saved_row("g", "1021", "2026-10-07", "有休", Some("t1")),
            ),
            // codes に入らない乗務員の行は比べない
            (p(8), saved_row("h", "9999", "2026-10-08", "有休", None)),
        ];
        let rows = [
            row("1021", "2026-10-01", "有休", Some("a"), Some("t1")),
            row("1021", "2026-10-02", "有休", Some("b"), Some("t2")),
            row("1021", "2026-10-03", "午前休", Some("c"), None),
            row("1021", "2026-10-04", "有休", Some("d"), Some("t9")),
            row("1021", "2026-10-16", "有休", Some("f"), Some("t1")),
            row("1022", "2026-10-07", "有休", Some("g"), Some("t1")),
            row("1021", "2026-10-09", "有休", None, None),
        ];
        let got = stale_pages(&saved, &rows, &codes(&["1021", "1022"]));
        assert_eq!(got, BTreeSet::from([p(2), p(3), p(5), p(6), p(7)]));
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
