//! Secret detection and redaction for prompt messages.
//!
//! This module implements entropy-based secret detection inspired by ripsecrets.
//! It identifies high-entropy strings (likely secrets/API keys) and redacts them
//! in-place before saving to git notes.

use std::sync::OnceLock;

/// Minimum length for a string to be considered a potential secret
const MIN_SECRET_LENGTH: usize = 15;

/// Maximum length for a string to be considered a potential secret
const MAX_SECRET_LENGTH: usize = 90;

/// Number of characters to keep visible at start and end when redacting
const REDACT_VISIBLE_CHARS: usize = 4;

/// Common source code bigrams (roughly 10% of possible base64 bigrams)
/// Used to distinguish random strings from natural code/text
const BIGRAMS: &[&[u8]] = &[
    b"er", b"te", b"an", b"en", b"ma", b"ke", b"10", b"at", b"/m", b"on", b"09", b"ti", b"al",
    b"io", b".h", b"./", b"..", b"ra", b"ht", b"es", b"or", b"tm", b"pe", b"ml", b"re", b"in",
    b"3/", b"n3", b"0F", b"ok", b"ey", b"00", b"80", b"08", b"ss", b"07", b"15", b"81", b"F3",
    b"st", b"52", b"KE", b"To", b"01", b"it", b"2B", b"2C", b"/E", b"P_", b"EY", b"B7", b"se",
    b"73", b"de", b"VP", b"EV", b"to", b"od", b"B0", b"0E", b"nt", b"et", b"_P", b"A0", b"60",
    b"90", b"0A", b"ri", b"30", b"ar", b"C0", b"op", b"03", b"ec", b"ns", b"as", b"FF", b"F7",
    b"po", b"PK", b"la", b".p", b"AE", b"62", b"me", b"F4", b"71", b"8E", b"yp", b"pa", b"50",
    b"qu", b"D7", b"7D", b"rs", b"ea", b"Y_", b"t_", b"ha", b"3B", b"c/", b"D2", b"ls", b"DE",
    b"pr", b"am", b"E0", b"oc", b"06", b"li", b"do", b"id", b"05", b"51", b"40", b"ED", b"_p",
    b"70", b"ed", b"04", b"02", b"t.", b"rd", b"mp", b"20", b"d_", b"co", b"ro", b"ex", b"11",
    b"ua", b"nd", b"0C", b"0D", b"D0", b"Eq", b"le", b"EF", b"wo", b"e_", b"e.", b"ct", b"0B",
    b"_c", b"Li", b"45", b"rT", b"pt", b"14", b"61", b"Th", b"56", b"sT", b"E6", b"DF", b"nT",
    b"16", b"85", b"em", b"BF", b"9E", b"ne", b"_s", b"25", b"91", b"78", b"57", b"BE", b"ta",
    b"ng", b"cl", b"_t", b"E1", b"1F", b"y_", b"xp", b"cr", b"4F", b"si", b"s_", b"E5", b"pl",
    b"AB", b"ge", b"7E", b"F8", b"35", b"E2", b"s.", b"CF", b"58", b"32", b"2F", b"E7", b"1B",
    b"ve", b"B1", b"3D", b"nc", b"Gr", b"EB", b"C6", b"77", b"64", b"sl", b"8A", b"6A", b"_k",
    b"79", b"C8", b"88", b"ce", b"Ex", b"5C", b"28", b"EA", b"A6", b"2A", b"Ke", b"A7", b"th",
    b"CA", b"ry", b"F0", b"B6", b"7/", b"D9", b"6B", b"4D", b"DA", b"3C", b"ue", b"n7", b"9C",
    b".c", b"7B", b"72", b"ac", b"98", b"22", b"/o", b"va", b"2D", b"n.", b"_m", b"B8", b"A3",
    b"8D", b"n_", b"12", b"nE", b"ca", b"3A", b"is", b"AD", b"rt", b"r_", b"l-", b"_C", b"n1",
    b"_v", b"y.", b"yw", b"1/", b"ov", b"_n", b"_d", b"ut", b"no", b"ul", b"sa", b"CT", b"_K",
    b"SS", b"_e", b"F1", b"ty", b"ou", b"nG", b"tr", b"s/", b"il", b"na", b"iv", b"L_", b"AA",
    b"da", b"Ty", b"EC", b"ur", b"TX", b"xt", b"lu", b"No", b"r.", b"SL", b"Re", b"sw", b"_1",
    b"om", b"e/", b"Pa", b"xc", b"_g", b"_a", b"X_", b"/e", b"vi", b"ds", b"ai", b"==", b"ts",
    b"ni", b"mg", b"ic", b"o/", b"mt", b"gm", b"pk", b"d.", b"ch", b"/p", b"tu", b"sp", b"17",
    b"/c", b"ym", b"ot", b"ki", b"Te", b"FE", b"ub", b"nL", b"eL", b".k", b"if", b"he", b"34",
    b"e-", b"23", b"ze", b"rE", b"iz", b"St", b"EE", b"-p", b"be", b"In", b"ER", b"67", b"13",
    b"yn", b"ig", b"ib", b"_f", b".o", b"el", b"55", b"Un", b"21", b"fi", b"54", b"mo", b"mb",
    b"gi", b"_r", b"Qu", b"FD", b"-o", b"ie", b"fo", b"As", b"7F", b"48", b"41", b"/i", b"eS",
    b"ab", b"FB", b"1E", b"h_", b"ef", b"rr", b"rc", b"di", b"b.", b"ol", b"im", b"eg", b"ap",
    b"_l", b"Se", b"19", b"oS", b"ew", b"bs", b"Su", b"F5", b"Co", b"BC", b"ud", b"C1", b"r-",
    b"ia", b"_o", b"65", b".r", b"sk", b"o_", b"ck", b"CD", b"Am", b"9F", b"un", b"fa", b"F6",
    b"5F", b"nk", b"lo", b"ev", b"/f", b".t", b"sE", b"nO", b"a_", b"EN", b"E4", b"Di", b"AC",
    b"95", b"74", b"1_", b"1A", b"us", b"ly", b"ll", b"_b", b"SA", b"FC", b"69", b"5E", b"43",
    b"um", b"tT", b"OS", b"CE", b"87", b"7A", b"59", b"44", b"t-", b"bl", b"ad", b"Or", b"D5",
    b"A_", b"31", b"24", b"t/", b"ph", b"mm", b"f.", b"ag", b"RS", b"Of", b"It", b"FA", b"De",
    b"1D", b"/d", b"-k", b"lf", b"hr", b"gu", b"fy", b"D6", b"89", b"6F", b"4E", b"/k", b"w_",
    b"cu", b"br", b"TE", b"ST", b"R_", b"E8", b"/O",
];

