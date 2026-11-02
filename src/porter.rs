//! Tokenizer + classic Porter stemmer, matching SQLite FTS5's
//! `porter unicode61 remove_diacritics 2` so ranking stays identical after
//! dropping SQLite. Verified token-for-token against SQLite (see tests and
//! `bench/` notes in the README).

/// SQLite's porter tokenizer leaves tokens outside 3..=64 bytes unstemmed.
const MIN_STEM: usize = 3;
const MAX_STEM: usize = 64;

/// Fold a Latin letter with a diacritic to its base letter (unicode61 remove_diacritics).
fn fold(c: char) -> char {
    const MAP: &[(&str, char)] = &[
        ("àáâãäåāăą", 'a'),
        ("çćĉċč", 'c'),
        ("ď", 'd'),
        ("èéêëēĕėęě", 'e'),
        ("ĝğġģ", 'g'),
        ("ĥ", 'h'),
        ("ìíîïĩīĭįİ", 'i'),
        ("ĵ", 'j'),
        ("ķ", 'k'),
        ("ĺļľ", 'l'),
        ("ñńņň", 'n'),
        ("òóôõöōŏő", 'o'),
        ("ŕŗř", 'r'),
        ("śŝşš", 's'),
        ("ţť", 't'),
        ("ùúûüũūŭůűų", 'u'),
        ("ŵ", 'w'),
        ("ýÿŷ", 'y'),
        ("źżž", 'z'),
    ];
    if c.is_ascii() {
        return c;
    }
    MAP.iter()
        .find(|(set, _)| set.contains(c))
        .map(|(_, b)| *b)
        .unwrap_or(c)
}

/// unicode61: letters and digits are token characters, everything else separates;
/// lower-cased and with diacritics removed.
pub fn raw_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            // Fold before lower-casing so İ → i (Rust would give i + U+0307).
            let ch = match ch {
                '\u{00B5}' => '\u{03BC}', // micro sign → Greek mu (Unicode case folding)
                '\u{017F}' => 's',        // long s → s (Unicode case folding)
                c => fold(c),
            };
            for l in ch.to_lowercase() {
                if !('\u{0300}'..='\u{036F}').contains(&l) {
                    cur.push(fold(l));
                }
            }
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Tokens as stored in the index: unicode61 tokens, Porter-stemmed.
pub fn tokens(text: &str) -> Vec<String> {
    raw_tokens(text).into_iter().map(|t| stem(&t)).collect()
}

/// Classic Porter (1980) stemmer as implemented by SQLite FTS5.
pub fn stem(word: &str) -> String {
    let n = word.len();
    if !(MIN_STEM..=MAX_STEM).contains(&n) {
        return word.to_string();
    }
    let mut b = word.as_bytes().to_vec();
    step1ab(&mut b);
    step1c(&mut b);
    step2(&mut b);
    step3(&mut b);
    step4(&mut b);
    step5(&mut b);
    String::from_utf8(b).unwrap_or_else(|_| word.to_string())
}

/// Is b[i] a consonant? `y` is a consonant at the start or after a vowel.
fn cons(b: &[u8], i: usize) -> bool {
    match b[i] {
        b'a' | b'e' | b'i' | b'o' | b'u' => false,
        b'y' => i == 0 || !cons(b, i - 1),
        _ => true,
    }
}

/// m(): the number of VC sequences in b[..len].
fn measure(b: &[u8], len: usize) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < len && cons(b, i) {
        i += 1;
    }
    loop {
        while i < len && !cons(b, i) {
            i += 1;
        }
        if i >= len {
            return n;
        }
        while i < len && cons(b, i) {
            i += 1;
        }
        n += 1;
        if i >= len {
            return n;
        }
    }
}

fn has_vowel(b: &[u8], len: usize) -> bool {
    (0..len).any(|i| !cons(b, i))
}

/// Ends in a double consonant.
fn double_cons(b: &[u8], len: usize) -> bool {
    len >= 2 && b[len - 1] == b[len - 2] && cons(b, len - 1)
}

/// consonant-vowel-consonant ending, last consonant not w, x or y.
fn cvc(b: &[u8], len: usize) -> bool {
    if len < 3 || !cons(b, len - 1) || cons(b, len - 2) || !cons(b, len - 3) {
        return false;
    }
    !matches!(b[len - 1], b'w' | b'x' | b'y')
}

fn ends(b: &[u8], suffix: &str) -> bool {
    b.ends_with(suffix.as_bytes())
}

/// Replace `suffix` with `rep` if the stem before it has m() > `min_m`.
fn replace_if(b: &mut Vec<u8>, suffix: &str, rep: &str, min_m: usize) -> bool {
    if !ends(b, suffix) {
        return false;
    }
    let stem_len = b.len() - suffix.len();
    if measure(b, stem_len) > min_m {
        b.truncate(stem_len);
        b.extend_from_slice(rep.as_bytes());
    }
    true
}

