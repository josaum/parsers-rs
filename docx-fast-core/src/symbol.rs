//! Symbol-font characters to Unicode.
//!
//! WordprocessingML stores a character set in a symbol-encoded font either as
//! a `w:sym` element (`w:font` plus a hexadecimal `w:char`) or as run text in
//! the Private Use Area. Either way the stored value is a position in the
//! font, not the character a reader sees, so plain-text extraction has to map
//! it.
//!
//! Only the Symbol font has a published Unicode mapping, so only it is mapped.
//! Characters of other symbol fonts (Wingdings, Webdings, ...) are kept at the
//! code point the document stores; they are never dropped.

/// First code point of the Private Use Area window that symbol fonts occupy.
///
/// A symbol font exposes its 8-bit code `0xNN` at `0xF0NN`, which is the form
/// Word writes in `w:char` and in run text.
const SYMBOL_PUA_BASE: u32 = 0xF000;
/// Last code point of that window (`0xF000 + 0xFF`).
const SYMBOL_PUA_LAST: u32 = 0xF0FF;

/// Adobe Symbol encoding to Unicode, sorted by Symbol code.
///
/// Generated from the Unicode Consortium's `symbol.txt`, table version 1.0
/// (<https://www.unicode.org/Public/MAPPINGS/VENDORS/ADOBE/symbol.txt>).
/// Where that table gives a code two Unicode values, the plain or Greek one
/// is kept: SPACE over NO-BREAK SPACE, GREEK CAPITAL LETTER DELTA over
/// INCREMENT, GREEK CAPITAL LETTER OMEGA over OHM SIGN, GREEK SMALL LETTER MU
/// over MICRO SIGN, FRACTION SLASH over DIVISION SLASH. The bracket and
/// radical pieces have no Unicode character and stay in Adobe's Corporate Use
/// Subarea (U+F8E5..=U+F8FE), exactly as the source table assigns them.
const SYMBOL_TO_UNICODE: [(u8, char); 189] = [
    (0x20, '\u{0020}'), // SPACE
    (0x21, '\u{0021}'), // EXCLAMATION MARK
    (0x22, '\u{2200}'), // FOR ALL
    (0x23, '\u{0023}'), // NUMBER SIGN
    (0x24, '\u{2203}'), // THERE EXISTS
    (0x25, '\u{0025}'), // PERCENT SIGN
    (0x26, '\u{0026}'), // AMPERSAND
    (0x27, '\u{220B}'), // CONTAINS AS MEMBER
    (0x28, '\u{0028}'), // LEFT PARENTHESIS
    (0x29, '\u{0029}'), // RIGHT PARENTHESIS
    (0x2A, '\u{2217}'), // ASTERISK OPERATOR
    (0x2B, '\u{002B}'), // PLUS SIGN
    (0x2C, '\u{002C}'), // COMMA
    (0x2D, '\u{2212}'), // MINUS SIGN
    (0x2E, '\u{002E}'), // FULL STOP
    (0x2F, '\u{002F}'), // SOLIDUS
    (0x30, '\u{0030}'), // DIGIT ZERO
    (0x31, '\u{0031}'), // DIGIT ONE
    (0x32, '\u{0032}'), // DIGIT TWO
    (0x33, '\u{0033}'), // DIGIT THREE
    (0x34, '\u{0034}'), // DIGIT FOUR
    (0x35, '\u{0035}'), // DIGIT FIVE
    (0x36, '\u{0036}'), // DIGIT SIX
    (0x37, '\u{0037}'), // DIGIT SEVEN
    (0x38, '\u{0038}'), // DIGIT EIGHT
    (0x39, '\u{0039}'), // DIGIT NINE
    (0x3A, '\u{003A}'), // COLON
    (0x3B, '\u{003B}'), // SEMICOLON
    (0x3C, '\u{003C}'), // LESS-THAN SIGN
    (0x3D, '\u{003D}'), // EQUALS SIGN
    (0x3E, '\u{003E}'), // GREATER-THAN SIGN
    (0x3F, '\u{003F}'), // QUESTION MARK
    (0x40, '\u{2245}'), // APPROXIMATELY EQUAL TO
    (0x41, '\u{0391}'), // GREEK CAPITAL LETTER ALPHA
    (0x42, '\u{0392}'), // GREEK CAPITAL LETTER BETA
    (0x43, '\u{03A7}'), // GREEK CAPITAL LETTER CHI
    (0x44, '\u{0394}'), // GREEK CAPITAL LETTER DELTA
    (0x45, '\u{0395}'), // GREEK CAPITAL LETTER EPSILON
    (0x46, '\u{03A6}'), // GREEK CAPITAL LETTER PHI
    (0x47, '\u{0393}'), // GREEK CAPITAL LETTER GAMMA
    (0x48, '\u{0397}'), // GREEK CAPITAL LETTER ETA
    (0x49, '\u{0399}'), // GREEK CAPITAL LETTER IOTA
    (0x4A, '\u{03D1}'), // GREEK THETA SYMBOL
    (0x4B, '\u{039A}'), // GREEK CAPITAL LETTER KAPPA
    (0x4C, '\u{039B}'), // GREEK CAPITAL LETTER LAMDA
    (0x4D, '\u{039C}'), // GREEK CAPITAL LETTER MU
    (0x4E, '\u{039D}'), // GREEK CAPITAL LETTER NU
    (0x4F, '\u{039F}'), // GREEK CAPITAL LETTER OMICRON
    (0x50, '\u{03A0}'), // GREEK CAPITAL LETTER PI
    (0x51, '\u{0398}'), // GREEK CAPITAL LETTER THETA
    (0x52, '\u{03A1}'), // GREEK CAPITAL LETTER RHO
    (0x53, '\u{03A3}'), // GREEK CAPITAL LETTER SIGMA
    (0x54, '\u{03A4}'), // GREEK CAPITAL LETTER TAU
    (0x55, '\u{03A5}'), // GREEK CAPITAL LETTER UPSILON
    (0x56, '\u{03C2}'), // GREEK SMALL LETTER FINAL SIGMA
    (0x57, '\u{03A9}'), // GREEK CAPITAL LETTER OMEGA
    (0x58, '\u{039E}'), // GREEK CAPITAL LETTER XI
    (0x59, '\u{03A8}'), // GREEK CAPITAL LETTER PSI
    (0x5A, '\u{0396}'), // GREEK CAPITAL LETTER ZETA
    (0x5B, '\u{005B}'), // LEFT SQUARE BRACKET
    (0x5C, '\u{2234}'), // THEREFORE
    (0x5D, '\u{005D}'), // RIGHT SQUARE BRACKET
    (0x5E, '\u{22A5}'), // UP TACK
    (0x5F, '\u{005F}'), // LOW LINE
    (0x60, '\u{F8E5}'), // RADICAL EXTENDER
    (0x61, '\u{03B1}'), // GREEK SMALL LETTER ALPHA
    (0x62, '\u{03B2}'), // GREEK SMALL LETTER BETA
    (0x63, '\u{03C7}'), // GREEK SMALL LETTER CHI
    (0x64, '\u{03B4}'), // GREEK SMALL LETTER DELTA
    (0x65, '\u{03B5}'), // GREEK SMALL LETTER EPSILON
    (0x66, '\u{03C6}'), // GREEK SMALL LETTER PHI
    (0x67, '\u{03B3}'), // GREEK SMALL LETTER GAMMA
    (0x68, '\u{03B7}'), // GREEK SMALL LETTER ETA
    (0x69, '\u{03B9}'), // GREEK SMALL LETTER IOTA
    (0x6A, '\u{03D5}'), // GREEK PHI SYMBOL
    (0x6B, '\u{03BA}'), // GREEK SMALL LETTER KAPPA
    (0x6C, '\u{03BB}'), // GREEK SMALL LETTER LAMDA
    (0x6D, '\u{03BC}'), // GREEK SMALL LETTER MU
    (0x6E, '\u{03BD}'), // GREEK SMALL LETTER NU
    (0x6F, '\u{03BF}'), // GREEK SMALL LETTER OMICRON
    (0x70, '\u{03C0}'), // GREEK SMALL LETTER PI
    (0x71, '\u{03B8}'), // GREEK SMALL LETTER THETA
    (0x72, '\u{03C1}'), // GREEK SMALL LETTER RHO
    (0x73, '\u{03C3}'), // GREEK SMALL LETTER SIGMA
    (0x74, '\u{03C4}'), // GREEK SMALL LETTER TAU
    (0x75, '\u{03C5}'), // GREEK SMALL LETTER UPSILON
    (0x76, '\u{03D6}'), // GREEK PI SYMBOL
    (0x77, '\u{03C9}'), // GREEK SMALL LETTER OMEGA
    (0x78, '\u{03BE}'), // GREEK SMALL LETTER XI
    (0x79, '\u{03C8}'), // GREEK SMALL LETTER PSI
    (0x7A, '\u{03B6}'), // GREEK SMALL LETTER ZETA
    (0x7B, '\u{007B}'), // LEFT CURLY BRACKET
    (0x7C, '\u{007C}'), // VERTICAL LINE
    (0x7D, '\u{007D}'), // RIGHT CURLY BRACKET
    (0x7E, '\u{223C}'), // TILDE OPERATOR
    (0xA0, '\u{20AC}'), // EURO SIGN
    (0xA1, '\u{03D2}'), // GREEK UPSILON WITH HOOK SYMBOL
    (0xA2, '\u{2032}'), // PRIME
    (0xA3, '\u{2264}'), // LESS-THAN OR EQUAL TO
    (0xA4, '\u{2044}'), // FRACTION SLASH
    (0xA5, '\u{221E}'), // INFINITY
    (0xA6, '\u{0192}'), // LATIN SMALL LETTER F WITH HOOK
    (0xA7, '\u{2663}'), // BLACK CLUB SUIT
    (0xA8, '\u{2666}'), // BLACK DIAMOND SUIT
    (0xA9, '\u{2665}'), // BLACK HEART SUIT
    (0xAA, '\u{2660}'), // BLACK SPADE SUIT
    (0xAB, '\u{2194}'), // LEFT RIGHT ARROW
    (0xAC, '\u{2190}'), // LEFTWARDS ARROW
    (0xAD, '\u{2191}'), // UPWARDS ARROW
    (0xAE, '\u{2192}'), // RIGHTWARDS ARROW
    (0xAF, '\u{2193}'), // DOWNWARDS ARROW
    (0xB0, '\u{00B0}'), // DEGREE SIGN
    (0xB1, '\u{00B1}'), // PLUS-MINUS SIGN
    (0xB2, '\u{2033}'), // DOUBLE PRIME
    (0xB3, '\u{2265}'), // GREATER-THAN OR EQUAL TO
    (0xB4, '\u{00D7}'), // MULTIPLICATION SIGN
    (0xB5, '\u{221D}'), // PROPORTIONAL TO
    (0xB6, '\u{2202}'), // PARTIAL DIFFERENTIAL
    (0xB7, '\u{2022}'), // BULLET
    (0xB8, '\u{00F7}'), // DIVISION SIGN
    (0xB9, '\u{2260}'), // NOT EQUAL TO
    (0xBA, '\u{2261}'), // IDENTICAL TO
    (0xBB, '\u{2248}'), // ALMOST EQUAL TO
    (0xBC, '\u{2026}'), // HORIZONTAL ELLIPSIS
    (0xBD, '\u{F8E6}'), // VERTICAL ARROW EXTENDER
    (0xBE, '\u{F8E7}'), // HORIZONTAL ARROW EXTENDER
    (0xBF, '\u{21B5}'), // DOWNWARDS ARROW WITH CORNER LEFTWARDS
    (0xC0, '\u{2135}'), // ALEF SYMBOL
    (0xC1, '\u{2111}'), // BLACK-LETTER CAPITAL I
    (0xC2, '\u{211C}'), // BLACK-LETTER CAPITAL R
    (0xC3, '\u{2118}'), // SCRIPT CAPITAL P
    (0xC4, '\u{2297}'), // CIRCLED TIMES
    (0xC5, '\u{2295}'), // CIRCLED PLUS
    (0xC6, '\u{2205}'), // EMPTY SET
    (0xC7, '\u{2229}'), // INTERSECTION
    (0xC8, '\u{222A}'), // UNION
    (0xC9, '\u{2283}'), // SUPERSET OF
    (0xCA, '\u{2287}'), // SUPERSET OF OR EQUAL TO
    (0xCB, '\u{2284}'), // NOT A SUBSET OF
    (0xCC, '\u{2282}'), // SUBSET OF
    (0xCD, '\u{2286}'), // SUBSET OF OR EQUAL TO
    (0xCE, '\u{2208}'), // ELEMENT OF
    (0xCF, '\u{2209}'), // NOT AN ELEMENT OF
    (0xD0, '\u{2220}'), // ANGLE
    (0xD1, '\u{2207}'), // NABLA
    (0xD2, '\u{F6DA}'), // REGISTERED SIGN SERIF
    (0xD3, '\u{F6D9}'), // COPYRIGHT SIGN SERIF
    (0xD4, '\u{F6DB}'), // TRADE MARK SIGN SERIF
    (0xD5, '\u{220F}'), // N-ARY PRODUCT
    (0xD6, '\u{221A}'), // SQUARE ROOT
    (0xD7, '\u{22C5}'), // DOT OPERATOR
    (0xD8, '\u{00AC}'), // NOT SIGN
    (0xD9, '\u{2227}'), // LOGICAL AND
    (0xDA, '\u{2228}'), // LOGICAL OR
    (0xDB, '\u{21D4}'), // LEFT RIGHT DOUBLE ARROW
    (0xDC, '\u{21D0}'), // LEFTWARDS DOUBLE ARROW
    (0xDD, '\u{21D1}'), // UPWARDS DOUBLE ARROW
    (0xDE, '\u{21D2}'), // RIGHTWARDS DOUBLE ARROW
    (0xDF, '\u{21D3}'), // DOWNWARDS DOUBLE ARROW
    (0xE0, '\u{25CA}'), // LOZENGE
    (0xE1, '\u{2329}'), // LEFT-POINTING ANGLE BRACKET
    (0xE2, '\u{F8E8}'), // REGISTERED SIGN SANS SERIF
    (0xE3, '\u{F8E9}'), // COPYRIGHT SIGN SANS SERIF
    (0xE4, '\u{F8EA}'), // TRADE MARK SIGN SANS SERIF
    (0xE5, '\u{2211}'), // N-ARY SUMMATION
    (0xE6, '\u{F8EB}'), // LEFT PAREN TOP
    (0xE7, '\u{F8EC}'), // LEFT PAREN EXTENDER
    (0xE8, '\u{F8ED}'), // LEFT PAREN BOTTOM
    (0xE9, '\u{F8EE}'), // LEFT SQUARE BRACKET TOP
    (0xEA, '\u{F8EF}'), // LEFT SQUARE BRACKET EXTENDER
    (0xEB, '\u{F8F0}'), // LEFT SQUARE BRACKET BOTTOM
    (0xEC, '\u{F8F1}'), // LEFT CURLY BRACKET TOP
    (0xED, '\u{F8F2}'), // LEFT CURLY BRACKET MID
    (0xEE, '\u{F8F3}'), // LEFT CURLY BRACKET BOTTOM
    (0xEF, '\u{F8F4}'), // CURLY BRACKET EXTENDER
    (0xF1, '\u{232A}'), // RIGHT-POINTING ANGLE BRACKET
    (0xF2, '\u{222B}'), // INTEGRAL
    (0xF3, '\u{2320}'), // TOP HALF INTEGRAL
    (0xF4, '\u{F8F5}'), // INTEGRAL EXTENDER
    (0xF5, '\u{2321}'), // BOTTOM HALF INTEGRAL
    (0xF6, '\u{F8F6}'), // RIGHT PAREN TOP
    (0xF7, '\u{F8F7}'), // RIGHT PAREN EXTENDER
    (0xF8, '\u{F8F8}'), // RIGHT PAREN BOTTOM
    (0xF9, '\u{F8F9}'), // RIGHT SQUARE BRACKET TOP
    (0xFA, '\u{F8FA}'), // RIGHT SQUARE BRACKET EXTENDER
    (0xFB, '\u{F8FB}'), // RIGHT SQUARE BRACKET BOTTOM
    (0xFC, '\u{F8FC}'), // RIGHT CURLY BRACKET TOP
    (0xFD, '\u{F8FD}'), // RIGHT CURLY BRACKET MID
    (0xFE, '\u{F8FE}'), // RIGHT CURLY BRACKET BOTTOM
];