/// Pre-computed ln(n!) lookup table for n = 0..=MAX_SECRET_LENGTH
/// This avoids O(n) factorial computation on every call to p_binomial
static LOG_FACTORIALS: OnceLock<[f64; MAX_SECRET_LENGTH + 1]> = OnceLock::new();

/// Get the pre-computed log-factorial lookup table
fn get_log_factorials() -> &'static [f64; MAX_SECRET_LENGTH + 1] {
    LOG_FACTORIALS.get_or_init(|| {
        let mut table = [0.0; MAX_SECRET_LENGTH + 1];
        // ln(0!) = ln(1) = 0, already set
        for i in 1..=MAX_SECRET_LENGTH {
            table[i] = table[i - 1] + (i as f64).ln();
        }
        table
    })
}

/// Get ln(n!) from the lookup table
#[inline]
fn log_factorial(n: usize) -> f64 {
    get_log_factorials()[n]
}

/// Pre-computed bigram lookup table for O(1) access.
/// A 128x128 bool array where BIGRAM_TABLE[a][b] = true if "ab" is a common bigram.
static BIGRAM_TABLE: OnceLock<[[bool; 128]; 128]> = OnceLock::new();

/// Get the pre-computed bigram lookup table.
fn get_bigram_table() -> &'static [[bool; 128]; 128] {
    BIGRAM_TABLE.get_or_init(|| {
        let mut table = [[false; 128]; 128];
        for bigram in BIGRAMS {
            if bigram.len() == 2 && bigram[0] < 128 && bigram[1] < 128 {
                table[bigram[0] as usize][bigram[1] as usize] = true;
            }
        }
        table
    })
}

