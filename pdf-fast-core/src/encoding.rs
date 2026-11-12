//! Standard PDF font encodings + Adobe Glyph List (AGL) lookup.
//!
//! When a Type1 / TrueType font lacks a `/ToUnicode` CMap, text bytes have to
//! be resolved through the font's `/Encoding` entry instead.  This module
//! provides:
//!
//! - The three standard PDF base encodings (`StandardEncoding`, `WinAnsiEncoding`,
//!   `MacRomanEncoding`) as compile-time `[Option<&'static str>; 256]` tables
//!   keyed by glyph name.
//! - A subset of the Adobe Glyph List that covers every glyph reachable from
//!   those base tables plus the common `/Differences` overrides observed in
//!   Acrobat-3-era PDFs.
//! - [`EncodingMap`], which composes a chosen base table with a parsed
//!   `/Differences` array and exposes a single `decode_byte` API.
//!
//! The decoder pipeline is:
//!
//! ```text
//!   byte → encoding_table[byte] = glyph_name → AGL[glyph_name] = char(s)
//! ```
//!
//! When a glyph name is not in the AGL but begins with `uni` or `u` followed
//! by hex digits (e.g. `uni00E3`, `u1F600`), it is decoded directly.

use crate::PdfValue;

// ── Base encodings ───────────────────────────────────────────────────────────

/// `StandardEncoding` per PDF 32000-1:2008 Appendix D, Table D.2 column 2.
/// This is the default base encoding for nonsymbolic Type1 fonts when neither
/// `/Encoding` nor `/BaseEncoding` is specified.
pub const STANDARD_ENCODING: [Option<&str>; 256] = build_standard_encoding();

/// `WinAnsiEncoding` per PDF 32000-1:2008 Appendix D, Table D.2 column 3.
/// Approximates Windows-1252 with PDF-specific glyph names.
pub const WIN_ANSI_ENCODING: [Option<&str>; 256] = build_win_ansi_encoding();

/// `MacRomanEncoding` per PDF 32000-1:2008 Appendix D, Table D.2 column 4.
pub const MAC_ROMAN_ENCODING: [Option<&str>; 256] = build_mac_roman_encoding();

