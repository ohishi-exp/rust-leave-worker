//! 申請書のページ (`leave_pages`) と期間 (`leave_periods`) の形と、一覧の条件・更新の本文の検証。
//! 値の集合と制約は表の CHECK と同じ (ippoan/alc-migrations の 163_leave_tables.sql)。

use alc_worker_db::TxOutput;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const STATUSES: [&str; 3] = ["unlinked", "linked", "ignored"];
pub const KINDS: [&str; 4] = ["yukyu", "rousai", "tokukyu", "kekkin"];
pub const HALVES: [&str; 2] = ["am", "pm"];

/// 一覧の件数の既定と上限 (上限を超えた指定は上限に寄せる)
pub const DEFAULT_LIMIT: i64 = 50;
pub const MAX_LIMIT: i64 = 200;
/// 1 ページに付けられる期間の行数の上限
pub const MAX_PERIODS: usize = 50;
pub const MAX_EMPLOYEE_CD_LEN: usize = 32;
pub const MAX_NOTE_CHARS: usize = 2000;

/// ページ 1 行 (応答の形)
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Page {
    pub id: Uuid,
    pub r2_key: String,
    pub page_no: i32,
    pub mail_from: Option<String>,
    pub subject: Option<String>,
    pub received_at: DateTime<Utc>,
    pub employee_cd: Option<String>,
    pub status: String,
    pub linked_by: Option<String>,
    pub linked_at: Option<DateTime<Utc>>,
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl TxOutput for Page {}

/// 期間 1 行 (応答の形であり、検証済みの書き込みの値でもある)
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Period {
    pub kind: String,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub half: Option<String>,
}

impl TxOutput for Period {}

/// `GET /api/leave/pages/{id}` の応答
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct PageDetail {
    #[serde(flatten)]
    pub page: Page,
    pub periods: Vec<Period>,
}

impl TxOutput for PageDetail {}

/// `GET /api/leave/pages` の条件 (query の生の文字列。検証は [`ListQuery::parse`])
#[derive(Deserialize, Debug, Default)]
pub struct ListQueryRaw {
    pub status: Option<String>,
    pub employee_cd: Option<String>,
    pub limit: Option<String>,
    pub offset: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListQuery {
    pub status: Option<String>,
    pub employee_cd: Option<String>,
    pub limit: i64,
    pub offset: i64,
}

pub(crate) fn non_empty(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

impl ListQuery {
    pub fn parse(raw: ListQueryRaw) -> Result<Self, &'static str> {
        let status = non_empty(raw.status);
        if status.as_deref().is_some_and(|s| !STATUSES.contains(&s)) {
            return Err("invalid_status");
        }
        let limit = match non_empty(raw.limit) {
            None => DEFAULT_LIMIT,
            Some(s) => match s.parse::<i64>() {
                Ok(n) if n >= 1 => n.min(MAX_LIMIT),
                _ => return Err("invalid_limit"),
            },
        };
        let offset = match non_empty(raw.offset) {
            None => 0,
            Some(s) => match s.parse::<i64>() {
                Ok(n) if n >= 0 => n,
                _ => return Err("invalid_offset"),
            },
        };
        Ok(Self {
            status,
            employee_cd: non_empty(raw.employee_cd),
            limit,
            offset,
        })
    }
}

/// `PUT /api/leave/pages/{id}` の期間 1 行
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct PeriodInput {
    pub kind: String,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    #[serde(default)]
    pub half: Option<String>,
}

/// `PUT /api/leave/pages/{id}` の本文
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct PageUpdateInput {
    #[serde(default)]
    pub employee_cd: Option<String>,
    pub status: String,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub periods: Vec<PeriodInput>,
}

/// 検証済みの更新 (DB へ書く値)
#[derive(Debug, Clone, PartialEq)]
pub struct PageUpdate {
    pub employee_cd: Option<String>,
    pub status: String,
    pub note: Option<String>,
    pub periods: Vec<Period>,
}

/// 期間 1 行の検証: 区分・半日の値・逆順・半日は単日だけ
pub fn validate_period(p: PeriodInput) -> Result<Period, &'static str> {
    if !KINDS.contains(&p.kind.as_str()) {
        return Err("invalid_kind");
    }
    if p.end_date < p.start_date {
        return Err("period_end_before_start");
    }
    let half = non_empty(p.half);
    if let Some(h) = half.as_deref() {
        if !HALVES.contains(&h) {
            return Err("invalid_half");
        }
        if p.start_date != p.end_date {
            return Err("half_day_must_be_single_day");
        }
    }
    Ok(Period {
        kind: p.kind,
        start_date: p.start_date,
        end_date: p.end_date,
        half,
    })
}

/// 更新の本文の検証。`status = linked` は社員C と期間 1 行以上が必須
pub fn validate_update(input: PageUpdateInput) -> Result<PageUpdate, &'static str> {
    if !STATUSES.contains(&input.status.as_str()) {
        return Err("invalid_status");
    }
    let employee_cd = non_empty(input.employee_cd);
    if employee_cd
        .as_deref()
        .is_some_and(|c| c.len() > MAX_EMPLOYEE_CD_LEN)
    {
        return Err("invalid_employee_cd");
    }
    let note = non_empty(input.note);
    if note
        .as_deref()
        .is_some_and(|n| n.chars().count() > MAX_NOTE_CHARS)
    {
        return Err("note_too_long");
    }
    if input.periods.len() > MAX_PERIODS {
        return Err("too_many_periods");
    }
    let periods = input
        .periods
        .into_iter()
        .map(validate_period)
        .collect::<Result<Vec<_>, _>>()?;
    if input.status == "linked" {
        if employee_cd.is_none() {
            return Err("linked_requires_employee_cd");
        }
        if periods.is_empty() {
            return Err("linked_requires_periods");
        }
    }
    Ok(PageUpdate {
        employee_cd,
        status: input.status,
        note,
        periods,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn period(kind: &str, start: &str, end: &str, half: Option<&str>) -> PeriodInput {
        PeriodInput {
            kind: kind.into(),
            start_date: d(start),
            end_date: d(end),
            half: half.map(Into::into),
        }
    }

    fn update(status: &str, cd: Option<&str>, periods: Vec<PeriodInput>) -> PageUpdateInput {
        PageUpdateInput {
            employee_cd: cd.map(Into::into),
            status: status.into(),
            note: None,
            periods,
        }
    }

    #[test]
    fn period_ok() {
        for k in KINDS {
            assert!(validate_period(period(k, "2026-10-01", "2026-10-03", None)).is_ok());
        }
        let p = validate_period(period("yukyu", "2026-10-01", "2026-10-01", Some("am"))).unwrap();
        assert_eq!(p.half.as_deref(), Some("am"));
        // 空の half は None
        let p = validate_period(period("yukyu", "2026-10-01", "2026-10-02", Some(" "))).unwrap();
        assert_eq!(p.half, None);
    }

    #[test]
    fn period_rejects() {
        assert_eq!(
            validate_period(period("paid", "2026-10-01", "2026-10-01", None)),
            Err("invalid_kind")
        );
        assert_eq!(
            validate_period(period("yukyu", "2026-10-02", "2026-10-01", None)),
            Err("period_end_before_start")
        );
        assert_eq!(
            validate_period(period("yukyu", "2026-10-01", "2026-10-02", Some("am"))),
            Err("half_day_must_be_single_day")
        );
        assert_eq!(
            validate_period(period("yukyu", "2026-10-01", "2026-10-01", Some("noon"))),
            Err("invalid_half")
        );
    }

    #[test]
    fn linked_requires_employee_and_periods() {
        let p = || vec![period("rousai", "2026-10-01", "2026-10-01", None)];
        assert_eq!(
            validate_update(update("linked", None, p())),
            Err("linked_requires_employee_cd")
        );
        assert_eq!(
            validate_update(update("linked", Some("  "), p())),
            Err("linked_requires_employee_cd")
        );
        assert_eq!(
            validate_update(update("linked", Some("1021"), vec![])),
            Err("linked_requires_periods")
        );
        let ok = validate_update(update("linked", Some(" 1021 "), p())).unwrap();
        assert_eq!(ok.employee_cd.as_deref(), Some("1021"));
        assert_eq!(ok.periods.len(), 1);
    }

    #[test]
    fn unlinked_and_ignored_may_be_empty() {
        for s in ["unlinked", "ignored"] {
            let ok = validate_update(update(s, None, vec![])).unwrap();
            assert_eq!(ok.status, s);
            assert!(ok.periods.is_empty());
        }
    }

    #[test]
    fn update_rejects() {
        assert_eq!(
            validate_update(update("done", Some("1"), vec![])),
            Err("invalid_status")
        );
        let long = "1".repeat(MAX_EMPLOYEE_CD_LEN + 1);
        assert_eq!(
            validate_update(update("unlinked", Some(&long), vec![])),
            Err("invalid_employee_cd")
        );
        let mut u = update("unlinked", None, vec![]);
        u.note = Some("あ".repeat(MAX_NOTE_CHARS + 1));
        assert_eq!(validate_update(u), Err("note_too_long"));
        let many = (0..=MAX_PERIODS)
            .map(|_| period("yukyu", "2026-10-01", "2026-10-01", None))
            .collect();
        assert_eq!(
            validate_update(update("unlinked", None, many)),
            Err("too_many_periods")
        );
        // 期間 1 行のエラーはそのまま返る
        assert_eq!(
            validate_update(update(
                "linked",
                Some("1"),
                vec![period("yukyu", "2026-10-03", "2026-10-01", None)]
            )),
            Err("period_end_before_start")
        );
    }

    #[test]
    fn note_is_trimmed() {
        let mut u = update("ignored", None, vec![]);
        u.note = Some("  重複  ".into());
        assert_eq!(validate_update(u).unwrap().note.as_deref(), Some("重複"));
    }

    #[test]
    fn list_query_defaults_and_clamp() {
        let q = ListQuery::parse(ListQueryRaw::default()).unwrap();
        assert_eq!(
            q,
            ListQuery {
                status: None,
                employee_cd: None,
                limit: DEFAULT_LIMIT,
                offset: 0
            }
        );
        let q = ListQuery::parse(ListQueryRaw {
            status: Some("linked".into()),
            employee_cd: Some(" 1021 ".into()),
            limit: Some("1000".into()),
            offset: Some("40".into()),
        })
        .unwrap();
        assert_eq!(q.status.as_deref(), Some("linked"));
        assert_eq!(q.employee_cd.as_deref(), Some("1021"));
        assert_eq!(q.limit, MAX_LIMIT);
        assert_eq!(q.offset, 40);
        // 空の値は省いたのと同じ
        let q = ListQuery::parse(ListQueryRaw {
            status: Some("".into()),
            limit: Some("".into()),
            offset: Some("".into()),
            employee_cd: Some("".into()),
        })
        .unwrap();
        assert_eq!(q, ListQuery::parse(ListQueryRaw::default()).unwrap());
    }

    #[test]
    fn list_query_rejects() {
        let raw = |status: Option<&str>, limit: Option<&str>, offset: Option<&str>| ListQueryRaw {
            status: status.map(Into::into),
            employee_cd: None,
            limit: limit.map(Into::into),
            offset: offset.map(Into::into),
        };
        assert_eq!(
            ListQuery::parse(raw(Some("x"), None, None)),
            Err("invalid_status")
        );
        for l in ["0", "-1", "a"] {
            assert_eq!(
                ListQuery::parse(raw(None, Some(l), None)),
                Err("invalid_limit")
            );
        }
        for o in ["-1", "a"] {
            assert_eq!(
                ListQuery::parse(raw(None, None, Some(o))),
                Err("invalid_offset")
            );
        }
    }

    #[test]
    fn update_rejects_unknown_fields() {
        let r: Result<PageUpdateInput, _> =
            serde_json::from_str(r#"{"status":"unlinked","tenant_id":"x"}"#);
        assert!(r.is_err());
    }
}