/// Check if a bigram is in the common set. O(1) lookup.
#[inline]
fn is_common_bigram(a: u8, b: u8) -> bool {
    if a >= 128 || b >= 128 {
        return false;
    }
    get_bigram_table()[a as usize][b as usize]
}

/// Get bigram count for p_binomial calculations.
fn get_bigram_set_len() -> usize {
    BIGRAMS.len()
}

/// Statistics collected in a single pass over the token.
struct CharStats {
    distinct_count: usize,
    digit_count: usize,
    upper_count: usize,
    lower_count: usize,
    is_all_hex: bool,
    is_all_cap_num: bool,
    bigram_count: usize,
}

/// Analyze a token in a single pass, collecting all needed statistics.
#[inline]
fn analyze_token(s: &[u8]) -> CharStats {
    let mut seen = [false; 256];
    let mut distinct_count = 0;
    let mut digit_count = 0;
    let mut upper_count = 0;
    let mut lower_count = 0;
    let mut is_all_hex = true;
    let mut is_all_cap_num = true;
    let mut bigram_count = 0;

    for (i, &b) in s.iter().enumerate() {
        // Track distinct characters
        if !seen[b as usize] {
            seen[b as usize] = true;
            distinct_count += 1;
        }

        // Count character classes
        if b.is_ascii_digit() {
            digit_count += 1;
        } else if b.is_ascii_uppercase() {
            upper_count += 1;
        } else if b.is_ascii_lowercase() {
            lower_count += 1;
            is_all_cap_num = false;
        } else {
            // Not alphanumeric
            is_all_hex = false;
            is_all_cap_num = false;
        }

        // Check hex validity (0-9, a-f, A-F)
        if is_all_hex && !b.is_ascii_hexdigit() {
            is_all_hex = false;
        }

        // Count bigrams using O(1) table lookup
        if i + 1 < s.len() && is_common_bigram(b, s[i + 1]) {
            bigram_count += 1;
        }
    }

    CharStats {
        distinct_count,
        digit_count,
        upper_count,
        lower_count,
        is_all_hex: is_all_hex && s.len() >= 16,
        is_all_cap_num: is_all_cap_num && s.len() >= 16,
        bigram_count,
    }
}

/// Calculate the probability that a string is random based on various heuristics.
/// Uses a single-pass analysis for efficiency.
///
/// Tokens must be at most MAX_SECRET_LENGTH bytes (as produced by
/// `scan_tokens`); longer inputs exceed the precomputed table domains and
/// panic on the Stirling table lookup.
pub fn p_random(s: &[u8]) -> f64 {
    let stats = analyze_token(s);

    // Determine alphabet base
    let base = if stats.is_all_hex {
        16.0
    } else if stats.is_all_cap_num {
        36.0
    } else {
        64.0
    };

    // Calculate distinct values probability using precomputed Stirling
    let p_distinct = p_random_distinct_values_with_stats(s.len(), stats.distinct_count, base);

    // Calculate character class probability
    let p_class = p_random_char_class_with_stats(&stats, s.len(), base);

    let mut p = p_distinct * p_class;

    // Bigram probability (only for base64)
    if base == 64.0 {
        p *= p_binomial(
            s.len(),
            stats.bigram_count,
            (get_bigram_set_len() as f64) / (64.0 * 64.0),
        );
    }

    p
}

