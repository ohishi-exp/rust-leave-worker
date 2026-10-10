//! 有給の付与 (労基法 39 条の通常の労働者の法定付与) と、付与から消化を引いた残日数 (純粋関数)。
//!
//! - 日数は 0.1 日単位の整数 (`tenths`) で持つ (表の `days NUMERIC(4,1)`。終日 = 10、半日 = 5)
//! - 付与は `grant_date <= 日 < expires_on` の日に使える (`expires_on` = 失効する日。付与日 + 2 年)
//! - 比例付与 (パート) は扱わない (画面で手で直す)

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
}