/// Returns whether `font` names the Symbol font.
pub(crate) fn is_symbol_font(font: &str) -> bool {
    font.trim().eq_ignore_ascii_case("Symbol")
}

/// Maps a Symbol-font code to Unicode.
///
/// Accepts the 8-bit code or its Private Use Area form (`0xF061` and `0x61`
/// are both alpha). Returns `None` for codes the font does not define.
pub(crate) fn symbol_to_unicode(code: u32) -> Option<char> {
    let code = match code {
        SYMBOL_PUA_BASE..=SYMBOL_PUA_LAST => code - SYMBOL_PUA_BASE,
        _ => code,
    };
    let code = u8::try_from(code).ok()?;
    SYMBOL_TO_UNICODE
        .binary_search_by_key(&code, |(symbol, _)| *symbol)
        .ok()
        .map(|index| SYMBOL_TO_UNICODE[index].1)
}

/// Resolves a `w:sym` element to the character it displays.
///
/// `char_hex` is the `w:char` attribute. Symbol-font characters are mapped to
/// Unicode; any other font keeps the stored code point, since no mapping for
/// it is published. Returns `None` only when `char_hex` is not a code point.
pub(crate) fn sym_char(font: Option<&str>, char_hex: &str) -> Option<char> {
    let code = u32::from_str_radix(char_hex.trim(), 16).ok()?;
    if font.is_some_and(is_symbol_font) {
        if let Some(mapped) = symbol_to_unicode(code) {
            return Some(mapped);
        }
    }
    char::from_u32(code).filter(|ch| *ch != '\0')
}

