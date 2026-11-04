// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! Decoding of DVB text strings (ETSI EN 300 468, annex A).
//!
//! A string starts with an optional byte that selects the character table.
//! Without it, the default table (ISO/IEC 6937) is used. Only the tables used
//! by European cable networks are implemented; others fall back to ISO
//! 8859-1.
//!
//! Some broadcasters send UTF-8 without the selector byte. As UTF-8 with
//! non-ASCII characters hardly ever is valid ISO 6937 (where the bytes
//! `0xC1..=0xCF` are diacritical marks followed by a letter), such text is
//! decoded as UTF-8.

/// Decodes a DVB text string. Control codes (emphasis, line breaks) are
/// handled: line breaks are kept, everything else is dropped.
pub fn decode(bytes: &[u8]) -> String {
    let Some((&first, rest)) = bytes.split_first() else {
        return String::new();
    };

    let text = match first {
        0x20..=0xFF if !bytes.is_ascii() && std::str::from_utf8(bytes).is_ok() => {
            decode_utf8(bytes)
        }
        0x20..=0xFF => decode_iso6937(bytes),
        0x01..=0x0B => decode_iso8859(first_8859_part(first), rest),
        0x10 if rest.len() >= 2 => decode_iso8859(rest[1], &rest[2..]),
        0x11 => decode_ucs2(rest),
        0x15 => decode_utf8(rest),
        // KS X 1001, GB 2312, Big5 and unknown tables
        _ => decode_iso8859(1, rest),
    };

    text.trim().to_owned()
}

/// Control code for a line break in 8-bit tables.
const LINE_BREAK: u8 = 0x8A;

/// Maps the selector byte `0x01..=0x0B` to the part of ISO/IEC 8859: 0x01
/// selects part 5, 0x0B selects part 15.
fn first_8859_part(selector: u8) -> u8 {
    selector + 4
}

fn decode_iso8859(part: u8, bytes: &[u8]) -> String {
    bytes
        .iter()
        .filter_map(|&byte| match byte {
            LINE_BREAK => Some('\n'),
            0x00..=0x1F | 0x7F..=0x9F => None,
            0x20..=0x7E => Some(byte as char),
            _ => Some(match part {
                9 => latin5(byte),
                15 => latin9(byte),
                // Latin-1 is used for all other parts
                _ => byte as char,
            }),
        })
        .collect()
}

/// ISO 8859-15 differs from ISO 8859-1 in 8 positions.
fn latin9(byte: u8) -> char {
    match byte {
        0xA4 => '€',
        0xA6 => 'Š',
        0xA8 => 'š',
        0xB4 => 'Ž',
        0xB8 => 'ž',
        0xBC => 'Œ',
        0xBD => 'œ',
        0xBE => 'Ÿ',
        _ => byte as char,
    }
}

/// ISO 8859-9 differs from ISO 8859-1 in 6 positions.
fn latin5(byte: u8) -> char {
    match byte {
        0xD0 => 'Ğ',
        0xDD => 'İ',
        0xDE => 'Ş',
        0xF0 => 'ğ',
        0xFD => 'ı',
        0xFE => 'ş',
        _ => byte as char,
    }
}

/// Keeps line breaks and drops other control characters. In Unicode
/// strings, the control codes `0x80..=0x9F` are mapped to the private use
/// area `E080..E09F`.
fn without_control_codes(c: char) -> Option<char> {
    match c {
        '\u{8A}' | '\u{E08A}' | '\n' => Some('\n'),
        '\u{E080}'..='\u{E09F}' => None,
        c if c.is_control() => None,
        c => Some(c),
    }
}

fn decode_utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter_map(without_control_codes)
        .collect()
}

fn decode_ucs2(bytes: &[u8]) -> String {
    char::decode_utf16(
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| u16::from_be_bytes(pair)),
    )
    .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
    .filter_map(without_control_codes)
    .collect()
}

