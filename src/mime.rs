//! Minimal RFC 5322 / MIME parser for recording SMTP messages in a stable,
//! mailer-independent form (envelope + decoded headers + decoded parts).

use std::collections::BTreeMap;

use percent_encoding::percent_decode_str;

const MAX_DEPTH: usize = 16;

/// Parse a raw SMTP DATA payload into a stable, mailer-independent view:
/// canonical decoded headers (`From`, `To`, `Cc`, `Subject`) followed by each
/// leaf MIME part (`part <type>` + decoded body, or `attachment <name> (<type>, <size> bytes, sha256:<hex>)`).
pub fn format_email(raw: &str) -> String {
    let normalized = raw.replace("\r\n", "\n");
    let (headers, body) = split_headers_body(&normalized);
    let mut out = String::new();

    for (key, label) in [
        ("from", "From"),
        ("to", "To"),
        ("cc", "Cc"),
        ("subject", "Subject"),
    ] {
        let values: Vec<String> = headers
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| decode_header_value(v))
            .filter(|v| !v.is_empty())
            .collect();
        if !values.is_empty() {
            out.push_str(label);
            out.push_str(": ");
            out.push_str(&values.join(", "));
            out.push('\n');
        }
    }

    let mut parts = Vec::new();
    collect_parts(&headers, body, 0, &mut parts);
    for part in parts {
        match part {
            Part::Text { media_type, body } => {
                out.push_str("part ");
                out.push_str(&media_type);
                out.push('\n');
                let trimmed = body.trim_end_matches('\n');
                if !trimmed.is_empty() {
                    out.push_str(trimmed);
                    out.push('\n');
                }
            }
            Part::Attachment {
                filename,
                media_type,
                bytes,
            } => {
                out.push_str(&format!(
                    "attachment {filename} ({media_type}, {} bytes, sha256:{})\n",
                    bytes.len(),
                    sha256_hex(&bytes),
                ));
            }
        }
    }
    out
}

enum Part {
    Text {
        media_type: String,
        body: String,
    },
    Attachment {
        filename: String,
        media_type: String,
        bytes: Vec<u8>,
    },
}

fn collect_parts(headers: &[(String, String)], body: &str, depth: usize, out: &mut Vec<Part>) {
    let ct_raw = find_header(headers, "content-type").unwrap_or("text/plain");
    let (media_type, ct_params) = parse_header_params(ct_raw);
    let media_type = if media_type.is_empty() {
        "text/plain".to_string()
    } else {
        media_type
    };

    if depth < MAX_DEPTH
        && media_type.starts_with("multipart/")
        && let Some(boundary) = ct_params.get("boundary").filter(|b| !b.is_empty())
    {
        for child in split_multipart(body, boundary) {
            let (child_headers, child_body) = split_headers_body(child);
            collect_parts(&child_headers, child_body, depth + 1, out);
        }
        return;
    }

    let cte = find_header(headers, "content-transfer-encoding")
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let decoded = match cte.as_str() {
        "base64" => decode_base64(body.as_bytes()),
        "quoted-printable" => decode_quoted_printable(body.as_bytes()),
        _ => body.as_bytes().to_vec(),
    };

    let (disp_type, disp_params) = find_header(headers, "content-disposition")
        .map(parse_header_params)
        .unwrap_or_default();
    let filename = disp_params
        .get("filename")
        .or_else(|| ct_params.get("name"))
        .cloned();
    let is_attachment =
        disp_type == "attachment" || filename.is_some() || !media_type.starts_with("text/");

    if is_attachment {
        out.push(Part::Attachment {
            filename: filename.unwrap_or_else(|| "unnamed".to_string()),
            media_type,
            bytes: decoded,
        });
    } else {
        let charset = ct_params
            .get("charset")
            .map(String::as_str)
            .unwrap_or("utf-8");
        let text = decode_charset(&decoded, charset).replace("\r\n", "\n");
        out.push(Part::Text {
            media_type,
            body: text,
        });
    }
}

/// Split an RFC 5322 message/part (already LF-normalized) into unfolded
/// `(lowercase_name, value)` headers and the body slice.
fn split_headers_body(input: &str) -> (Vec<(String, String)>, &str) {
    let (header_block, body) = if let Some(rest) = input.strip_prefix('\n') {
        ("", rest)
    } else if let Some(idx) = input.find("\n\n") {
        (&input[..idx], &input[idx + 2..])
    } else if input.lines().next().is_some_and(|l| l.contains(':')) {
        (input, "")
    } else {
        ("", input)
    };

    let mut headers: Vec<(String, String)> = Vec::new();
    for line in header_block.split('\n') {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, value)) = headers.last_mut() {
                if !value.is_empty() {
                    value.push(' ');
                }
                value.push_str(line.trim());
            }
        } else if let Some((name, val)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), val.trim().to_string()));
        }
    }
    (headers, body)
}

