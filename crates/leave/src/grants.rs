//! 有給の付与 (労基法 39 条の通常の労働者の法定付与) と、付与から消化を引いた残日数 (純粋関数)。
//!
//! - 日数は 0.1 日単位の整数 (`tenths`) で持つ (表の `days NUMERIC(4,1)`。終日 = 10、半日 = 5)
//! - 付与は `grant_date <= 日 < expires_on` の日に使える (`expires_on` = 失効する日。付与日 + 2 年)
//! - 比例付与 (パート) は扱わない (画面で手で直す)

use std::collections::BTreeSet;

use alc_worker_db::TxOutput;
use chrono::{DateTime, Datelike, Months, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::pages::{non_empty, MAX_EMPLOYEE_CD_LEN, MAX_NOTE_CHARS};

/// 入社から付与までの月数と日数 (6 年 6 か月以降は毎年 20 日)
const SCHEDULE: [(u32, i32); 7] = [
    (6, 100),
    (18, 110),
    (30, 120),
    (42, 140),
    (54, 160),
    (66, 180),
    (78, 200),
];
const AFTER_SCHEDULE: i32 = 200;
/// 有効期間 (月)
pub const VALID_MONTHS: u32 = 24;

/// 計算した付与 1 件
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoGrant {
    pub grant_date: NaiveDate,
    pub tenths: i32,
    pub expires_on: NaiveDate,
}

fn is_month_end(d: NaiveDate) -> bool {
    d.succ_opt().is_none_or(|n| n.month() != d.month())
}

/// `d` の `months` か月後。`d` が月末日なら行き先の月の末日にそろえる (4/30 → 10/31、8/31 → 2/28 か 2/29)。
/// 月末日でなく行き先に同じ日が無ければ、その月の末日 (chrono の既定)。表せない遠い日付なら `None`
pub fn add_months(d: NaiveDate, months: u32) -> Option<NaiveDate> {
    let shifted = d.checked_add_months(Months::new(months))?;
    if !is_month_end(d) {
        return Some(shifted);
    }
    // 行き先の月の翌月 1 日の前日 (12 月なら翌年 1 月)
    let (y, m) = if shifted.month() == 12 {
        (shifted.year() + 1, 1)
    } else {
        (shifted.year(), shifted.month() + 1)
    };
    NaiveDate::from_ymd_opt(y, m, 1)?.pred_opt()
}

/// 有効期限 (付与日 + 2 年。月末日の規則は [`add_months`] と同じ)
pub fn expires_on(grant_date: NaiveDate) -> Option<NaiveDate> {
    add_months(grant_date, VALID_MONTHS)
}

/// 入社日 `hire` の法定付与のうち、`today` までに到来したもの (付与日 <= today)。
/// 退職日 `retire` の当日以降の付与は作らない
pub fn statutory(hire: NaiveDate, retire: Option<NaiveDate>, today: NaiveDate) -> Vec<AutoGrant> {
    let mut out = Vec::new();
    for i in 0u32.. {
        let (months, tenths) = match SCHEDULE.get(i as usize) {
            Some(&s) => s,
            None => (78 + 12 * (i - 6), AFTER_SCHEDULE),
        };
        let Some(grant_date) = add_months(hire, months) else {
            break;
        };
        if grant_date > today || retire.is_some_and(|r| grant_date >= r) {
            break;
        }
        let Some(expires_on) = expires_on(grant_date) else {
            break;
        };
        out.push(AutoGrant {
            grant_date,
            tenths,
            expires_on,
        });
    }
    out
}

/// 残日数の計算に渡す付与 1 件 (auto と manual の区別は要らない)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantIn {
    pub id: Uuid,
    pub grant_date: NaiveDate,
    pub expires_on: NaiveDate,
    pub tenths: i32,
}

/// 付与 1 件の基準日時点の内訳 (0.1 日単位)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantBalance {
    pub id: Uuid,
    /// 消化した日数
    pub used: i32,
    /// 基準日時点で使える残 (失効していれば 0)
    pub remaining: i32,
    /// 失効で消えた日数 (基準日までに失効していれば、その時点の残)
    pub expired: i32,
}

/// 基準日時点の残日数
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Balance {
    pub remaining: i32,
    pub expired: i32,
    /// どの付与にも当てられなかった消化 (付与が足りない・付与前の消化)
    pub unallocated: i32,
    /// 入力の付与の順
    pub grants: Vec<GrantBalance>,
}

