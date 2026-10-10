//! tenant ごとの設定 (`leave_settings` の 1 行) の形と、`PUT /api/leave/settings` の入力の検証。

use alc_worker_db::TxOutput;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// `grace_days` の既定値と範囲 (表の DEFAULT・CHECK と同じ)
pub const DEFAULT_GRACE_DAYS: i32 = 7;
pub const GRACE_DAYS_MAX: i32 = 60;

/// 設定 1 行 (応答の形)。行が無い tenant には [`Settings::default`] を返す (`updated_at` = null)
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Settings {
    pub mailbox: Option<String>,
    pub allowed_senders: Vec<String>,
    pub dept_codes: Vec<String>,
    pub lineworks_channel_id: Option<Uuid>,
    /// 個人の宛先 (`notify_recipients.id`、LINE WORKS の行)。グループ (`lineworks_channel_id`) とどちらか 1 つ
    pub notify_recipient_id: Option<Uuid>,
    pub grace_days: i32,
    pub enabled: bool,
    pub updated_at: Option<DateTime<Utc>>,
}

impl TxOutput for Settings {}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mailbox: None,
            allowed_senders: Vec::new(),
            dept_codes: Vec::new(),
            lineworks_channel_id: None,
            notify_recipient_id: None,
            grace_days: DEFAULT_GRACE_DAYS,
            enabled: false,
            updated_at: None,
        }
    }
}

/// `PUT /api/leave/settings` の本文。PUT なので全体の置き換えで、省いた項目は既定値になる
#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct SettingsInput {
    #[serde(default)]
    pub mailbox: Option<String>,
    #[serde(default)]
    pub allowed_senders: Vec<String>,
    #[serde(default)]
    pub dept_codes: Vec<String>,
    #[serde(default)]
    pub lineworks_channel_id: Option<Uuid>,
    #[serde(default)]
    pub notify_recipient_id: Option<Uuid>,
    #[serde(default)]
    pub grace_days: Option<i32>,
    #[serde(default)]
    pub enabled: bool,
}

/// 検証済みの設定 (DB へ書く値)
#[derive(Debug, Clone, PartialEq)]
pub struct ValidSettings {
    pub mailbox: Option<String>,
    pub allowed_senders: Vec<String>,
    pub dept_codes: Vec<String>,
    pub lineworks_channel_id: Option<Uuid>,
    pub notify_recipient_id: Option<Uuid>,
    pub grace_days: i32,
    pub enabled: bool,
}

/// mailbox (受信アドレスの local-part) を小文字にして `^[a-z0-9-]{3,64}$` を確かめる。
/// 前後の空白は落とし、空なら `None` (受信しない)
pub fn normalize_mailbox(raw: Option<&str>) -> Result<Option<String>, &'static str> {
    let Some(v) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let v = v.to_ascii_lowercase();
    let ok = (3..=64).contains(&v.len())
        && v.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok {
        Ok(Some(v))
    } else {
        Err("invalid_mailbox")
    }
}

