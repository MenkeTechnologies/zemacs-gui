//! Encoding ops — detect and transcode a file's character encoding, the on-disk complement to
//! `text_tools::convert_file` (which handles line endings / tabs but assumes UTF-8 text). This is the
//! "file is mojibake / has a BOM / came from Windows-1252" workflow:
//!
//! * **Detect** — sniff the byte-order mark, then UTF-8 validity, then a UTF-16 zero-byte heuristic,
//!   falling back to Latin-1. Also reports the BOM and dominant line ending, without touching the file.
//! * **Convert** — decode from the source encoding and re-encode as the target (`utf-8`, `utf-16le`,
//!   `utf-16be`, `latin1`). UTF-8 output is written without a BOM; UTF-16 output gets one.
//!
//! Same host contract as the rest of the workbench: only `convert_encoding` with `apply` mutates the
//! file; the front-end re-opens it afterward. The pure sniff/decode/encode helpers are unit tested.

use serde::Serialize;
use std::fs;
use std::path::PathBuf;

/// Files above this size aren't sniffed/transcoded (mirrors the search-tool cap).
const MAX_ENCODING_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Serialize)]
pub struct EncodingInfo {
    /// Best-guess encoding label: "UTF-8", "UTF-16LE", "UTF-16BE", or "Latin-1".
    pub encoding: String,
    /// True when a byte-order mark was found.
    pub bom: bool,
    /// "CRLF", "LF", "Mixed", or "none".
    pub line_ending: String,
    /// True when the bytes are valid UTF-8 (after any BOM).
    pub valid_utf8: bool,
    pub bytes: u64,
}

/// Sniff the encoding of a byte slice: BOM first, then UTF-8 validity, then a UTF-16 heuristic (a run
/// of NUL bytes concentrated on even or odd offsets), else Latin-1. Returns (label, had_bom). Pure.
pub(crate) fn sniff_encoding(bytes: &[u8]) -> (&'static str, bool) {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return ("UTF-8", true);
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return ("UTF-16LE", true);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return ("UTF-16BE", true);
    }
    // Plain text never contains embedded NUL bytes; a NUL is the tell-tale of UTF-16 (ASCII-range
    // chars leave a zero high byte). Without any NUL, decide UTF-8 vs Latin-1 by UTF-8 validity —
    // pure ASCII with interspersed NULs would otherwise mis-classify as UTF-8.
    let sample = &bytes[..bytes.len().min(4096)];
    if !sample.contains(&0) {
        return if std::str::from_utf8(bytes).is_ok() {
            ("UTF-8", false)
        } else {
            ("Latin-1", false)
        };
    }
    // NULs present → UTF-16. The zero (high) byte sits at odd offsets for LE, even offsets for BE.
    let zeros_even = sample.iter().step_by(2).filter(|&&b| b == 0).count();
    let zeros_odd = sample
        .iter()
        .skip(1)
        .step_by(2)
        .filter(|&&b| b == 0)
        .count();
    if zeros_odd >= zeros_even {
        ("UTF-16LE", false)
    } else {
        ("UTF-16BE", false)
    }
}

fn line_ending_of(s: &str) -> &'static str {
    let crlf = s.contains("\r\n");
    // A bare \n that isn't part of a \r\n indicates LF usage.
    let lf = s.replace("\r\n", "").contains('\n');
    match (crlf, lf) {
        (true, true) => "Mixed",
        (true, false) => "CRLF",
        (false, true) => "LF",
        (false, false) => "none",
    }
}

/// Detect a file's encoding, BOM and line ending without modifying it.
#[tauri::command]
pub fn detect_encoding(path: String) -> Result<EncodingInfo, String> {
    let p = PathBuf::from(&path);
    let meta = fs::metadata(&p).map_err(|e| e.to_string())?;
    if meta.len() > MAX_ENCODING_BYTES {
        return Err("file too large".into());
    }
    let bytes = fs::read(&p).map_err(|e| e.to_string())?;
    let (encoding, bom) = sniff_encoding(&bytes);
    let decoded = decode(&bytes, encoding, bom);
    Ok(EncodingInfo {
        encoding: encoding.to_string(),
        bom,
        line_ending: line_ending_of(&decoded).to_string(),
        valid_utf8: std::str::from_utf8(strip_bom(&bytes, encoding, bom)).is_ok(),
        bytes: meta.len(),
    })
}

/// Byte slice with any leading BOM for `encoding` removed.
fn strip_bom<'a>(bytes: &'a [u8], encoding: &str, bom: bool) -> &'a [u8] {
    if !bom {
        return bytes;
    }
    match encoding {
        "UTF-8" => bytes.get(3..).unwrap_or(&[]),
        "UTF-16LE" | "UTF-16BE" => bytes.get(2..).unwrap_or(&[]),
        _ => bytes,
    }
}