/// Non-spacing diacritical marks of ISO 6937 (`0xC1..=0xCF`) as Unicode
/// combining characters, and the precomposed characters for common bases.
const DIACRITICS: &[(u8, char, &str, &str)] = &[
    (0xC1, '\u{0300}', "AEIOUaeiou", "ÀÈÌÒÙàèìòù"),
    (
        0xC2,
        '\u{0301}',
        "AEIOUYaeiouyCcLlNnRrSsZz",
        "ÁÉÍÓÚÝáéíóúýĆćĹĺŃńŔŕŚśŹź",
    ),
    (
        0xC3,
        '\u{0302}',
        "AEIOUaeiouCcGgHhJjSsWwYy",
        "ÂÊÎÔÛâêîôûĈĉĜĝĤĥĴĵŜŝŴŵŶŷ",
    ),
    (0xC4, '\u{0303}', "ANOanoIiUu", "ÃÑÕãñõĨĩŨũ"),
    (0xC5, '\u{0304}', "AEIOUaeiou", "ĀĒĪŌŪāēīōū"),
    (0xC6, '\u{0306}', "AGUagu", "ĂĞŬăğŭ"),
    (0xC7, '\u{0307}', "CEGIZcegz", "ĊĖĠİŻċėġż"),
    (0xC8, '\u{0308}', "AEIOUaeiouyY", "ÄËÏÖÜäëïöüÿŸ"),
    (0xCA, '\u{030A}', "AaUu", "ÅåŮů"),
    (0xCB, '\u{0327}', "CcGKkLlNnRrSsTt", "ÇçĢĶķĻļŅņŖŗŞşŢţ"),
    (0xCD, '\u{030B}', "OUou", "ŐŰőű"),
    (0xCE, '\u{0328}', "AEIUaeiu", "ĄĘĮŲąęįų"),
    (0xCF, '\u{030C}', "CDELNRSTZcdelnrstz", "ČĎĚĽŇŘŠŤŽčďěľňřšťž"),
];

/// Characters of ISO 6937 in `0xA0..=0xFF` that are not diacritical marks.
fn iso6937_char(byte: u8) -> Option<char> {
    Some(match byte {
        0xA0 => '\u{A0}',
        0xA1 => '¡',
        0xA2 => '¢',
        0xA3 => '£',
        0xA4 => '€',
        0xA5 => '¥',
        0xA7 => '§',
        0xA8 => '¤',
        0xA9 => '‘',
        0xAA => '“',
        0xAB => '«',
        0xAC => '←',
        0xAD => '↑',
        0xAE => '→',
        0xAF => '↓',
        0xB0 => '°',
        0xB1 => '±',
        0xB2 => '²',
        0xB3 => '³',
        0xB4 => '×',
        0xB5 => 'µ',
        0xB6 => '¶',
        0xB7 => '·',
        0xB8 => '÷',
        0xB9 => '’',
        0xBA => '”',
        0xBB => '»',
        0xBC => '¼',
        0xBD => '½',
        0xBE => '¾',
        0xBF => '¿',
        0xD0 => '―',
        0xD1 => '¹',
        0xD2 => '®',
        0xD3 => '©',
        0xD4 => '™',
        0xD5 => '♪',
        0xD6 => '¬',
        0xD7 => '¦',
        0xDC => '⅛',
        0xDD => '⅜',
        0xDE => '⅝',
        0xDF => '⅞',
        0xE0 => 'Ω',
        0xE1 => 'Æ',
        0xE2 => 'Đ',
        0xE3 => 'ª',
        0xE4 => 'Ħ',
        0xE6 => 'Ĳ',
        0xE7 => 'Ŀ',
        0xE8 => 'Ł',
        0xE9 => 'Ø',
        0xEA => 'Œ',
        0xEB => 'º',
        0xEC => 'Þ',
        0xED => 'Ŧ',
        0xEE => 'Ŋ',
        0xEF => 'ŉ',
        0xF0 => 'ĸ',
        0xF1 => 'æ',
        0xF2 => 'đ',
        0xF3 => 'ð',
        0xF4 => 'ħ',
        0xF5 => 'ı',
        0xF6 => 'ĳ',
        0xF7 => 'ŀ',
        0xF8 => 'ł',
        0xF9 => 'ø',
        0xFA => 'œ',
        0xFB => 'ß',
        0xFC => 'þ',
        0xFD => 'ŧ',
        0xFE => 'ŋ',
        _ => return None,
    })
}