/// 消化 (`(日, 0.1 日単位)`) を日付順に、その日に有効な付与の古い方から引く (FIFO)。`as_of` より後の消化は数えない
pub fn balance(grants: &[GrantIn], usages: &[(NaiveDate, i32)], as_of: NaiveDate) -> Balance {
    let mut left: Vec<i32> = grants.iter().map(|g| g.tenths).collect();
    // 古い付与から (付与日・失効日・入力の順)
    let mut order: Vec<usize> = (0..grants.len()).collect();
    order.sort_by_key(|&i| (grants[i].grant_date, grants[i].expires_on, i));
    let mut usages: Vec<(NaiveDate, i32)> = usages
        .iter()
        .copied()
        .filter(|(d, _)| *d <= as_of)
        .collect();
    usages.sort();
    let mut unallocated = 0;
    for (day, mut need) in usages {
        for &i in &order {
            let g = &grants[i];
            if need == 0 {
                break;
            }
            if g.grant_date <= day && day < g.expires_on {
                let take = need.min(left[i]);
                left[i] -= take;
                need -= take;
            }
        }
        unallocated += need;
    }
    let grants: Vec<GrantBalance> = grants
        .iter()
        .zip(&left)
        .map(|(g, &l)| {
            let expired = g.expires_on <= as_of;
            let started = g.grant_date <= as_of;
            GrantBalance {
                id: g.id,
                used: g.tenths - l,
                remaining: if expired || !started { 0 } else { l },
                expired: if expired { l } else { 0 },
            }
        })
        .collect();
    Balance {
        remaining: grants.iter().map(|g| g.remaining).sum(),
        expired: grants.iter().map(|g| g.expired).sum(),
        unallocated,
        grants,
    }
}

/// 期首残 (乗務員 1 人の「`as_of` 時点の残」。日数は 0.1 日単位)。前年からの繰越と今年度の付与の残を分けて持つ (失効日を正確にするため)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opening {
    pub as_of: NaiveDate,
    pub carryover: i32,
    pub current: i32,
}

impl TxOutput for Opening {}

/// 期首残を付与 2 件に置き換えたもの。期首日以前の法定付与日の直近を G1 (今年度)、その前を G0 (繰越) とし、
/// 日数を期首残に差し替える (失効は法定付与と同じ付与日 + 2 年)。付与日が無ければ `None` (入社日が無い・入社から 6 か月未満など)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpeningGrants {
    pub opening: Opening,
    pub carryover: Option<AutoGrant>,
    pub current: Option<AutoGrant>,
}

pub fn opening_grants(
    opening: Opening,
    hire: Option<NaiveDate>,
    retire: Option<NaiveDate>,
) -> OpeningGrants {
    let s = hire
        .map(|h| statutory(h, retire, opening.as_of))
        .unwrap_or_default();
    let pick = |back: usize, tenths: i32| {
        s.len()
            .checked_sub(back)
            .map(|i| AutoGrant { tenths, ..s[i] })
    };
    OpeningGrants {
        opening,
        carryover: pick(2, opening.carryover),
        current: pick(1, opening.current),
    }
}

impl OpeningGrants {
    /// 保存できない期首残の語 (繰越があるのに G0 が無い・今年度があるのに G1 が無い)
    pub fn error(&self) -> Option<&'static str> {
        if self.opening.carryover > 0 && self.carryover.is_none() {
            Some("opening_no_prior_grant")
        } else if self.opening.current > 0 && self.current.is_none() {
            Some("opening_no_current_grant")
        } else {
            None
        }
    }

    /// 基準日 `as_of` の残に使うか (期首日より前の基準日には使わない)
    pub fn applies(&self, as_of: NaiveDate) -> bool {
        self.opening.as_of <= as_of
    }

    /// 付与台帳の行が期首残で置き換わったか (付与日 <= 期首日)
    pub fn supersedes(&self, grant_date: NaiveDate) -> bool {
        grant_date <= self.opening.as_of
    }

    pub fn view(&self) -> OpeningView {
        OpeningView {
            as_of: self.opening.as_of,
            carryover: days(self.opening.carryover),
            current: days(self.opening.current),
            carryover_grant_date: self.carryover.map(|g| g.grant_date),
            carryover_expires_on: self.carryover.map(|g| g.expires_on),
            current_grant_date: self.current.map(|g| g.grant_date),
            current_expires_on: self.current.map(|g| g.expires_on),
        }
    }
}

/// 付与台帳の応答の `opening`
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct OpeningView {
    pub as_of: NaiveDate,
    pub carryover: f64,
    pub current: f64,
    pub carryover_grant_date: Option<NaiveDate>,
    pub carryover_expires_on: Option<NaiveDate>,
    pub current_grant_date: Option<NaiveDate>,
    pub current_expires_on: Option<NaiveDate>,
}