/// Decode bytes in a known `encoding` (respecting a `bom`) into a Rust `String`. Lossy for invalid
/// sequences (replacement char), never fails. Pure.
pub(crate) fn decode(bytes: &[u8], encoding: &str, bom: bool) -> String {
    let body = strip_bom(bytes, encoding, bom);
    match encoding {
        "UTF-16LE" => {
            let units: Vec<u16> = body
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        "UTF-16BE" => {
            let units: Vec<u16> = body
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        "Latin-1" => body.iter().map(|&b| b as char).collect(),
        // UTF-8 (and any unexpected label): lossy UTF-8.
        _ => String::from_utf8_lossy(body).into_owned(),
    }
}

/// Encode a `String` into `target` bytes. UTF-8 has no BOM; UTF-16 targets are prefixed with a BOM.
/// Latin-1 maps code points > U+00FF to `?`. Pure.
pub(crate) fn encode(text: &str, target: &str) -> Result<Vec<u8>, String> {
    match target {
        "utf-8" => Ok(text.as_bytes().to_vec()),
        "utf-16le" => {
            let mut out = vec![0xFF, 0xFE];
            for u in text.encode_utf16() {
                out.extend_from_slice(&u.to_le_bytes());
            }
            Ok(out)
        }
        "utf-16be" => {
            let mut out = vec![0xFE, 0xFF];
            for u in text.encode_utf16() {
                out.extend_from_slice(&u.to_be_bytes());
            }
            Ok(out)
        }
        "latin1" => Ok(text
            .chars()
            .map(|c| if (c as u32) <= 0xFF { c as u8 } else { b'?' })
            .collect()),
        other => Err(format!("unknown target encoding: {other}")),
    }
}

#[derive(Serialize)]
pub struct ConvertEncResult {
    pub from: String,
    pub to: String,
    pub bytes_before: usize,
    pub bytes_after: usize,
    pub differs: bool,
    pub applied: bool,
}

/// Transcode a file to `to` (`utf-8` / `utf-16le` / `utf-16be` / `latin1`). The source encoding is
/// auto-detected. Preview by default; `apply` rewrites the file.
#[tauri::command]
pub fn convert_encoding(
    path: String,
    to: String,
    apply: Option<bool>,
) -> Result<ConvertEncResult, String> {
    let target = to.trim().to_lowercase();
    let p = PathBuf::from(&path);
    let meta = fs::metadata(&p).map_err(|e| e.to_string())?;
    if meta.len() > MAX_ENCODING_BYTES {
        return Err("file too large".into());
    }
    let bytes = fs::read(&p).map_err(|e| e.to_string())?;
    let (from, bom) = sniff_encoding(&bytes);
    let text = decode(&bytes, from, bom);
    let out = encode(&text, &target)?;
    let differs = out != bytes;
    let applied = apply.unwrap_or(false) && differs;
    if applied {
        fs::write(&p, &out).map_err(|e| e.to_string())?;
    }
    Ok(ConvertEncResult {
        from: from.to_string(),
        to: target,
        bytes_before: bytes.len(),
        bytes_after: out.len(),
        differs,
        applied,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_bom_variants() {
        assert_eq!(sniff_encoding(&[0xEF, 0xBB, 0xBF, b'h']), ("UTF-8", true));
        assert_eq!(sniff_encoding(&[0xFF, 0xFE, b'h', 0]), ("UTF-16LE", true));
        assert_eq!(sniff_encoding(&[0xFE, 0xFF, 0, b'h']), ("UTF-16BE", true));
    }

    #[test]
    fn sniff_plain_utf8_and_latin1() {
        assert_eq!(sniff_encoding("hello".as_bytes()), ("UTF-8", false));
        // 0xE9 alone is invalid UTF-8, few NULs → Latin-1.
        assert_eq!(
            sniff_encoding(&[b'c', b'a', b'f', 0xE9]),
            ("Latin-1", false)
        );
    }

    #[test]
    fn sniff_utf16_without_bom() {
        // "hi" in UTF-16LE without BOM: 68 00 69 00 → zeros on odd offsets → LE.
        let le = [b'h', 0, b'i', 0];
        assert_eq!(sniff_encoding(&le), ("UTF-16LE", false));
        // UTF-16BE: 00 68 00 69 → zeros on even offsets → BE.
        let be = [0, b'h', 0, b'i'];
        assert_eq!(sniff_encoding(&be), ("UTF-16BE", false));
    }

    #[test]
    fn decode_encode_roundtrips() {
        // Latin-1 é (0xE9) decodes to U+00E9, re-encodes to the same byte.
        assert_eq!(decode(&[0xE9], "Latin-1", false), "é");
        assert_eq!(encode("é", "latin1").unwrap(), vec![0xE9]);

        // UTF-16LE with BOM decodes to "hi".
        let le = [0xFF, 0xFE, b'h', 0, b'i', 0];
        assert_eq!(decode(&le, "UTF-16LE", true), "hi");

        // Encode to UTF-16LE prefixes a BOM and lays out little-endian units.
        assert_eq!(encode("hi", "utf-16le").unwrap(), le);

        // UTF-8 target is BOM-free.
        assert_eq!(encode("hi", "utf-8").unwrap(), b"hi".to_vec());
    }

    #[test]
    fn convert_latin1_file_to_utf8() {
        let dir = tempdir();
        let f = dir.join("l.txt");
        // "café\n" in Latin-1: 0xE9 for é.
        std::fs::write(&f, [b'c', b'a', b'f', 0xE9, b'\n']).unwrap();

        let info = detect_encoding(f.to_string_lossy().into()).unwrap();
        assert_eq!(info.encoding, "Latin-1");

        let r = convert_encoding(f.to_string_lossy().into(), "utf-8".into(), Some(true)).unwrap();
        assert_eq!(r.from, "Latin-1");
        assert!(r.applied && r.differs);
        // Now valid UTF-8 with the two-byte é.
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "café\n");

        let info2 = detect_encoding(f.to_string_lossy().into()).unwrap();
        assert_eq!(info2.encoding, "UTF-8");
        assert!(info2.valid_utf8);
        assert_eq!(info2.line_ending, "LF");
        cleanup(&dir);
    }

    // ── tiny tempdir helpers (no external dev-dep) ──
    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "zmax-gui-enc-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }
    fn cleanup(dir: &std::path::Path) {
        let _ = std::fs::remove_dir_all(dir);
    }
}