fn decode_iso6937(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len());
    let mut iter = bytes.iter().copied().peekable();

    while let Some(byte) = iter.next() {
        match byte {
            LINE_BREAK => text.push('\n'),
            0x00..=0x1F | 0x7F..=0x9F => (),
            0x20..=0x7E => text.push(byte as char),
            0xC1..=0xC8 | 0xCA..=0xCB | 0xCD..=0xCF => {
                let (_, combining, bases, composed) = DIACRITICS
                    .iter()
                    .find(|(mark, ..)| *mark == byte)
                    .expect("all marks are in the table");

                match iter.peek().copied().filter(u8::is_ascii_alphabetic) {
                    Some(base) => {
                        iter.next();
                        let base = base as char;
                        match bases.chars().position(|c| c == base) {
                            Some(index) => text.push(
                                composed
                                    .chars()
                                    .nth(index)
                                    .expect("bases and composed have the same length"),
                            ),
                            None => {
                                text.push(base);
                                text.push(*combining);
                            }
                        }
                    }
                    // a mark without a base letter, e.g. the standalone
                    // diaeresis `0xC8 0x20`
                    None => {
                        if iter.peek() == Some(&b' ') {
                            iter.next();
                        }
                    }
                }
            }
            _ => text.extend(iso6937_char(byte)),
        }
    }

    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_default_table() {
        assert_eq!(decode(b"Tagesschau"), "Tagesschau");
        assert_eq!(decode(b""), "");
        // `0xC8 a` is “ä”, `0xFB` is “ß”
        assert_eq!(decode(b"\xC8a\xC8o\xC8u\xC8A \xFB"), "äöüÄ ß");
        assert_eq!(decode(b"Caf\xC2e \xC1a la carte"), "Café à la carte");
        assert_eq!(decode(b"\xCFSkoda \xE9"), "Škoda Ø");
        // unknown combinations keep the base letter and add the mark
        assert_eq!(decode(b"\xC8x"), "x\u{0308}");
    }

    #[test]
    fn detects_utf8_without_selector() {
        assert_eq!(
            decode("Gründe für Käse – Ärger".as_bytes()),
            "Gründe für Käse – Ärger"
        );
        // control codes are two bytes in UTF-8: U+008A is a line break
        assert_eq!(decode("Zeile\u{8A}Zeile".as_bytes()), "Zeile\nZeile");
        // ISO 6937 text is not valid UTF-8
        assert_eq!(decode(b"Gr\xC8unde"), "Gründe");
    }

    #[test]
    fn handles_control_codes() {
        // emphasis on/off around a word and a line break
        assert_eq!(decode(b"\x86Wetter\x87\x8Amorgen"), "Wetter\nmorgen");
        assert_eq!(decode(b"  Titel \x8A\x8A"), "Titel");
    }

    #[test]
    fn decodes_selected_tables() {
        // ISO 8859-15 (selector 0x0B): € and Š
        assert_eq!(decode(b"\x0B5 \xA4 \xA6"), "5 € Š");
        // ISO 8859-9 (selector 0x05): ğ and ş
        assert_eq!(decode(b"\x05\xF0\xFE"), "ğş");
        // ISO 8859-1 via the three byte form
        assert_eq!(decode(b"\x10\x00\x01K\xE4se"), "Käse");
        // ISO 8859-15 via the three byte form
        assert_eq!(decode(b"\x10\x00\x0F\xA4"), "€");
        // UTF-8
        assert_eq!(decode(b"\x15K\xC3\xA4se"), "Käse");
        // UCS-2 with a line break (private use code)
        assert_eq!(decode(&[0x11, 0x00, b'A', 0xE0, 0x8A, 0x00, 0xE4]), "A\nä");
    }
}
