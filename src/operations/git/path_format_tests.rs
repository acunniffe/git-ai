use super::*;

// =========================================================================
// unescape_git_path Tests
// =========================================================================

#[test]
fn test_unescape_git_path_simple() {
    // Unquoted path - no change
    assert_eq!(unescape_git_path("simple.txt"), "simple.txt");
    assert_eq!(unescape_git_path("path/to/file.rs"), "path/to/file.rs");
}

#[test]
fn test_unescape_git_path_quoted_with_spaces() {
    // Quoted path with spaces
    assert_eq!(
        unescape_git_path("\"path with spaces.txt\""),
        "path with spaces.txt"
    );
    assert_eq!(
        unescape_git_path("\"dir name/file name.txt\""),
        "dir name/file name.txt"
    );
}

#[test]
fn test_unescape_git_path_chinese_characters() {
    // Chinese characters "中文" encoded as octal: \344\270\255\346\226\207
    assert_eq!(
        unescape_git_path("\"\\344\\270\\255\\346\\226\\207.txt\""),
        "中文.txt"
    );

    // More complex Chinese filename: "中文文件.txt"
    // 中 = \344\270\255, 文 = \346\226\207, 件 = \344\273\266
    assert_eq!(
        unescape_git_path("\"\\344\\270\\255\\346\\226\\207\\346\\226\\207\\344\\273\\266.txt\""),
        "中文文件.txt"
    );
}

#[test]
fn test_unescape_git_path_emoji() {
    // Emoji "🚀" (rocket) = U+1F680 = \360\237\232\200 in octal UTF-8
    assert_eq!(unescape_git_path("\"\\360\\237\\232\\200.txt\""), "🚀.txt");

    // Emoji "😀" (grinning face) = U+1F600 = \360\237\230\200 in octal UTF-8
    assert_eq!(unescape_git_path("\"\\360\\237\\230\\200.txt\""), "😀.txt");

    // Mixed: "test_🎉_file.txt" where 🎉 = \360\237\216\211
    assert_eq!(
        unescape_git_path("\"test_\\360\\237\\216\\211_file.txt\""),
        "test_🎉_file.txt"
    );
}

#[test]
fn test_unescape_git_path_escaped_characters() {
    // Escaped backslash
    assert_eq!(
        unescape_git_path("\"path\\\\with\\\\slashes\""),
        "path\\with\\slashes"
    );

    // Escaped quotes
    assert_eq!(unescape_git_path("\"file\\\"name.txt\""), "file\"name.txt");

    // Escaped newline and tab
    assert_eq!(unescape_git_path("\"line1\\nline2\""), "line1\nline2");
    assert_eq!(unescape_git_path("\"col1\\tcol2\""), "col1\tcol2");
}

#[test]
fn test_unescape_git_path_mixed_content() {
    // Mix of ASCII, Chinese, and escapes
    assert_eq!(
        unescape_git_path("\"src/\\344\\270\\255\\346\\226\\207/file.txt\""),
        "src/中文/file.txt"
    );
}

// =========================================================================
// Phase 1: CJK Extended Coverage Tests
// =========================================================================

#[test]
fn test_unescape_japanese_hiragana() {
    // Japanese Hiragana "ひらがな" = \343\201\262\343\202\211\343\201\214\343\201\252
    assert_eq!(
        unescape_git_path("\"\\343\\201\\262\\343\\202\\211\\343\\201\\214\\343\\201\\252.txt\""),
        "ひらがな.txt"
    );
}

#[test]
fn test_unescape_japanese_katakana() {
    // Japanese Katakana "カタカナ" = \343\202\253\343\202\277\343\202\253\343\203\212
    assert_eq!(
        unescape_git_path("\"\\343\\202\\253\\343\\202\\277\\343\\202\\253\\343\\203\\212.txt\""),
        "カタカナ.txt"
    );
}

#[test]
fn test_unescape_korean_hangul() {
    // Korean Hangul "한글" = \355\225\234\352\270\200
    assert_eq!(
        unescape_git_path("\"\\355\\225\\234\\352\\270\\200.txt\""),
        "한글.txt"
    );
}