/// Maps Private Use Area characters of Symbol-font run text to Unicode.
///
/// Only code points in the symbol window are touched, so ordinary text in the
/// same run is unchanged.
pub(crate) fn map_symbol_run_text(text: &str) -> String {
    text.chars()
        .map(|ch| match u32::from(ch) {
            code @ SYMBOL_PUA_BASE..=SYMBOL_PUA_LAST => symbol_to_unicode(code).unwrap_or(ch),
            _ => ch,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_so_binary_search_is_valid() {
        assert!(SYMBOL_TO_UNICODE
            .windows(2)
            .all(|pair| pair[0].0 < pair[1].0));
    }

    #[test]
    fn symbol_codes_map_in_both_forms() {
        assert_eq!(symbol_to_unicode(0x61), Some('α'));
        assert_eq!(symbol_to_unicode(0xF061), Some('α'));
        assert_eq!(symbol_to_unicode(0xF0B0), Some('°'));
        assert_eq!(symbol_to_unicode(0xF0A0), Some('€'));
        assert_eq!(symbol_to_unicode(0x44), Some('Δ'));
        assert_eq!(symbol_to_unicode(0x1F), None);
        assert_eq!(symbol_to_unicode(0x1_0000), None);
    }

    #[test]
    fn sym_keeps_the_code_point_for_fonts_without_a_mapping() {
        assert_eq!(sym_char(Some("Symbol"), "F0B3"), Some('≥'));
        assert_eq!(sym_char(Some("symbol"), "00B3"), Some('≥'));
        assert_eq!(sym_char(Some("Wingdings"), "F0FC"), Some('\u{F0FC}'));
        assert_eq!(sym_char(None, "2022"), Some('•'));
        assert_eq!(sym_char(Some("Symbol"), "not hex"), None);
        assert_eq!(sym_char(Some("Wingdings"), "D800"), None);
    }

    #[test]
    fn run_text_mapping_touches_only_the_symbol_window() {
        assert_eq!(map_symbol_run_text("a\u{F061}\u{F062} é"), "aαβ é");
    }
}