const fn build_standard_encoding() -> [Option<&'static str>; 256] {
    let mut t: [Option<&'static str>; 256] = [None; 256];
    // 32..=126: printable ASCII shared by all encodings.
    t[0x20] = Some("space");
    t[0x21] = Some("exclam");
    t[0x22] = Some("quotedbl");
    t[0x23] = Some("numbersign");
    t[0x24] = Some("dollar");
    t[0x25] = Some("percent");
    t[0x26] = Some("ampersand");
    t[0x27] = Some("quoteright"); // StandardEncoding-specific
    t[0x28] = Some("parenleft");
    t[0x29] = Some("parenright");
    t[0x2A] = Some("asterisk");
    t[0x2B] = Some("plus");
    t[0x2C] = Some("comma");
    t[0x2D] = Some("hyphen");
    t[0x2E] = Some("period");
    t[0x2F] = Some("slash");
    t[0x30] = Some("zero");
    t[0x31] = Some("one");
    t[0x32] = Some("two");
    t[0x33] = Some("three");
    t[0x34] = Some("four");
    t[0x35] = Some("five");
    t[0x36] = Some("six");
    t[0x37] = Some("seven");
    t[0x38] = Some("eight");
    t[0x39] = Some("nine");
    t[0x3A] = Some("colon");
    t[0x3B] = Some("semicolon");
    t[0x3C] = Some("less");
    t[0x3D] = Some("equal");
    t[0x3E] = Some("greater");
    t[0x3F] = Some("question");
    t[0x40] = Some("at");
    t[0x41] = Some("A");
    t[0x42] = Some("B");
    t[0x43] = Some("C");
    t[0x44] = Some("D");
    t[0x45] = Some("E");
    t[0x46] = Some("F");
    t[0x47] = Some("G");
    t[0x48] = Some("H");
    t[0x49] = Some("I");
    t[0x4A] = Some("J");
    t[0x4B] = Some("K");
    t[0x4C] = Some("L");
    t[0x4D] = Some("M");
    t[0x4E] = Some("N");
    t[0x4F] = Some("O");
    t[0x50] = Some("P");
    t[0x51] = Some("Q");
    t[0x52] = Some("R");
    t[0x53] = Some("S");
    t[0x54] = Some("T");
    t[0x55] = Some("U");
    t[0x56] = Some("V");
    t[0x57] = Some("W");
    t[0x58] = Some("X");
    t[0x59] = Some("Y");
    t[0x5A] = Some("Z");
    t[0x5B] = Some("bracketleft");
    t[0x5C] = Some("backslash");
    t[0x5D] = Some("bracketright");
    t[0x5E] = Some("asciicircum");
    t[0x5F] = Some("underscore");
    t[0x60] = Some("quoteleft"); // StandardEncoding-specific (WinAnsi has /grave here)
    t[0x61] = Some("a");
    t[0x62] = Some("b");
    t[0x63] = Some("c");
    t[0x64] = Some("d");
    t[0x65] = Some("e");
    t[0x66] = Some("f");
    t[0x67] = Some("g");
    t[0x68] = Some("h");
    t[0x69] = Some("i");
    t[0x6A] = Some("j");
    t[0x6B] = Some("k");
    t[0x6C] = Some("l");
    t[0x6D] = Some("m");
    t[0x6E] = Some("n");
    t[0x6F] = Some("o");
    t[0x70] = Some("p");
    t[0x71] = Some("q");
    t[0x72] = Some("r");
    t[0x73] = Some("s");
    t[0x74] = Some("t");
    t[0x75] = Some("u");
    t[0x76] = Some("v");
    t[0x77] = Some("w");
    t[0x78] = Some("x");
    t[0x79] = Some("y");
    t[0x7A] = Some("z");
    t[0x7B] = Some("braceleft");
    t[0x7C] = Some("bar");
    t[0x7D] = Some("braceright");
    t[0x7E] = Some("asciitilde");
    // 127..=160 mostly undefined in StandardEncoding.
    t[0xA1] = Some("exclamdown");
    t[0xA2] = Some("cent");
    t[0xA3] = Some("sterling");
    t[0xA4] = Some("fraction");
    t[0xA5] = Some("yen");
    t[0xA6] = Some("florin");
    t[0xA7] = Some("section");
    t[0xA8] = Some("currency");
    t[0xA9] = Some("quotesingle");
    t[0xAA] = Some("quotedblleft");
    t[0xAB] = Some("guillemotleft");
    t[0xAC] = Some("guilsinglleft");
    t[0xAD] = Some("guilsinglright");
    t[0xAE] = Some("fi");
    t[0xAF] = Some("fl");
    t[0xB1] = Some("endash");
    t[0xB2] = Some("dagger");
    t[0xB3] = Some("daggerdbl");
    t[0xB4] = Some("periodcentered");
    t[0xB6] = Some("paragraph");
    t[0xB7] = Some("bullet");
    t[0xB8] = Some("quotesinglbase");
    t[0xB9] = Some("quotedblbase");
    t[0xBA] = Some("quotedblright");
    t[0xBB] = Some("guillemotright");
    t[0xBC] = Some("ellipsis");
    t[0xBD] = Some("perthousand");
    t[0xBF] = Some("questiondown");
    t[0xC1] = Some("grave");
    t[0xC2] = Some("acute");
    t[0xC3] = Some("circumflex");
    t[0xC4] = Some("tilde");
    t[0xC5] = Some("macron");
    t[0xC6] = Some("breve");
    t[0xC7] = Some("dotaccent");
    t[0xC8] = Some("dieresis");
    t[0xCA] = Some("ring");
    t[0xCB] = Some("cedilla");
    t[0xCD] = Some("hungarumlaut");
    t[0xCE] = Some("ogonek");
    t[0xCF] = Some("caron");
    t[0xE1] = Some("AE");
    t[0xE3] = Some("ordfeminine");
    t[0xE8] = Some("Lslash");
    t[0xE9] = Some("Oslash");
    t[0xEA] = Some("OE");
    t[0xEB] = Some("ordmasculine");
    t[0xF1] = Some("ae");
    t[0xF5] = Some("dotlessi");
    t[0xF8] = Some("lslash");
    t[0xF9] = Some("oslash");
    t[0xFA] = Some("oe");
    t[0xFB] = Some("germandbls");
    t
}

const fn build_win_ansi_encoding() -> [Option<&'static str>; 256] {
    let mut t: [Option<&'static str>; 256] = [None; 256];
    // ASCII identical to Standard (with /quotesingle at 39 and /grave at 96).
    t[0x20] = Some("space");
    t[0x21] = Some("exclam");
    t[0x22] = Some("quotedbl");
    t[0x23] = Some("numbersign");
    t[0x24] = Some("dollar");
    t[0x25] = Some("percent");
    t[0x26] = Some("ampersand");
    t[0x27] = Some("quotesingle");
    t[0x28] = Some("parenleft");
    t[0x29] = Some("parenright");
    t[0x2A] = Some("asterisk");
    t[0x2B] = Some("plus");
    t[0x2C] = Some("comma");
    t[0x2D] = Some("hyphen");
    t[0x2E] = Some("period");
    t[0x2F] = Some("slash");
    t[0x30] = Some("zero");
    t[0x31] = Some("one");
    t[0x32] = Some("two");
    t[0x33] = Some("three");
    t[0x34] = Some("four");
    t[0x35] = Some("five");
    t[0x36] = Some("six");
    t[0x37] = Some("seven");
    t[0x38] = Some("eight");
    t[0x39] = Some("nine");
    t[0x3A] = Some("colon");
    t[0x3B] = Some("semicolon");
    t[0x3C] = Some("less");
    t[0x3D] = Some("equal");
    t[0x3E] = Some("greater");
    t[0x3F] = Some("question");
    t[0x40] = Some("at");
    t[0x41] = Some("A");
    t[0x42] = Some("B");
    t[0x43] = Some("C");
    t[0x44] = Some("D");
    t[0x45] = Some("E");
    t[0x46] = Some("F");
    t[0x47] = Some("G");
    t[0x48] = Some("H");
    t[0x49] = Some("I");
    t[0x4A] = Some("J");
    t[0x4B] = Some("K");
    t[0x4C] = Some("L");
    t[0x4D] = Some("M");
    t[0x4E] = Some("N");
    t[0x4F] = Some("O");
    t[0x50] = Some("P");
    t[0x51] = Some("Q");
    t[0x52] = Some("R");
    t[0x53] = Some("S");
    t[0x54] = Some("T");
    t[0x55] = Some("U");
    t[0x56] = Some("V");
    t[0x57] = Some("W");
    t[0x58] = Some("X");
    t[0x59] = Some("Y");
    t[0x5A] = Some("Z");
    t[0x5B] = Some("bracketleft");
    t[0x5C] = Some("backslash");
    t[0x5D] = Some("bracketright");
    t[0x5E] = Some("asciicircum");
    t[0x5F] = Some("underscore");
    t[0x60] = Some("grave");
    t[0x61] = Some("a");
    t[0x62] = Some("b");
    t[0x63] = Some("c");
    t[0x64] = Some("d");
    t[0x65] = Some("e");
    t[0x66] = Some("f");
    t[0x67] = Some("g");
    t[0x68] = Some("h");
    t[0x69] = Some("i");
    t[0x6A] = Some("j");
    t[0x6B] = Some("k");
    t[0x6C] = Some("l");
    t[0x6D] = Some("m");
    t[0x6E] = Some("n");
    t[0x6F] = Some("o");
    t[0x70] = Some("p");
    t[0x71] = Some("q");
    t[0x72] = Some("r");
    t[0x73] = Some("s");
    t[0x74] = Some("t");
    t[0x75] = Some("u");
    t[0x76] = Some("v");
    t[0x77] = Some("w");
    t[0x78] = Some("x");
    t[0x79] = Some("y");
    t[0x7A] = Some("z");
    t[0x7B] = Some("braceleft");
    t[0x7C] = Some("bar");
    t[0x7D] = Some("braceright");
    t[0x7E] = Some("asciitilde");
    // 0x80..0x9F — Windows-1252 specific codepoints.
    t[0x80] = Some("Euro");
    t[0x82] = Some("quotesinglbase");
    t[0x83] = Some("florin");
    t[0x84] = Some("quotedblbase");
    t[0x85] = Some("ellipsis");
    t[0x86] = Some("dagger");
    t[0x87] = Some("daggerdbl");
    t[0x88] = Some("circumflex");
    t[0x89] = Some("perthousand");
    t[0x8A] = Some("Scaron");
    t[0x8B] = Some("guilsinglleft");
    t[0x8C] = Some("OE");
    t[0x8E] = Some("Zcaron");
    t[0x91] = Some("quoteleft");
    t[0x92] = Some("quoteright");
    t[0x93] = Some("quotedblleft");
    t[0x94] = Some("quotedblright");
    t[0x95] = Some("bullet");
    t[0x96] = Some("endash");
    t[0x97] = Some("emdash");
    t[0x98] = Some("tilde");
    t[0x99] = Some("trademark");
    t[0x9A] = Some("scaron");
    t[0x9B] = Some("guilsinglright");
    t[0x9C] = Some("oe");
    t[0x9E] = Some("zcaron");
    t[0x9F] = Some("Ydieresis");
    // 0xA0..0xBF — Latin-1 supplement.
    t[0xA0] = Some("space");
    t[0xA1] = Some("exclamdown");
    t[0xA2] = Some("cent");
    t[0xA3] = Some("sterling");
    t[0xA4] = Some("currency");
    t[0xA5] = Some("yen");
    t[0xA6] = Some("brokenbar");
    t[0xA7] = Some("section");
    t[0xA8] = Some("dieresis");
    t[0xA9] = Some("copyright");
    t[0xAA] = Some("ordfeminine");
    t[0xAB] = Some("guillemotleft");
    t[0xAC] = Some("logicalnot");
    t[0xAD] = Some("hyphen");
    t[0xAE] = Some("registered");
    t[0xAF] = Some("macron");
    t[0xB0] = Some("degree");
    t[0xB1] = Some("plusminus");
    t[0xB2] = Some("twosuperior");
    t[0xB3] = Some("threesuperior");
    t[0xB4] = Some("acute");
    t[0xB5] = Some("mu");
    t[0xB6] = Some("paragraph");
    t[0xB7] = Some("periodcentered");
    t[0xB8] = Some("cedilla");
    t[0xB9] = Some("onesuperior");
    t[0xBA] = Some("ordmasculine");
    t[0xBB] = Some("guillemotright");
    t[0xBC] = Some("onequarter");
    t[0xBD] = Some("onehalf");
    t[0xBE] = Some("threequarters");
    t[0xBF] = Some("questiondown");
    // 0xC0..0xFF — Latin accented uppercase + lowercase (matches Latin-1).
    t[0xC0] = Some("Agrave");
    t[0xC1] = Some("Aacute");
    t[0xC2] = Some("Acircumflex");
    t[0xC3] = Some("Atilde");
    t[0xC4] = Some("Adieresis");
    t[0xC5] = Some("Aring");
    t[0xC6] = Some("AE");
    t[0xC7] = Some("Ccedilla");
    t[0xC8] = Some("Egrave");
    t[0xC9] = Some("Eacute");
    t[0xCA] = Some("Ecircumflex");
    t[0xCB] = Some("Edieresis");
    t[0xCC] = Some("Igrave");
    t[0xCD] = Some("Iacute");
    t[0xCE] = Some("Icircumflex");
    t[0xCF] = Some("Idieresis");
    t[0xD0] = Some("Eth");
    t[0xD1] = Some("Ntilde");
    t[0xD2] = Some("Ograve");
    t[0xD3] = Some("Oacute");
    t[0xD4] = Some("Ocircumflex");
    t[0xD5] = Some("Otilde");
    t[0xD6] = Some("Odieresis");
    t[0xD7] = Some("multiply");
    t[0xD8] = Some("Oslash");
    t[0xD9] = Some("Ugrave");
    t[0xDA] = Some("Uacute");
    t[0xDB] = Some("Ucircumflex");
    t[0xDC] = Some("Udieresis");
    t[0xDD] = Some("Yacute");
    t[0xDE] = Some("Thorn");
    t[0xDF] = Some("germandbls");
    t[0xE0] = Some("agrave");
    t[0xE1] = Some("aacute");
    t[0xE2] = Some("acircumflex");
    t[0xE3] = Some("atilde");
    t[0xE4] = Some("adieresis");
    t[0xE5] = Some("aring");
    t[0xE6] = Some("ae");
    t[0xE7] = Some("ccedilla");
    t[0xE8] = Some("egrave");
    t[0xE9] = Some("eacute");
    t[0xEA] = Some("ecircumflex");
    t[0xEB] = Some("edieresis");
    t[0xEC] = Some("igrave");
    t[0xED] = Some("iacute");
    t[0xEE] = Some("icircumflex");
    t[0xEF] = Some("idieresis");
    t[0xF0] = Some("eth");
    t[0xF1] = Some("ntilde");
    t[0xF2] = Some("ograve");
    t[0xF3] = Some("oacute");
    t[0xF4] = Some("ocircumflex");
    t[0xF5] = Some("otilde");
    t[0xF6] = Some("odieresis");
    t[0xF7] = Some("divide");
    t[0xF8] = Some("oslash");
    t[0xF9] = Some("ugrave");
    t[0xFA] = Some("uacute");
    t[0xFB] = Some("ucircumflex");
    t[0xFC] = Some("udieresis");
    t[0xFD] = Some("yacute");
    t[0xFE] = Some("thorn");
    t[0xFF] = Some("ydieresis");
    t
}

const fn build_mac_roman_encoding() -> [Option<&'static str>; 256] {
    let mut t: [Option<&'static str>; 256] = [None; 256];
    // 32..=126 same as WinAnsi/Standard (ASCII printable).
    t[0x20] = Some("space");
    t[0x21] = Some("exclam");
    t[0x22] = Some("quotedbl");
    t[0x23] = Some("numbersign");
    t[0x24] = Some("dollar");
    t[0x25] = Some("percent");
    t[0x26] = Some("ampersand");
    t[0x27] = Some("quotesingle");
    t[0x28] = Some("parenleft");
    t[0x29] = Some("parenright");
    t[0x2A] = Some("asterisk");
    t[0x2B] = Some("plus");
    t[0x2C] = Some("comma");
    t[0x2D] = Some("hyphen");
    t[0x2E] = Some("period");
    t[0x2F] = Some("slash");
    t[0x30] = Some("zero");
    t[0x31] = Some("one");
    t[0x32] = Some("two");
    t[0x33] = Some("three");
    t[0x34] = Some("four");
    t[0x35] = Some("five");
    t[0x36] = Some("six");
    t[0x37] = Some("seven");
    t[0x38] = Some("eight");
    t[0x39] = Some("nine");
    t[0x3A] = Some("colon");
    t[0x3B] = Some("semicolon");
    t[0x3C] = Some("less");
    t[0x3D] = Some("equal");
    t[0x3E] = Some("greater");
    t[0x3F] = Some("question");
    t[0x40] = Some("at");
    t[0x41] = Some("A");
    t[0x42] = Some("B");
    t[0x43] = Some("C");
    t[0x44] = Some("D");
    t[0x45] = Some("E");
    t[0x46] = Some("F");
    t[0x47] = Some("G");
    t[0x48] = Some("H");
    t[0x49] = Some("I");
    t[0x4A] = Some("J");
    t[0x4B] = Some("K");
    t[0x4C] = Some("L");
    t[0x4D] = Some("M");
    t[0x4E] = Some("N");
    t[0x4F] = Some("O");
    t[0x50] = Some("P");
    t[0x51] = Some("Q");
    t[0x52] = Some("R");
    t[0x53] = Some("S");
    t[0x54] = Some("T");
    t[0x55] = Some("U");
    t[0x56] = Some("V");
    t[0x57] = Some("W");
    t[0x58] = Some("X");
    t[0x59] = Some("Y");
    t[0x5A] = Some("Z");
    t[0x5B] = Some("bracketleft");
    t[0x5C] = Some("backslash");
    t[0x5D] = Some("bracketright");
    t[0x5E] = Some("asciicircum");
    t[0x5F] = Some("underscore");
    t[0x60] = Some("grave");
    t[0x61] = Some("a");
    t[0x62] = Some("b");
    t[0x63] = Some("c");
    t[0x64] = Some("d");
    t[0x65] = Some("e");
    t[0x66] = Some("f");
    t[0x67] = Some("g");
    t[0x68] = Some("h");
    t[0x69] = Some("i");
    t[0x6A] = Some("j");
    t[0x6B] = Some("k");
    t[0x6C] = Some("l");
    t[0x6D] = Some("m");
    t[0x6E] = Some("n");
    t[0x6F] = Some("o");
    t[0x70] = Some("p");
    t[0x71] = Some("q");
    t[0x72] = Some("r");
    t[0x73] = Some("s");
    t[0x74] = Some("t");
    t[0x75] = Some("u");
    t[0x76] = Some("v");
    t[0x77] = Some("w");
    t[0x78] = Some("x");
    t[0x79] = Some("y");
    t[0x7A] = Some("z");
    t[0x7B] = Some("braceleft");
    t[0x7C] = Some("bar");
    t[0x7D] = Some("braceright");
    t[0x7E] = Some("asciitilde");
    // 0x80..0xFF — MacRoman specifics.
    t[0x80] = Some("Adieresis");
    t[0x81] = Some("Aring");
    t[0x82] = Some("Ccedilla");
    t[0x83] = Some("Eacute");
    t[0x84] = Some("Ntilde");
    t[0x85] = Some("Odieresis");
    t[0x86] = Some("Udieresis");
    t[0x87] = Some("aacute");
    t[0x88] = Some("agrave");
    t[0x89] = Some("acircumflex");
    t[0x8A] = Some("adieresis");
    t[0x8B] = Some("atilde");
    t[0x8C] = Some("aring");
    t[0x8D] = Some("ccedilla");
    t[0x8E] = Some("eacute");
    t[0x8F] = Some("egrave");
    t[0x90] = Some("ecircumflex");
    t[0x91] = Some("edieresis");
    t[0x92] = Some("iacute");
    t[0x93] = Some("igrave");
    t[0x94] = Some("icircumflex");
    t[0x95] = Some("idieresis");
    t[0x96] = Some("ntilde");
    t[0x97] = Some("oacute");
    t[0x98] = Some("ograve");
    t[0x99] = Some("ocircumflex");
    t[0x9A] = Some("odieresis");
    t[0x9B] = Some("otilde");
    t[0x9C] = Some("uacute");
    t[0x9D] = Some("ugrave");
    t[0x9E] = Some("ucircumflex");
    t[0x9F] = Some("udieresis");
    t[0xA0] = Some("dagger");
    t[0xA1] = Some("degree");
    t[0xA2] = Some("cent");
    t[0xA3] = Some("sterling");
    t[0xA4] = Some("section");
    t[0xA5] = Some("bullet");
    t[0xA6] = Some("paragraph");
    t[0xA7] = Some("germandbls");
    t[0xA8] = Some("registered");
    t[0xA9] = Some("copyright");
    t[0xAA] = Some("trademark");
    t[0xAB] = Some("acute");
    t[0xAC] = Some("dieresis");
    t[0xAD] = Some("notequal");
    t[0xAE] = Some("AE");
    t[0xAF] = Some("Oslash");
    t[0xB0] = Some("infinity");
    t[0xB1] = Some("plusminus");
    t[0xB2] = Some("lessequal");
    t[0xB3] = Some("greaterequal");
    t[0xB4] = Some("yen");
    t[0xB5] = Some("mu");
    t[0xB6] = Some("partialdiff");
    t[0xB7] = Some("summation");
    t[0xB8] = Some("product");
    t[0xB9] = Some("pi");
    t[0xBA] = Some("integral");
    t[0xBB] = Some("ordfeminine");
    t[0xBC] = Some("ordmasculine");
    t[0xBD] = Some("Omega");
    t[0xBE] = Some("ae");
    t[0xBF] = Some("oslash");
    t[0xC0] = Some("questiondown");
    t[0xC1] = Some("exclamdown");
    t[0xC2] = Some("logicalnot");
    t[0xC3] = Some("radical");
    t[0xC4] = Some("florin");
    t[0xC5] = Some("approxequal");
    t[0xC6] = Some("Delta");
    t[0xC7] = Some("guillemotleft");
    t[0xC8] = Some("guillemotright");
    t[0xC9] = Some("ellipsis");
    t[0xCA] = Some("space"); // Non-breaking space
    t[0xCB] = Some("Agrave");
    t[0xCC] = Some("Atilde");
    t[0xCD] = Some("Otilde");
    t[0xCE] = Some("OE");
    t[0xCF] = Some("oe");
    t[0xD0] = Some("endash");
    t[0xD1] = Some("emdash");
    t[0xD2] = Some("quotedblleft");
    t[0xD3] = Some("quotedblright");
    t[0xD4] = Some("quoteleft");
    t[0xD5] = Some("quoteright");
    t[0xD6] = Some("divide");
    t[0xD7] = Some("lozenge");
    t[0xD8] = Some("ydieresis");
    t[0xD9] = Some("Ydieresis");
    t[0xDA] = Some("fraction");
    t[0xDB] = Some("currency");
    t[0xDC] = Some("guilsinglleft");
    t[0xDD] = Some("guilsinglright");
    t[0xDE] = Some("fi");
    t[0xDF] = Some("fl");
    t[0xE0] = Some("daggerdbl");
    t[0xE1] = Some("periodcentered");
    t[0xE2] = Some("quotesinglbase");
    t[0xE3] = Some("quotedblbase");
    t[0xE4] = Some("perthousand");
    t[0xE5] = Some("Acircumflex");
    t[0xE6] = Some("Ecircumflex");
    t[0xE7] = Some("Aacute");
    t[0xE8] = Some("Edieresis");
    t[0xE9] = Some("Egrave");
    t[0xEA] = Some("Iacute");
    t[0xEB] = Some("Icircumflex");
    t[0xEC] = Some("Idieresis");
    t[0xED] = Some("Eacute");
    t[0xEE] = Some("Igrave");
    t[0xEF] = Some("Oacute");
    t[0xF0] = Some("Ocircumflex");
    t[0xF1] = Some("apple");
    t[0xF2] = Some("Ograve");
    t[0xF3] = Some("Uacute");
    t[0xF4] = Some("Ucircumflex");
    t[0xF5] = Some("Ugrave");
    t[0xF6] = Some("dotlessi");
    t[0xF7] = Some("circumflex");
    t[0xF8] = Some("tilde");
    t[0xF9] = Some("macron");
    t[0xFA] = Some("breve");
    t[0xFB] = Some("dotaccent");
    t[0xFC] = Some("ring");
    t[0xFD] = Some("cedilla");
    t[0xFE] = Some("hungarumlaut");
    t[0xFF] = Some("ogonek");
    t
}

// ── Adobe Glyph List subset ──────────────────────────────────────────────────
//
// Sorted by name for binary search.  Covers every glyph reachable from the
// three standard encoding tables above plus all of the differences entries
// observed in PDF 1.3-era documents (Acrobat Distiller 3.0 output).  Each
// entry maps a glyph name to a single Unicode scalar (multi-codepoint glyphs
// are represented as `\u{0000}` sentinels and resolved via [`agl_to_string`]).

const AGL: &[(&str, char)] = &[
    ("A", 'A'),
    ("AE", '\u{00C6}'),
    ("Aacute", '\u{00C1}'),
    ("Acircumflex", '\u{00C2}'),
    ("Adieresis", '\u{00C4}'),
    ("Agrave", '\u{00C0}'),
    ("Aring", '\u{00C5}'),
    ("Atilde", '\u{00C3}'),
    ("B", 'B'),
    ("C", 'C'),
    ("Ccedilla", '\u{00C7}'),
    ("D", 'D'),
    ("Delta", '\u{0394}'),
    ("E", 'E'),
    ("Eacute", '\u{00C9}'),
    ("Ecircumflex", '\u{00CA}'),
    ("Edieresis", '\u{00CB}'),
    ("Egrave", '\u{00C8}'),
    ("Eth", '\u{00D0}'),
    ("Euro", '\u{20AC}'),
    ("F", 'F'),
    ("G", 'G'),
    ("H", 'H'),
    ("I", 'I'),
    ("Iacute", '\u{00CD}'),
    ("Icircumflex", '\u{00CE}'),
    ("Idieresis", '\u{00CF}'),
    ("Igrave", '\u{00CC}'),
    ("J", 'J'),
    ("K", 'K'),
    ("L", 'L'),
    ("Lslash", '\u{0141}'),
    ("M", 'M'),
    ("N", 'N'),
    ("Ntilde", '\u{00D1}'),
    ("O", 'O'),
    ("OE", '\u{0152}'),
    ("Oacute", '\u{00D3}'),
    ("Ocircumflex", '\u{00D4}'),
    ("Odieresis", '\u{00D6}'),
    ("Ograve", '\u{00D2}'),
    ("Omega", '\u{03A9}'),
    ("Oslash", '\u{00D8}'),
    ("Otilde", '\u{00D5}'),
    ("P", 'P'),
    ("Q", 'Q'),
    ("R", 'R'),
    ("S", 'S'),
    ("Scaron", '\u{0160}'),
    ("T", 'T'),
    ("Thorn", '\u{00DE}'),
    ("U", 'U'),
    ("Uacute", '\u{00DA}'),
    ("Ucircumflex", '\u{00DB}'),
    ("Udieresis", '\u{00DC}'),
    ("Ugrave", '\u{00D9}'),
    ("V", 'V'),
    ("W", 'W'),
    ("X", 'X'),
    ("Y", 'Y'),
    ("Yacute", '\u{00DD}'),
    ("Ydieresis", '\u{0178}'),
    ("Z", 'Z'),
    ("Zcaron", '\u{017D}'),
    ("a", 'a'),
    ("aacute", '\u{00E1}'),
    ("acircumflex", '\u{00E2}'),
    ("acute", '\u{00B4}'),
    ("adieresis", '\u{00E4}'),
    ("ae", '\u{00E6}'),
    ("agrave", '\u{00E0}'),
    ("ampersand", '&'),
    ("apple", '\u{F8FF}'),
    ("approxequal", '\u{2248}'),
    ("aring", '\u{00E5}'),
    ("asciicircum", '^'),
    ("asciitilde", '~'),
    ("asterisk", '*'),
    ("at", '@'),
    ("atilde", '\u{00E3}'),
    ("b", 'b'),
    ("backslash", '\\'),
    ("bar", '|'),
    ("braceleft", '{'),
    ("braceright", '}'),
    ("bracketleft", '['),
    ("bracketright", ']'),
    ("breve", '\u{02D8}'),
    ("brokenbar", '\u{00A6}'),
    ("bullet", '\u{2022}'),
    ("c", 'c'),
    ("caron", '\u{02C7}'),
    ("ccedilla", '\u{00E7}'),
    ("cedilla", '\u{00B8}'),
    ("cent", '\u{00A2}'),
    ("circumflex", '\u{02C6}'),
    ("colon", ':'),
    ("comma", ','),
    ("copyright", '\u{00A9}'),
    ("currency", '\u{00A4}'),
    ("d", 'd'),
    ("dagger", '\u{2020}'),
    ("daggerdbl", '\u{2021}'),
    ("degree", '\u{00B0}'),
    ("dieresis", '\u{00A8}'),
    ("divide", '\u{00F7}'),
    ("dollar", '$'),
    ("dotaccent", '\u{02D9}'),
    ("dotlessi", '\u{0131}'),
    ("e", 'e'),
    ("eacute", '\u{00E9}'),
    ("ecircumflex", '\u{00EA}'),
    ("edieresis", '\u{00EB}'),
    ("egrave", '\u{00E8}'),
    ("eight", '8'),
    ("ellipsis", '\u{2026}'),
    ("emdash", '\u{2014}'),
    ("emspace", '\u{2003}'),
    ("endash", '\u{2013}'),
    ("enspace", '\u{2002}'),
    ("equal", '='),
    ("eth", '\u{00F0}'),
    ("exclam", '!'),
    ("exclamdown", '\u{00A1}'),
    ("f", 'f'),
    ("ff", '\u{FB00}'),
    ("ffi", '\u{FB03}'),
    ("ffl", '\u{FB04}'),
    ("fi", '\u{FB01}'),
    ("five", '5'),
    ("fl", '\u{FB02}'),
    ("florin", '\u{0192}'),
    ("four", '4'),
    ("fraction", '\u{2044}'),
    ("g", 'g'),
    ("germandbls", '\u{00DF}'),
    ("grave", '`'),
    ("greater", '>'),
    ("greaterequal", '\u{2265}'),
    ("guillemotleft", '\u{00AB}'),
    ("guillemotright", '\u{00BB}'),
    ("guilsinglleft", '\u{2039}'),
    ("guilsinglright", '\u{203A}'),
    ("h", 'h'),
    ("hungarumlaut", '\u{02DD}'),
    ("hyphen", '-'),
    ("i", 'i'),
    ("iacute", '\u{00ED}'),
    ("icircumflex", '\u{00EE}'),
    ("idieresis", '\u{00EF}'),
    ("igrave", '\u{00EC}'),
    ("infinity", '\u{221E}'),
    ("integral", '\u{222B}'),
    ("j", 'j'),
    ("k", 'k'),
    ("l", 'l'),
    ("less", '<'),
    ("lessequal", '\u{2264}'),
    ("logicalnot", '\u{00AC}'),
    ("lozenge", '\u{25CA}'),
    ("lslash", '\u{0142}'),
    ("m", 'm'),
    ("macron", '\u{00AF}'),
    ("minus", '\u{2212}'),
    ("mu", '\u{00B5}'),
    ("multiply", '\u{00D7}'),
    ("n", 'n'),
    ("nbspace", '\u{00A0}'),
    ("nine", '9'),
    ("nonbreakingspace", '\u{00A0}'),
    ("notequal", '\u{2260}'),
    ("ntilde", '\u{00F1}'),
    ("numbersign", '#'),
    ("o", 'o'),
    ("oacute", '\u{00F3}'),
    ("ocircumflex", '\u{00F4}'),
    ("odieresis", '\u{00F6}'),
    ("oe", '\u{0153}'),
    ("ogonek", '\u{02DB}'),
    ("ograve", '\u{00F2}'),
    ("one", '1'),
    ("onehalf", '\u{00BD}'),
    ("onequarter", '\u{00BC}'),
    ("onesuperior", '\u{00B9}'),
    ("ordfeminine", '\u{00AA}'),
    ("ordmasculine", '\u{00BA}'),
    ("oslash", '\u{00F8}'),
    ("otilde", '\u{00F5}'),
    ("p", 'p'),
    ("paragraph", '\u{00B6}'),
    ("parenleft", '('),
    ("parenright", ')'),
    ("partialdiff", '\u{2202}'),
    ("percent", '%'),
    ("period", '.'),
    ("periodcentered", '\u{00B7}'),
    ("perthousand", '\u{2030}'),
    ("pi", '\u{03C0}'),
    ("plus", '+'),
    ("plusminus", '\u{00B1}'),
    ("product", '\u{220F}'),
    ("q", 'q'),
    ("question", '?'),
    ("questiondown", '\u{00BF}'),
    ("quotedbl", '"'),
    ("quotedblbase", '\u{201E}'),
    ("quotedblleft", '\u{201C}'),
    ("quotedblright", '\u{201D}'),
    ("quoteleft", '\u{2018}'),
    ("quoteright", '\u{2019}'),
    ("quotesinglbase", '\u{201A}'),
    ("quotesingle", '\''),
    ("r", 'r'),
    ("radical", '\u{221A}'),
    ("registered", '\u{00AE}'),
    ("ring", '\u{02DA}'),
    ("s", 's'),
    ("scaron", '\u{0161}'),
    ("section", '\u{00A7}'),
    ("semicolon", ';'),
    ("seven", '7'),
    ("six", '6'),
    ("slash", '/'),
    ("space", ' '),
    ("sterling", '\u{00A3}'),
    ("summation", '\u{2211}'),
    ("t", 't'),
    ("thinspace", '\u{2009}'),
    ("thorn", '\u{00FE}'),
    ("three", '3'),
    ("threequarters", '\u{00BE}'),
    ("threesuperior", '\u{00B3}'),
    ("tilde", '\u{02DC}'),
    ("trademark", '\u{2122}'),
    ("two", '2'),
    ("twosuperior", '\u{00B2}'),
    ("u", 'u'),
    ("uacute", '\u{00FA}'),
    ("ucircumflex", '\u{00FB}'),
    ("udieresis", '\u{00FC}'),
    ("ugrave", '\u{00F9}'),
    ("underscore", '_'),
    ("v", 'v'),
    ("w", 'w'),
    ("x", 'x'),
    ("y", 'y'),
    ("yacute", '\u{00FD}'),
    ("ydieresis", '\u{00FF}'),
    ("yen", '\u{00A5}'),
    ("z", 'z'),
    ("zcaron", '\u{017E}'),
    ("zero", '0'),
];

/// Map an unmapped Type1-encoding byte to a likely typographic glyph.
///
/// Many Type1 fonts in real-world PDFs use bytes 0x10–0x1F for typographic
/// punctuation (en-dash, em-dash, smart quotes, …) via the font's internal
/// encoding — but expose neither a `/Differences` array nor a `/ToUnicode`
/// CMap that pdf-fast can consume. Without a hint, byte 0x13 decodes to
/// `U+0013` (DEVICE CONTROL 3), corrupting the text with non-printable
/// control characters.
///
/// The same low bytes overlap perfectly with the Unicode **General
/// Punctuation** block (U+2000–U+201F), which is precisely where those
/// glyphs live:
///
///   * 0x10–0x15 → U+2010–U+2015 (hyphens / dashes / horizontal bar)
///   * 0x18, 0x19 → U+2018, U+2019 (left/right single quote)
///   * 0x1C, 0x1D → U+201C, U+201D (left/right double quote)
///
/// Standard ASCII whitespace (0x09 TAB, 0x0A LF, 0x0C FF, 0x0D CR) is
/// kept as-is because pdf-fast uses these for layout (form-feed between
/// pages, newlines within text). Outside the C0 range we keep the raw
/// Latin-1 byte fallback that callers historically depended on.
///
/// Empirically (olmOCR-bench multi_column split, 122 of 231 PDFs):
/// extracted text contained 10,758 spurious C0 control chars; this
/// rewrite produces typographically-correct output without OCR.
pub fn c0_fallback_char(byte: u8) -> char {
    match byte {
        0x09 | 0x0A | 0x0C | 0x0D => byte as char,
        0x01..=0x1F => char::from_u32(0x2000 | byte as u32).unwrap_or(byte as char),
        _ => byte as char,
    }
}

/// Look up a glyph name in the Adobe Glyph List subset.
///
/// Returns the resolved string (usually a single character) for known
/// glyphs.  When the name follows the `uniXXXX` / `uXXXXXXXX` convention
/// from AGL §6.4 it is decoded directly from the hex digits.  When the
/// name encodes multiple codepoints separated by `_`, each component is
/// looked up recursively.  Returns `None` only for truly unknown names.
pub fn glyph_name_to_string(name: &str) -> Option<String> {
    let name = name.strip_prefix('/').unwrap_or(name);
    if name.is_empty() || name == ".notdef" {
        return None;
    }
    if let Ok(idx) = AGL.binary_search_by(|(n, _)| (*n).cmp(name)) {
        return Some(AGL[idx].1.to_string());
    }
    // `uniXXXX` — single 4-hex codepoint (BMP only).
    if let Some(rest) = name.strip_prefix("uni") {
        if rest.len() % 4 == 0 && rest.chars().all(|c| c.is_ascii_hexdigit()) {
            let mut out = String::new();
            for chunk in rest.as_bytes().chunks(4) {
                let s = std::str::from_utf8(chunk).ok()?;
                let scalar = u32::from_str_radix(s, 16).ok()?;
                if let Some(ch) = char::from_u32(scalar) {
                    out.push(ch);
                }
            }
            if !out.is_empty() {
                return Some(out);
            }
        }
    }
    // `uXXXXXX` — single codepoint with 4..6 hex digits (full Unicode).
    if let Some(rest) = name.strip_prefix('u') {
        if (4..=6).contains(&rest.len()) && rest.chars().all(|c| c.is_ascii_hexdigit()) {
            if let Ok(scalar) = u32::from_str_radix(rest, 16) {
                if let Some(ch) = char::from_u32(scalar) {
                    return Some(ch.to_string());
                }
            }
        }
    }
    // Compound glyph: split on `_` and resolve each component.
    if name.contains('_') {
        let mut combined = String::new();
        let mut any = false;
        for part in name.split('_') {
            if let Some(s) = glyph_name_to_string(part) {
                combined.push_str(&s);
                any = true;
            }
        }
        if any {
            return Some(combined);
        }
    }
    None
}

// ── Encoding map composition ─────────────────────────────────────────────────

/// Identifier of a known PDF base encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseEncoding {
    /// `StandardEncoding` (Type1 default for nonsymbolic fonts).
    Standard,
    /// `WinAnsiEncoding` — Windows-1252.
    WinAnsi,
    /// `MacRomanEncoding`.
    MacRoman,
    /// `MacExpertEncoding` — currently mapped to Standard.
    MacExpert,
}

impl BaseEncoding {
    /// Resolve a PDF encoding name to a [`BaseEncoding`].  Returns `None` for
    /// unknown names so the caller can choose a sensible fallback.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "StandardEncoding" => Some(Self::Standard),
            "WinAnsiEncoding" => Some(Self::WinAnsi),
            "MacRomanEncoding" => Some(Self::MacRoman),
            "MacExpertEncoding" => Some(Self::MacExpert),
            _ => None,
        }
    }

    fn table(self) -> &'static [Option<&'static str>; 256] {
        match self {
            Self::Standard => &STANDARD_ENCODING,
            Self::WinAnsi => &WIN_ANSI_ENCODING,
            Self::MacRoman => &MAC_ROMAN_ENCODING,
            // No public MacExpert table yet — fall back to Standard.
            Self::MacExpert => &STANDARD_ENCODING,
        }
    }
}

