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
/// `r2_key` の絞りの長さ (バイト) の上限。object key (`ingest::seeded_object_key`) より十分長く取る
pub const MAX_R2_KEY_LEN: usize = 512;
/// `GET /api/leave/documents` の `state` の値 (open = 未紐づけのページが 1 つでも在る PDF / done = 無い PDF)
pub const DOCUMENT_STATES: [&str; 3] = ["open", "done", "all"];
pub const MAX_NOTE_CHARS: usize = 2000;
/// 1 ページに結べる一番星の行の数と、行 ID 1 つの長さ (バイト) の上限
pub const MAX_ICHIBAN_ROWS: usize = 200;
pub const MAX_ROW_ID_LEN: usize = 32;

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
    /// PDF の基準日 (同じ `r2_key` のページで同じ値。`None` は受信日を使う)
    pub base_date: Option<NaiveDate>,
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

/// PDF 1 件 (同じ `r2_key` のページの集約。応答の形)
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Document {
    pub r2_key: String,
    pub first_page_id: Uuid,
    pub received_at: DateTime<Utc>,
    pub mail_from: Option<String>,
    pub subject: Option<String>,
    pub page_count: i64,
    pub unlinked_count: i64,
    pub linked_count: i64,
    pub ignored_count: i64,
}

impl TxOutput for Document {}

