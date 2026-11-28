//! Mail decoding for recorded SMTP effects: encoded-word headers and part bodies in
//! windows-1252 (printable punctuation in 0x80..=0x9F) and Latin-1 (byte for byte).

use rust_anything::mime::format_email;

#[test]
fn windows_1252_header_and_quoted_printable_body_decode() {
    let mail = "Subject: =?windows-1252?Q?=80_=93hi=94?=\r\n\
                Content-Type: text/plain; charset=\"windows-1252\"\r\n\
                Content-Transfer-Encoding: quoted-printable\r\n\
                \r\n\
                =93hi=94 costs =805\r\n";
    let out = format_email(mail);
    assert!(out.contains("Subject: € “hi”\n"), "{out}");
    assert!(out.contains("part text/plain\n“hi” costs €5\n"), "{out}");
    assert!(!out.contains('\u{80}') && !out.contains('\u{93}'), "{out}");
}

#[test]
fn latin1_header_decodes_byte_for_byte() {
    let out = format_email("Subject: =?ISO-8859-1?Q?Caf=E9?=\r\n\r\n");
    assert!(out.contains("Subject: Café\n"), "{out}");
}

#[test]
fn headers_with_multibyte_text_around_encoded_words_decode() {
    // Plain multibyte text, as a mailer that needs no encoding writes it.
    let out = format_email("Subject: é\r\n\r\n");
    assert!(out.contains("Subject: é\n"), "{out}");
    // An encoded word followed by multibyte text.
    let out = format_email("Subject: =?UTF-8?Q?a?=é\r\n\r\n");
    assert!(out.contains("Subject: aé\n"), "{out}");
    // An ASCII encoded word, unchanged.
    let out = format_email("Subject: =?UTF-8?Q?hello?=\r\n\r\n");
    assert!(out.contains("Subject: hello\n"), "{out}");
    // Multibyte text before an encoded word: the search for `=?` must step over
    // the whole first character, not into the middle of it.
    let out = format_email("Subject: é =?UTF-8?Q?a?=\r\n\r\n");
    assert!(out.contains("Subject: é a\n"), "{out}");
}