/// Calculate distinct values probability using pre-analyzed stats.
fn p_random_distinct_values_with_stats(n: usize, num_distinct: usize, base: f64) -> f64 {
    let total_possible = base.powi(n as i32);

    let mut num_more_extreme: f64 = 0.0;
    let mut falling = base;

    // A random string over `base` characters has at most `base` distinct
    // values, so every term past k = base is zero (`falling` reaches exactly
    // 0.0 there). Bounding k at `base` skips those zero terms and keeps k
    // within the Stirling table bounds (k <= 64) when a token has more
    // distinct characters than the base (the secret-char alphabet has 68).
    for k in 1..=num_distinct.min(base as usize) {
        num_more_extreme += stirling(n, k) * falling;
        falling *= base - k as f64;
    }

    num_more_extreme / total_possible
}

/// Calculate character class probability using pre-analyzed stats.
fn p_random_char_class_with_stats(stats: &CharStats, n: usize, base: f64) -> f64 {
    if base == 16.0 {
        // For hex, check digit ratio
        return p_binomial(n, stats.digit_count, 10.0 / 16.0);
    }

    // For base36 and base64, check each character class
    let digit_p = p_binomial(n, stats.digit_count, 10.0 / base);
    let upper_p = p_binomial(n, stats.upper_count, 26.0 / base);

    if base == 36.0 {
        return digit_p.min(upper_p);
    }

    // base64
    let lower_p = p_binomial(n, stats.lower_count, 26.0 / base);
    digit_p.min(upper_p).min(lower_p)
}

/// Fast erfc approximation (Abramowitz & Stegun 7.1.26), max error ~1.5e-7.
/// Used by Normal approximation in p_binomial.
#[inline]
fn erfc_approx(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.3275911 * x.abs());
    let poly = t
        * (0.254829592
            + t * (-0.284496736 + t * (1.421413741 + t * (-1.453152027 + t * 1.061405429))));
    let result = poly * (-x * x).exp(); // Single exp() call
    if x >= 0.0 { result } else { 2.0 - result }
}

/// Calculate binomial probability (cumulative tail probability).
/// Uses Normal approximation for large n (O(1)) and falls back to exact
/// calculation for small n (O(n)).
fn p_binomial(n: usize, x: usize, p: f64) -> f64 {
    // Handle edge cases
    if p <= 0.0 {
        return if x == 0 { 1.0 } else { 0.0 };
    }
    if p >= 1.0 {
        return if x == n { 1.0 } else { 0.0 };
    }

    let mean = n as f64 * p;
    let variance = n as f64 * p * (1.0 - p);

    // Use Normal approximation when variance is sufficient (np > 5 and n(1-p) > 5)
    // This is O(1) with a single exp() call vs O(n) exp() calls
    if variance > 2.0 {
        let std = variance.sqrt();
        let left_tail = (x as f64) < mean;

        // Continuity correction: use x+0.5 or x-0.5 depending on tail
        let x_corrected = if left_tail {
            x as f64 + 0.5
        } else {
            x as f64 - 0.5
        };
        let z = (x_corrected - mean) / std;

        // P(X <= x) for left tail, P(X >= x) for right tail
        // Using: P(Z <= z) = 0.5 * erfc(-z / sqrt(2))
        return if left_tail {
            0.5 * erfc_approx(-z * std::f64::consts::FRAC_1_SQRT_2)
        } else {
            0.5 * erfc_approx(z * std::f64::consts::FRAC_1_SQRT_2)
        };
    }

    // Fallback to exact calculation for small variance (rare for our use case)
    let left_tail = (x as f64) < mean;
    let min = if left_tail { 0 } else { x };
    let max = if left_tail { x } else { n };

    let log_p = p.ln();
    let log_1_minus_p = (1.0 - p).ln();

    let mut total_p = 0.0;
    for i in min..=max {
        let log_binom_coeff = log_factorial(n) - log_factorial(n - i) - log_factorial(i);
        let log_term = log_binom_coeff + (i as f64) * log_p + ((n - i) as f64) * log_1_minus_p;
        total_p += log_term.exp();
    }

    total_p
}

/// Pre-computed Stirling numbers table for O(1) lookup.
/// STIRLING_TABLE[n][k] = S(n, k) for n=0..=MAX_SECRET_LENGTH, k=0..=64
static STIRLING_TABLE: OnceLock<[[f64; 65]; MAX_SECRET_LENGTH + 1]> = OnceLock::new();