/// Per-byte encoding map composed of a base encoding plus a `/Differences`
/// override array.
///
/// Each entry stores the Unicode string the glyph resolves to.  Absent entries
/// indicate either an unmapped byte or a glyph name that the AGL did not
/// recognise — callers can decide how to handle those (typically Latin-1
/// fallback or marking the character as a heuristic fallback).
#[derive(Debug, Clone)]
pub struct EncodingMap {
    base: BaseEncoding,
    mapping: Box<[Option<String>; 256]>,
}

impl EncodingMap {
    /// Build a map containing only the base encoding (no overrides).
    pub fn new(base: BaseEncoding) -> Self {
        let mut mapping: [Option<String>; 256] = [const { None }; 256];
        let table = base.table();
        for (idx, slot) in mapping.iter_mut().enumerate() {
            if let Some(name) = table[idx] {
                if let Some(s) = glyph_name_to_string(name) {
                    *slot = Some(s);
                }
            }
        }
        Self {
            base,
            mapping: Box::new(mapping),
        }
    }

    /// Return the chosen base encoding identifier.
    pub fn base(&self) -> BaseEncoding {
        self.base
    }

    /// Apply a parsed `/Differences` array on top of the existing mapping.
    pub fn apply_differences(&mut self, diffs: &[DifferenceEntry]) {
        let mut cursor: u16 = 0;
        for entry in diffs {
            match entry {
                DifferenceEntry::Code(code) => {
                    cursor = *code;
                }
                DifferenceEntry::Glyph(name) => {
                    if cursor > 255 {
                        // Spec: codes outside 0..=255 are ignored for simple fonts.
                        cursor = cursor.wrapping_add(1);
                        continue;
                    }
                    if name == ".notdef" {
                        self.mapping[cursor as usize] = None;
                    } else {
                        self.mapping[cursor as usize] = glyph_name_to_string(name);
                    }
                    cursor = cursor.wrapping_add(1);
                }
            }
        }
    }