fn find_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// Split a multipart body by `--boundary` / `--boundary--` lines (RFC 2046 §5.1.1).
fn split_multipart<'a>(body: &'a str, boundary: &str) -> Vec<&'a str> {
    let delim = format!("--{boundary}");
    let close = format!("--{boundary}--");
    let mut parts = Vec::new();
    let mut part_start: Option<usize> = None;
    let mut pos = 0;

    while pos <= body.len() {
        let rest = &body[pos..];
        let (line, next_pos) = match rest.find('\n') {
            Some(i) => (&rest[..i], pos + i + 1),
            None => (rest, body.len() + 1),
        };
        let trimmed = line.trim_end_matches([' ', '\t']);
        if trimmed == delim || trimmed == close {
            if let Some(start) = part_start {
                // The LF immediately preceding the delimiter belongs to the delimiter.
                let end = pos.saturating_sub(1).max(start);
                parts.push(&body[start..end]);
            }
            if trimmed == close {
                break;
            }
            part_start = Some(next_pos.min(body.len()));
        }
        if next_pos > body.len() {
            break;
        }
        pos = next_pos;
    }
    parts
}

/// Parse a structured MIME header like `text/plain; charset="utf-8"; name*=UTF-8''a%20b.txt`.
fn parse_header_params(raw: &str) -> (String, BTreeMap<String, String>) {
    let segments = split_semicolons(raw);
    let main = segments
        .first()
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default();

    let mut plain: BTreeMap<String, String> = BTreeMap::new();
    let mut ext_single: BTreeMap<String, String> = BTreeMap::new();
    let mut cont: BTreeMap<String, BTreeMap<usize, (bool, String)>> = BTreeMap::new();

    for seg in segments.iter().skip(1) {
        let Some((k, v)) = seg.split_once('=') else {
            continue;
        };
        let key = k.trim().to_ascii_lowercase();
        let val = unquote_param(v.trim());
        if let Some(base) = key.strip_suffix('*') {
            if let Some((name, idx)) = split_continuation(base) {
                cont.entry(name).or_default().insert(idx, (true, val));
            } else {
                ext_single.insert(base.to_string(), decode_rfc2231(&val));
            }
        } else if let Some((name, idx)) = split_continuation(&key) {
            cont.entry(name).or_default().insert(idx, (false, val));
        } else {
            plain.insert(key, decode_header_value(&val));
        }
    }

    let mut params = plain;
    for (name, pieces) in cont {
        let mut combined = String::new();
        let mut any_ext = false;
        for (_, (is_ext, piece)) in pieces {
            any_ext |= is_ext;
            combined.push_str(&piece);
        }
        let decoded = if any_ext {
            decode_rfc2231(&combined)
        } else {
            decode_header_value(&combined)
        };
        params.insert(name, decoded);
    }
    for (k, v) in ext_single {
        params.insert(k, v);
    }
    (main, params)
}

fn split_continuation(key: &str) -> Option<(String, usize)> {
    let (name, num) = key.rsplit_once('*')?;
    if name.is_empty() || num.is_empty() {
        return None;
    }
    let idx = num.parse::<usize>().ok()?;
    Some((name.to_string(), idx))
}

fn split_semicolons(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut escape = false;
    for (i, ch) in s.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' if in_quotes => escape = true,
            '"' => in_quotes = !in_quotes,
            ';' if !in_quotes => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

fn unquote_param(s: &str) -> String {
    let Some(inner) = s.strip_prefix('"').and_then(|t| t.strip_suffix('"')) else {
        return s.to_string();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(next) = chars.next()
        {
            out.push(next);
            continue;
        }
        out.push(c);
    }
    out
}

/// Decode an RFC 2231 `charset'lang'percent-encoded` value.
fn decode_rfc2231(s: &str) -> String {
    if let Some((charset, rest)) = s.split_once('\'')
        && let Some((_, encoded)) = rest.split_once('\'')
    {
        let bytes = percent_decode_str(encoded).collect::<Vec<u8>>();
        return decode_charset(&bytes, charset);
    }
    let bytes = percent_decode_str(s).collect::<Vec<u8>>();
    decode_charset(&bytes, "utf-8")
}

/// Decode RFC 2047 encoded-words (`=?charset?Q?...?=` / `=?charset?B?...?=`)
/// inside a header value, ignoring linear whitespace between adjacent encoded-words.
pub(crate) fn decode_header_value(input: &str) -> String {
    let trimmed = input.trim();
    let mut out = String::with_capacity(trimmed.len());
    let mut rest = trimmed;
    let mut pending_charset: Option<String> = None;
    let mut pending_bytes: Vec<u8> = Vec::new();

    let flush = |out: &mut String, charset: &mut Option<String>, bytes: &mut Vec<u8>| {
        if !bytes.is_empty() {
            let cs = charset.as_deref().unwrap_or("utf-8");
            out.push_str(&decode_charset(bytes, cs));
            bytes.clear();
        }
        *charset = None;
    };

    while !rest.is_empty() {
        if let Some((charset, bytes, after)) = parse_encoded_word(rest) {
            let same_cs = pending_charset
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case(charset));
            if !same_cs {
                flush(&mut out, &mut pending_charset, &mut pending_bytes);
                pending_charset = Some(charset.to_string());
            }
            pending_bytes.extend_from_slice(&bytes);
            rest = after;
            // RFC 2047 §6.2: linear whitespace between two adjacent encoded-words is ignored.
            let ws_len = rest
                .bytes()
                .take_while(|b| matches!(b, b' ' | b'\t'))
                .count();
            if ws_len > 0 && parse_encoded_word(&rest[ws_len..]).is_some() {
                rest = &rest[ws_len..];
            }
            continue;
        }

        flush(&mut out, &mut pending_charset, &mut pending_bytes);
        // Advance past the whole first character: slicing at byte 1 would split a
        // multibyte one (`é` and the like) and panic.
        let first = rest.chars().next().map_or(0, char::len_utf8);
        if let Some(pos) = rest[first..].find("=?") {
            out.push_str(&rest[..pos + first]);
            rest = &rest[pos + first..];
        } else {
            out.push_str(rest);
            break;
        }
    }

    flush(&mut out, &mut pending_charset, &mut pending_bytes);
    out
}