/// Initialize the Stirling numbers table using DP.
fn get_stirling_table() -> &'static [[f64; 65]; MAX_SECRET_LENGTH + 1] {
    STIRLING_TABLE.get_or_init(|| {
        let mut table = [[0.0; 65]; MAX_SECRET_LENGTH + 1];

        // S(n, 1) = 1 for all n >= 1
        // S(n, n) = 1 for all n >= 1
        #[allow(clippy::needless_range_loop)]
        for n in 1..=MAX_SECRET_LENGTH {
            table[n][1] = 1.0;
            if n <= 64 {
                table[n][n] = 1.0;
            }
        }

        // Fill using DP: S(n,k) = k*S(n-1,k) + S(n-1,k-1)
        // The bound must be inclusive: for n > 64 nothing else fills
        // table[n][64] (the base cases above only cover k == n for n <= 64).
        for n in 2..=MAX_SECRET_LENGTH {
            let max_k = n.min(64);
            #[allow(clippy::needless_range_loop)]
            for k in 2..=max_k {
                table[n][k] = k as f64 * table[n - 1][k] + table[n - 1][k - 1];
            }
        }

        table
    })
}

/// Get Stirling number S(n, k) from precomputed table. O(1) lookup.
/// The table only covers n <= MAX_SECRET_LENGTH and k <= 64, so callers must
/// stay within that domain; anything larger panics on the table indexing.
#[inline]
fn stirling(n: usize, k: usize) -> f64 {
    if k == 0 || n == 0 || k > n {
        return 0.0;
    }
    if k == 1 || k == n {
        return 1.0;
    }
    debug_assert!(
        n <= MAX_SECRET_LENGTH && k <= 64,
        "stirling(n={n}, k={k}) is outside the precomputed table bounds"
    );
    get_stirling_table()[n][k]
}

/// Check if a string is likely a random/secret string.
/// Returns true if the string appears to be a secret.
pub fn is_random(s: &[u8]) -> bool {
    let p = p_random(s);

    if p < 1.0 / 1e5 {
        return false;
    }

    // If no digits, require higher probability threshold
    let contains_num = s.iter().any(|&b| b.is_ascii_digit());
    if !contains_num && p < 1.0 / 1e4 {
        return false;
    }

    true
}

/// Check if a byte is a valid secret character (alphanumeric or common secret chars).
/// Excludes `=` which is typically a delimiter (e.g., KEY=value).
/// Note: `=` at the end of base64 strings is handled specially.
fn is_secret_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'_' | b'-' | b'.' | b'~')
}

/// Scan text for potential secret tokens (contiguous runs of secret chars in the
/// valid length range) and invoke `f` for each. Returns `true` if `f` ever
/// returns `true` (short-circuit). This is the single token-scanning loop shared
/// by both `extract_tokens` and `text_contains_secrets`.
#[inline]
fn scan_tokens(text: &str, mut f: impl FnMut(usize, usize) -> bool) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !is_secret_char(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_secret_char(bytes[i]) {
            i += 1;
        }
        let len = i - start;
        if (MIN_SECRET_LENGTH..=MAX_SECRET_LENGTH).contains(&len) && f(start, i) {
            return true;
        }
    }
    false
}

/// Extract potential secret tokens from text.
/// Returns a vector of (start_index, end_index) pairs to avoid string allocations.
pub fn extract_tokens(text: &str) -> Vec<(usize, usize)> {
    let mut tokens = Vec::new();
    scan_tokens(text, |start, end| {
        tokens.push((start, end));
        false // never short-circuit, collect all
    });
    tokens
}

/// Redact a secret string, keeping first and last few characters visible.
/// Format: "sk_live_abc123" -> "sk_l********c123"
pub fn redact_secret(secret: &str) -> String {
    let len = secret.len();
    if len <= REDACT_VISIBLE_CHARS * 2 {
        // Too short to meaningfully redact
        return "*".repeat(len);
    }

    let prefix = &secret[..REDACT_VISIBLE_CHARS];
    let suffix = &secret[len - REDACT_VISIBLE_CHARS..];
    format!("{}********{}", prefix, suffix)
}