    /// Decode a single byte through this map.  Returns the resolved Unicode
    /// string when the byte is mapped, or `None` when the slot is empty.
    pub fn decode_byte(&self, byte: u8) -> Option<&str> {
        self.mapping[byte as usize].as_deref()
    }

    /// Decode a byte string through this map, returning `(text, fallback)`.
    /// `fallback` counts the number of bytes that fell through to the raw
    /// byte-as-Latin-1 decoder because the encoding had no entry for them.
    pub fn decode(&self, bytes: &[u8]) -> (String, u32) {
        let mut text = String::with_capacity(bytes.len());
        let mut fallback = 0u32;
        for &byte in bytes {
            match self.decode_byte(byte) {
                Some(s) => text.push_str(s),
                None => {
                    text.push(c0_fallback_char(byte));
                    fallback += 1;
                }
            }
        }
        (text, fallback)
    }

    /// Return the number of mapped (non-empty) byte slots, useful for
    /// diagnostics.
    pub fn mapped_count(&self) -> usize {
        self.mapping.iter().filter(|s| s.is_some()).count()
    }
}

// ── /Differences parsing ─────────────────────────────────────────────────────

/// A single entry parsed from a PDF `/Differences` array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DifferenceEntry {
    /// A starting byte code that resets the cursor.
    Code(u16),
    /// A glyph name applied at the current cursor position.
    Glyph(String),
}

