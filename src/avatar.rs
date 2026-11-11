//! Author avatars: initials and a stable color per author.

/// First letter of the first name and of the last name ("Antoine Jaquet" → "AJ").
/// A single word gives one letter, except `john.doe` / `john_doe` which gives "JD".
pub fn initials(name: &str) -> String {
    let mut words: Vec<&str> = name.split_whitespace().collect();
    if words.len() == 1 {
        words = words[0].split(['.', '_']).filter(|w| !w.is_empty()).collect();
    }
    let first_letter = |w: &str| {
        w.chars().find(|c| c.is_alphanumeric()).and_then(|c| c.to_uppercase().next())
    };
    let letters: Vec<char> = match words.as_slice() {
        [] => vec![],
        [only] => first_letter(only).into_iter().collect(),
        [first, .., last] => first_letter(first).into_iter().chain(first_letter(last)).collect(),
    };
    if letters.is_empty() { "?".into() } else { letters.into_iter().collect() }
}

/// Hue in `0.0..1.0` derived from the author name (FNV-1a, so it is the same on every run
/// and every platform, unlike `std`'s randomly seeded hasher).
pub fn hue(name: &str) -> f32 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.trim().to_lowercase().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h % 360) as f32 / 360.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_and_last_name() {
        assert_eq!(initials("Antoine Jaquet"), "AJ");
        assert_eq!(initials("antoine jaquet"), "AJ");
        assert_eq!(initials("  Jean  Pierre   Dupont "), "JD"); // first and last word
    }

    #[test]
    fn single_word_gives_one_letter() {
        assert_eq!(initials("antoine"), "A");
        assert_eq!(initials("ROOT"), "R");
    }

    #[test]
    fn dotted_or_underscored_logins_are_split() {
        assert_eq!(initials("john.doe"), "JD");
        assert_eq!(initials("john_doe"), "JD");
        assert_eq!(initials("a..b"), "AB");
    }

    #[test]
    fn non_alphanumeric_prefixes_and_accents() {
        assert_eq!(initials("(bot) [ci]"), "BC");
        assert_eq!(initials("Émile Zola"), "ÉZ");
        assert_eq!(initials("ßeta Gamma"), "SG"); // ß uppercases to "SS", only one letter kept
    }

    #[test]
    fn empty_or_symbol_only_names_give_a_placeholder() {
        assert_eq!(initials(""), "?");
        assert_eq!(initials("   "), "?");
        assert_eq!(initials("---"), "?");
    }

    #[test]
    fn hue_is_stable_in_range_and_case_insensitive() {
        assert_eq!(hue("Antoine Jaquet"), hue("antoine jaquet "));
        assert_ne!(hue("Antoine"), hue("Bob"));
        for n in ["a", "b", "Antoine Jaquet", "", "日本語"] {
            assert!((0.0..1.0).contains(&hue(n)), "{n}");
        }
        // Pinned value: guards against an accidental change of the hash (colors would all shift).
        assert_eq!(hue("antoine"), 151.0 / 360.0);
    }
}
