//! 取り込む PDF の検査と R2 の key (純粋関数。接続も R2 も持たない)。スキャンの inbox ([`crate::inbox`]) と
//! 取り込みの本体 ([`crate::internal::ingest_pdf`]) が使う。
//!
//! 検査の順: 大きさと `%PDF-` ([`check_pdf_bytes`]) → ページ数 ([`count_pages`]、`unreadable_pdf` / `too_many_pages`)。

use chrono::{DateTime, Datelike, Utc};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// PDF の大きさの上限
pub const MAX_PDF_BYTES: usize = 10 * 1024 * 1024;
/// 1 通の PDF のページ数の上限
pub const MAX_PAGES: usize = 50;

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

/// 中身の sha256 (hex 小文字 64 桁)
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 何度呼んでも同じになる R2 の key (Queue の再配送で二重に取り込まないため)。`seed` (inbox の object key) は
/// そのまま使わず、sha256 の先頭 32 桁だけを入れる (入力の文字を key に持ち込まない)
pub fn seeded_object_key(tenant_id: Uuid, received_at: DateTime<Utc>, seed: &str) -> String {
    format!(
        "{tenant_id}/{:04}/{:02}/{}.pdf",
        received_at.year(),
        received_at.month(),
        &sha256_hex(seed.as_bytes())[..32]
    )
}

/// PDF の検査: 大きさ (`pdf_too_large`) → `%PDF-` (`not_pdf`)
pub fn check_pdf_bytes(pdf: &[u8]) -> Result<(), &'static str> {
    if pdf.len() > MAX_PDF_BYTES {
        return Err("pdf_too_large");
    }
    if !pdf.starts_with(b"%PDF-") {
        return Err("not_pdf");
    }
    Ok(())
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
    fn seeded_key_is_stable_and_hides_the_seed() {
        let t = Uuid::nil();
        let at: DateTime<Utc> = "2026-10-01T00:00:00Z".parse().unwrap();
        let a = seeded_object_key(t, at, "x-申請.pdf");
        assert_eq!(a, seeded_object_key(t, at, "x-申請.pdf"));
        assert_ne!(a, seeded_object_key(t, at, "y-申請.pdf"));
        let name = a.strip_prefix(&format!("{t}/2026/10/")).unwrap();
        assert_eq!(name.len(), 32 + ".pdf".len());
        assert!(!a.contains("申請"));
    }

    #[test]
    fn sha256_is_lowercase_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn raw_pdf_is_checked_for_size_then_magic() {
        assert_eq!(check_pdf_bytes(&minimal_pdf(1)), Ok(()));
        assert_eq!(check_pdf_bytes(b"GIF89a"), Err("not_pdf"));
        let mut big = b"%PDF-".to_vec();
        big.resize(MAX_PDF_BYTES + 1, 0);
        assert_eq!(check_pdf_bytes(&big), Err("pdf_too_large"));
        big.truncate(MAX_PDF_BYTES);
        assert_eq!(check_pdf_bytes(&big), Ok(()));
    }
}
