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
    pub dept_codes: Vec<String>,
    pub lineworks_channel_id: Option<Uuid>,
    /// 個人の宛先 (`notify_recipients.id`、LINE WORKS の行)。グループ (`lineworks_channel_id`) とどちらか 1 つ
    pub notify_recipient_id: Option<Uuid>,
    /// 個人宛てに使う LINE WORKS の bot (`bot_configs.id`)。個人を選んだときだけ。無ければ送信側の既定の bot
    pub notify_bot_config_id: Option<Uuid>,
    pub grace_days: i32,
    pub enabled: bool,
    pub updated_at: Option<DateTime<Utc>>,
}

impl TxOutput for Settings {}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dept_codes: Vec::new(),
            lineworks_channel_id: None,
            notify_recipient_id: None,
            notify_bot_config_id: None,
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
    pub dept_codes: Vec<String>,
    #[serde(default)]
    pub lineworks_channel_id: Option<Uuid>,
    #[serde(default)]
    pub notify_recipient_id: Option<Uuid>,
    #[serde(default)]
    pub notify_bot_config_id: Option<Uuid>,
    #[serde(default)]
    pub grace_days: Option<i32>,
    #[serde(default)]
    pub enabled: bool,
}

/// 検証済みの設定 (DB へ書く値)
#[derive(Debug, Clone, PartialEq)]
pub struct ValidSettings {
    pub dept_codes: Vec<String>,
    pub lineworks_channel_id: Option<Uuid>,
    pub notify_recipient_id: Option<Uuid>,
    pub notify_bot_config_id: Option<Uuid>,
    pub grace_days: i32,
    pub enabled: bool,
}

/// 前後の空白を落とし、空を捨て、重複を最初の 1 つに寄せる (順は保つ)
fn clean_list(items: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        let item = item.trim().to_owned();
        if !item.is_empty() && !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

/// 入力を検証して DB へ書く値にする。エラーは 400 の本文の語
pub fn validate(input: SettingsInput) -> Result<ValidSettings, &'static str> {
    let grace_days = input.grace_days.unwrap_or(DEFAULT_GRACE_DAYS);
    if !(0..=GRACE_DAYS_MAX).contains(&grace_days) {
        return Err("invalid_grace_days");
    }
    // 宛先はグループか個人のどちらか 1 つ (表の CHECK と同じ)
    if input.lineworks_channel_id.is_some() && input.notify_recipient_id.is_some() {
        return Err("both_notify_targets");
    }
    // bot は個人宛てのときだけ (表の CHECK と同じ)
    if input.notify_bot_config_id.is_some() && input.notify_recipient_id.is_none() {
        return Err("notify_bot_without_recipient");
    }
    Ok(ValidSettings {
        dept_codes: clean_list(input.dept_codes),
        lineworks_channel_id: input.lineworks_channel_id,
        notify_recipient_id: input.notify_recipient_id,
        notify_bot_config_id: input.notify_bot_config_id,
        grace_days,
        enabled: input.enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_defaults() {
        let v = validate(SettingsInput::default()).unwrap();
        assert_eq!(
            v,
            ValidSettings {
                dept_codes: vec![],
                lineworks_channel_id: None,
                notify_recipient_id: None,
                notify_bot_config_id: None,
                grace_days: DEFAULT_GRACE_DAYS,
                enabled: false,
            }
        );
    }

    #[test]
    fn validate_cleans_lists() {
        let v = validate(SettingsInput {
            dept_codes: vec!["10".into(), " 10".into(), "".into(), "20".into()],
            grace_days: Some(0),
            enabled: true,
            ..Default::default()
        })
        .unwrap();
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
        // bot だけ (個人なし) は弾く。個人と一緒なら通る
        let r = validate(SettingsInput {
            notify_bot_config_id: Some(Uuid::from_u128(3)),
            ..Default::default()
        });
        assert_eq!(r, Err("notify_bot_without_recipient"));
        let r = validate(SettingsInput {
            lineworks_channel_id: Some(Uuid::from_u128(1)),
            notify_bot_config_id: Some(Uuid::from_u128(3)),
            ..Default::default()
        });
        assert_eq!(r, Err("notify_bot_without_recipient"));
        let v = validate(SettingsInput {
            notify_recipient_id: Some(Uuid::from_u128(2)),
            notify_bot_config_id: Some(Uuid::from_u128(3)),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(v.notify_bot_config_id, Some(Uuid::from_u128(3)));
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
                "dept_codes": [],
                "lineworks_channel_id": null, "notify_recipient_id": null, "notify_bot_config_id": null, "grace_days": 7, "enabled": false, "updated_at": null
            })
        );
    }

    #[test]
    fn input_rejects_unknown_fields() {
        let r: Result<SettingsInput, _> = serde_json::from_str(r#"{"tenant_id":"x"}"#);
        assert!(r.is_err());
    }
}