/// Returns true if the text contains at least one high-entropy token that looks
/// like a secret. Performs no heap allocations — scans inline and short-circuits
/// on the first match.
pub fn text_contains_secrets(text: &str) -> bool {
    scan_tokens(text, |start, end| is_random(&text.as_bytes()[start..end]))
}

/// Redact all detected secrets in a text string.
/// Returns a tuple of (redacted_text, redaction_count).
pub fn redact_secrets_in_text(text: &str) -> (String, usize) {
    let tokens = extract_tokens(text);

    // Filter to only actual secrets (start, end positions)
    let secrets: Vec<(usize, usize)> = tokens
        .into_iter()
        .filter(|&(start, end)| is_random(&text.as_bytes()[start..end]))
        .collect();

    let count = secrets.len();

    if secrets.is_empty() {
        return (text.to_string(), 0);
    }

    // Build result efficiently by copying non-secret parts and redacted secrets
    let mut result = String::with_capacity(text.len());
    let mut prev_end = 0;

    for (start, end) in &secrets {
        // Copy text before this secret
        result.push_str(&text[prev_end..*start]);
        // Add redacted secret
        result.push_str(&redact_secret(&text[*start..*end]));
        prev_end = *end;
    }
    // Copy remaining text after last secret
    result.push_str(&text[prev_end..]);

    (result, count)
}

#[cfg(test)]
mod tests {
    use insta::assert_debug_snapshot;

    use super::*;

    #[test]
    fn test_p_random_random_strings() {
        // These should be detected as random
        assert!(p_random(b"pk_test_TYooMQauvdEDq54NiTphI7jx") > 1.0 / 1e4);
        assert!(p_random(b"sk_test_4eC39HqLyjWDarjtT1zdp7dc") > 1.0 / 1e4);
    }

    #[test]
    fn test_p_random_non_random_strings() {
        // These should NOT be detected as random
        assert!(p_random(b"hello_world") < 1.0 / 1e6);
        assert!(p_random(b"PROJECT_NAME_ALIAS") < 1.0 / 1e4);
    }

    #[test]
    fn test_is_random() {
        // Secrets
        assert!(is_random(b"pk_test_TYooMQauvdEDq54NiTphI7jx"));
        assert!(is_random(b"sk_test_4eC39HqLyjWDarjtT1zdp7dc"));
        assert!(is_random(b"AKIAIOSFODNN7EXAMPLE"));

        // Not secrets
        assert!(!is_random(b"hello_world"));
        assert!(!is_random(b"my_variable_name"));
    }

    #[test]
    fn test_extract_tokens() {
        let text = "API_KEY=sk_test_4eC39HqLyjWDarjtT1zdp7dc";
        let tokens = extract_tokens(text);
        assert!(!tokens.is_empty());
        // The token should be extracted (API_KEY is 7 chars, too short; the secret is 32 chars)
        assert!(
            tokens
                .iter()
                .any(|&(start, end)| &text[start..end] == "sk_test_4eC39HqLyjWDarjtT1zdp7dc")
        );
    }

    #[test]
    fn test_redact_secret() {
        assert_eq!(
            redact_secret("sk_test_4eC39HqLyjWDarjtT1zdp7dc"),
            "sk_t********p7dc"
        );
        assert_eq!(redact_secret("AKIAIOSFODNN7EXAMPLE"), "AKIA********MPLE");
        assert_eq!(redact_secret("short"), "*****"); // Too short
    }

    #[test]
    fn test_redact_secrets_in_text() {
        let text = "Set API_KEY=sk_test_4eC39HqLyjWDarjtT1zdp7dc in your config";
        let (redacted, count) = redact_secrets_in_text(text);
        assert!(!redacted.contains("sk_test_4eC39HqLyjWDarjtT1zdp7dc"));
        assert!(redacted.contains("sk_t********p7dc"));
        assert_eq!(count, 1);
    }

