//! メール取り込み口 (`POST /internal/ingest`) の入力の検証 (純粋関数。接続も R2 も持たない)。
//!
//! 送り手は ippoan/email-receiver (Service Binding)。契約は固定:
//! body = `{mailbox, header_from, envelope_from, subject, message_id, received_at, filename, pdf_base64}`、
//! 応答 = 200 `{"pages": n}` / 202 `{"dropped": "<理由>"}` (意図して捨てた) / 400 / 401。
//!
//! 検証の順 (先に当たった方で返す): mailbox の形 (400) → base64 と大きさ (400) → `%PDF-` (202 `not_pdf`) →
//! (DB で) mailbox の tenant と送信元の照合 (202 `unknown_mailbox` / `sender_not_allowed`、[`sender_allowed`]) →
//! ページ数 (202 `unreadable_pdf` / `too_many_pages`、[`count_pages`])。
//! **PDF の parser (lopdf) には、許可された送信元の PDF だけを渡す** (深くネストした PDF は parser の stack を
//! 使い切るため。差出人の許可リストが、parser の手前の関所になる)。

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono::{DateTime, Datelike, Utc};
use serde::Deserialize;
use uuid::Uuid;

/// PDF の大きさの上限 (decode 後)
pub const MAX_PDF_BYTES: usize = 10 * 1024 * 1024;
/// 1 通の PDF のページ数の上限
pub const MAX_PAGES: usize = 50;
/// body (JSON) の大きさの上限。base64 は 4/3 倍なので、上限の PDF を包める大きさ
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// `mail_from` と `subject` に入れる長さ (文字数) の上限
const MAIL_FROM_MAX: usize = 320;
const SUBJECT_MAX: usize = 256;

/// email-receiver が送る body。使わない項目 (`envelope_from`・`message_id`・`filename`) も契約の一部として受ける
#[derive(Debug, Deserialize)]
pub struct IngestInput {
    pub mailbox: String,
    pub header_from: String,
    pub envelope_from: String,
    pub subject: Option<String>,
    pub message_id: Option<String>,
    pub received_at: DateTime<Utc>,
    pub filename: Option<String>,
    pub pdf_base64: String,
}

/// 検証を通った PDF
#[derive(Debug, PartialEq)]
pub struct Checked {
    /// 小文字にした mailbox
    pub mailbox: String,
    pub pdf: Vec<u8>,
}