fn parse_encoded_word(s: &str) -> Option<(&str, Vec<u8>, &str)> {
    let after_prefix = s.strip_prefix("=?")?;
    let q1 = after_prefix.find('?')?;
    let charset = &after_prefix[..q1];
    if charset.is_empty() || charset.bytes().any(|b| b.is_ascii_whitespace()) {
        return None;
    }
    let after_cs = &after_prefix[q1 + 1..];
    let q2 = after_cs.find('?')?;
    let encoding = &after_cs[..q2];
    let after_enc = &after_cs[q2 + 1..];
    let end = after_enc.find("?=")?;
    let payload = &after_enc[..end];
    if payload.bytes().any(|b| b.is_ascii_whitespace()) {
        return None;
    }
    let bytes = if encoding.eq_ignore_ascii_case("B") {
        decode_base64(payload.as_bytes())
    } else if encoding.eq_ignore_ascii_case("Q") {
        decode_q_word(payload.as_bytes())
    } else {
        return None;
    };
    Some((charset, bytes, &after_enc[end + 2..]))
}

fn decode_q_word(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            b'_' => {
                out.push(b' ');
                i += 1;
            }
            b'=' if i + 2 < input.len() => {
                if let (Some(hi), Some(lo)) = (hex_val(input[i + 1]), hex_val(input[i + 2])) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    out.push(b'=');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

fn decode_quoted_printable(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'=' {
            // Soft line break: `=` followed by optional spaces/tabs and LF/CRLF.
            let mut j = i + 1;
            while j < input.len() && matches!(input[j], b' ' | b'\t') {
                j += 1;
            }
            if j < input.len() && input[j] == b'\r' && j + 1 < input.len() && input[j + 1] == b'\n'
            {
                i = j + 2;
                continue;
            }
            if j < input.len() && input[j] == b'\n' {
                i = j + 1;
                continue;
            }
            if i + 2 < input.len()
                && let (Some(hi), Some(lo)) = (hex_val(input[i + 1]), hex_val(input[i + 2]))
            {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

fn decode_base64(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u8 = 0;
    for &b in input {
        if b == b'=' {
            break;
        }
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        buf = (buf << 6) | u32::from(val);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1u32 << bits) - 1;
        }
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// What windows-1252 puts in 0x80..=0x9F, where Latin-1 has the C1 controls. The five
/// undefined bytes (0x81, 0x8D, 0x8F, 0x90, 0x9D) stay as they are.
const WINDOWS_1252_HIGH: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}',
    '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
];

fn decode_charset(bytes: &[u8], charset: &str) -> String {
    let cs = charset.trim().to_ascii_lowercase();
    if matches!(cs.as_str(), "windows-1252" | "cp1252") {
        bytes
            .iter()
            .map(|&b| match b {
                0x80..=0x9f => WINDOWS_1252_HIGH[usize::from(b - 0x80)],
                _ => b as char,
            })
            .collect()
    } else if matches!(
        cs.as_str(),
        "iso-8859-1" | "iso8859-1" | "latin1" | "latin-1"
    ) {
        bytes.iter().map(|&b| b as char).collect()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// FIPS 180-4 SHA-256 hex digest (zero dependencies, safe Rust).
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = Vec::with_capacity(data.len() + 72);
    msg.extend_from_slice(data);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, &word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(word);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, delta) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(delta);
        }
    }
    let mut out = String::with_capacity(64);
    for word in h {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_standard_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn lettre_and_python_mime_produce_identical_output() {
        // Simulates Python email.message: quoted-printable, different header order,
        // MIME-Version at top, boundary "===py_bound===", folded Subject.
        let python_mail = "\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"===py_bound===\"\r\n\
To: Alice <alice@example.test>\r\n\
From: =?utf-8?q?Caf=C3=A9?= <app@example.test>\r\n\
Subject: =?utf-8?q?Welcome_=E2=80=94?=\r\n\
\x20=?utf-8?b?IEFs?= =?utf-8?b?aWNl?=\r\n\
Date: Mon, 24 Nov 2026 12:00:00 +0000\r\n\
\r\n\
--===py_bound===\r\n\
Content-Type: multipart/alternative; boundary=\"===alt_py===\"\r\n\
\r\n\
--===alt_py===\r\n\
Content-Type: text/plain; charset=\"utf-8\"\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
Hello Alice =\r\n\
from Caf=C3=A9!\r\n\
--===alt_py===\r\n\
Content-Type: text/html; charset=\"utf-8\"\r\n\
Content-Transfer-Encoding: 7bit\r\n\
\r\n\
<p>Hello Alice</p>\r\n\
--===alt_py===--\r\n\
--===py_bound===\r\n\
Content-Type: application/pdf\r\n\
Content-Disposition: attachment; filename*=UTF-8''invoice%201.pdf\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
JVBERi0xLjQ=\r\n\
--===py_bound===--\r\n";

        // Simulates Rust lettre: different header order, base64 text/plain,
        // different boundary strings, Message-ID, MIME-Version at bottom.
        let lettre_mail = "\
From: =?UTF-8?B?Q2Fmw6k=?= <app@example.test>\r\n\
To: Alice <alice@example.test>\r\n\
Subject: Welcome — Alice\r\n\
Message-ID: <abc123@rust.local>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=lettre_outer_99\r\n\
\r\n\
--lettre_outer_99\r\n\
Content-Type: multipart/alternative; boundary=lettre_inner_42\r\n\
\r\n\
--lettre_inner_42\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
SGVsbG8gQWxpY2UgZnJvbSBDYWbDqSE=\r\n\
--lettre_inner_42\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
<p>Hello Alice</p>\r\n\
--lettre_inner_42--\r\n\
--lettre_outer_99\r\n\
Content-Type: application/pdf; name=\"invoice 1.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
Content-Disposition: attachment; filename=\"invoice 1.pdf\"\r\n\
\r\n\
JVBERi0xLjQ=\r\n\
--lettre_outer_99--\r\n";

        let py_out = format_email(python_mail);
        let rs_out = format_email(lettre_mail);
        assert_eq!(py_out, rs_out);
        assert_eq!(
            py_out,
            format!(
                "From: Café <app@example.test>\n\
                 To: Alice <alice@example.test>\n\
                 Subject: Welcome — Alice\n\
                 part text/plain\n\
                 Hello Alice from Café!\n\
                 part text/html\n\
                 <p>Hello Alice</p>\n\
                 attachment invoice 1.pdf (application/pdf, 8 bytes, sha256:{})\n",
                sha256_hex(b"%PDF-1.4")
            )
        );
    }

    #[test]
    fn rfc2047_split_multibyte_and_latin1_decode() {
        // UTF-8 é (0xC3 0xA9) split across two encoded words:
        assert_eq!(decode_header_value("=?utf-8?q?=C3?= =?utf-8?q?=A9?="), "é");
        // ISO-8859-1:
        assert_eq!(
            decode_header_value("=?ISO-8859-1?Q?Caf=E9?= bar"),
            "Café bar"
        );
    }

    #[test]
    fn windows_1252_decodes_its_printable_punctuation() {
        assert_eq!(
            decode_header_value("=?windows-1252?Q?=80_=93hi=94?="),
            "€ “hi”"
        );
        let mail = "Content-Type: text/plain; charset=\"windows-1252\"\r\n\
                    Content-Transfer-Encoding: quoted-printable\r\n\
                    \r\n\
                    =93hi=94 costs =805\r\n";
        let out = format_email(mail);
        assert!(out.contains("part text/plain\n“hi” costs €5\n"), "{out}");
        // `cp1252` is the same encoding, undefined bytes stay as they are, and
        // Latin-1 keeps decoding byte for byte.
        assert_eq!(decode_charset(&[0x80, 0x81, 0x93], "cp1252"), "€\u{81}“");
        assert_eq!(
            decode_charset(&[0x80, 0x93, 0x94], "iso-8859-1"),
            "\u{80}\u{93}\u{94}"
        );
        assert_eq!(decode_charset(&[0xe9], "latin1"), "é");
    }
}