    #[test]
    fn test_no_redaction_for_normal_text() {
        let text = "This is normal text without any secrets";
        let (redacted, count) = redact_secrets_in_text(text);
        assert_eq!(text, redacted);
        assert_eq!(count, 0);
    }

    #[test]
    fn test_distinct_values() {
        assert_eq!(analyze_token(b"abca").distinct_count, 3);
        assert_eq!(analyze_token(b"aaaaaa").distinct_count, 1);
        assert_eq!(analyze_token(b"abcdef").distinct_count, 6);
    }

    #[test]
    fn test_redact_secret_in_lorem_ipsum() {
        let text = r#"
Lorem ipsum dolor sit amet, consectetur adipiscing elit. Sed do eiusmod tempor 
incididunt ut labore et dolore magna aliqua. Here is my API key: 
sk_live_51HG8vDKj2xPmVnRqT9wYzABC and you should use it carefully.
Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut 
aliquip ex ea commodo consequat. Duis aute irure dolor in reprehenderit in 
voluptate velit esse cillum dolore eu fugiat nulla pariatur.
"#;
        let (redacted, count) = redact_secrets_in_text(text);

        // Secret should be redacted
        assert!(!redacted.contains("sk_live_51HG8vDKj2xPmVnRqT9wYzABC"));
        assert!(redacted.contains("sk_l********zABC"));
        assert_eq!(count, 1);

        // Rest of text should be intact
        assert!(redacted.contains("Lorem ipsum dolor sit amet"));
        assert!(redacted.contains("consectetur adipiscing elit"));
        assert!(redacted.contains("Here is my API key:"));
    }

    #[test]
    fn test_redact_multiple_secrets_in_code() {
        let code = r#"
use std::env;

fn main() {
    // Database credentials
    let db_password = "xK9mP2nQ7rS4tU6vW8yZ1aB3cD5eF7gH";
    
    // API configuration
    let stripe_key = "sk_test_4eC39HqLyjWDarjtT1zdp7dc";
    let aws_key = "AKIAIOSFODNN7EXAMPLE";
    
    // Normal config values - should NOT be redacted
    let app_name = "my_application_name";
    let log_level = "debug";
    let max_connections = 100;
    
    println!("Starting application...");
}
"#;
        let (redacted, count) = redact_secrets_in_text(code);

        // Secrets should be redacted
        assert!(!redacted.contains("xK9mP2nQ7rS4tU6vW8yZ1aB3cD5eF7gH"));
        assert!(!redacted.contains("sk_test_4eC39HqLyjWDarjtT1zdp7dc"));
        assert!(!redacted.contains("AKIAIOSFODNN7EXAMPLE"));
        assert_eq!(count, 3);

        // Normal identifiers should remain
        assert!(redacted.contains("my_application_name"));
        assert!(redacted.contains("debug"));
        assert!(redacted.contains("max_connections"));
        assert!(redacted.contains("println!"));
    }

    #[test]
    fn test_redact_secret_in_json_config() {
        let json = r#"{
    "database": {
        "host": "localhost",
        "port": 5432,
        "password": "Rj7kL9mN2pQ4sT6vX8zA1bC3dE5fG7hI"
    },
    "api": {
        "endpoint": "https://api.example.com",
        "key": "pk_live_TYooMQauvdEDq54NiTphI7jx"
    },
    "logging": {
        "level": "info",
        "format": "json"
    }
}"#;
        let (redacted, count) = redact_secrets_in_text(json);

        // Secrets should be redacted
        assert!(!redacted.contains("Rj7kL9mN2pQ4sT6vX8zA1bC3dE5fG7hI"));
        assert!(!redacted.contains("pk_live_TYooMQauvdEDq54NiTphI7jx"));
        assert_eq!(count, 2);