/// 検証で落ちたときの返し方。語は応答の本文にそのまま出す (固定の語だけ)
#[derive(Debug, PartialEq)]
pub enum Reject {
    /// 400 `{"error": <語>}`
    Bad(&'static str),
    /// 202 `{"dropped": <語>}` (意図して捨てた)
    Drop(&'static str),
}

/// 共有 secret の比較。長さが違っても、先頭が違っても、全部の byte を見る (constant-time)。
/// どちらかが空なら一致させない (未設定の secret と空のヘッダーを通さない)
pub fn secret_matches(expected: &str, given: Option<&str>) -> bool {
    let Some(given) = given else { return false };
    if expected.is_empty() || given.is_empty() {
        return false;
    }
    let (a, b) = (expected.as_bytes(), given.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

/// mailbox を小文字にして `^[a-z0-9-]{3,64}$` かを見る (DB の CHECK と同じ形)。
/// ASCII だけを小文字にする (Unicode の小文字化は、ASCII でない字を ASCII に写すことがある)
pub fn normalize_mailbox(raw: &str) -> Option<String> {
    let m = raw.to_ascii_lowercase();
    let ok = (3..=64).contains(&m.len())
        && m.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-');
    ok.then_some(m)
}

/// base64 を decode する。大きさの上限は decode の前に (長さから) も見る
pub fn decode_pdf(b64: &str) -> Result<Vec<u8>, &'static str> {
    if b64.len() > MAX_PDF_BYTES.div_ceil(3) * 4 {
        return Err("pdf_too_large");
    }
    let bytes = STANDARD.decode(b64).map_err(|_| "invalid_base64")?;
    if bytes.len() > MAX_PDF_BYTES {
        return Err("pdf_too_large");
    }
    Ok(bytes)
}

/// PDF のページ数。読めない・0 ページは `unreadable_pdf`、[`MAX_PAGES`] 超は `too_many_pages`
pub fn count_pages(pdf: &[u8]) -> Result<usize, &'static str> {
    let doc = lopdf::Document::load_mem(pdf).map_err(|_| "unreadable_pdf")?;
    let n = doc.get_pages().len();
    if n == 0 {
        return Err("unreadable_pdf");
    }
    if n > MAX_PAGES {
        return Err("too_many_pages");
    }
    Ok(n)
}

/// DB を引く前の検証 (parser には掛けない)
pub fn check_input(input: &IngestInput) -> Result<Checked, Reject> {
    let mailbox = normalize_mailbox(&input.mailbox).ok_or(Reject::Bad("invalid_mailbox"))?;
    let pdf = decode_pdf(&input.pdf_base64).map_err(Reject::Bad)?;
    if !pdf.starts_with(b"%PDF-") {
        return Err(Reject::Drop("not_pdf"));
    }
    Ok(Checked { mailbox, pdf })
}

/// 送信元 (`header_from`) が許可リストに在るか (小文字で比べる)。空のリストは全拒否
pub fn sender_allowed(header_from: &str, allowed: &[String]) -> bool {
    let from = header_from.to_lowercase();
    !from.is_empty() && allowed.iter().any(|a| a.to_lowercase() == from)
}

/// R2 の key。**入力から作らない**: tenant (DB が返したもの)・受信時刻の年月・乱数の UUID だけで組む
pub fn object_key(tenant_id: Uuid, received_at: DateTime<Utc>, id: Uuid) -> String {
    format!(
        "{tenant_id}/{:04}/{:02}/{id}.pdf",
        received_at.year(),
        received_at.month()
    )
}

/// 文字数で切る (DB に長い文字列を入れない)
pub fn clip(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// `leave_pages.mail_from` に入れる値
pub fn clip_mail_from(s: &str) -> String {
    clip(s, MAIL_FROM_MAX)
}

/// `leave_pages.subject` に入れる値
pub fn clip_subject(s: &str) -> String {
    clip(s, SUBJECT_MAX)
}

/// テストと実 DB のテストが使う最小の PDF (1 ページ = 空の MediaBox だけ)。`pages = 0` は読めるが 0 ページの PDF
pub fn minimal_pdf(pages: usize) -> Vec<u8> {
    let kids: String = (0..pages).map(|i| format!("{} 0 R ", 3 + i)).collect();
    let mut objs = vec![
        "<</Type/Catalog/Pages 2 0 R>>".to_owned(),
        format!("<</Type/Pages/Kids[{kids}]/Count {pages}>>"),
    ];
    objs.extend((0..pages).map(|_| "<</Type/Page/Parent 2 0 R/MediaBox[0 0 10 10]>>".to_owned()));
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    let size = objs.len() + 1;
    out.extend(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
    for off in &offsets {
        out.extend(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend(
        format!("trailer\n<</Size {size} /Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(mailbox: &str, pdf: &[u8]) -> IngestInput {
        IngestInput {
            mailbox: mailbox.to_owned(),
            header_from: "a@example.com".to_owned(),
            envelope_from: "a@example.com".to_owned(),
            subject: None,
            message_id: None,
            received_at: "2026-10-10T01:02:03Z".parse().unwrap(),
            filename: None,
            pdf_base64: STANDARD.encode(pdf),
        }
    }

    #[test]
    fn secret_must_match_exactly() {
        assert!(secret_matches("s3cret", Some("s3cret")));
        assert!(!secret_matches("s3cret", Some("s3cre")));
        assert!(!secret_matches("s3cret", Some("s3cretx")));
        assert!(!secret_matches("s3cret", Some("S3cret")));
        assert!(!secret_matches("s3cret", None));
        assert!(!secret_matches("s3cret", Some("")));
        assert!(!secret_matches("", Some("")));
    }

    #[test]
    fn mailbox_is_lowercased_and_checked() {
        assert_eq!(normalize_mailbox("Leave-01").as_deref(), Some("leave-01"));
        assert_eq!(normalize_mailbox("abc").as_deref(), Some("abc"));
        assert_eq!(normalize_mailbox(&"a".repeat(64)), Some("a".repeat(64)));
        assert_eq!(normalize_mailbox("ab"), None);
        assert_eq!(normalize_mailbox(&"a".repeat(65)), None);
        assert_eq!(normalize_mailbox("a_b"), None);
        assert_eq!(normalize_mailbox("a b"), None);
        assert_eq!(normalize_mailbox("a@b.c"), None);
        // ケルビン記号 (U+212A) は Unicode では k に小文字化されるが、通さない
        assert_eq!(normalize_mailbox("\u{212A}ab"), None);
        assert_eq!(normalize_mailbox("休暇申請"), None);
    }

    #[test]
    fn decode_checks_base64_and_size() {
        assert_eq!(decode_pdf(&STANDARD.encode(b"%PDF-x")).unwrap(), b"%PDF-x");
        assert_eq!(decode_pdf("***"), Err("invalid_base64"));
        let max = vec![0u8; MAX_PDF_BYTES];
        assert_eq!(
            decode_pdf(&STANDARD.encode(&max)).unwrap().len(),
            MAX_PDF_BYTES
        );
        let over = vec![0u8; MAX_PDF_BYTES + 1];
        assert_eq!(decode_pdf(&STANDARD.encode(&over)), Err("pdf_too_large"));
        // decode 前の長さの検査 (中身が base64 でなくても大きさで落ちる)
        assert_eq!(decode_pdf(&"*".repeat(14_000_000)), Err("pdf_too_large"));
    }

    #[test]
    fn pages_are_counted() {
        assert_eq!(count_pages(&minimal_pdf(1)), Ok(1));
        assert_eq!(count_pages(&minimal_pdf(3)), Ok(3));
        assert_eq!(count_pages(&minimal_pdf(MAX_PAGES)), Ok(MAX_PAGES));
        assert_eq!(
            count_pages(&minimal_pdf(MAX_PAGES + 1)),
            Err("too_many_pages")
        );
        assert_eq!(count_pages(&minimal_pdf(0)), Err("unreadable_pdf"));
        assert_eq!(count_pages(b"%PDF-1.4\nbroken"), Err("unreadable_pdf"));
        let cut = minimal_pdf(2);
        assert_eq!(count_pages(&cut[..cut.len() / 2]), Err("unreadable_pdf"));
    }

    #[test]
    fn input_is_checked_in_order() {
        let ok = check_input(&input("Leave-01", &minimal_pdf(2))).unwrap();
        assert_eq!(ok.mailbox, "leave-01");
        assert!(ok.pdf.starts_with(b"%PDF-"));
        let bad_mailbox = check_input(&input("a", &minimal_pdf(1)));
        assert_eq!(bad_mailbox, Err(Reject::Bad("invalid_mailbox")));
        let mut b = input("abc", b"");
        b.pdf_base64 = "***".to_owned();
        assert_eq!(check_input(&b), Err(Reject::Bad("invalid_base64")));
        let not_pdf = check_input(&input("abc", b"GIF89a"));
        assert_eq!(not_pdf, Err(Reject::Drop("not_pdf")));
        // ページ数は parser に掛ける (check_input では数えない)
        assert!(check_input(&input("abc", b"%PDF-1.4\nbroken")).is_ok());
        // mailbox の誤りは PDF の中身より先に落ちる
        assert_eq!(
            check_input(&input("a", b"GIF89a")),
            Err(Reject::Bad("invalid_mailbox"))
        );
    }

    #[test]
    fn sender_is_compared_in_lowercase() {
        let allowed = vec!["Boss@Example.com".to_owned(), "b@example.com".to_owned()];
        assert!(sender_allowed("boss@example.com", &allowed));
        assert!(sender_allowed("B@EXAMPLE.COM", &allowed));
        assert!(!sender_allowed("c@example.com", &allowed));
        assert!(!sender_allowed("boss@example.com", &[]));
        assert!(!sender_allowed("", &["".to_owned()]));
    }

    #[test]
    fn key_is_built_from_server_values() {
        let t = Uuid::nil();
        let id = Uuid::from_u128(1);
        let at: DateTime<Utc> = "2026-03-09T23:59:59Z".parse().unwrap();
        let key = object_key(t, at, id);
        assert_eq!(key, format!("{t}/2026/03/{id}.pdf"));
    }

    #[test]
    fn long_strings_are_clipped_by_chars() {
        assert_eq!(clip("あいうえお", 3), "あいう");
        assert_eq!(clip_mail_from(&"a".repeat(400)).len(), 320);
        assert_eq!(clip_subject(&"あ".repeat(300)).chars().count(), 256);
        assert_eq!(clip_subject("休暇"), "休暇");
    }
}