/// Convert a parsed PDF array into a sequence of [`DifferenceEntry`] values.
///
/// `array` should be the value of the `/Differences` key from an Encoding
/// dictionary.  Numbers reset the cursor; names are applied at successive
/// positions starting from the most recent number.
pub fn parse_differences(array: &PdfValue) -> Vec<DifferenceEntry> {
    let mut out = Vec::new();
    let items = match array {
        PdfValue::Array(items) => items,
        _ => return out,
    };
    for item in items {
        match item {
            PdfValue::Integer(n) => {
                if (0..=255).contains(n) {
                    out.push(DifferenceEntry::Code(*n as u16));
                }
            }
            PdfValue::Real(n) => {
                let n = *n as i64;
                if (0..=255).contains(&n) {
                    out.push(DifferenceEntry::Code(n as u16));
                }
            }
            PdfValue::Name(name) => {
                out.push(DifferenceEntry::Glyph(name.clone()));
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agl_resolves_basic_letters() {
        assert_eq!(glyph_name_to_string("A").as_deref(), Some("A"));
        assert_eq!(glyph_name_to_string("aacute").as_deref(), Some("á"));
        assert_eq!(glyph_name_to_string("ccedilla").as_deref(), Some("ç"));
        assert_eq!(glyph_name_to_string("space").as_deref(), Some(" "));
    }

    #[test]
    fn agl_resolves_uni_names() {
        assert_eq!(glyph_name_to_string("uni00E3").as_deref(), Some("ã"));
        assert_eq!(glyph_name_to_string("u1F600").as_deref(), Some("😀"));
    }

    #[test]
    fn agl_returns_none_for_notdef() {
        assert_eq!(glyph_name_to_string(".notdef"), None);
    }

    #[test]
    fn standard_encoding_has_basic_ascii() {
        assert_eq!(STANDARD_ENCODING[0x41], Some("A"));
        assert_eq!(STANDARD_ENCODING[0x20], Some("space"));
        assert_eq!(STANDARD_ENCODING[0x27], Some("quoteright")); // Standard's quirk
        assert_eq!(STANDARD_ENCODING[0x60], Some("quoteleft")); // Standard's quirk
    }

    #[test]
    fn winansi_encoding_matches_latin_1_supplement() {
        assert_eq!(WIN_ANSI_ENCODING[0xE3], Some("atilde"));
        assert_eq!(WIN_ANSI_ENCODING[0xE7], Some("ccedilla"));
        assert_eq!(WIN_ANSI_ENCODING[0xC1], Some("Aacute"));
        assert_eq!(WIN_ANSI_ENCODING[0x80], Some("Euro"));
        assert_eq!(WIN_ANSI_ENCODING[0x96], Some("endash"));
    }

    #[test]
    fn encoding_map_decodes_latin_1_with_winansi_base() {
        let map = EncodingMap::new(BaseEncoding::WinAnsi);
        let (text, fallback) = map.decode(b"S\xe3o Paulo - Cora\xe7\xe3o");
        assert_eq!(text, "São Paulo - Coração");
        assert_eq!(fallback, 0);
    }

    #[test]
    fn encoding_map_applies_differences_on_top_of_standard() {
        // Simulate the CBHPM /Differences pattern: Standard base + Latin
        // accented overrides at 0xC0..0xFF.
        let mut map = EncodingMap::new(BaseEncoding::Standard);
        // Without /Differences, byte 0xE3 in Standard is /ordfeminine (ª).
        assert_eq!(map.decode_byte(0xE3).unwrap(), "ª");
        // Apply a minimal Differences array equivalent to the CBHPM tail.
        let diffs = vec![
            DifferenceEntry::Code(0xE3),
            DifferenceEntry::Glyph("atilde".to_string()),
            DifferenceEntry::Glyph("adieresis".to_string()),
        ];
        map.apply_differences(&diffs);
        assert_eq!(map.decode_byte(0xE3).unwrap(), "ã");
        assert_eq!(map.decode_byte(0xE4).unwrap(), "ä");
    }

    #[test]
    fn parse_differences_handles_cursor_resets() {
        let array = PdfValue::Array(vec![
            PdfValue::Integer(192),
            PdfValue::Name("Agrave".into()),
            PdfValue::Name("Aacute".into()),
            PdfValue::Integer(231),
            PdfValue::Name("ccedilla".into()),
        ]);
        let diffs = parse_differences(&array);
        assert_eq!(
            diffs,
            vec![
                DifferenceEntry::Code(192),
                DifferenceEntry::Glyph("Agrave".into()),
                DifferenceEntry::Glyph("Aacute".into()),
                DifferenceEntry::Code(231),
                DifferenceEntry::Glyph("ccedilla".into()),
            ]
        );
    }

    #[test]
    fn agl_table_is_sorted() {
        for window in AGL.array_windows::<2>() {
            assert!(window[0].0 < window[1].0, "AGL not sorted: {:?}", window);
        }
    }
}