#[test]
fn test_unescape_traditional_chinese() {
    // Traditional Chinese "繁體" = \347\271\201\351\253\224
    assert_eq!(
        unescape_git_path("\"\\347\\271\\201\\351\\253\\224.txt\""),
        "繁體.txt"
    );
}

#[test]
fn test_unescape_mixed_cjk() {
    // Mixed CJK: "日中韓" (Japanese, Chinese, Korean characters mixed)
    // 日 = \346\227\245, 中 = \344\270\255, 韓 = \351\237\223
    assert_eq!(
        unescape_git_path("\"\\346\\227\\245\\344\\270\\255\\351\\237\\223.txt\""),
        "日中韓.txt"
    );
}

// =========================================================================
// Phase 2: RTL Scripts Tests (Arabic, Hebrew, Persian, Urdu)
// =========================================================================

#[test]
fn test_unescape_arabic() {
    // Arabic "مرحبا" (marhaba = hello)
    // م = \331\205, ر = \330\261, ح = \330\255, ب = \330\250, ا = \330\247
    assert_eq!(
        unescape_git_path("\"\\331\\205\\330\\261\\330\\255\\330\\250\\330\\247.txt\""),
        "مرحبا.txt"
    );
}

#[test]
fn test_unescape_hebrew() {
    // Hebrew "שלום" (shalom = hello/peace)
    // ש = \327\251, ל = \327\234, ו = \327\225, ם = \327\235
    assert_eq!(
        unescape_git_path("\"\\327\\251\\327\\234\\327\\225\\327\\235.txt\""),
        "שלום.txt"
    );
}

#[test]
fn test_unescape_persian() {
    // Persian "فارسی" (farsi)
    // ف = \331\201, ا = \330\247, ر = \330\261, س = \330\263, ی = \333\214
    assert_eq!(
        unescape_git_path("\"\\331\\201\\330\\247\\330\\261\\330\\263\\333\\214.txt\""),
        "فارسی.txt"
    );
}

#[test]
fn test_unescape_urdu() {
    // Urdu "اردو" (urdu)
    // ا = \330\247, ر = \330\261, د = \330\257, و = \331\210
    assert_eq!(
        unescape_git_path("\"\\330\\247\\330\\261\\330\\257\\331\\210.txt\""),
        "اردو.txt"
    );
}

#[test]
fn test_unescape_mixed_rtl_ltr() {
    // Mixed RTL/LTR: "test_مرحبا_file" (ASCII + Arabic + ASCII)
    assert_eq!(
        unescape_git_path("\"test_\\331\\205\\330\\261\\330\\255\\330\\250\\330\\247_file.txt\""),
        "test_مرحبا_file.txt"
    );
}

// =========================================================================
// Phase 3: Indic Scripts Tests (Hindi, Tamil, Bengali, Telugu, Gujarati)
// =========================================================================

#[test]
fn test_unescape_hindi_devanagari() {
    // Hindi "हिंदी" (Hindi in Devanagari script)
    // ह = \340\244\271, ि = \340\244\277, ं = \340\244\202, द = \340\244\246, ी = \340\245\200
    assert_eq!(
        unescape_git_path(
            "\"\\340\\244\\271\\340\\244\\277\\340\\244\\202\\340\\244\\246\\340\\245\\200.txt\""
        ),
        "हिंदी.txt"
    );
}

#[test]
fn test_unescape_tamil() {
    // Tamil "தமிழ்" (Tamil)
    // த = \340\256\244, ம = \340\256\256, ி = \340\256\277, ழ = \340\256\264, ் = \340\257\215
    assert_eq!(
        unescape_git_path(
            "\"\\340\\256\\244\\340\\256\\256\\340\\256\\277\\340\\256\\264\\340\\257\\215.txt\""
        ),
        "தமிழ்.txt"
    );
}

#[test]
fn test_unescape_bengali() {
    // Bengali "বাংলা" (Bangla)
    // ব = \340\246\254, া = \340\246\276, ং = \340\246\202, ল = \340\246\262, া = \340\246\276
    assert_eq!(
        unescape_git_path(
            "\"\\340\\246\\254\\340\\246\\276\\340\\246\\202\\340\\246\\262\\340\\246\\276.txt\""
        ),
        "বাংলা.txt"
    );
}