        // Normal config should remain
        assert!(redacted.contains("localhost"));
        assert!(redacted.contains("5432"));
        assert!(redacted.contains("https://api.example.com"));
        assert!(redacted.contains("info"));
    }

    #[test]
    fn test_redact_secret_in_env_file() {
        let env_content = r#"
# Application configuration
APP_NAME=my-cool-app
DEBUG=true
LOG_LEVEL=debug

# Secrets - these should be redacted
DATABASE_URL=postgres://user:pA5sW0rD9xK2mN7qR4tU6vY8zA1bC3dE@localhost:5432/mydb
STRIPE_SECRET_KEY=sk_live_51HG8vDKj2xPmVnRqT9wYzABC
AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE
JWT_SECRET=eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9

# More normal config
PORT=3000
HOST=0.0.0.0
"#;
        let (redacted, count) = redact_secrets_in_text(env_content);

        println!("redacted: {}", redacted);
        assert_debug_snapshot!(redacted);
        // Secrets should be redacted
        assert!(!redacted.contains("pA5sW0rD9xK2mN7qR4tU6vY8zA1bC3dE"));
        assert!(!redacted.contains("sk_live_51HG8vDKj2xPmVnRqT9wYzABC"));
        assert!(!redacted.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(count >= 3); // At least 3 secrets

        // Normal values should remain
        assert!(redacted.contains("my-cool-app"));
        assert!(redacted.contains("DEBUG=true"));
        assert!(redacted.contains("PORT=3000"));
    }

    #[test]
    fn test_no_false_positives_in_normal_code() {
        let code = r#"
pub fn calculate_total(items: &[Item]) -> f64 {
    items.iter().map(|item| item.price * item.quantity as f64).sum()
}

struct Configuration {
    database_host: String,
    database_port: u16,
    application_name: String,
    max_retry_attempts: u32,
}

impl Configuration {
    pub fn from_environment() -> Self {
        Self {
            database_host: std::env::var("DB_HOST").unwrap_or_default(),
            database_port: 5432,
            application_name: "my_service".to_string(),
            max_retry_attempts: 3,
        }
    }
}
"#;
        let (redacted, count) = redact_secrets_in_text(code);

        // Code should be completely unchanged - no false positives
        assert_eq!(code, redacted);
        assert_eq!(count, 0);
    }

    #[test]
    fn test_no_panic_on_tokens_with_more_than_64_distinct_chars() {
        // Regression test: the secret-char alphabet has 68 characters, so a
        // token can have more than 64 distinct values while the Stirling table
        // only covers k=0..=64. These previously panicked with
        // "index out of bounds: the len is 65 but the index is 65".

        // 66 chars, all distinct.
        let token_66 = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/-_";
        assert_eq!(token_66.len(), 66);
        assert_eq!(analyze_token(token_66).distinct_count, 66);
        assert!(p_random(token_66).is_finite());

        // MAX_SECRET_LENGTH (90) chars with all 68 distinct secret chars.
        let mut token_90 =
            String::from("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/-_.~");
        token_90.push_str("ABCDEFGHIJKLMNOPQRSTUV");
        assert_eq!(token_90.len(), MAX_SECRET_LENGTH);
        assert_eq!(analyze_token(token_90.as_bytes()).distinct_count, 68);
        assert!(p_random(token_90.as_bytes()).is_finite());

        // Scanning text that embeds such a token must not panic either.
        let text = format!(
            "token={} and also {}",
            std::str::from_utf8(token_66).unwrap(),
            token_90
        );
        let _ = text_contains_secrets(&text);
        let _ = redact_secrets_in_text(&text);
    }

    #[test]
    fn test_stirling_table_covers_k_64_for_n_above_64() {
        // Regression test: the DP fill previously used an exclusive bound
        // (`2..max_k`), so table[n][64] was never written for n > 64 and
        // stirling(n, 64) returned 0.0 instead of the true value.
        assert_eq!(stirling(65, 64), 2080.0); // S(65, 64) = C(65, 2)
        assert!(stirling(MAX_SECRET_LENGTH, 64) > 0.0);
        assert!(stirling(MAX_SECRET_LENGTH, 64).is_finite());
    }
}
