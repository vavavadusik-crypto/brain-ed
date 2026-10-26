//! Query-only normalization for the existing unicode61 FTS index.
//!
//! This is a small inflection trimmer, not a lemmatizer or full Snowball.
//! Stems are literal prefixes of the input, never rewritten dictionary words.
//! The unchanged index can therefore answer them with FTS5 quoted `term*`.

/// Function words that would make an OR query match nearly every message.
pub const STOP_WORDS: &[&str] = &[
    "a",
    "an",
    "as",
    "at",
    "be",
    "by",
    "do",
    "he",
    "i",
    "in",
    "is",
    "it",
    "me",
    "my",
    "of",
    "on",
    "or",
    "so",
    "to",
    "us",
    "we",
    "а",
    "бы",
    "в",
    "во",
    "да",
    "до",
    "же",
    "за",
    "и",
    "из",
    "к",
    "ко",
    "ли",
    "мы",
    "на",
    "не",
    "но",
    "ну",
    "о",
    "об",
    "он",
    "от",
    "по",
    "с",
    "со",
    "то",
    "ты",
    "у",
    "я",
    "её",
    "ее",
    "их",
    "ему",
    "ей",
    "какое",
    "каких",
    "которое",
    "которых",
    "the",
    "and",
    "for",
    "with",
    "what",
    "why",
    "how",
    "when",
    "where",
    "who",
    "was",
    "were",
    "are",
    "did",
    "does",
    "this",
    "that",
    "from",
    "into",
    "about",
    "have",
    "has",
    "had",
    "our",
    "you",
    "your",
    "его",
    "она",
    "они",
    "оно",
    "что",
    "как",
    "так",
    "где",
    "когда",
    "почему",
    "зачем",
    "кто",
    "чем",
    "это",
    "этот",
    "эта",
    "эти",
    "тот",
    "все",
    "всё",
    "был",
    "была",
    "были",
    "было",
    "для",
    "при",
    "про",
    "над",
    "под",
    "без",
    "или",
    "уже",
    "еще",
    "ещё",
    "нас",
    "нам",
    "вас",
    "мне",
    "меня",
    "там",
    "тут",
    "здесь",
    "сейчас",
    "потом",
    "который",
    "которые",
    "какие",
    "какой",
    "какая",
    "чтобы",
    "если",
    "тогда",
    "между",
    "после",
    "перед",
    "через",
    "очень",
    "можно",
    "нужно",
    "надо",
];

