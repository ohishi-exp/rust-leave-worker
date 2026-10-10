//! 一番星の休暇入力 (品名N) を休暇の区分に正規化する (純粋関数)。
//!
//! 正本は社内の勤怠画面の変換表 (`$henkan_sentence_ab`、TimeCardController)。NFKC で正規化して前後の空白を
//! 落としてから、表の語と完全一致で比べる (全角の括弧・数字の揺れは NFKC で半角にそろう)。
//!
//! | 品名N | 区分 |
//! |---|---|
//! | 有休 / 有休(積置) / 有休(積置き) | yukyu (終日) |
//! | 午前休 / 午後休 | yukyu (半日 am / pm) |
//! | 特別休暇 | tokukyu |
//! | 欠勤 | kekkin |
//! | 労災 | rousai (変換表に無いが一番星の品名N に在る) |
//! | 休み / 積置休み / 病休 / 泊休み / 積置泊休 / 遅刻 / 早退 | 対象外 |
//! | それ以外 | 未知の語 (対象外。呼び出し側が件数を数える) |

use unicode_normalization::UnicodeNormalization;

/// 区分 1 つ (値は表の CHECK と同じ語)。`half` は午前休 / 午後休のときだけ
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DayKind {
    pub kind: &'static str,
    pub half: Option<&'static str>,
}

/// 正規化の結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Normalized {
    /// 突き合わせの対象の区分
    Leave(DayKind),
    /// 表に在るが対象外の語 (公休・遅刻など)
    Ignored,
    /// 表に無い語 (正規化後の語を持つ。ログに出してよい — 氏名ではない)
    Unknown(String),
}

const fn leave(kind: &'static str, half: Option<&'static str>) -> Normalized {
    Normalized::Leave(DayKind { kind, half })
}

/// 品名N 1 つを正規化する
pub fn normalize(item: &str) -> Normalized {
    let word: String = item.nfkc().collect::<String>().trim().to_owned();
    match word.as_str() {
        "有休" | "有休(積置)" | "有休(積置き)" => leave("yukyu", None),
        "午前休" => leave("yukyu", Some("am")),
        "午後休" => leave("yukyu", Some("pm")),
        "特別休暇" => leave("tokukyu", None),
        "欠勤" => leave("kekkin", None),
        "労災" => leave("rousai", None),
        "休み" | "積置休み" | "病休" | "泊休み" | "積置泊休" | "遅刻" | "早退" => {
            Normalized::Ignored
        }
        _ => Normalized::Unknown(word),
    }
}

/// 区分の日本語 (通知の文面)
pub fn label(kind: &str) -> &'static str {
    match kind {
        "yukyu" => "有休",
        "rousai" => "労災",
        "tokukyu" => "特別休暇",
        _ => "欠勤",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(item: &str) -> Option<(&'static str, Option<&'static str>)> {
        match normalize(item) {
            Normalized::Leave(k) => Some((k.kind, k.half)),
            _ => None,
        }
    }

    #[test]
    fn yukyu_and_its_variants() {
        for item in [
            "有休",
            " 有休 ",
            "有休(積置)",
            "有休（積置）",
            "有休(積置き)",
            "有休（積置き）",
            "有休（積置)",
            "　有休　",
        ] {
            assert_eq!(kind(item), Some(("yukyu", None)), "{item:?}");
        }
    }

    #[test]
    fn half_days() {
        assert_eq!(kind("休み"), None);
        assert_eq!(kind("午前休"), Some(("yukyu", Some("am"))));
        assert_eq!(kind("午後休 "), Some(("yukyu", Some("pm"))));
    }

    #[test]
    fn other_kinds() {
        assert_eq!(kind("特別休暇"), Some(("tokukyu", None)));
        assert_eq!(kind("欠勤"), Some(("kekkin", None)));
        assert_eq!(kind("労災"), Some(("rousai", None)));
    }

    #[test]
    fn ignored_words() {
        for item in [
            "休み",
            "積置休み",
            "病休",
            "泊休み",
            "積置泊休",
            "遅刻",
            "早退",
        ] {
            assert_eq!(normalize(item), Normalized::Ignored, "{item:?}");
        }
    }

    #[test]
    fn unknown_words_are_normalized() {
        assert_eq!(normalize(" 有給 "), Normalized::Unknown("有給".into()));
        assert_eq!(normalize("ＡＢＣ"), Normalized::Unknown("ABC".into()));
        assert_eq!(normalize(""), Normalized::Unknown(String::new()));
    }

    #[test]
    fn labels() {
        let got: Vec<_> = ["yukyu", "rousai", "tokukyu", "kekkin"]
            .iter()
            .map(|k| label(k))
            .collect();
        assert_eq!(got, vec!["有休", "労災", "特別休暇", "欠勤"]);
    }
}