/// `GET /api/leave/pages` の条件 (query の生の文字列。検証は [`ListQuery::parse`])
#[derive(Deserialize, Debug, Default)]
pub struct ListQueryRaw {
    pub status: Option<String>,
    pub employee_cd: Option<String>,
    pub r2_key: Option<String>,
    pub limit: Option<String>,
    pub offset: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListQuery {
    pub status: Option<String>,
    pub employee_cd: Option<String>,
    pub r2_key: Option<String>,
    pub limit: i64,
    pub offset: i64,
}

/// `GET /api/leave/documents` の条件 (query の生の文字列。検証は [`DocumentsQuery::parse`])
#[derive(Deserialize, Debug, Default)]
pub struct DocumentsQueryRaw {
    pub state: Option<String>,
    pub limit: Option<String>,
    pub offset: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DocumentsQuery {
    pub state: String,
    pub limit: i64,
    pub offset: i64,
}

pub(crate) fn non_empty(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

/// `limit` (既定・上限あり) と `offset` の検証。一覧の 2 つの口で同じ
fn parse_paging(limit: Option<String>, offset: Option<String>) -> Result<(i64, i64), &'static str> {
    let limit = match non_empty(limit) {
        None => DEFAULT_LIMIT,
        Some(s) => match s.parse::<i64>() {
            Ok(n) if n >= 1 => n.min(MAX_LIMIT),
            _ => return Err("invalid_limit"),
        },
    };
    let offset = match non_empty(offset) {
        None => 0,
        Some(s) => match s.parse::<i64>() {
            Ok(n) if n >= 0 => n,
            _ => return Err("invalid_offset"),
        },
    };
    Ok((limit, offset))
}

impl ListQuery {
    pub fn parse(raw: ListQueryRaw) -> Result<Self, &'static str> {
        let status = non_empty(raw.status);
        if status.as_deref().is_some_and(|s| !STATUSES.contains(&s)) {
            return Err("invalid_status");
        }
        let r2_key = non_empty(raw.r2_key);
        if r2_key.as_deref().is_some_and(|k| k.len() > MAX_R2_KEY_LEN) {
            return Err("invalid_r2_key");
        }
        let (limit, offset) = parse_paging(raw.limit, raw.offset)?;
        Ok(Self {
            status,
            employee_cd: non_empty(raw.employee_cd),
            r2_key,
            limit,
            offset,
        })
    }
}

impl DocumentsQuery {
    pub fn parse(raw: DocumentsQueryRaw) -> Result<Self, &'static str> {
        let state = non_empty(raw.state).unwrap_or_else(|| "open".to_owned());
        if !DOCUMENT_STATES.contains(&state.as_str()) {
            return Err("invalid_state");
        }
        let (limit, offset) = parse_paging(raw.limit, raw.offset)?;
        Ok(Self {
            state,
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
    /// 紐づける一番星の行 (`row_id`)。`status = linked` のときだけ使う
    #[serde(default)]
    pub ichiban_row_ids: Vec<String>,
}

/// 検証済みの更新 (DB へ書く値)
#[derive(Debug, Clone, PartialEq)]
pub struct PageUpdate {
    pub employee_cd: Option<String>,
    pub status: String,
    pub note: Option<String>,
    pub periods: Vec<Period>,
    /// 重複を除いて並べたもの
    pub ichiban_row_ids: Vec<String>,
}

/// `PUT /api/leave/pages/{id}/base-date` の本文 (`null` で受信日に戻す)
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct BaseDateInput {
    pub base_date: Option<NaiveDate>,
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
    if input.ichiban_row_ids.len() > MAX_ICHIBAN_ROWS {
        return Err("too_many_rows");
    }
    if input
        .ichiban_row_ids
        .iter()
        .any(|r| r.is_empty() || r.len() > MAX_ROW_ID_LEN)
    {
        return Err("invalid_row_id");
    }
    let mut ichiban_row_ids = input.ichiban_row_ids;
    ichiban_row_ids.sort();
    ichiban_row_ids.dedup();
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
        ichiban_row_ids,
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
            ichiban_row_ids: Vec::new(),
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
    fn row_ids_are_limited_and_deduplicated() {
        let with = |ids: Vec<String>| {
            let mut u = update(
                "linked",
                Some("1021"),
                vec![period("yukyu", "2026-10-01", "2026-10-01", None)],
            );
            u.ichiban_row_ids = ids;
            validate_update(u)
        };
        let ok = with(vec!["b".into(), "a".into(), "b".into()]).unwrap();
        assert_eq!(ok.ichiban_row_ids, vec!["a", "b"]);
        assert_eq!(
            with(vec!["a".into(); MAX_ICHIBAN_ROWS + 1]),
            Err("too_many_rows")
        );
        assert_eq!(with(vec![String::new()]), Err("invalid_row_id"));
        assert_eq!(
            with(vec!["1".repeat(MAX_ROW_ID_LEN + 1)]),
            Err("invalid_row_id")
        );
        assert!(with(vec!["1".repeat(MAX_ROW_ID_LEN)]).is_ok());
    }

    #[test]
    fn base_date_body() {
        let b: BaseDateInput = serde_json::from_str(r#"{"base_date":"2026-10-01"}"#).unwrap();
        assert_eq!(b.base_date, Some(d("2026-10-01")));
        let b: BaseDateInput = serde_json::from_str(r#"{"base_date":null}"#).unwrap();
        assert_eq!(b.base_date, None);
        assert!(serde_json::from_str::<BaseDateInput>(r#"{"base_date":null,"x":1}"#).is_err());
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
                r2_key: None,
                limit: DEFAULT_LIMIT,
                offset: 0
            }
        );
        let q = ListQuery::parse(ListQueryRaw {
            status: Some("linked".into()),
            employee_cd: Some(" 1021 ".into()),
            r2_key: Some(" inbox/a.pdf ".into()),
            limit: Some("1000".into()),
            offset: Some("40".into()),
        })
        .unwrap();
        assert_eq!(q.status.as_deref(), Some("linked"));
        assert_eq!(q.employee_cd.as_deref(), Some("1021"));
        assert_eq!(q.r2_key.as_deref(), Some("inbox/a.pdf"));
        assert_eq!(q.limit, MAX_LIMIT);
        assert_eq!(q.offset, 40);
        // 空の値は省いたのと同じ
        let q = ListQuery::parse(ListQueryRaw {
            status: Some("".into()),
            limit: Some("".into()),
            offset: Some("".into()),
            employee_cd: Some("".into()),
            r2_key: Some("".into()),
        })
        .unwrap();
        assert_eq!(q, ListQuery::parse(ListQueryRaw::default()).unwrap());
    }

    #[test]
    fn list_query_rejects() {
        let raw = |status: Option<&str>, limit: Option<&str>, offset: Option<&str>| ListQueryRaw {
            status: status.map(Into::into),
            employee_cd: None,
            r2_key: None,
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
    fn list_query_r2_key_length() {
        let raw = |k: String| ListQueryRaw {
            r2_key: Some(k),
            ..Default::default()
        };
        assert!(ListQuery::parse(raw("k".repeat(MAX_R2_KEY_LEN))).is_ok());
        assert_eq!(
            ListQuery::parse(raw("k".repeat(MAX_R2_KEY_LEN + 1))),
            Err("invalid_r2_key")
        );
    }

    #[test]
    fn documents_query_defaults_and_clamp() {
        let q = DocumentsQuery::parse(DocumentsQueryRaw::default()).unwrap();
        assert_eq!(
            q,
            DocumentsQuery {
                state: "open".into(),
                limit: DEFAULT_LIMIT,
                offset: 0
            }
        );
        for s in DOCUMENT_STATES {
            let q = DocumentsQuery::parse(DocumentsQueryRaw {
                state: Some(format!(" {s} ")),
                limit: Some("1000".into()),
                offset: Some("40".into()),
            })
            .unwrap();
            assert_eq!((q.state.as_str(), q.limit, q.offset), (s, MAX_LIMIT, 40));
        }
        // 空の state は既定の open
        let q = DocumentsQuery::parse(DocumentsQueryRaw {
            state: Some("".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(q.state, "open");
    }

    #[test]
    fn documents_query_rejects() {
        let raw =
            |state: Option<&str>, limit: Option<&str>, offset: Option<&str>| DocumentsQueryRaw {
                state: state.map(Into::into),
                limit: limit.map(Into::into),
                offset: offset.map(Into::into),
            };
        assert_eq!(
            DocumentsQuery::parse(raw(Some("linked"), None, None)),
            Err("invalid_state")
        );
        assert_eq!(
            DocumentsQuery::parse(raw(None, Some("0"), None)),
            Err("invalid_limit")
        );
        assert_eq!(
            DocumentsQuery::parse(raw(None, None, Some("-1"))),
            Err("invalid_offset")
        );
    }

    #[test]
    fn update_rejects_unknown_fields() {
        let r: Result<PageUpdateInput, _> =
            serde_json::from_str(r#"{"status":"unlinked","tenant_id":"x"}"#);
        assert!(r.is_err());
    }
}