/// 期首残を入れた残。`opening` が無いか基準日が期首日より前なら [`balance`] と同じ。
/// 入れるときは、付与日が期首日以前の付与を外し (内訳は 0)、合成の付与 2 件 (0 日でも) を足し、消化は期首日の翌日以降だけを数える
/// (期首日当日までの消化は期首残に含まれている)
pub fn balance_with_opening(
    grants: &[GrantIn],
    usages: &[(NaiveDate, i32)],
    as_of: NaiveDate,
    opening: Option<&OpeningGrants>,
) -> Balance {
    let Some(o) = opening.filter(|o| o.applies(as_of)) else {
        return balance(grants, usages, as_of);
    };
    let kept: Vec<usize> = (0..grants.len())
        .filter(|&i| !o.supersedes(grants[i].grant_date))
        .collect();
    let mut inputs: Vec<GrantIn> = kept.iter().map(|&i| grants[i]).collect();
    inputs.extend(
        [o.carryover, o.current]
            .into_iter()
            .flatten()
            .map(|g| GrantIn {
                id: Uuid::nil(),
                grant_date: g.grant_date,
                expires_on: g.expires_on,
                tenths: g.tenths,
            }),
    );
    let usages: Vec<(NaiveDate, i32)> = usages
        .iter()
        .copied()
        .filter(|(d, _)| !o.supersedes(*d))
        .collect();
    let b = balance(&inputs, &usages, as_of);
    let mut out: Vec<GrantBalance> = grants
        .iter()
        .map(|g| GrantBalance {
            id: g.id,
            used: 0,
            remaining: 0,
            expired: 0,
        })
        .collect();
    for (k, &i) in kept.iter().enumerate() {
        out[i] = b.grants[k];
    }
    Balance { grants: out, ..b }
}

/// `PUT /api/leave/opening` で 1 回に送れる行数
pub const MAX_OPENING_ROWS: usize = 1000;

/// `PUT /api/leave/opening` の本文。`carryover` と `current` が両方 null の行は削除、片方 null は 0
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct OpeningPutInput {
    pub as_of: String,
    pub rows: Vec<OpeningRowInput>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct OpeningRowInput {
    pub employee_cd: String,
    #[serde(default)]
    pub carryover: Option<f64>,
    #[serde(default)]
    pub current: Option<f64>,
}

/// 検証済みの期首残の一括保存 (乗務員が一覧に居るか・G0 / G1 は口が見る)
#[derive(Debug, Clone, PartialEq)]
pub struct OpeningPut {
    pub as_of: NaiveDate,
    /// (乗務員コード, 繰越, 今年度)
    pub upserts: Vec<(String, i32, i32)>,
    pub deletes: Vec<String>,
}

/// 年度 (4 月始まり) の初日と末日。`d` を含む 4/1〜翌 3/31 (表せなければ `None`)
pub fn fiscal_year_of(d: NaiveDate) -> Option<(NaiveDate, NaiveDate)> {
    let y = if d.month() >= 4 {
        d.year()
    } else {
        d.year() - 1
    };
    let start = NaiveDate::from_ymd_opt(y, 4, 1)?;
    let end = NaiveDate::from_ymd_opt(y + 1, 3, 31)?;
    Some((start, end))
}

/// `YYYY-MM-DD` だけを日付に
pub fn parse_date(raw: &str) -> Option<NaiveDate> {
    if raw.len() != 10 {
        return None;
    }
    NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()
}

/// 1 行でも不正なら全体を `Err` (一部だけ保存しない)。同じ乗務員が 2 回あれば `invalid_body`
pub fn validate_opening(input: OpeningPutInput) -> Result<OpeningPut, &'static str> {
    let as_of = parse_date(&input.as_of).ok_or("invalid_as_of")?;
    if input.rows.len() > MAX_OPENING_ROWS {
        return Err("too_many_rows");
    }
    let mut seen = BTreeSet::new();
    let mut out = OpeningPut {
        as_of,
        upserts: Vec::new(),
        deletes: Vec::new(),
    };
    for row in input.rows {
        let code = row.employee_cd.trim().to_owned();
        if !seen.insert(code.clone()) {
            return Err("invalid_body");
        }
        if row.carryover.is_none() && row.current.is_none() {
            out.deletes.push(code);
            continue;
        }
        let t = |v: Option<f64>| tenths(v.unwrap_or(0.0)).ok_or("invalid_days");
        out.upserts.push((code, t(row.carryover)?, t(row.current)?));
    }
    Ok(out)
}

/// 0.1 日単位を日数 (JSON の数値) に
pub fn days(tenths: i32) -> f64 {
    f64::from(tenths) / 10.0
}

/// 日数 (入力) を 0.1 日単位に。0 以上 999.9 以下で、小数は 1 桁までだけ
pub fn tenths(days: f64) -> Option<i32> {
    let t = (days * 10.0).round();
    let ok = days.is_finite() && (0.0..=9999.0).contains(&t) && (days * 10.0 - t).abs() < 1e-6;
    ok.then_some(t as i32)
}

/// 応答の付与 1 件の残の内訳 (日数)
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct BalanceView {
    pub as_of: NaiveDate,
    pub remaining: f64,
    pub expired: f64,
    pub unallocated: f64,
}

impl Balance {
    pub fn view(&self, as_of: NaiveDate) -> BalanceView {
        BalanceView {
            as_of,
            remaining: days(self.remaining),
            expired: days(self.expired),
            unallocated: days(self.unallocated),
        }
    }
}