#[test]
fn test_unescape_telugu() {
    // Telugu "తెలుగు" (Telugu)
    // త = \340\260\244, ె = \340\261\206, ల = \340\260\262, ు = \340\261\201, గ = \340\260\227, ు = \340\261\201
    assert_eq!(
        unescape_git_path(
            "\"\\340\\260\\244\\340\\261\\206\\340\\260\\262\\340\\261\\201\\340\\260\\227\\340\\261\\201.txt\""
        ),
        "తెలుగు.txt"
    );
}

#[test]
fn test_unescape_gujarati() {
    // Gujarati "ગુજરાતી" (Gujarati)
    // ગ = \340\252\227, ુ = \340\253\201, જ = \340\252\234, ર = \340\252\260, ા = \340\252\276, ત = \340\252\244, ી = \340\253\200
    assert_eq!(
        unescape_git_path(
            "\"\\340\\252\\227\\340\\253\\201\\340\\252\\234\\340\\252\\260\\340\\252\\276\\340\\252\\244\\340\\253\\200.txt\""
        ),
        "ગુજરાતી.txt"
    );
}

// =========================================================================
// Phase 4: Southeast Asian Scripts Tests (Thai, Vietnamese, Khmer, Lao)
// =========================================================================

#[test]
fn test_unescape_thai() {
    // Thai "ไทย" (Thai)
    // ไ = \340\271\204, ท = \340\270\227, ย = \340\270\242
    assert_eq!(
        unescape_git_path("\"\\340\\271\\204\\340\\270\\227\\340\\270\\242.txt\""),
        "ไทย.txt"
    );
}

#[test]
fn test_unescape_vietnamese() {
    // Vietnamese "tiếng" with tone marks
    // t = 't', i = 'i', ế = \341\272\277, n = 'n', g = 'g'
    assert_eq!(
        unescape_git_path("\"ti\\341\\272\\277ng.txt\""),
        "tiếng.txt"
    );
}

#[test]
fn test_unescape_khmer() {
    // Khmer "ខ្មែរ" (Khmer)
    // ខ = \341\236\201, ្ = \341\237\222, ម = \341\236\230, ែ = \341\237\202, រ = \341\236\232
    assert_eq!(
        unescape_git_path(
            "\"\\341\\236\\201\\341\\237\\222\\341\\236\\230\\341\\237\\202\\341\\236\\232.txt\""
        ),
        "ខ្មែរ.txt"
    );
}

#[test]
fn test_unescape_lao() {
    // Lao "ລາວ" (Lao)
    // ລ = \340\272\245, າ = \340\272\262, ວ = \340\272\247
    assert_eq!(
        unescape_git_path("\"\\340\\272\\245\\340\\272\\262\\340\\272\\247.txt\""),
        "ລາວ.txt"
    );
}

// =========================================================================
// Phase 5: Cyrillic and Greek Scripts Tests
// =========================================================================

#[test]
fn test_unescape_russian_cyrillic() {
    // Russian "Русский" (Russian)
    // Р = \320\240, у = \321\203, с = \321\201, к = \320\272, и = \320\270, й = \320\271
    assert_eq!(
        unescape_git_path(
            "\"\\320\\240\\321\\203\\321\\201\\321\\201\\320\\272\\320\\270\\320\\271.txt\""
        ),
        "Русский.txt"
    );
}

#[test]
fn test_unescape_ukrainian_cyrillic() {
    // Ukrainian "Україна" (Ukraine)
    // У = \320\243, к = \320\272, р = \321\200, а = \320\260, ї = \321\227, н = \320\275, а = \320\260
    assert_eq!(
        unescape_git_path(
            "\"\\320\\243\\320\\272\\321\\200\\320\\260\\321\\227\\320\\275\\320\\260.txt\""
        ),
        "Україна.txt"
    );
}

#[test]
fn test_unescape_greek() {
    // Greek "Ελλάδα" (Greece)
    // Ε = \316\225, λ = \316\273, λ = \316\273, ά = \316\254, δ = \316\264, α = \316\261
    assert_eq!(
        unescape_git_path("\"\\316\\225\\316\\273\\316\\273\\316\\254\\316\\264\\316\\261.txt\""),
        "Ελλάδα.txt"
    );
}