/// 前後の空白を落とし、空を捨て、重複を最初の 1 つに寄せる (順は保つ)
fn clean_list(items: Vec<String>, lower: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        let item = item.trim();
        let item = if lower {
            item.to_ascii_lowercase()
        } else {
            item.to_owned()
        };
        if !item.is_empty() && !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

/// 入力を検証して DB へ書く値にする。エラーは 400 の本文の語
pub fn validate(input: SettingsInput) -> Result<ValidSettings, &'static str> {
    let mailbox = normalize_mailbox(input.mailbox.as_deref())?;
    let grace_days = input.grace_days.unwrap_or(DEFAULT_GRACE_DAYS);
    if !(0..=GRACE_DAYS_MAX).contains(&grace_days) {
        return Err("invalid_grace_days");
    }
    // 宛先はグループか個人のどちらか 1 つ (表の CHECK と同じ)
    if input.lineworks_channel_id.is_some() && input.notify_recipient_id.is_some() {
        return Err("both_notify_targets");
    }
    Ok(ValidSettings {
        mailbox,
        allowed_senders: clean_list(input.allowed_senders, true),
        dept_codes: clean_list(input.dept_codes, false),
        lineworks_channel_id: input.lineworks_channel_id,
        notify_recipient_id: input.notify_recipient_id,
        grace_days,
        enabled: input.enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailbox_lowercased_and_trimmed() {
        assert_eq!(
            normalize_mailbox(Some("  Ohishi-Leave1 ")),
            Ok(Some("ohishi-leave1".to_owned()))
        );
        assert_eq!(normalize_mailbox(Some("abc")), Ok(Some("abc".to_owned())));
        let max = "a".repeat(64);
        assert_eq!(normalize_mailbox(Some(&max)), Ok(Some(max.clone())));
    }

    #[test]
    fn mailbox_empty_is_none() {
        assert_eq!(normalize_mailbox(None), Ok(None));
        assert_eq!(normalize_mailbox(Some("")), Ok(None));
        assert_eq!(normalize_mailbox(Some("   ")), Ok(None));
    }

    #[test]
    fn mailbox_rejects_bad_shapes() {
        let long = "a".repeat(65);
        for bad in [
            "ab",
            long.as_str(),
            "a_b_c",
            "a.b.c",
            "abc@x",
            "あいう",
            "a b c",
        ] {
            let got = normalize_mailbox(Some(bad));
            assert_eq!(got, Err("invalid_mailbox"), "{bad:?}");
        }
    }

    #[test]
    fn validate_defaults() {
        let v = validate(SettingsInput::default()).unwrap();
        assert_eq!(
            v,
            ValidSettings {
                mailbox: None,
                allowed_senders: vec![],
                dept_codes: vec![],
                lineworks_channel_id: None,
                notify_recipient_id: None,
                grace_days: DEFAULT_GRACE_DAYS,
                enabled: false,
            }
        );
    }

    #[test]
    fn validate_cleans_lists() {
        let v = validate(SettingsInput {
            allowed_senders: vec![" A@X.example ".into(), "a@x.example".into(), "".into()],
            dept_codes: vec!["10".into(), " 10".into(), "".into(), "20".into()],
            grace_days: Some(0),
            enabled: true,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(v.allowed_senders, vec!["a@x.example"]);
        assert_eq!(v.dept_codes, vec!["10", "20"]);
        assert_eq!(v.grace_days, 0);
        assert!(v.enabled);
    }

    #[test]
    fn validate_rejects() {
        for g in [-1, GRACE_DAYS_MAX + 1] {
            let r = validate(SettingsInput {
                grace_days: Some(g),
                ..Default::default()
            });
            assert_eq!(r, Err("invalid_grace_days"));
        }
        let r = validate(SettingsInput {
            mailbox: Some("x".into()),
            ..Default::default()
        });
        assert_eq!(r, Err("invalid_mailbox"));
        let r = validate(SettingsInput {
            lineworks_channel_id: Some(Uuid::from_u128(1)),
            notify_recipient_id: Some(Uuid::from_u128(2)),
            ..Default::default()
        });
        assert_eq!(r, Err("both_notify_targets"));
        let v = validate(SettingsInput {
            notify_recipient_id: Some(Uuid::from_u128(2)),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            (v.lineworks_channel_id, v.notify_recipient_id),
            (None, Some(Uuid::from_u128(2)))
        );
        assert!(validate(SettingsInput {
            grace_days: Some(GRACE_DAYS_MAX),
            ..Default::default()
        })
        .is_ok());
    }

    #[test]
    fn default_settings_shape() {
        let json = serde_json::to_value(Settings::default()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "mailbox": null, "allowed_senders": [], "dept_codes": [],
                "lineworks_channel_id": null, "notify_recipient_id": null, "grace_days": 7, "enabled": false, "updated_at": null
            })
        );
    }

    #[test]
    fn input_rejects_unknown_fields() {
        let r: Result<SettingsInput, _> = serde_json::from_str(r#"{"tenant_id":"x"}"#);
        assert!(r.is_err());
    }
}