/// 付与台帳の 1 行 (応答の形。`days` は日数)
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Grant {
    pub id: Uuid,
    pub employee_cd: String,
    pub grant_date: NaiveDate,
    pub days: f64,
    pub expires_on: NaiveDate,
    pub source: String,
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TxOutput for Grant {}

impl Grant {
    /// 残日数の計算の入力
    pub fn input(&self) -> GrantIn {
        GrantIn {
            id: self.id,
            grant_date: self.grant_date,
            expires_on: self.expires_on,
            tenths: (self.days * 10.0).round() as i32,
        }
    }
}

/// `POST /api/leave/grants` の本文 (`expires_on` を省けば付与日 + 2 年)
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct NewGrantInput {
    pub employee_cd: String,
    pub grant_date: NaiveDate,
    pub days: f64,
    #[serde(default)]
    pub expires_on: Option<NaiveDate>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `PUT /api/leave/grants/{id}` の本文 (全体の置き換え。`note` を省けば消す)
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct GrantUpdateInput {
    pub days: f64,
    pub expires_on: NaiveDate,
    #[serde(default)]
    pub note: Option<String>,
}

/// 検証済みの付与 (DB へ書く値)。`employee_cd` と `grant_date` は追加のときだけ使う
#[derive(Debug, Clone, PartialEq)]
pub struct GrantWrite {
    pub employee_cd: String,
    pub grant_date: Option<NaiveDate>,
    pub tenths: i32,
    pub expires_on: NaiveDate,
    pub note: Option<String>,
}

fn valid_note(note: Option<String>) -> Result<Option<String>, &'static str> {
    let note = non_empty(note);
    if note
        .as_ref()
        .is_some_and(|n| n.chars().count() > MAX_NOTE_CHARS)
    {
        return Err("note_too_long");
    }
    Ok(note)
}

/// 乗務員コードの検証 (前後の空白を落とし、空・長すぎは `invalid_employee_cd`)
pub fn valid_employee_cd(raw: Option<String>) -> Result<String, &'static str> {
    non_empty(raw)
        .filter(|c| c.len() <= MAX_EMPLOYEE_CD_LEN)
        .ok_or("invalid_employee_cd")
}

pub fn validate_new(input: NewGrantInput) -> Result<GrantWrite, &'static str> {
    let employee_cd = valid_employee_cd(Some(input.employee_cd))?;
    let tenths = tenths(input.days).ok_or("invalid_days")?;
    let expires_on = match input.expires_on {
        Some(e) => e,
        None => expires_on(input.grant_date).ok_or("invalid_grant_date")?,
    };
    if expires_on <= input.grant_date {
        return Err("expires_before_grant");
    }
    Ok(GrantWrite {
        employee_cd,
        grant_date: Some(input.grant_date),
        tenths,
        expires_on,
        note: valid_note(input.note)?,
    })
}