#[test]
fn test_unescape_greek_polytonic() {
    // Greek polytonic "Ἑλληνική" (Hellenic with diacritics)
    // Ἑ = \341\274\231, λ = \316\273, λ = \316\273, η = \316\267, ν = \316\275, ι = \316\271, κ = \316\272, ή = \316\256
    assert_eq!(
        unescape_git_path(
            "\"\\341\\274\\231\\316\\273\\316\\273\\316\\267\\316\\275\\316\\271\\316\\272\\316\\256.txt\""
        ),
        "Ἑλληνική.txt"
    );
}

// =========================================================================
// Phase 6: Extended Emoji Tests (ZWJ, skin tones, flags)
// =========================================================================

#[test]
fn test_unescape_emoji_skin_tone() {
    // Emoji with skin tone modifier 👋🏽 = 👋 (U+1F44B) + 🏽 (U+1F3FD)
    // 👋 = \360\237\221\213, 🏽 = \360\237\217\275
    assert_eq!(
        unescape_git_path("\"\\360\\237\\221\\213\\360\\237\\217\\275.txt\""),
        "👋🏽.txt"
    );
}

#[test]
fn test_unescape_emoji_zwj_sequence() {
    // ZWJ emoji sequence: 👨‍💻 (man technologist) = man + ZWJ + laptop
    // 👨 = \360\237\221\250, ZWJ = \342\200\215, 💻 = \360\237\222\273
    assert_eq!(
        unescape_git_path("\"\\360\\237\\221\\250\\342\\200\\215\\360\\237\\222\\273.txt\""),
        "👨‍💻.txt"
    );
}

#[test]
fn test_unescape_emoji_flag() {
    // Flag emoji 🇯🇵 (Japan) = regional indicator J + regional indicator P
    // 🇯 = \360\237\207\257, 🇵 = \360\237\207\265
    assert_eq!(
        unescape_git_path("\"\\360\\237\\207\\257\\360\\237\\207\\265.txt\""),
        "🇯🇵.txt"
    );
}

#[test]
fn test_unescape_multiple_emoji() {
    // Multiple emoji: 🚀🎉 (rocket + party)
    // 🚀 = \360\237\232\200, 🎉 = \360\237\216\211
    assert_eq!(
        unescape_git_path("\"\\360\\237\\232\\200\\360\\237\\216\\211.txt\""),
        "🚀🎉.txt"
    );
}

// =========================================================================
// Phase 7: Special Unicode Characters Tests (math, currency, symbols)
// =========================================================================

#[test]
fn test_unescape_math_symbols() {
    // Math symbols: ∑ (summation) = \342\210\221
    assert_eq!(unescape_git_path("\"\\342\\210\\221.txt\""), "∑.txt");
}

#[test]
fn test_unescape_currency_symbols() {
    // Currency: € (euro) = \342\202\254
    assert_eq!(unescape_git_path("\"\\342\\202\\254.txt\""), "€.txt");
}

#[test]
fn test_unescape_box_drawing() {
    // Box drawing: ┌ (box drawings light down and right) = \342\224\214
    assert_eq!(unescape_git_path("\"\\342\\224\\214.txt\""), "┌.txt");
}

#[test]
fn test_unescape_dingbats() {
    // Dingbats: ✓ (check mark) = \342\234\223
    assert_eq!(unescape_git_path("\"\\342\\234\\223.txt\""), "✓.txt");
}

// =========================================================================
// Phase 8: Unicode Normalization Tests (NFC vs NFD)
// =========================================================================

#[test]
fn test_unescape_nfc_precomposed() {
    // NFC precomposed: é (U+00E9) = \303\251
    assert_eq!(unescape_git_path("\"caf\\303\\251.txt\""), "café.txt");
}

#[test]
fn test_unescape_nfd_decomposed() {
    // NFD decomposed: e + combining acute (U+0065 + U+0301) = e + \314\201
    assert_eq!(
        unescape_git_path("\"cafe\\314\\201.txt\""),
        "cafe\u{0301}.txt"
    );
}

#[test]
fn test_unescape_combining_diaeresis() {
    // Combining diaeresis: i + ̈ (U+0069 + U+0308) = i + \314\210
    assert_eq!(
        unescape_git_path("\"nai\\314\\210ve.txt\""),
        "nai\u{0308}ve.txt"
    );
}