fn step1ab(b: &mut Vec<u8>) {
    if ends(b, "sses") || ends(b, "ies") {
        b.truncate(b.len() - 2);
    } else if ends(b, "ss") {
    } else if ends(b, "s") {
        b.pop();
    }
    if ends(b, "eed") {
        if measure(b, b.len() - 3) > 0 {
            b.pop();
        }
        return;
    }
    let cut = if ends(b, "ed") && has_vowel(b, b.len() - 2) {
        2
    } else if ends(b, "ing") && has_vowel(b, b.len() - 3) {
        3
    } else {
        return;
    };
    b.truncate(b.len() - cut);
    if ends(b, "at") || ends(b, "bl") || ends(b, "iz") {
        b.push(b'e');
    } else if double_cons(b, b.len()) && !matches!(b[b.len() - 1], b'l' | b's' | b'z') {
        b.pop();
    } else if measure(b, b.len()) == 1 && cvc(b, b.len()) {
        b.push(b'e');
    }
}

fn step1c(b: &mut [u8]) {
    let n = b.len();
    if n > 1 && b[n - 1] == b'y' && has_vowel(b, n - 1) {
        b[n - 1] = b'i';
    }
}

fn step2(b: &mut Vec<u8>) {
    const RULES: &[(&str, &str)] = &[
        ("ational", "ate"),
        ("tional", "tion"),
        ("enci", "ence"),
        ("anci", "ance"),
        ("izer", "ize"),
        ("bli", "ble"),
        ("alli", "al"),
        ("entli", "ent"),
        ("eli", "e"),
        ("ousli", "ous"),
        ("ization", "ize"),
        ("ation", "ate"),
        ("ator", "ate"),
        ("alism", "al"),
        ("iveness", "ive"),
        ("fulness", "ful"),
        ("ousness", "ous"),
        ("aliti", "al"),
        ("iviti", "ive"),
        ("biliti", "ble"),
        ("logi", "log"),
    ];
    for (s, r) in RULES {
        if replace_if(b, s, r, 0) {
            return;
        }
    }
}

fn step3(b: &mut Vec<u8>) {
    const RULES: &[(&str, &str)] = &[
        ("icate", "ic"),
        ("ative", ""),
        ("alize", "al"),
        ("iciti", "ic"),
        ("ical", "ic"),
        ("ful", ""),
        ("ness", ""),
    ];
    for (s, r) in RULES {
        if replace_if(b, s, r, 0) {
            return;
        }
    }
}

fn step4(b: &mut Vec<u8>) {
    const SUFFIXES: &[&str] = &[
        "al", "ance", "ence", "er", "ic", "able", "ible", "ant", "ement", "ment", "ent", "ion", "ou", "ism", "ate",
        "iti", "ous", "ive", "ize",
    ];
    for s in SUFFIXES {
        if !ends(b, s) {
            continue;
        }
        let stem_len = b.len() - s.len();
        // "ion" only after s or t; "ement" is tried before "ment"/"ent" (longest first).
        if *s == "ion" && !(stem_len > 0 && matches!(b[stem_len - 1], b's' | b't')) {
            return;
        }
        if measure(b, stem_len) > 1 {
            b.truncate(stem_len);
        }
        return;
    }
}

fn step5(b: &mut Vec<u8>) {
    let n = b.len();
    if b[n - 1] == b'e' {
        let m = measure(b, n - 1);
        if m > 1 || (m == 1 && !cvc(b, n - 1)) {
            b.pop();
        }
    }
    let n = b.len();
    if n > 1 && b[n - 1] == b'l' && double_cons(b, n) && measure(b, n) > 1 {
        b.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porter_reference_vocabulary() {
        // From Martin Porter's published test vocabulary.
        for (w, s) in [
            ("caresses", "caress"),
            ("ponies", "poni"),
            ("ties", "ti"),
            ("cats", "cat"),
            ("feed", "feed"),
            ("agreed", "agre"),
            ("plastered", "plaster"),
            ("motoring", "motor"),
            ("sing", "sing"),
            ("conflated", "conflat"),
            ("troubled", "troubl"),
            ("sized", "size"),
            ("hopping", "hop"),
            ("falling", "fall"),
            ("filing", "file"),
            ("happy", "happi"),
            ("sky", "sky"),
            ("relational", "relat"),
            ("conditional", "condit"),
            ("digitizer", "digit"),
            ("generalizations", "gener"),
            ("oscillators", "oscil"),
            ("electrical", "electr"),
            ("hopefulness", "hope"),
            ("adjustment", "adjust"),
            ("adoption", "adopt"),
            ("controll", "control"),
            ("daily", "daili"),
            ("was", "wa"),
            ("running", "run"),
            ("runner", "runner"),
        ] {
            assert_eq!(stem(w), s, "{w}");
        }
    }

    #[test]
    fn unicode61_tokens() {
        assert_eq!(
            raw_tokens("Résumé café, naïve snake_case 203.0.113.7"),
            ["resume", "cafe", "naive", "snake", "case", "203", "0", "113", "7"]
        );
        assert_eq!(
            raw_tokens("İstanbul Øresund Łódź 5µs"),
            ["istanbul", "øresund", "łodz", "5μs"]
        );
        assert_eq!(stem("5μs"), "5μ");
    }
}