/// 付与日は変えない (付与日との前後は DB の CHECK が見る)
pub fn validate_update(input: GrantUpdateInput) -> Result<GrantWrite, &'static str> {
    Ok(GrantWrite {
        employee_cd: String::new(),
        grant_date: None,
        tenths: tenths(input.days).ok_or("invalid_days")?,
        expires_on: input.expires_on,
        note: valid_note(input.note)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn dates(v: &[AutoGrant]) -> Vec<String> {
        v.iter().map(|g| g.grant_date.to_string()).collect()
    }

    #[test]
    fn add_months_keeps_day_or_aligns_month_end() {
        assert_eq!(add_months(d("2020-04-01"), 6), Some(d("2020-10-01")));
        assert_eq!(add_months(d("2020-04-30"), 6), Some(d("2020-10-31")));
        assert_eq!(add_months(d("2021-08-31"), 6), Some(d("2022-02-28")));
        assert_eq!(add_months(d("2023-08-31"), 6), Some(d("2024-02-29")));
        assert_eq!(add_months(d("2023-08-30"), 6), Some(d("2024-02-29")));
        assert_eq!(add_months(d("2021-08-30"), 6), Some(d("2022-02-28")));
        // 閏年 2/29 の入社は月末日扱い
        assert_eq!(add_months(d("2024-02-29"), 6), Some(d("2024-08-31")));
        assert_eq!(add_months(d("2024-02-29"), 24), Some(d("2026-02-28")));
        assert_eq!(add_months(d("2024-02-28"), 24), Some(d("2026-02-28")));
        // 12 月の末日の行き先
        assert_eq!(add_months(d("2024-06-30"), 6), Some(d("2024-12-31")));
        // 表せない
        assert_eq!(add_months(NaiveDate::MAX, 1), None);
        assert_eq!(add_months(d("+262142-12-31"), 0), None);
    }

    #[test]
    fn schedule_up_to_six_and_half_years_and_after() {
        let g = statutory(d("2015-04-01"), None, d("2026-10-10"));
        let got: Vec<(String, i32, String)> = g
            .iter()
            .map(|g| (g.grant_date.to_string(), g.tenths, g.expires_on.to_string()))
            .collect();
        assert_eq!(got[0], ("2015-10-01".into(), 100, "2017-10-01".into()));
        let tenths: Vec<i32> = g.iter().map(|g| g.tenths).collect();
        assert_eq!(
            tenths,
            vec![100, 110, 120, 140, 160, 180, 200, 200, 200, 200, 200, 200]
        );
        assert_eq!(dates(&g).last().unwrap(), "2026-10-01");
    }

    #[test]
    fn only_grants_up_to_today() {
        assert!(statutory(d("2026-04-11"), None, d("2026-10-10")).is_empty());
        assert_eq!(
            dates(&statutory(d("2026-04-10"), None, d("2026-10-10"))),
            vec!["2026-10-10"]
        );
    }

    #[test]
    fn month_end_hire() {
        let g = statutory(d("2023-08-31"), None, d("2025-03-01"));
        assert_eq!(dates(&g), vec!["2024-02-29", "2025-02-28"]);
        assert_eq!(g[0].expires_on, d("2026-02-28"));
        let leap = statutory(d("2024-02-29"), None, d("2026-01-01"));
        assert_eq!(dates(&leap), vec!["2024-08-31", "2025-08-31"]);
    }

    #[test]
    fn no_grants_on_or_after_retirement() {
        let g = statutory(d("2020-04-01"), Some(d("2021-10-01")), d("2026-10-10"));
        assert_eq!(dates(&g), vec!["2020-10-01"]);
        let g = statutory(d("2020-04-01"), Some(d("2021-10-02")), d("2026-10-10"));
        assert_eq!(dates(&g), vec!["2020-10-01", "2021-10-01"]);
        assert!(statutory(d("2020-04-01"), Some(d("2020-05-01")), d("2026-10-10")).is_empty());
    }

    #[test]
    fn far_dates_stop() {
        let g = statutory(d("+262140-01-01"), None, NaiveDate::MAX);
        assert_eq!(g.len(), 1);
        // 失効日が表せない
        assert!(statutory(d("+262142-03-01"), None, NaiveDate::MAX).is_empty());
        // 付与日が表せない
        assert!(statutory(d("+262142-08-01"), None, NaiveDate::MAX).is_empty());
    }

    fn grant(n: u128, from: &str, to: &str, tenths: i32) -> GrantIn {
        GrantIn {
            id: Uuid::from_u128(n),
            grant_date: d(from),
            expires_on: d(to),
            tenths,
        }
    }

    #[test]
    fn fifo_from_oldest_valid_grant() {
        let grants = [
            grant(2, "2025-10-01", "2027-10-01", 110),
            grant(1, "2024-10-01", "2026-10-01", 100),
        ];
        let usages = [
            (d("2025-11-01"), 10),
            (d("2025-11-02"), 5),
            (d("2025-09-01"), 10),
        ];
        let b = balance(&grants, &usages, d("2026-03-31"));
        assert_eq!(
            b.grants[1],
            GrantBalance {
                id: Uuid::from_u128(1),
                used: 25,
                remaining: 75,
                expired: 0
            }
        );
        assert_eq!(
            b.grants[0],
            GrantBalance {
                id: Uuid::from_u128(2),
                used: 0,
                remaining: 110,
                expired: 0
            }
        );
        assert_eq!((b.remaining, b.expired, b.unallocated), (185, 0, 0));
    }

    #[test]
    fn spill_to_next_grant_and_expire() {
        let grants = [
            grant(1, "2024-10-01", "2026-10-01", 10),
            grant(2, "2025-10-01", "2027-10-01", 110),
        ];
        let usages = [
            (d("2025-10-02"), 10),
            (d("2025-10-03"), 10),
            (d("2026-11-01"), 5),
        ];
        let b = balance(&grants, &usages, d("2026-11-30"));
        assert_eq!(
            b.grants[0],
            GrantBalance {
                id: Uuid::from_u128(1),
                used: 10,
                remaining: 0,
                expired: 0
            }
        );
        assert_eq!(
            b.grants[1],
            GrantBalance {
                id: Uuid::from_u128(2),
                used: 15,
                remaining: 95,
                expired: 0
            }
        );
        // 古い付与が残ったまま失効する
        let b = balance(&grants, &[(d("2025-10-02"), 5)], d("2026-10-01"));
        assert_eq!(
            b.grants[0],
            GrantBalance {
                id: Uuid::from_u128(1),
                used: 5,
                remaining: 0,
                expired: 5
            }
        );
        assert_eq!((b.remaining, b.expired), (110, 5));
    }

    #[test]
    fn unallocated_and_future_and_not_started() {
        let grants = [grant(1, "2026-10-01", "2028-10-01", 100)];
        // 付与前の消化・付与を超える消化は当てられない。基準日より後の消化は数えない
        let usages = [
            (d("2026-09-30"), 10),
            (d("2026-10-02"), 100),
            (d("2026-10-03"), 5),
            (d("2026-12-01"), 10),
        ];
        let b = balance(&grants, &usages, d("2026-11-01"));
        assert_eq!((b.remaining, b.unallocated), (0, 15));
        // 基準日より後の付与は残に入らない
        let b = balance(&grants, &[], d("2026-09-01"));
        assert_eq!(b.grants[0].remaining, 0);
        assert_eq!(b.remaining, 0);
        let empty = balance(&[], &[], d("2026-09-01"));
        assert_eq!(
            (empty.remaining, empty.expired, empty.unallocated),
            (0, 0, 0)
        );
    }

    #[test]
    fn manual_and_auto_mixed_same_day_order() {
        // 同じ付与日なら先に失効する方から
        let grants = [
            grant(1, "2025-04-01", "2027-04-01", 30),
            grant(2, "2025-04-01", "2026-04-01", 20),
        ];
        let b = balance(&grants, &[(d("2025-05-01"), 25)], d("2025-06-01"));
        assert_eq!((b.grants[0].used, b.grants[1].used), (5, 20));
        let view = b.view(d("2025-06-01"));
        assert_eq!(
            view,
            BalanceView {
                as_of: d("2025-06-01"),
                remaining: 2.5,
                expired: 0.0,
                unallocated: 0.0
            }
        );
    }

    fn opening(as_of: &str, carryover: i32, current: i32) -> Opening {
        Opening {
            as_of: d(as_of),
            carryover,
            current,
        }
    }

    fn og_dates(o: &OpeningGrants) -> (Option<String>, Option<String>) {
        (
            o.carryover.map(|g| g.grant_date.to_string()),
            o.current.map(|g| g.grant_date.to_string()),
        )
    }

    #[test]
    fn opening_picks_g0_and_g1() {
        // 入社 2020-04-01: X 以前の法定付与日の直近が G1、その前が G0。日数は期首残に差し替え、失効は付与日 + 2 年
        let o = opening_grants(opening("2026-09-30", 35, 120), Some(d("2020-04-01")), None);
        assert_eq!(
            o.carryover,
            Some(AutoGrant {
                grant_date: d("2024-10-01"),
                tenths: 35,
                expires_on: d("2026-10-01")
            })
        );
        assert_eq!(
            o.current,
            Some(AutoGrant {
                grant_date: d("2025-10-01"),
                tenths: 120,
                expires_on: d("2027-10-01")
            })
        );
        assert_eq!(o.error(), None);
        // X が付与日の当日なら、その付与が G1
        let o = opening_grants(opening("2025-10-01", 0, 0), Some(d("2020-04-01")), None);
        assert_eq!(
            og_dates(&o),
            (Some("2024-10-01".into()), Some("2025-10-01".into()))
        );
        // 月末日の入社は失効も月末日にそろう
        let o = opening_grants(opening("2025-03-01", 0, 0), Some(d("2023-08-31")), None);
        assert_eq!(o.carryover.unwrap().expires_on, d("2026-02-28"));
        assert_eq!(o.current.unwrap().expires_on, d("2027-02-28"));
    }

    #[test]
    fn opening_without_prior_grants() {
        // 入社 6 か月未満: G0 も G1 も無い
        let o = opening_grants(opening("2026-09-30", 0, 0), Some(d("2026-04-01")), None);
        assert_eq!(og_dates(&o), (None, None));
        assert_eq!(o.error(), None);
        assert_eq!(
            opening_grants(opening("2026-09-30", 0, 5), Some(d("2026-04-01")), None).error(),
            Some("opening_no_current_grant")
        );
        // 1 年半未満: G1 だけ
        let o = opening_grants(opening("2026-09-30", 0, 100), Some(d("2025-04-01")), None);
        assert_eq!(og_dates(&o), (None, Some("2025-10-01".into())));
        assert_eq!(o.error(), None);
        assert_eq!(
            opening_grants(opening("2026-09-30", 5, 100), Some(d("2025-04-01")), None).error(),
            Some("opening_no_prior_grant")
        );
        // 入社日が無い
        let o = opening_grants(opening("2026-09-30", 0, 0), None, None);
        assert_eq!(og_dates(&o), (None, None));
        // 退職日の当日以降の付与は作らない
        let o = opening_grants(
            opening("2026-09-30", 0, 0),
            Some(d("2020-04-01")),
            Some(d("2025-10-01")),
        );
        assert_eq!(
            og_dates(&o),
            (Some("2023-10-01".into()), Some("2024-10-01".into()))
        );
    }

    #[test]
    fn balance_with_opening_replaces_old_grants() {
        // 入社 2020-04-01、X = 2026-09-30 (G0 = 2024-10-01 繰越 3.5 日、G1 = 2025-10-01 今年度 12 日)
        let o = opening_grants(opening("2026-09-30", 35, 120), Some(d("2020-04-01")), None);
        let grants = [
            // 毎朝の自動付与が入れた古い行 (X 以前) は外す
            grant(1, "2024-10-01", "2026-10-01", 180),
            grant(2, "2025-10-01", "2027-10-01", 200),
            // X より後の行は従来どおり
            grant(3, "2026-10-01", "2028-10-01", 200),
        ];
        let usages = [
            // X 当日までの消化は期首残に含まれている
            (d("2026-09-30"), 10),
            (d("2025-12-01"), 10),
            // X の翌日以降だけを数える (繰越は 2026-10-01 に失効しているので今年度から)
            (d("2026-10-01"), 5),
            (d("2026-10-02"), 10),
        ];
        let b = balance_with_opening(&grants, &usages, d("2026-10-10"), Some(&o));
        // 繰越は 2026-10-01 に失効 (その日の消化は今年度から)。今年度 12 - 1.5 = 10.5、2026-10-01 の付与 20
        assert_eq!((b.remaining, b.expired, b.unallocated), (305, 35, 0));
        assert_eq!(
            b.grants[0],
            GrantBalance {
                id: Uuid::from_u128(1),
                used: 0,
                remaining: 0,
                expired: 0
            }
        );
        assert_eq!(b.grants[1].remaining, 0);
        assert_eq!(
            b.grants[2],
            GrantBalance {
                id: Uuid::from_u128(3),
                used: 0,
                remaining: 200,
                expired: 0
            }
        );
        assert!(o.supersedes(d("2026-09-30")) && !o.supersedes(d("2026-10-01")));
        // X の翌日の基準日: 繰越がまだ使える
        let b = balance_with_opening(&grants, &[(d("2026-09-30"), 10)], d("2026-09-30"), Some(&o));
        assert_eq!((b.remaining, b.expired), (155, 0));
    }

    #[test]
    fn balance_with_opening_zero_and_absent() {
        let grants = [grant(1, "2025-10-01", "2027-10-01", 110)];
        let usages = [(d("2026-01-10"), 10)];
        // 期首残が無ければ従来どおり
        let plain = balance(&grants, &usages, d("2026-10-10"));
        assert_eq!(
            balance_with_opening(&grants, &usages, d("2026-10-10"), None),
            plain
        );
        // 基準日が期首日より前なら使わない
        let o = opening_grants(opening("2026-09-30", 0, 0), Some(d("2025-04-01")), None);
        assert!(!o.applies(d("2026-09-29")) && o.applies(d("2026-09-30")));
        assert_eq!(
            balance_with_opening(&grants, &usages, d("2026-09-29"), Some(&o)),
            balance(&grants, &usages, d("2026-09-29"))
        );
        // 0 日でも合成する (X 時点で 0)。X の翌日以降の消化は当てられない
        let b = balance_with_opening(&grants, &[(d("2026-10-01"), 10)], d("2026-10-10"), Some(&o));
        assert_eq!((b.remaining, b.expired, b.unallocated), (0, 0, 10));
        assert_eq!(b.grants[0].used, 0);
    }

    #[test]
    fn opening_view() {
        let o = opening_grants(opening("2026-09-30", 35, 0), Some(d("2025-04-01")), None);
        assert_eq!(
            o.view(),
            OpeningView {
                as_of: d("2026-09-30"),
                carryover: 3.5,
                current: 0.0,
                carryover_grant_date: None,
                carryover_expires_on: None,
                current_grant_date: Some(d("2025-10-01")),
                current_expires_on: Some(d("2027-10-01")),
            }
        );
    }

    fn put_input(as_of: &str, rows: Vec<(&str, Option<f64>, Option<f64>)>) -> OpeningPutInput {
        OpeningPutInput {
            as_of: as_of.into(),
            rows: rows
                .into_iter()
                .map(|(c, carryover, current)| OpeningRowInput {
                    employee_cd: c.into(),
                    carryover,
                    current,
                })
                .collect(),
        }
    }

    #[test]
    fn opening_put_validation() {
        let ok = validate_opening(put_input(
            "2026-09-30",
            vec![
                (" 1021 ", Some(3.5), Some(12.0)),
                ("1022", None, Some(1.0)),
                ("1023", None, None),
            ],
        ))
        .unwrap();
        assert_eq!(
            ok,
            OpeningPut {
                as_of: d("2026-09-30"),
                upserts: vec![("1021".into(), 35, 120), ("1022".into(), 0, 10)],
                deletes: vec!["1023".into()],
            }
        );
        let bad_dates = ["2026-9-30", "2026-02-30", "", "2026/09/30"]
            .map(|bad| validate_opening(put_input(bad, vec![])));
        assert_eq!(bad_dates, [const { Err("invalid_as_of") }; 4]);
        let many = vec![("1021", None, None); MAX_OPENING_ROWS + 1];
        assert_eq!(
            validate_opening(put_input("2026-09-30", many)),
            Err("too_many_rows")
        );
        for (c, r) in [
            (Some(-1.0), Some(1.0)),
            (Some(1.0), Some(1000.0)),
            (None, Some(0.05)),
        ] {
            assert_eq!(
                validate_opening(put_input("2026-09-30", vec![("1021", c, r)])),
                Err("invalid_days")
            );
        }
        assert_eq!(
            validate_opening(put_input(
                "2026-09-30",
                vec![("1021", None, None), (" 1021", Some(1.0), None)]
            )),
            Err("invalid_body")
        );
        // 1000 行ちょうどは行数では落ちない (同じ乗務員が並ぶので invalid_body)
        let full = vec![("1", None, None); MAX_OPENING_ROWS];
        assert_eq!(
            validate_opening(put_input("2026-09-30", full)),
            Err("invalid_body")
        );
    }

    #[test]
    fn tenths_and_days() {
        assert_eq!(tenths(10.0), Some(100));
        assert_eq!(tenths(0.5), Some(5));
        assert_eq!(tenths(999.9), Some(9999));
        assert_eq!(tenths(0.0), Some(0));
        for bad in [-0.5, 1000.0, 0.25, f64::NAN, f64::INFINITY] {
            assert_eq!(tenths(bad), None, "{bad}");
        }
        assert_eq!(days(125), 12.5);
        assert_eq!(expires_on(d("2025-10-01")), Some(d("2027-10-01")));
    }

    #[test]
    fn new_grant_validation() {
        let input = |days: f64, exp: Option<&str>| NewGrantInput {
            employee_cd: " 1021 ".into(),
            grant_date: d("2026-04-01"),
            days,
            expires_on: exp.map(d),
            note: Some(" 前職 ".into()),
        };
        let ok = validate_new(input(5.5, None)).unwrap();
        assert_eq!(
            ok,
            GrantWrite {
                employee_cd: "1021".into(),
                grant_date: Some(d("2026-04-01")),
                tenths: 55,
                expires_on: d("2028-04-01"),
                note: Some("前職".into())
            }
        );
        assert_eq!(
            validate_new(input(5.0, Some("2027-01-01")))
                .unwrap()
                .expires_on,
            d("2027-01-01")
        );
        assert_eq!(validate_new(input(-1.0, None)), Err("invalid_days"));
        assert_eq!(
            validate_new(input(1.0, Some("2026-04-01"))),
            Err("expires_before_grant")
        );
        let mut far = input(1.0, None);
        far.grant_date = NaiveDate::MAX;
        assert_eq!(validate_new(far), Err("invalid_grant_date"));
        let mut long = input(1.0, None);
        long.note = Some("あ".repeat(MAX_NOTE_CHARS + 1));
        assert_eq!(validate_new(long), Err("note_too_long"));
        let mut blank = input(1.0, None);
        blank.employee_cd = " ".into();
        assert_eq!(validate_new(blank), Err("invalid_employee_cd"));
        assert_eq!(
            valid_employee_cd(Some("1".repeat(MAX_EMPLOYEE_CD_LEN + 1))),
            Err("invalid_employee_cd")
        );
    }

    #[test]
    fn update_grant_validation() {
        let ok = validate_update(GrantUpdateInput {
            days: 3.0,
            expires_on: d("2027-01-01"),
            note: None,
        })
        .unwrap();
        assert_eq!((ok.tenths, ok.grant_date, ok.note), (30, None, None));
        assert_eq!(
            validate_update(GrantUpdateInput {
                days: 0.05,
                expires_on: d("2027-01-01"),
                note: None
            }),
            Err("invalid_days")
        );
        let long = GrantUpdateInput {
            days: 1.0,
            expires_on: d("2027-01-01"),
            note: Some("あ".repeat(MAX_NOTE_CHARS + 1)),
        };
        assert_eq!(validate_update(long), Err("note_too_long"));
    }

    #[test]
    fn fiscal_year_starts_in_april() {
        let fy = |s| fiscal_year_of(d(s));
        // 3/31 は前の年度、4/1 から新しい年度
        assert_eq!(fy("2027-03-31"), Some((d("2026-04-01"), d("2027-03-31"))));
        assert_eq!(fy("2026-04-01"), Some((d("2026-04-01"), d("2027-03-31"))));
        assert_eq!(fy("2026-03-31"), Some((d("2025-04-01"), d("2026-03-31"))));
        // 年をまたぐ (1 月・12 月)
        assert_eq!(fy("2026-01-15"), Some((d("2025-04-01"), d("2026-03-31"))));
        assert_eq!(fy("2026-12-31"), Some((d("2026-04-01"), d("2027-03-31"))));
        // 閏年の 2/29 も同じ年度の末日は 3/31
        assert_eq!(fy("2028-02-29"), Some((d("2027-04-01"), d("2028-03-31"))));
        // 表せない
        assert_eq!(fiscal_year_of(NaiveDate::MAX), None);
        assert_eq!(fiscal_year_of(NaiveDate::MIN), None);
    }
}