#[test]
fn test_unescape_angstrom() {
    // Å (A with ring above, U+00C5) = \303\205
    assert_eq!(
        unescape_git_path("\"\\303\\205ngstr\\303\\266m.txt\""),
        "Ångström.txt"
    );
}

// =========================================================================
// Phase 9: Escape Sequence Edge Cases
// =========================================================================

#[test]
fn test_unescape_incomplete_octal() {
    // Incomplete octal at end of string
    assert_eq!(unescape_git_path("\"file\\34\""), "file\x1c");
    assert_eq!(unescape_git_path("\"file\\3\""), "file\x03");
}

#[test]
fn test_unescape_invalid_octal() {
    // Invalid octal digit (8 and 9 are not valid octal)
    assert_eq!(
        unescape_git_path("\"file\\389.txt\""),
        "file\x038\u{0039}.txt"
    );
}

#[test]
fn test_unescape_backslash_only() {
    // Backslash at end without following character
    assert_eq!(unescape_git_path("\"file\\\""), "file\\");
}

#[test]
fn test_unescape_mixed_escapes() {
    // Mix of different escape types
    assert_eq!(
        unescape_git_path("\"path\\nwith\\ttab\\\\and\\344\\270\\255.txt\""),
        "path\nwith\ttab\\and中.txt"
    );
}

#[test]
fn test_unescape_empty_quoted() {
    // Empty quoted string
    assert_eq!(unescape_git_path("\"\""), "");
}

#[test]
fn test_unescape_unmatched_quotes() {
    // Unmatched quotes - returned as-is
    assert_eq!(unescape_git_path("\"unmatched"), "\"unmatched");
    assert_eq!(unescape_git_path("unmatched\""), "unmatched\"");
}

// =========================================================================
// normalize_to_posix Tests
// =========================================================================

#[test]
fn test_normalize_to_posix_no_change() {
    // Already POSIX paths
    assert_eq!(normalize_to_posix("path/to/file.txt"), "path/to/file.txt");
    assert_eq!(normalize_to_posix("src/main.rs"), "src/main.rs");
}

#[test]
fn test_normalize_to_posix_windows() {
    // Windows paths
    assert_eq!(normalize_to_posix("path\\to\\file.txt"), "path/to/file.txt");
    assert_eq!(normalize_to_posix("C:\\Users\\file"), "C:/Users/file");
}

#[test]
fn test_normalize_to_posix_mixed() {
    // Mixed separators
    assert_eq!(
        normalize_to_posix("path/to\\some\\file.txt"),
        "path/to/some/file.txt"
    );
}

#[test]
fn test_normalize_to_posix_empty() {
    assert_eq!(normalize_to_posix(""), "");
}

#[test]
fn test_normalize_diff_path_token_contract() {
    let cases = [
        ("a/file.txt", "file.txt"),
        ("b/file.txt", "file.txt"),
        ("c/file.txt", "file.txt"),
        ("w/file.txt", "file.txt"),
        ("i/file.txt", "file.txt"),
        ("o/file.txt", "file.txt"),
        ("a/b/file.txt", "b/file.txt"),
        ("unprefixed.txt", "unprefixed.txt"),
        ("b/trailing.txt \t", "trailing.txt"),
        ("\"b/file with spaces.txt\"", "file with spaces.txt"),
        ("\"b/\\344\\270\\255.txt\"", "中.txt"),
        ("b/cafe\u{301}.txt", "café.txt"),
    ];

    for (input, expected) in cases {
        assert_eq!(normalize_diff_path_token(input), expected);
    }
}

#[test]
fn test_parse_diff_path_from_header_line_contract() {
    let cases: [(&str, &str, Option<Option<&str>>); 5] = [
        ("+++ b/file.txt", "+++ ", Some(Some("file.txt"))),
        ("--- c/old.txt", "--- ", Some(Some("old.txt"))),
        ("+++ /dev/null\t", "+++ ", Some(None)),
        ("+++ \"/dev/null\"", "+++ ", Some(Some("/dev/null"))),
        ("--- b/file.txt", "+++ ", None),
    ];

    for (line, prefix, expected) in cases {
        assert_eq!(
            parse_diff_path_from_header_line(line, prefix),
            expected.map(|path| path.map(str::to_owned))
        );
    }
}