/// Lowercase tokens, preserving identifiers for literal FTS quoting.
pub fn terms(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|term| !term.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// A single shared vocabulary for core BM25 and archive question search.
pub fn is_stop_word(term: &str) -> bool {
    STOP_WORDS.contains(&term)
}

/// A conservative query prefix: retain at least four letters and never stem
/// mixed scripts, numbers or identifiers. Short words still match literally.
/// Irregular forms and changes inside a root are deliberately not guessed.
pub fn inflection_prefix(term: &str) -> &str {
    if term.chars().count() < 5 {
        return term;
    }
    let russian = term.chars().all(|c| ('а'..='я').contains(&c) || c == 'ё');
    let english = term.chars().all(|c| c.is_ascii_lowercase());
    let suffixes: &[&str] = if russian {
        &[
            "ившись",
            "ывшись",
            "авшись",
            "яться",
            "аться",
            "иться",
            "ились",
            "ались",
            "яются",
            "аются",
            "иями",
            "ями",
            "ами",
            "ого",
            "его",
            "ому",
            "ему",
            "ыми",
            "ими",
            "иях",
            "ия",
            "ии",
            "ию",
            "ью",
            "ала",
            "али",
            "ало",
            "ить",
            "ать",
            "ять",
            "еть",
            "ешь",
            "ете",
            "ют",
            "ут",
            "ет",
            "ит",
            "ая",
            "яя",
            "ое",
            "ее",
            "ые",
            "ие",
            "ый",
            "ий",
            "ой",
            "ую",
            "юю",
            "ом",
            "ем",
            "ам",
            "ям",
            "ах",
            "ях",
            "ов",
            "ев",
            "ей",
            "а",
            "я",
            "ы",
            "и",
            "у",
            "ю",
            "е",
            "о",
            "ь",
        ]
    } else if english {
        &[
            "ingly", "edly", "ing", "ied", "ies", "ed", "es", "s", "e", "y",
        ]
    } else {
        return term;
    };
    for suffix in suffixes {
        if let Some(stem) = term.strip_suffix(suffix) {
            // English -es normally removes just -s (rules -> rule); sibilants
            // take -es (branches -> branch). Keep words ending -ss intact.
            if english
                && *suffix == "es"
                && !["s", "x", "z", "ch", "sh"]
                    .iter()
                    .any(|end| stem.ends_with(end))
            {
                continue;
            }
            if english && *suffix == "s" && term.ends_with("ss") {
                continue;
            }
            if stem.chars().count() >= 4 {
                // Silent -e: compile/compiled/compiling share compil. Doubled
                // consonants: planned/planning share plan with plans.
                let mut stem = stem;
                if english {
                    if stem.len() >= 5 && stem.ends_with('e') {
                        stem = &stem[..stem.len() - 1];
                    }
                    if matches!(*suffix, "ing" | "ingly" | "ed" | "edly") && stem.len() >= 5 {
                        let bytes = stem.as_bytes();
                        if bytes[bytes.len() - 1] == bytes[bytes.len() - 2]
                            && b"bdgmnprt".contains(&bytes[bytes.len() - 1])
                        {
                            stem = &stem[..stem.len() - 1];
                        }
                    }
                }
                return stem;
            }
        }
    }
    term
}

/// Builds a quoted OR expression, deduplicated after normalization. Explicit
/// incremental matching may also prefix short tokens and identifiers; normal
/// natural-language search keeps those literal to avoid broad code matches.
pub fn bm25_query(text: &str, incremental: bool) -> String {
    let mut result = Vec::new();
    for term in terms(text) {
        if is_stop_word(&term) {
            continue;
        }
        let natural_word = term.chars().all(|c| c.is_ascii_lowercase())
            || term.chars().all(|c| ('а'..='я').contains(&c) || c == 'ё');
        let stem = inflection_prefix(&term);
        let prefix = incremental || (natural_word && stem.chars().count() >= 4);
        let quoted = format!("\"{stem}\"{}", if prefix { "*" } else { "" });
        if !result.contains(&quoted) {
            result.push(quoted);
        }
    }
    result.join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_identifiers_numbers_and_short_words_are_not_stemmed() {
        assert_eq!(
            bm25_query("8803 parse_config_v2 v12 C++ SMC", false),
            "\"8803\" OR \"parse_config_v2\" OR \"v12\" OR \"c\" OR \"smc\""
        );
        assert_eq!(
            bm25_query("8803 parse_config_v2", true),
            "\"8803\"* OR \"parse_config_v2\"*"
        );
    }

    #[test]
    fn grammar_is_removed_and_normalized_forms_are_deduplicated() {
        assert_eq!(
            bm25_query("Какие правила работы агентов", false),
            "\"правил\"* OR \"работ\"* OR \"агент\"*"
        );
        assert_eq!(bm25_query("Агент АГЕНТЫ агентами", false), "\"агент\"*");
        assert_eq!(bm25_query("что как где и the for a", false), "");
        assert_eq!(
            bm25_query("NEAR(\"agents\") OR* column:8803", false),
            "\"near\"* OR \"agent\"* OR \"column\"* OR \"8803\""
        );
    }

    #[test]
    fn every_prefix_is_a_literal_nonempty_slice_with_a_four_letter_floor() {
        for word in [
            "работать",
            "агентами",
            "модель",
            "памятью",
            "rules",
            "compiled",
            "planned",
            "policies",
            "ss",
            "class",
            "12345",
            "x_y",
            "ёЖИК",
            "漢字",
        ] {
            let stem = inflection_prefix(word);
            assert!(word.starts_with(stem));
            assert!(!stem.is_empty());
            assert!(stem == word || stem.chars().count() >= 4);
        }
        assert_eq!(inflection_prefix("class"), "class");
    }
}
