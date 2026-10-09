//! Temporal questions answered the way a person asks them.
//!
//! Three commands share one idea: a period of work is only useful when it is
//! named by something a human recognises. Counting rows is easy; picking the
//! one record that stands for a week of work is the whole problem, because the
//! stored `label` is often a markdown heading (`Память`, `Назначение`,
//! `SKILL.md`) reused across hundreds of records, or a 300-character prompt
//! pasted verbatim. Both are useless as an answer, so titles are recovered
//! from the body and ranked against corpus-wide statistics.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use chrono::{DateTime, Datelike, Local, LocalResult, NaiveDate, TimeZone};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::AsOf;

// ---------- tuning ----------

/// Upper bound on periods shown by `when`; more than this stops being a shape
/// the reader can hold in their head.
const MAX_PERIODS: usize = 16;
/// Rows pulled from FTS before ranking.
const MAX_MATCHES: usize = 2_000;
/// Alternatives listed next to a period's representative.
const ALSO_PER_PERIOD: usize = 2;
/// Themes reported for one timeline period.
const MAX_THEMES: usize = 8;
/// Decisions and pitfalls reported for one timeline period.
const MAX_HIGHLIGHTS: usize = 6;
/// Two themes covering mostly the same records are one theme.
const THEME_OVERLAP: f64 = 0.6;
/// A theme has to name at least this many records.
const MIN_THEME_RECORDS: usize = 3;
/// A theme also has to appear under this many different titles.
///
/// Counting records alone made a template heading look like a subject: 68 turns
/// of a reflection form all labelled `Reusable knowledge` were reported as what
/// July was spent on. Work leaves differently named traces; a form does not.
const MIN_THEME_TITLES: usize = 3;
/// Below this inverse document frequency a word describes the corpus, not the
/// period.
const MIN_THEME_IDF: f64 = 0.25;
/// A title longer than this gets cut at a word boundary.
const TITLE_LIMIT: usize = 90;
/// Lines of a body examined while looking for a title.
const BODY_LINES_SCANNED: usize = 40;
/// Neighbours and time-context entries reported by `explain`.
const MAX_CONTEXT: usize = 6;
/// Half-width of the window `explain` reads around a decision, in days.
const EXPLAIN_WINDOW_DAYS: i64 = 3;

const MONTHS_NOMINATIVE: [&str; 12] = [
    "январь",
    "февраль",
    "март",
    "апрель",
    "май",
    "июнь",
    "июль",
    "август",
    "сентябрь",
    "октябрь",
    "ноябрь",
    "декабрь",
];

const MONTHS_GENITIVE: [&str; 12] = [
    "января",
    "февраля",
    "марта",
    "апреля",
    "мая",
    "июня",
    "июля",
    "августа",
    "сентября",
    "октября",
    "ноября",
    "декабря",
];

const STOP_WORDS: [&str; 139] = [
    // Structural Russian words that describe any record, never a topic.
    "и",
    "в",
    "во",
    "не",
    "на",
    "с",
    "со",
    "что",
    "как",
    "но",
    "для",
    "по",
    "из",
    "за",
    "от",
    "до",
    "то",
    "же",
    "уже",
    "или",
    "если",
    "это",
    "этот",
    "эта",
    "эти",
    "тот",
    "там",
    "тут",
    "был",
    "была",
    "было",
    "были",
    "быть",
    "есть",
    "нет",
    "да",
    "все",
    "всё",
    "весь",
    "вся",
    "его",
    "её",
    "их",
    "он",
    "она",
    "они",
    "мы",
    "вы",
    "ты",
    "я",
    "мой",
    "наш",
    "при",
    "над",
    "под",
    "без",
    "про",
    "чтобы",
    "когда",
    "где",
    "почему",
    "так",
    "ещё",
    "еще",
    "очень",
    "только",
    "также",
    "затем",
    "после",
    "перед",
    "через",
    "между",
    "который",
    "которые",
    "которая",
    "надо",
    "нужно",
    "можно",
    // English equivalents: the corpus mixes both languages.
    "the",
    "a",
    "an",
    "and",
    "or",
    "but",
    "if",
    "then",
    "else",
    "for",
    "of",
    "to",
    "in",
    "on",
    "at",
    "by",
    "with",
    "from",
    "as",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "have",
    "has",
    "had",
    "do",
    "does",
    "did",
    "will",
    "would",
    "should",
    "can",
    "could",
    "this",
    "that",
    "these",
    "those",
    "it",
    "its",
    "not",
    "no",
    "so",
    "just",
    "very",
    "also",
    "about",
    "into",
    "all",
    "any",
    "some",
    "more",
    "most",
    "than",
    "when",
    "where",
    "which",
    "while",
    "what",
];

/// Conversation-role prefixes: a transcript pasted verbatim.
const CHAT_ROLE_PREFIXES: [&str; 8] = [
    "system:",
    "user:",
    "assistant:",
    "prompt:",
    "промпт:",
    "задача:",
    "задание:",
    "роль:",
];

/// Second-person address. A record of work has no addressee.
const SECOND_PERSON: [&str; 14] = [
    "ты", "тебе", "тебя", "твой", "твоя", "твои", "твоё", "вы", "вам", "ваш", "ваша", "you",
    "your", "yours",
];

/// Section headers of a task briefing.
///
/// A handover names the ground before it names the work — `Репозиторий: …`,
/// `Рабочий каталог: …` — and the resulting label reads as a topic on every
/// statistical test while being a form. They also mark where such a label stops
/// saying anything, so the same list cuts overlong titles down to their first
/// clause.
const BRIEFING_HEADERS: [&str; 12] = [
    "репозиторий:",
    "рабочий каталог:",
    "рабочая директория:",
    "рабочее дерево:",
    "ограничения:",
    "приёмка:",
    "лейн:",
    "repo:",
    "repository:",
    "working directory:",
    "lane:",
    "constraints:",
];

/// Imperative verbs that open an order.
///
/// Mood is what separates a prompt from a record, and mood in both languages of
/// this corpus is carried by the first word: `Прочитай файл…`, `Provide the
/// report…`. A lexicon is unavoidable here — no statistic distinguishes a
/// command from a statement — so it is kept to verbs that actually open the
/// pasted prompts, and it is checked against the first word only.
const IMPERATIVE_VERBS: [&str; 40] = [
    "прочитай",
    "выполни",
    "сделай",
    "создай",
    "проверь",
    "запусти",
    "добавь",
    "исправь",
    "продолжи",
    "изучи",
    "собери",
    "опиши",
    "найди",
    "верни",
    "оформи",
    "используй",
    "работай",
    "составь",
    "подготовь",
    "убедись",
    "проанализируй",
    "напиши",
    "начни",
    "заверши",
    "read",
    "write",
    "provide",
    "create",
    "analyze",
    "analyse",
    "review",
    "check",
    "ensure",
    "generate",
    "summarize",
    "return",
    "use",
    "follow",
    "act",
    "answer",
];

/// Obligation markers. One is normal prose, two in the same opening is a rule
/// list being handed to somebody.
const OBLIGATION_MARKERS: [&str; 15] = [
    "ты должен",
    "вы должны",
    "необходимо",
    "обязательно",
    "железные правила",
    "жёсткие правила",
    "не должен",
    "никогда не",
    "верни только",
    "строго из набора",
    "you must",
    "you should",
    "do not ",
    "output only",
    "make sure",
];

/// Words that mean something went wrong.
const FAILURE_WORDS: [&str; 24] = [
    "ошибк",
    "сломал",
    "упал",
    "падает",
    "не работает",
    "не удалось",
    "не смог",
    "провал",
    "грабл",
    "баг",
    "сбой",
    "завис",
    "отказ",
    "таймаут",
    "timeout",
    "fail",
    "error",
    "panic",
    "traceback",
    "exception",
    "broken",
    "crash",
    "regression",
    "утечк",
];

/// Words that mean it worked. A record stored as an error that carries more of
/// these than of the failure words is a green run, not a pitfall.
const SUCCESS_WORDS: [&str; 14] = [
    "прошёл",
    "прошел",
    "прошли",
    "готово",
    "зелён",
    "зелен",
    "успешно",
    "исправлено",
    "починено",
    "passed",
    "success",
    "fixed",
    "resolved",
    "all green",
];

// ---------- shared record model ----------

/// One stored revision together with everything ranking needs.
#[derive(Clone)]
struct Record {
    node_id: String,
    kind: String,
    label: String,
    body: String,
    source: Option<String>,
    /// World time at which the recorded fact started being true.
    valid_from: i64,
    /// System time at which Brain learned the fact.
    recorded_at: i64,
    salience: f64,
    /// FTS relevance; lower is better. Zero when the query had no lexical part.
    rank: f64,
}

/// Namespace and bi-temporal coordinate shared by every temporal query.
#[derive(Clone, Copy)]
struct Scope<'a> {
    space_id: &'a str,
    as_of: AsOf,
}

/// Corpus-wide statistics that separate a descriptive title from a structural
/// one.
///
/// Without them every heuristic degenerates into a hand-written blacklist:
/// `Память` is a bad title here only because 58 other records carry it too.
struct Corpus {
    total: usize,
    label_df: HashMap<String, usize>,
    word_df: HashMap<String, usize>,
    degree: HashMap<String, usize>,
    /// File stem to the sentence the corpus uses for it.
    slug_titles: HashMap<String, String>,
}

impl Corpus {
    fn load(conn: &Connection, scope: Scope<'_>) -> Result<Self, String> {
        let mut label_df: HashMap<String, usize> = HashMap::new();
        let mut total = 0usize;
        {
            let mut stmt = conn
                .prepare(
                    "SELECT label, COUNT(*)
                     FROM node_versions
                     WHERE space_id = ?1
                       AND valid_from <= ?2
                       AND (valid_to IS NULL OR ?2 < valid_to)
                       AND recorded_at <= ?3
                       AND (superseded_at IS NULL OR ?3 < superseded_at)
                     GROUP BY label",
                )
                .map_err(|error| error.to_string())?;
            let rows = stmt
                .query_map(
                    params![
                        scope.space_id,
                        scope.as_of.valid_at,
                        scope.as_of.recorded_at
                    ],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize)),
                )
                .map_err(|error| error.to_string())?;
            for row in rows {
                let (label, count) = row.map_err(|error| error.to_string())?;
                total += count;
                *label_df.entry(normalize(&label)).or_insert(0) += count;
            }
        }

        // Word frequency is derived from the label histogram rather than from a
        // second scan: identical labels contribute identical words.
        let mut word_df: HashMap<String, usize> = HashMap::new();
        for (label, count) in &label_df {
            for word in distinct_terms(label) {
                *word_df.entry(word).or_insert(0) += *count;
            }
        }

        let mut degree: HashMap<String, usize> = HashMap::new();
        {
            let mut stmt = conn
                .prepare(
                    "WITH active_edges AS (
                         SELECT e.from_node_id, e.to_node_id
                         FROM edges e
                         JOIN edge_versions ev
                           ON ev.space_id = e.space_id AND ev.edge_id = e.id
                         WHERE e.space_id = ?1
                           AND ev.valid_from <= ?2
                           AND (ev.valid_to IS NULL OR ?2 < ev.valid_to)
                           AND ev.recorded_at <= ?3
                           AND (ev.superseded_at IS NULL OR ?3 < ev.superseded_at)
                     )
                     SELECT nid, COUNT(*) FROM (
                         SELECT from_node_id AS nid FROM active_edges
                         UNION ALL
                         SELECT to_node_id AS nid FROM active_edges
                     ) GROUP BY nid",
                )
                .map_err(|error| error.to_string())?;
            let rows = stmt
                .query_map(
                    params![
                        scope.space_id,
                        scope.as_of.valid_at,
                        scope.as_of.recorded_at
                    ],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize)),
                )
                .map_err(|error| error.to_string())?;
            for row in rows {
                let (node_id, count) = row.map_err(|error| error.to_string())?;
                degree.insert(node_id, count);
            }
        }

        Ok(Self {
            total: total.max(1),
            label_df,
            word_df,
            degree,
            slug_titles: collect_slug_titles(conn, scope)?,
        })
    }

    /// Rarity of one word, floored by how often it occurs inside the period so
    /// that titles recovered from bodies cannot fake distinctiveness.
    fn word_rarity(&self, word: &str, local_frequency: usize) -> f64 {
        let frequency = self
            .word_df
            .get(word)
            .copied()
            .unwrap_or(0)
            .max(local_frequency);
        idf(frequency, self.total)
    }

    /// Rarity of a whole phrase, averaged over its content words.
    fn phrase_rarity(&self, phrase: &str) -> f64 {
        let words = terms(phrase);
        if words.is_empty() {
            return 0.0;
        }
        let total: f64 = words.iter().map(|word| self.word_rarity(word, 0)).sum();
        total / words.len() as f64
    }

    /// Rarity of a title, judged as a label when it is one and by its words
    /// otherwise.
    ///
    /// `label_rarity` alone cannot judge a title mined out of a body: such a
    /// string is not a label anywhere, so its document frequency is zero and it
    /// scores as maximally distinctive. That is how `Отчёт 1` — a heading
    /// fragment of the most common word in the corpus — outranked every written
    /// title.
    fn title_rarity(&self, title: &str) -> f64 {
        match self.label_df.get(&normalize(title)).copied() {
            Some(frequency) if frequency > 0 => idf(frequency, self.total),
            _ => self.phrase_rarity(title),
        }
    }

    fn degree_of(&self, node_id: &str) -> usize {
        self.degree.get(node_id).copied().unwrap_or(0)
    }
}

/// Maps every `slug.md` the corpus links to onto the words it links it under.
///
/// Memory files are stored one chunk per record and labelled with their own
/// file name, so `asset-skills-graph-binding` was the best title available for
/// four different records. The sentence a person wrote for that file is stored
/// too — in the index that lists it, as `[Asset-навыки в графе](…md) — …` — just
/// in another record. Reading the links once recovers the whole naming table,
/// and only the few dozen index records carry any.
fn collect_slug_titles(
    conn: &Connection,
    scope: Scope<'_>,
) -> Result<HashMap<String, String>, String> {
    let mut titles: HashMap<String, String> = HashMap::new();
    let mut statement = conn
        .prepare(
            "SELECT body FROM node_versions
             WHERE space_id = ?1
               AND body LIKE '%](%'
               AND valid_from <= ?2
               AND (valid_to IS NULL OR ?2 < valid_to)
               AND recorded_at <= ?3
               AND (superseded_at IS NULL OR ?3 < superseded_at)",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(
            params![
                scope.space_id,
                scope.as_of.valid_at,
                scope.as_of.recorded_at
            ],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| error.to_string())?;
    for row in rows {
        let body = row.map_err(|error| error.to_string())?;
        for (slug, text) in markdown_links(&body) {
            titles.entry(slug).or_insert(text);
        }
    }
    Ok(titles)
}

/// Every `[text](target)` pair of a body, keyed by the target's file stem.
fn markdown_links(body: &str) -> Vec<(String, String)> {
    let mut links = Vec::new();
    let mut rest = body;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let Some(close) = after.find("](") else { break };
        let text = clean_line(&after[..close]);
        let target_start = close + 2;
        let Some(end) = after[target_start..].find(')') else {
            break;
        };
        let target = after[target_start..target_start + end].trim();
        rest = &after[target_start + end..];

        let stem = target
            .rsplit('/')
            .next()
            .unwrap_or(target)
            .trim_end_matches(".md");
        if char_len(&text) >= 6 && looks_like_slug(stem) && title_quality(&text) >= 0.9 {
            links.push((normalize(stem), text));
        }
    }
    links
}

fn idf(frequency: usize, total: usize) -> f64 {
    let total = total.max(1) as f64;
    (((total + 1.0) / (frequency as f64 + 1.0)).ln() / (total + 1.0).ln()).clamp(0.0, 1.0)
}

// ---------- text handling ----------

fn normalize(text: &str) -> String {
    text.trim().to_lowercase()
}

/// Splits text into content words, dropping stop words and pure punctuation.
fn terms(text: &str) -> Vec<String> {
    let stops: HashSet<&str> = STOP_WORDS.iter().copied().collect();
    text.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '.')
        .filter_map(|word| {
            let word = word.trim_matches(|c: char| c == '-' || c == '.' || c == '_');
            let lowered = word.to_lowercase();
            let has_letter = lowered.chars().any(char::is_alphabetic);
            if lowered.chars().count() >= 3 && has_letter && !stops.contains(lowered.as_str()) {
                Some(lowered)
            } else {
                None
            }
        })
        .collect()
}

/// Unigrams plus adjacent bigrams, deduplicated.
///
/// Bigrams matter because the topics people ask about are two words long
/// (`release board`, `loop engine`) and a unigram split loses them.
fn distinct_terms(text: &str) -> Vec<String> {
    let words = terms(text);
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for index in 0..words.len() {
        if seen.insert(words[index].clone()) {
            result.push(words[index].clone());
        }
        if index + 1 < words.len() {
            let bigram = format!("{} {}", words[index], words[index + 1]);
            if seen.insert(bigram.clone()) {
                result.push(bigram);
            }
        }
    }
    result
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

/// Cuts a title at a word boundary so it still reads as a phrase.
fn shorten(text: &str, limit: usize) -> String {
    let text = text.trim();
    if char_len(text) <= limit {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(limit).collect();
    if let Some(space) = cut.rfind(' ')
        && space > limit / 2
    {
        cut.truncate(space);
    }
    format!("{}…", cut.trim_end_matches([',', ';', '—', '-', ' ']))
}

/// Clause separators a sentence can honestly be cut at.
const CLAUSE_BREAKS: [&str; 7] = [". ", ": ", "; ", " — ", " – ", " → ", ", "];

/// Cuts an overlong title where its first thought ends.
///
/// A hard cut at the character limit turns a pasted paragraph into a ragged
/// fragment with an ellipsis — `Продолжаю Atlas Board по протоколу
/// непрерывности (8.6): сначала STATE.md → git…` — which is neither a
/// title nor a quote. Cutting at the clause boundary before the limit leaves a
/// phrase that stands on its own, and stopping in front of a briefing header
/// drops the handover boilerplate the sentence turns into.
fn condense(text: &str, limit: usize) -> String {
    let text = text.trim();
    if char_len(text) <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    let lowered = head.to_lowercase();
    let floor = limit / 3;

    // A header means the sentence has stopped describing and started briefing.
    let header_cut = BRIEFING_HEADERS
        .iter()
        .filter_map(|header| lowered.find(header))
        .map(|byte| lowered[..byte].chars().count())
        .filter(|position| *position > floor)
        .min();
    // The earliest boundary past the floor, not the latest one inside the
    // limit: a title is the first complete thought, and reaching for the last
    // separator that still fits leaves the next clause dangling half-said.
    let clause_cut = CLAUSE_BREAKS
        .iter()
        .filter_map(|separator| {
            head.match_indices(separator)
                .map(|(byte, matched)| head[..byte + matched.len()].chars().count())
                .find(|position| *position > floor)
        })
        .min();

    match header_cut.or(clause_cut) {
        Some(position) => {
            let cut: String = head.chars().take(position).collect();
            cut.trim_end_matches([',', ';', ':', '—', '–', '→', '-', '.', ' '])
                .to_string()
        }
        None => shorten(text, limit),
    }
}

/// Strips markdown decoration that would otherwise leak into a title.
fn clean_line(line: &str) -> String {
    let line = line.trim_start_matches(['#', '>', '-', '*', ' ', '\t']);
    let mut cleaned = String::with_capacity(line.len());
    for ch in line.chars() {
        if !matches!(ch, '*' | '`' | '#' | '_') {
            cleaned.push(ch);
        }
    }
    cleaned.trim().to_string()
}

/// First word of a text, stripped of markdown and punctuation.
fn first_word(text: &str) -> String {
    text.split(|c: char| !c.is_alphanumeric())
        .find(|word| !word.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// True when text is an order given to somebody rather than a statement about
/// work that happened.
///
/// Corpus evidence: 411 of 10 590 records begin with a pasted prompt, and they
/// win on every neutral criterion — long, grammatical, lexically unique. What
/// separates them is grammatical: a prompt names an addressee and commands,
/// a record of work states. Only the opening is examined, so a body that quotes
/// an instruction further down is not condemned for it.
fn looks_like_instruction(text: &str) -> bool {
    let head: String = normalize(text).chars().take(200).collect();
    if CHAT_ROLE_PREFIXES
        .iter()
        .any(|prefix| head.starts_with(prefix))
    {
        return true;
    }
    // A pasted file reference or an absolute path opening a text: the address of
    // the work, handed over, not the work.
    if head.starts_with('@') || head.starts_with('/') || head.starts_with("./") {
        return true;
    }
    if BRIEFING_HEADERS.iter().any(|header| head.contains(header)) {
        return true;
    }
    // A text that opens by naming a directory is handing over a workspace —
    // "Задача в /home/<user>/projects/<name> (работай ТОЛЬКО в этом…" — rather
    // than describing work. A record about work cites its paths further in,
    // once it has said something.
    if head
        .chars()
        .take(60)
        .collect::<String>()
        .split_whitespace()
        .any(|token| token.matches('/').count() >= 2)
    {
        return true;
    }
    let opening = first_word(&head);
    if SECOND_PERSON.contains(&opening.as_str()) || IMPERATIVE_VERBS.contains(&opening.as_str()) {
        return true;
    }
    // Anywhere in the opening, not only in first position: `…рабочее дерево, в
    // котором ты работаешь` is as much an order as `Ты должен…`, and only the
    // second form was caught. Split here rather than through `terms`, which
    // drops words shorter than three letters and so never saw `ты` or `вы`.
    if head
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| SECOND_PERSON.contains(&word))
    {
        return true;
    }
    OBLIGATION_MARKERS
        .iter()
        .filter(|marker| head.contains(**marker))
        .count()
        >= 2
}

/// A file stem standing in for a title: `asset-skills-graph-binding`.
///
/// Memory files are stored under their own name, and that name passes every
/// test for a written title — short, unique, well formed — while reading as a
/// path. It is a name, so it is not thrown away; it is only stopped from
/// out-ranking the sentence a person actually wrote.
fn looks_like_slug(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.contains(char::is_whitespace)
        && trimmed.matches(['-', '_']).count() >= 2
        && trimmed.chars().any(char::is_alphabetic)
        && !trimmed.chars().any(char::is_uppercase)
}

/// Turns a slug into something readable when nothing better is stored.
fn deslugify(slug: &str) -> String {
    let words = slug.replace(['-', '_'], " ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => words,
    }
}

/// A token carrying a count rather than a word: `189/189`, `0`, `2026-07-29`.
fn is_numeric_token(token: &str) -> bool {
    let trimmed = token.trim_matches(|c: char| !c.is_alphanumeric());
    !trimmed.is_empty()
        && trimmed.chars().any(|c| c.is_ascii_digit())
        && !trimmed.chars().any(char::is_alphabetic)
}

/// Share of a text made of bare counts.
///
/// A green run pasted as a title — `01 source-монолит: 189/189, MISSING 0, HASH
/// MISMATCH 0` — is unique, well sized and lexically rare, so every other test
/// promotes it. What gives it away is that half of it is numbers.
fn numeric_share(text: &str) -> f64 {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    if tokens.is_empty() {
        return 0.0;
    }
    let numeric = tokens
        .iter()
        .filter(|token| is_numeric_token(token))
        .count();
    numeric as f64 / tokens.len() as f64
}

/// A run of at least eight hex digits, hyphens ignored: the shape every
/// session, thread and commit identifier in this corpus has.
fn is_hex_blob(token: &str) -> bool {
    let stripped: String = token.chars().filter(|c| *c != '-' && *c != '_').collect();
    stripped.chars().count() >= 8
        && stripped.chars().all(|c| c.is_ascii_hexdigit())
        && stripped.chars().any(|c| c.is_ascii_digit())
}

/// True when a string identifies a machine artefact instead of naming a topic.
///
/// These pass every other check — unique, short, well formed — and answer
/// nothing: `Thread 019f864e-672f-79d2-bebb-589fc6bd61d8`, `.hermeshistory`,
/// `SKILL.md`.
fn is_opaque_identifier(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.starts_with('.') {
        return true;
    }
    let is_single_token = !trimmed.contains(char::is_whitespace);
    if is_single_token
        && let Some((_, extension)) = trimmed.rsplit_once('.')
        && (1..=5).contains(&extension.chars().count())
        && extension.chars().all(char::is_alphanumeric)
    {
        return true;
    }

    let mut carries_identifier = false;
    let mut words = 0usize;
    let mut letters = 0usize;
    for token in trimmed.split_whitespace() {
        if is_hex_blob(token) {
            carries_identifier = true;
            continue;
        }
        let alphabetic = token.chars().filter(|c| c.is_alphabetic()).count();
        if alphabetic >= 3 {
            words += 1;
            letters += alphabetic;
        }
    }
    words == 0 || (carries_identifier && letters < 12)
}

/// Whether a record actually describes something going wrong.
///
/// The graph labels a whole class of session turns `error`, so a green test run
/// (`1017/1017, fail 0`) sits in the same bucket as a real crash and outranks it
/// by length. Counting both vocabularies and requiring failure to win separates
/// them without hard-coding either record.
fn reads_as_problem(record: &Record) -> bool {
    let opening: String = record.body.chars().take(400).collect();
    let text = normalize(&format!("{} {}", record.label, opening));
    let failures = FAILURE_WORDS
        .iter()
        .filter(|word| text.contains(**word))
        .count();
    let successes = SUCCESS_WORDS
        .iter()
        .filter(|word| text.contains(**word))
        .count();
    failures > successes
}

/// Text of the first markdown link, which in this corpus is how memory bodies
/// name themselves: `[Перегрев VS Code (фикс)](vscode-thermal-fix.md) — …`.
fn markdown_link_text(body: &str) -> Option<String> {
    let open = body.find('[')?;
    let close = body[open..].find("](")? + open;
    let text = clean_line(&body[open + 1..close]);
    (char_len(&text) >= 6).then_some(text)
}

/// First self-contained phrase of a body that reads as a statement about work.
///
/// Taking the very first line made every pasted prompt name itself with its own
/// opening order. Walking further down finds the line where such a body gets to
/// the point, and the rejected opening is still returned when nothing better
/// exists, so a title is always produced.
fn leading_phrase(body: &str) -> Option<String> {
    let mut fallback: Option<String> = None;
    for raw in body.lines().take(BODY_LINES_SCANNED) {
        let line = clean_line(raw);
        if char_len(&line) < 8 {
            continue;
        }
        let phrase = line
            .split_once(". ")
            .map_or(line.as_str(), |(head, _)| head)
            .trim()
            .to_string();
        if char_len(&phrase) < 8 {
            continue;
        }
        if looks_like_instruction(&phrase) || is_opaque_identifier(&phrase) {
            fallback.get_or_insert(phrase);
            continue;
        }
        return Some(phrase);
    }
    fallback
}

/// How much a string reads as a written title rather than machine output.
///
/// This has to be judged before truncation. Scoring the displayed title made
/// every candidate look well-sized, so pasted prompts and stack traces — which
/// are unique, and therefore maximally "rare" — went straight to the top.
fn title_quality(text: &str) -> f64 {
    let text = text.trim();
    let mut score = match char_len(text) {
        0..=7 => 0.15,
        8..=90 => 1.00,
        91..=140 => 0.40,
        _ => 0.10,
    };
    if text.matches('/').count() >= 3 || text.contains("Traceback") || text.contains("=====") {
        score *= 0.30;
    }
    if text.split_whitespace().count() > 16 {
        score *= 0.50;
    }
    if looks_like_instruction(text) {
        score *= 0.20;
    }
    if is_opaque_identifier(text) {
        score *= 0.25;
    }
    if numeric_share(text) >= 0.3 {
        score *= 0.35;
    }
    if looks_like_slug(text) {
        score *= 0.60;
    }
    score
}

/// True when a label was written to name this record.
///
/// Genericness is relative to corpus size: one label in ten thousand is a
/// title, a label repeated sixty times is a markdown heading.
fn label_is_descriptive(label: &str, corpus: &Corpus) -> bool {
    let frequency = corpus.label_df.get(&normalize(label)).copied().unwrap_or(0);
    let generic_from = 3.max(corpus.total / 1_000);
    frequency <= generic_from
        && !looks_like_slug(label)
        && title_quality(label) >= 0.9
        && !terms(label).is_empty()
}

/// Best human-readable title for a record, before and after truncation.
///
/// Order matters: a specific label always wins, because it was written as a
/// title. Only when the label is boilerplate (or a pasted prompt) is the body
/// mined, and the raw label is the last resort so something is always shown.
fn title_candidate(record: &Record, corpus: &Corpus) -> String {
    // A file stem the corpus itself has already translated into a sentence:
    // whoever wrote the index chose that wording, so it beats anything mined.
    if looks_like_slug(&record.label)
        && let Some(written) = corpus.slug_titles.get(&normalize(&record.label))
    {
        return written.clone();
    }
    if label_is_descriptive(&record.label, corpus) {
        return clean_line(&record.label);
    }
    let mined = markdown_link_text(&record.body).or_else(|| leading_phrase(&record.body));
    match mined {
        Some(text) if title_quality(&text) > title_quality(&record.label) => text,
        _ if looks_like_slug(&record.label) => deslugify(&record.label),
        _ => clean_line(&record.label),
    }
}

fn title_of(record: &Record, corpus: &Corpus) -> String {
    condense(&title_candidate(record, corpus), TITLE_LIMIT)
}

// ---------- ranking ----------

/// How much a record type tells a person about a period.
///
/// Sessions rank last on purpose: they store raw prompts, so they dominate by
/// volume while explaining nothing.
fn kind_weight(kind: &str) -> f64 {
    match kind {
        "decision" => 1.00,
        "error" => 0.95,
        "memory" => 0.90,
        "concept" => 0.80,
        "entity" => 0.75,
        "document" => 0.55,
        "code" => 0.45,
        "session" => 0.30,
        _ => 0.50,
    }
}

/// Composite score deciding which record stands for a period.
///
/// `rank_scale` is the best (lowest) BM25 value in the comparison set; passing
/// zero disables the relevance term for queries that had no lexical part.
fn representativeness(record: &Record, corpus: &Corpus, rank_scale: f64) -> f64 {
    let title = title_candidate(record, corpus);
    let relevance = if rank_scale < 0.0 {
        (record.rank / rank_scale).clamp(0.0, 1.0)
    } else {
        0.5
    };
    let degree = corpus.degree_of(&record.node_id) as f64;
    let degree_score = ((degree + 1.0).ln() / 6.0_f64).min(1.0);
    let body_score = ((record.body.len() as f64 + 1.0).ln() / 600.0_f64.ln()).min(1.0);

    let merit = 0.24 * title_quality(&title)
        + 0.20 * relevance
        + 0.18 * kind_weight(&record.kind)
        + 0.14 * corpus.title_rarity(&title)
        + 0.13 * record.salience
        + 0.06 * body_score
        + 0.05 * degree_score;
    merit * boilerplate_factor(record, &title)
}

/// How much a record is worth as the face of a period.
///
/// Penalising the title alone was not enough: a pasted prompt stored under its
/// own first line is a unique label, so it collected full marks for rarity and
/// relevance and still won the week. What is boilerplate is the record, not
/// just its title. The factor demotes rather than vetoes, so a period that
/// genuinely contains nothing else is still answered.
fn boilerplate_factor(record: &Record, title: &str) -> f64 {
    let is_boilerplate = looks_like_instruction(title)
        || looks_like_instruction(&record.body)
        || is_opaque_identifier(title);
    if is_boilerplate { 0.35 } else { 1.0 }
}

/// Sorts records best-first and keeps one entry per node.
fn rank_records(records: &[Record], corpus: &Corpus) -> Vec<Record> {
    let rank_scale = records
        .iter()
        .map(|record| record.rank)
        .fold(0.0_f64, f64::min);
    let mut scored: Vec<(f64, &Record)> = records
        .iter()
        .map(|record| (representativeness(record, corpus, rank_scale), record))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));

    let mut seen_nodes = HashSet::new();
    let mut seen_titles = HashSet::new();
    let mut result = Vec::new();
    for (_, record) in scored {
        if !seen_nodes.insert(record.node_id.clone()) {
            continue;
        }
        if !seen_titles.insert(normalize(&title_of(record, corpus))) {
            continue;
        }
        result.push(record.clone());
    }
    result
}

fn describe(record: &Record, corpus: &Corpus) -> Value {
    json!({
        "title": title_of(record, corpus),
        "kind": record.kind,
        "kind_ru": kind_ru(&record.kind),
        "node_id": record.node_id,
        "date": human_date(record.valid_from),
        "valid_from": iso_date(record.valid_from),
        "recorded_at": iso_date(record.recorded_at),
        "source": record.source,
        "excerpt": shorten(&clean_line(&record.body), 220),
    })
}

fn kind_ru(kind: &str) -> &'static str {
    match kind {
        "decision" => "решение",
        "error" => "ошибка",
        "memory" => "память",
        "concept" => "понятие",
        "entity" => "сущность",
        "document" => "документ",
        "code" => "код",
        "session" => "сессия",
        _ => "запись",
    }
}

// ---------- calendar ----------

/// Bucket width for grouping records.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Granularity {
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

impl Granularity {
    /// Coarsening order. Hours are deliberately absent: nobody asks what they
    /// were doing at 19:00 two months ago, and the stored hour is usually the
    /// import time anyway.
    const LADDER: [Self; 5] = [
        Self::Day,
        Self::Week,
        Self::Month,
        Self::Quarter,
        Self::Year,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Quarter => "quarter",
            Self::Year => "year",
        }
    }

    fn key(self, moment: DateTime<Local>) -> String {
        match self {
            Self::Day => moment.format("%Y-%m-%d").to_string(),
            Self::Week => {
                let week = moment.iso_week();
                format!("{:04}-W{:02}", week.year(), week.week())
            }
            Self::Month => moment.format("%Y-%m").to_string(),
            Self::Quarter => format!("{}-Q{}", moment.year(), (moment.month() - 1) / 3 + 1),
            Self::Year => moment.year().to_string(),
        }
    }

    fn label(self, moment: DateTime<Local>) -> String {
        match self {
            Self::Day => human_date(moment.timestamp()),
            Self::Week => {
                let weekday = moment.weekday().num_days_from_monday() as i64;
                let start = moment - chrono::Duration::days(weekday);
                let end = start + chrono::Duration::days(6);
                format!(
                    "неделя {} — {}",
                    day_and_month(start),
                    human_date(end.timestamp())
                )
            }
            Self::Month => format!(
                "{} {}",
                MONTHS_NOMINATIVE[(moment.month() - 1) as usize],
                moment.year()
            ),
            Self::Quarter => format!(
                "{} квартал {}",
                ["I", "II", "III", "IV"][((moment.month() - 1) / 3) as usize],
                moment.year()
            ),
            Self::Year => format!("{} год", moment.year()),
        }
    }
}

fn moment_of(seconds: i64) -> DateTime<Local> {
    Local.timestamp_opt(seconds, 0).single().unwrap_or_else(|| {
        Local
            .timestamp_opt(0, 0)
            .single()
            .expect("Unix epoch is valid")
    })
}

fn iso_date(seconds: i64) -> String {
    moment_of(seconds).format("%Y-%m-%d").to_string()
}

fn day_and_month(moment: DateTime<Local>) -> String {
    format!(
        "{} {}",
        moment.day(),
        MONTHS_GENITIVE[(moment.month() - 1) as usize]
    )
}

fn human_date(seconds: i64) -> String {
    let moment = moment_of(seconds);
    format!("{} {}", day_and_month(moment), moment.year())
}

/// Bucket width a span of this length deserves before crowding is considered.
///
/// Counting buckets alone is not enough. Two months of work happen to produce
/// fewer than sixteen active days, so `graphify` was answered as eight separate
/// dates — three of them holding a single record — when the honest shape of the
/// answer is weeks. People coarsen with distance: days within a fortnight,
/// weeks within a season, months within a couple of years.
fn natural_granularity(span_days: i64) -> Granularity {
    match span_days {
        ..=13 => Granularity::Day,
        14..=120 => Granularity::Week,
        121..=800 => Granularity::Month,
        801..=2_500 => Granularity::Quarter,
        _ => Granularity::Year,
    }
}

/// Picks the bucket width the span deserves, then coarsens it until the answer
/// fits on one screen.
///
/// Fixed thresholds were the old failure: four records seconds apart were
/// grouped by hour and reported as `2026-07-29 19:00`, which answers nothing.
fn choose_granularity(timestamps: &[i64]) -> Granularity {
    let earliest = timestamps.iter().copied().min().unwrap_or_default();
    let latest = timestamps.iter().copied().max().unwrap_or_default();
    let natural = natural_granularity((latest - earliest) / 86_400);
    let start = Granularity::LADDER
        .iter()
        .position(|granularity| *granularity == natural)
        .unwrap_or(0);

    for granularity in &Granularity::LADDER[start..] {
        let buckets: HashSet<String> = timestamps
            .iter()
            .map(|seconds| granularity.key(moment_of(*seconds)))
            .collect();
        if buckets.len() <= MAX_PERIODS {
            return *granularity;
        }
    }
    Granularity::Year
}

/// Russian plural agreement, needed everywhere counts are reported.
fn plural(count: usize, one: &str, few: &str, many: &str) -> String {
    let tail = count % 100;
    let last = count % 10;
    let word = if (11..=14).contains(&tail) {
        many
    } else if last == 1 {
        one
    } else if (2..=4).contains(&last) {
        few
    } else {
        many
    };
    format!("{count} {word}")
}

fn records_phrase(count: usize) -> String {
    plural(count, "запись", "записи", "записей")
}

/// Prepositional case, for "встречается в 4 записях".
fn records_in_phrase(count: usize) -> String {
    plural(count, "записи", "записях", "записях")
}

// ---------- queries ----------

fn row_to_record(row: &rusqlite::Row<'_>, with_rank: bool) -> rusqlite::Result<Record> {
    Ok(Record {
        node_id: row.get(0)?,
        kind: row.get(1)?,
        label: row.get(2)?,
        body: row.get(3)?,
        source: row.get(4)?,
        valid_from: row.get(5)?,
        recorded_at: row.get(6)?,
        salience: row.get(7)?,
        rank: if with_rank { row.get(8)? } else { 0.0 },
    })
}

const SELECT_COLUMNS: &str = "nv.node_id, nv.kind, nv.label, nv.body, nv.source,
     nv.valid_from, nv.recorded_at, COALESCE(n.salience, 0.0)";

fn escape_phrase(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

/// Runs one FTS query, returning an empty result instead of an error when the
/// query text is not valid FTS syntax.
fn fts_search(
    conn: &Connection,
    scope: Scope<'_>,
    expression: &str,
) -> Result<Vec<Record>, String> {
    let sql = format!(
        "SELECT {SELECT_COLUMNS}, bm25(node_versions_fts)
         FROM node_versions_fts f
         JOIN node_versions nv ON nv.row_id = f.rowid
         JOIN nodes n ON n.id = nv.node_id AND n.space_id = nv.space_id
         WHERE node_versions_fts MATCH ?1
           AND nv.space_id = ?2
           AND nv.valid_from <= ?3
           AND (nv.valid_to IS NULL OR ?3 < nv.valid_to)
           AND nv.recorded_at <= ?4
           AND (nv.superseded_at IS NULL OR ?4 < nv.superseded_at)
         ORDER BY bm25(node_versions_fts)
         LIMIT {MAX_MATCHES}"
    );
    let mut statement = match conn.prepare(&sql) {
        Ok(statement) => statement,
        Err(error) => return Err(error.to_string()),
    };
    let rows = match statement.query_map(
        params![
            expression,
            scope.space_id,
            scope.as_of.valid_at,
            scope.as_of.recorded_at
        ],
        |row| row_to_record(row, true),
    ) {
        Ok(rows) => rows,
        Err(_) => return Ok(Vec::new()),
    };
    let mut records = Vec::new();
    for row in rows {
        match row {
            Ok(record) => records.push(record),
            Err(_) => return Ok(records),
        }
    }
    Ok(records)
}

/// Stem of a word, obtained by dropping one trailing inflection letter.
///
/// The index tokenises exactly, so `перегрева` and `перегрев` are two different
/// tokens and produced two different histories for one question — four records
/// against two. Full morphology is not needed to fix that: Russian inflects by
/// suffix, and a prefix search from the stem covers the declensions that matter
/// (`грабли`/`граблю`/`граблями`) as well as English plurals (`swap`/`swaps`).
fn stem_prefix(word: &str) -> Option<String> {
    const INFLECTION_TAIL: [char; 12] =
        ['а', 'я', 'е', 'ё', 'и', 'о', 'у', 'ы', 'э', 'ю', 'й', 'ь'];
    if word.chars().count() < 4 {
        return None;
    }
    let stem = match word.chars().last() {
        Some(last) if INFLECTION_TAIL.contains(&last) && word.chars().count() > 4 => {
            let keep = word.chars().count() - 1;
            word.chars().take(keep).collect::<String>()
        }
        _ => word.to_string(),
    };
    Some(format!("{}*", escape_phrase(&stem)))
}

/// Adds records that are not already present, preserving the existing order.
fn merge_new(into: &mut Vec<Record>, extra: Vec<Record>) {
    let known: HashSet<String> = into.iter().map(|record| record.node_id.clone()).collect();
    for record in extra {
        if !known.contains(&record.node_id) {
            into.push(record);
        }
    }
}

/// Phrase first, then an OR of its content words, then stems.
///
/// A phrase-only search silently returns nothing for multi-word topics whose
/// words never sit next to each other, which reads as "we never worked on it".
fn search_topic(conn: &Connection, scope: Scope<'_>, topic: &str) -> Result<Vec<Record>, String> {
    let words = terms(topic);
    let mut found = fts_search(conn, scope, &escape_phrase(topic))?;

    // A one-word topic is one question whatever case it was typed in, so its
    // declensions belong in the same answer rather than in a separate history.
    if words.len() == 1
        && let Some(expression) = stem_prefix(&words[0])
    {
        merge_new(&mut found, fts_search(conn, scope, &expression)?);
    }
    if !found.is_empty() {
        return Ok(found);
    }
    if words.is_empty() {
        return Ok(Vec::new());
    }

    // Every word, in any order. A topic is still the same topic when its words
    // stand a sentence apart, but a record sharing one word out of four is a
    // different subject — and dropping straight to OR let exactly that record
    // be returned as the answer.
    if words.len() > 1 {
        let every_word = words
            .iter()
            .map(|word| escape_phrase(word))
            .collect::<Vec<_>>()
            .join(" AND ");
        let found = fts_search(conn, scope, &every_word)?;
        if !found.is_empty() {
            return Ok(found);
        }
        let stems: Vec<String> = words.iter().filter_map(|word| stem_prefix(word)).collect();
        if stems.len() == words.len() {
            let found = fts_search(conn, scope, &stems.join(" AND "))?;
            if !found.is_empty() {
                return Ok(found);
            }
        }

        // A multi-word temporal question names one subject. Returning records
        // that share only a subset silently changes that subject: a query for
        // a future sentinel used to fall back to an older, different sentinel
        // after the future row was correctly removed by AsOf filtering.
        return Ok(Vec::new());
    }

    // One-word queries have no partial-match ambiguity.
    let any_word = words
        .iter()
        .map(|word| escape_phrase(word))
        .collect::<Vec<_>>()
        .join(" OR ");
    let found = fts_search(conn, scope, &any_word)?;
    if !found.is_empty() {
        return Ok(found);
    }

    let stems: Vec<String> = words.iter().filter_map(|word| stem_prefix(word)).collect();
    if stems.is_empty() {
        return Ok(Vec::new());
    }
    fts_search(conn, scope, &stems.join(" OR "))
}

fn records_between(
    conn: &Connection,
    scope: Scope<'_>,
    start: i64,
    end: i64,
) -> Result<Vec<Record>, String> {
    let sql = format!(
        "SELECT {SELECT_COLUMNS}
         FROM node_versions nv
         JOIN nodes n ON n.id = nv.node_id AND n.space_id = nv.space_id
         WHERE nv.space_id = ?1
           AND nv.valid_from >= ?2 AND nv.valid_from < ?3
           AND nv.valid_from <= ?4
           AND (nv.valid_to IS NULL OR ?4 < nv.valid_to)
           AND nv.recorded_at <= ?5
           AND (nv.superseded_at IS NULL OR ?5 < nv.superseded_at)
         ORDER BY nv.valid_from"
    );
    let mut statement = conn.prepare(&sql).map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(
            params![
                scope.space_id,
                start,
                end,
                scope.as_of.valid_at,
                scope.as_of.recorded_at
            ],
            |row| row_to_record(row, false),
        )
        .map_err(|error| error.to_string())?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| error.to_string())
}

fn records_by_id(
    conn: &Connection,
    scope: Scope<'_>,
    ids: &[String],
) -> Result<Vec<Record>, String> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {SELECT_COLUMNS}
         FROM node_versions nv
         JOIN nodes n ON n.id = nv.node_id AND n.space_id = nv.space_id
         WHERE nv.node_id = ?1
           AND nv.space_id = ?2
           AND nv.valid_from <= ?3
           AND (nv.valid_to IS NULL OR ?3 < nv.valid_to)
           AND nv.recorded_at <= ?4
           AND (nv.superseded_at IS NULL OR ?4 < nv.superseded_at)
         ORDER BY nv.recorded_at DESC, nv.revision DESC
         LIMIT 1"
    );
    let mut statement = conn.prepare(&sql).map_err(|error| error.to_string())?;
    let mut records = Vec::with_capacity(ids.len());
    for id in ids {
        let record = statement
            .query_row(
                params![
                    id,
                    scope.space_id,
                    scope.as_of.valid_at,
                    scope.as_of.recorded_at
                ],
                |row| row_to_record(row, false),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if let Some(record) = record {
            records.push(record);
        }
    }
    Ok(records)
}

/// One graph step away from a node, with direction preserved.
struct Neighbour {
    relation: String,
    node_id: String,
    outgoing: bool,
}

fn neighbours(
    conn: &Connection,
    scope: Scope<'_>,
    node_id: &str,
) -> Result<Vec<Neighbour>, String> {
    let sql = "SELECT ev.relation, e.from_node_id, e.to_node_id
               FROM edges e
               JOIN edge_versions ev
                 ON ev.edge_id = e.id AND ev.space_id = e.space_id
               WHERE e.space_id = ?1
                 AND (e.from_node_id = ?2 OR e.to_node_id = ?2)
                 AND ev.valid_from <= ?3
                 AND (ev.valid_to IS NULL OR ?3 < ev.valid_to)
                 AND ev.recorded_at <= ?4
                 AND (ev.superseded_at IS NULL OR ?4 < ev.superseded_at)";
    let mut statement = conn.prepare(sql).map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(
            params![
                scope.space_id,
                node_id,
                scope.as_of.valid_at,
                scope.as_of.recorded_at
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(|error| error.to_string())?;

    let mut result = Vec::new();
    for row in rows {
        let (relation, from, to) = row.map_err(|error| error.to_string())?;
        if from == node_id {
            result.push(Neighbour {
                relation,
                node_id: to,
                outgoing: true,
            });
        } else {
            result.push(Neighbour {
                relation,
                node_id: from,
                outgoing: false,
            });
        }
    }
    Ok(result)
}

// ---------- brain when ----------

/// Answers "when did we work on this" with named periods.
pub fn when(conn: &Connection, space_id: &str, as_of: AsOf, topic: &str) -> Result<Value, String> {
    let scope = Scope { space_id, as_of };
    let matches = search_topic(conn, scope, topic)?;
    if matches.is_empty() {
        return Ok(json!({
            "topic": topic,
            "found": 0,
            "periods": [],
            "summary": format!("По запросу «{topic}» в памяти ничего нет."),
        }));
    }

    let corpus = Corpus::load(conn, scope)?;
    let timestamps: Vec<i64> = matches.iter().map(|record| record.valid_from).collect();
    let granularity = choose_granularity(&timestamps);

    let mut buckets: HashMap<String, Vec<Record>> = HashMap::new();
    for record in &matches {
        let key = granularity.key(moment_of(record.valid_from));
        buckets.entry(key).or_default().push(record.clone());
    }

    let mut periods: Vec<(String, Value, f64)> = Vec::new();
    for (key, records) in buckets {
        let ranked = rank_records(&records, &corpus);
        let Some(best) = ranked.first() else {
            continue;
        };
        let rank_scale = records
            .iter()
            .map(|record| record.rank)
            .fold(0.0_f64, f64::min);
        let strength = representativeness(best, &corpus, rank_scale);
        let first = records
            .iter()
            .map(|record| record.valid_from)
            .min()
            .unwrap_or(best.valid_from);
        let also: Vec<Value> = ranked
            .iter()
            .skip(1)
            .take(ALSO_PER_PERIOD)
            .map(|record| describe(record, &corpus))
            .collect();

        // Nothing in the period was written about the work; the whole bucket is
        // tasks handed to an executor. Saying so is the answer — dressing the
        // best-looking order up as a representative is not.
        let only_orders = ranked
            .iter()
            .all(|record| boilerplate_factor(record, &title_candidate(record, &corpus)) < 1.0);

        let mut period = json!({
            "period": key,
            "label": granularity.label(moment_of(first)),
            "count": records.len(),
            "representative": describe(best, &corpus),
            "also": also,
        });
        if only_orders {
            period["note"] = json!(
                "за этот период своих записей нет — в памяти только присланные задания, ниже самое внятное из них"
            );
        }
        periods.push((key.clone(), period, strength));
    }
    periods.sort_by(|a, b| a.0.cmp(&b.0));

    let headline = periods
        .iter()
        .max_by(|a, b| a.2.total_cmp(&b.2))
        .map(|(_, value, _)| value.clone());

    let first_seen = timestamps.iter().min().copied().unwrap_or_default();
    let last_seen = timestamps.iter().max().copied().unwrap_or_default();

    let mut summary = format!(
        "«{topic}» встречается в {} — {}.",
        records_in_phrase(matches.len()),
        period_span_phrase(&periods)
    );
    if let Some(head) = &headline {
        let title = head["representative"]["title"].as_str().unwrap_or_default();
        let kind = head["representative"]["kind_ru"].as_str().unwrap_or("");
        let label = head["label"].as_str().unwrap_or("");
        let _ = write!(summary, " Самое характерное — «{title}» ({kind}, {label}).");
    }

    let periods: Vec<Value> = periods.into_iter().map(|(_, value, _)| value).collect();
    Ok(json!({
        "topic": topic,
        "found": matches.len(),
        "granularity": granularity.name(),
        "first_seen": iso_date(first_seen),
        "last_seen": iso_date(last_seen),
        "summary": summary,
        "story": when_story(&summary, &periods),
        "periods": periods,
    }))
}

/// The same answer as prose, so a person does not have to read the JSON.
fn when_story(summary: &str, periods: &[Value]) -> String {
    let mut story = summary.to_string();
    for period in periods {
        let label = period["label"].as_str().unwrap_or("период");
        let count = period["count"].as_u64().unwrap_or(0) as usize;
        let title = period["representative"]["title"]
            .as_str()
            .unwrap_or("без названия");
        let kind = period["representative"]["kind_ru"].as_str().unwrap_or("");
        let _ = write!(
            story,
            "\n• {label}: {} — «{title}» ({kind}).",
            records_phrase(count)
        );
        if period["note"].is_string() {
            let _ = write!(
                story,
                " Своих записей за период нет, это присланное задание."
            );
        }
    }
    story
}

fn period_span_phrase(periods: &[(String, Value, f64)]) -> String {
    let labels: Vec<&str> = periods
        .iter()
        .filter_map(|(_, value, _)| value["label"].as_str())
        .collect();
    match labels.len() {
        0 => "период не определён".to_string(),
        1 => labels[0].to_string(),
        2 => format!("{} и {}", labels[0], labels[1]),
        _ => format!(
            "{}, от «{}» до «{}»",
            plural(labels.len(), "период", "периода", "периодов"),
            labels[0],
            labels[labels.len() - 1]
        ),
    }
}

// ---------- brain timeline ----------

/// Period selector for `timeline`.
pub struct TimelineArgs {
    /// Period expression: `YYYY-MM`, `YYYY`, a Russian month name, or a
    /// relative phrase such as `2 месяца назад`.
    pub month: Option<String>,
    /// Inclusive start date, `YYYY-MM-DD`.
    pub from: Option<String>,
    /// Exclusive end date, `YYYY-MM-DD`.
    pub to: Option<String>,
}

fn local_midnight(date: NaiveDate) -> Option<DateTime<Local>> {
    let naive = date.and_hms_opt(0, 0, 0)?;
    match Local.from_local_datetime(&naive) {
        LocalResult::Single(moment) => Some(moment),
        LocalResult::Ambiguous(first, second) => Some(first.min(second)),
        LocalResult::None => None,
    }
}

fn month_bounds(year: i32, month: u32) -> Option<(i64, i64)> {
    let start = NaiveDate::from_ymd_opt(year, month, 1)?;
    let end = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)?
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)?
    };
    Some((
        local_midnight(start)?.timestamp(),
        local_midnight(end)?.timestamp(),
    ))
}

/// Shifts a year/month pair back by `count` months.
fn months_back(year: i32, month: u32, count: i64) -> (i32, u32) {
    let absolute = i64::from(year) * 12 + i64::from(month) - 1 - count;
    let year = absolute.div_euclid(12) as i32;
    let month = absolute.rem_euclid(12) as u32 + 1;
    (year, month)
}

fn day_bounds(date: NaiveDate) -> Option<(i64, i64)> {
    let next = date.succ_opt()?;
    Some((
        local_midnight(date)?.timestamp(),
        local_midnight(next)?.timestamp(),
    ))
}

fn parse_number(text: &str) -> Option<i64> {
    let words = [
        ("один", 1),
        ("одну", 1),
        ("два", 2),
        ("две", 2),
        ("три", 3),
        ("четыре", 4),
        ("пять", 5),
        ("шесть", 6),
        ("семь", 7),
        ("восемь", 8),
        ("девять", 9),
        ("десять", 10),
    ];
    for token in text.split_whitespace() {
        if let Ok(value) = token.parse::<i64>() {
            return Some(value);
        }
    }
    text.split(|character: char| !character.is_alphanumeric())
        .find_map(|token| {
            words
                .iter()
                .find(|(word, _)| token == *word)
                .map(|(_, value)| *value)
        })
}

fn month_number(token: &str) -> Option<u32> {
    match token {
        "январь" | "января" | "январе" => Some(1),
        "февраль" | "февраля" | "феврале" => Some(2),
        "март" | "марта" | "марте" => Some(3),
        "апрель" | "апреля" | "апреле" => Some(4),
        "май" | "мая" | "мае" => Some(5),
        "июнь" | "июня" | "июне" => Some(6),
        "июль" | "июля" | "июле" => Some(7),
        "август" | "августа" | "августе" => Some(8),
        "сентябрь" | "сентября" | "сентябре" => Some(9),
        "октябрь" | "октября" | "октябре" => Some(10),
        "ноябрь" | "ноября" | "ноябре" => Some(11),
        "декабрь" | "декабря" | "декабре" => Some(12),
        _ => None,
    }
}

/// Turns a period expression into `[start, end)` plus a Russian label.
///
/// The CLI only exposes `--month`, so every phrasing a person is likely to
/// type has to be understood here rather than added as new flags.
fn parse_period(spec: &str, now: DateTime<Local>) -> Option<(i64, i64, String)> {
    let text = normalize(spec);
    if text.is_empty() {
        return None;
    }

    // Explicit calendar forms.
    if let Some((year, month)) = text.split_once('-')
        && let (Ok(year), Ok(month)) = (year.trim().parse::<i32>(), month.trim().parse::<u32>())
        && (1..=12).contains(&month)
    {
        let (start, end) = month_bounds(year, month)?;
        return Some((
            start,
            end,
            format!("{} {year}", MONTHS_NOMINATIVE[(month - 1) as usize]),
        ));
    }
    if let Ok(year) = text.parse::<i32>()
        && (1970..=2999).contains(&year)
    {
        let start = local_midnight(NaiveDate::from_ymd_opt(year, 1, 1)?)?.timestamp();
        let end = local_midnight(NaiveDate::from_ymd_opt(year + 1, 1, 1)?)?.timestamp();
        return Some((start, end, format!("{year} год")));
    }

    // Relative phrases.
    if text.contains("назад") || text.contains(" ago") {
        let count = parse_number(&text).unwrap_or(1);
        if text.contains("полгода") || text.contains("half a year") {
            let (year, month) = months_back(now.year(), now.month(), 6);
            let (start, end) = month_bounds(year, month)?;
            return Some((
                start,
                end,
                format!(
                    "{} {year} (полгода назад)",
                    MONTHS_NOMINATIVE[(month - 1) as usize]
                ),
            ));
        }
        if text.contains("год") || text.contains("лет") || text.contains("year") {
            let (year, month) = months_back(now.year(), now.month(), 12 * count);
            let (start, end) = month_bounds(year, month)?;
            return Some((
                start,
                end,
                format!(
                    "{} {year} ({} назад)",
                    MONTHS_NOMINATIVE[(month - 1) as usize],
                    plural(count as usize, "год", "года", "лет")
                ),
            ));
        }
        if text.contains("месяц") || text.contains("month") {
            let (year, month) = months_back(now.year(), now.month(), count);
            let (start, end) = month_bounds(year, month)?;
            return Some((
                start,
                end,
                format!(
                    "{} {year} ({} назад)",
                    MONTHS_NOMINATIVE[(month - 1) as usize],
                    plural(count as usize, "месяц", "месяца", "месяцев")
                ),
            ));
        }
        if text.contains("недел") || text.contains("week") {
            let end = now - chrono::Duration::days(7 * (count - 1));
            let start = end - chrono::Duration::days(7);
            return Some((
                start.timestamp(),
                end.timestamp(),
                format!(
                    "{} назад ({} — {})",
                    plural(count as usize, "неделя", "недели", "недель"),
                    day_and_month(start),
                    human_date(end.timestamp())
                ),
            ));
        }
        if text.contains("дн") || text.contains("day") {
            let day = (now - chrono::Duration::days(count)).date_naive();
            let (start, end) = day_bounds(day)?;
            return Some((start, end, human_date(start)));
        }
    }
    if text.contains("прошл") && (text.contains("месяц") || text.contains("month")) {
        let (year, month) = months_back(now.year(), now.month(), 1);
        let (start, end) = month_bounds(year, month)?;
        return Some((
            start,
            end,
            format!(
                "{} {year} (прошлый месяц)",
                MONTHS_NOMINATIVE[(month - 1) as usize]
            ),
        ));
    }
    if text.contains("последн") || text.contains("last ") {
        let days = if text.contains("недел") || text.contains("week") {
            parse_number(&text).unwrap_or(1) * 7
        } else if text.contains("месяц") || text.contains("month") {
            parse_number(&text).unwrap_or(1) * 30
        } else {
            parse_number(&text).unwrap_or(30)
        };
        let start = now - chrono::Duration::days(days);
        return Some((
            start.timestamp(),
            now.timestamp() + 1,
            format!(
                "последние {} ({} — {})",
                plural(days as usize, "день", "дня", "дней"),
                day_and_month(start),
                human_date(now.timestamp())
            ),
        ));
    }

    // Bare month names, optionally followed by a year.
    let year = text
        .split_whitespace()
        .find_map(|token| token.parse::<i32>().ok())
        .filter(|year| (1970..=2999).contains(year))
        .unwrap_or_else(|| now.year());
    let month = text
        .split(|character: char| !character.is_alphanumeric())
        .find_map(month_number)?;
    let (start, end) = month_bounds(year, month)?;
    let name = MONTHS_NOMINATIVE[(month - 1) as usize];
    Some((start, end, format!("{name} {year}")))
}

fn parse_date(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").ok()
}

/// Falls back to the calendar month of the newest record.
fn latest_month(
    conn: &Connection,
    scope: Scope<'_>,
    now: DateTime<Local>,
) -> Result<(i64, i64, String), String> {
    let newest: Option<i64> = conn
        .query_row(
            "SELECT MAX(valid_from)
             FROM node_versions
             WHERE space_id = ?1
               AND valid_from <= ?2
               AND (valid_to IS NULL OR ?2 < valid_to)
               AND recorded_at <= ?3
               AND (superseded_at IS NULL OR ?3 < superseded_at)",
            params![
                scope.space_id,
                scope.as_of.valid_at,
                scope.as_of.recorded_at
            ],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let moment = newest.map_or(now, moment_of);
    let (start, end) = month_bounds(moment.year(), moment.month())
        .ok_or_else(|| "не удалось определить период".to_string())?;
    Ok((
        start,
        end,
        format!(
            "{} {}",
            MONTHS_NOMINATIVE[(moment.month() - 1) as usize],
            moment.year()
        ),
    ))
}

/// A group of records that share a distinctive word.
struct Theme {
    term: String,
    members: Vec<usize>,
    score: f64,
}

/// Finds what a period was actually about.
///
/// Scoring is term frequency inside the period times corpus rarity: a word
/// used everywhere describes the archive, not the month.
fn themes_of(records: &[Record], corpus: &Corpus) -> Vec<Theme> {
    let mut postings: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        let title = title_of(record, corpus);
        // Repeating an instruction does not make it a meaningful topic of
        // work. Exclude instruction-like text from the topic summary.
        if looks_like_instruction(&title) || looks_like_instruction(&record.body) {
            continue;
        }
        for term in distinct_terms(&title) {
            postings.entry(term).or_default().push(index);
        }
    }

    let mut themes: Vec<Theme> = postings
        .into_iter()
        .filter(|(_, members)| members.len() >= MIN_THEME_RECORDS)
        .filter_map(|(term, members)| {
            let rarity = corpus.word_rarity(&term, members.len());
            if rarity < MIN_THEME_IDF {
                return None;
            }
            // One heading repeated by a form is one record filled in many
            // times, however many rows it occupies.
            let distinct: HashSet<String> = members
                .iter()
                .map(|index| normalize(&title_of(&records[*index], corpus)))
                .collect();
            if distinct.len() < MIN_THEME_TITLES {
                return None;
            }
            // Bigrams are worth more: they name a project, not a word.
            let specificity = if term.contains(' ') { 1.35 } else { 1.0 };
            // A month spent deciding and a month spent pasting prompts leave
            // the same row count; what differs is what kind of rows.
            let substance = members
                .iter()
                .map(|index| kind_weight(&records[*index].kind))
                .sum::<f64>()
                / members.len() as f64;
            Some(Theme {
                score: (distinct.len() as f64).ln_1p() * rarity * specificity * substance,
                term,
                members,
            })
        })
        .collect();
    themes.sort_by(|a, b| b.score.total_cmp(&a.score));

    // Drop themes that describe records an earlier theme already covered.
    let mut kept: Vec<Theme> = Vec::new();
    for theme in themes {
        let members: HashSet<usize> = theme.members.iter().copied().collect();
        let redundant = kept.iter().any(|other| {
            let existing: HashSet<usize> = other.members.iter().copied().collect();
            let shared = members.intersection(&existing).count() as f64;
            shared / members.len().min(existing.len()).max(1) as f64 > THEME_OVERLAP
        });
        if !redundant {
            kept.push(theme);
        }
        if kept.len() == MAX_THEMES {
            break;
        }
    }
    kept
}

fn highlights(records: &[Record], corpus: &Corpus, kind: &str) -> Vec<Record> {
    let selected: Vec<Record> = records
        .iter()
        .filter(|record| record.kind == kind)
        .cloned()
        .collect();
    let ranked = rank_records(&selected, corpus);
    // Machine output and pasted orders are stored under these kinds too, and
    // they win on rarity. Filtering after ranking can only shorten the section;
    // when it would empty it, the unfiltered list is still the honest answer.
    let filtered = readable(ranked.clone(), corpus);
    let chosen = if filtered.is_empty() {
        ranked
    } else {
        filtered
    };
    chosen.into_iter().take(MAX_HIGHLIGHTS).collect()
}

/// Pitfalls: records stored as errors that also read as one.
///
/// The filter is dropped when it would leave the section empty — reporting the
/// dubious ones is still more honest than claiming nothing went wrong.
fn pitfalls_of(records: &[Record], corpus: &Corpus) -> Vec<Record> {
    let genuine: Vec<Record> = records
        .iter()
        .filter(|record| record.kind == "error" && reads_as_problem(record))
        .cloned()
        .collect();
    if genuine.is_empty() {
        return highlights(records, corpus, "error");
    }
    let ranked = rank_records(&genuine, corpus);
    let filtered = readable(ranked.clone(), corpus);
    let chosen = if filtered.is_empty() {
        ranked
    } else {
        filtered
    };
    chosen.into_iter().take(MAX_HIGHLIGHTS).collect()
}

/// Tells the story of a period: what was worked on, decided, and tripped over.
pub fn timeline(
    conn: &Connection,
    space_id: &str,
    as_of: AsOf,
    args: TimelineArgs,
) -> Result<Value, String> {
    let scope = Scope { space_id, as_of };
    let now = moment_of(as_of.valid_at);
    let (start, end, label) = match (&args.month, &args.from, &args.to) {
        (Some(spec), _, _) => parse_period(spec, now).ok_or_else(|| {
            format!(
                "не понял период «{spec}»; примеры: 2026-07, июль, 2 месяца назад, полгода назад"
            )
        })?,
        (None, Some(from), Some(to)) => {
            let from_date = parse_date(from).ok_or_else(|| format!("неверная дата «{from}»"))?;
            let to_date = parse_date(to).ok_or_else(|| format!("неверная дата «{to}»"))?;
            let (start, _) = day_bounds(from_date).ok_or("неверная дата начала")?;
            let (end, _) = day_bounds(to_date).ok_or("неверная дата конца")?;
            (
                start,
                end,
                format!("{} — {}", day_and_month(moment_of(start)), human_date(end)),
            )
        }
        (None, _, _) => latest_month(conn, scope, now)?,
    };

    let records = records_between(conn, scope, start, end)?;
    if records.is_empty() {
        return Ok(json!({
            "period": {"label": label, "from": iso_date(start), "to": iso_date(end - 1)},
            "records": 0,
            "story": format!("За период «{label}» в памяти нет ни одной записи."),
            "themes": [],
            "decisions": [],
            "pitfalls": [],
        }));
    }

    let corpus = Corpus::load(conn, scope)?;
    let themes = themes_of(&records, &corpus);
    let decisions = highlights(&records, &corpus, "decision");
    let pitfalls = pitfalls_of(&records, &corpus);

    let mut per_day: HashMap<String, usize> = HashMap::new();
    for record in &records {
        *per_day.entry(iso_date(record.valid_from)).or_insert(0) += 1;
    }
    let mut busiest: Vec<(String, usize)> = per_day.into_iter().collect();
    busiest.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    busiest.truncate(3);

    let first = records.iter().map(|r| r.valid_from).min().unwrap_or(start);
    let last = records.iter().map(|r| r.valid_from).max().unwrap_or(end);

    let theme_values: Vec<Value> = themes
        .iter()
        .map(|theme| {
            let members: Vec<Record> = theme
                .members
                .iter()
                .map(|index| records[*index].clone())
                .collect();
            let ranked = rank_records(&members, &corpus);
            let span_first = members.iter().map(|r| r.valid_from).min().unwrap_or(first);
            let span_last = members.iter().map(|r| r.valid_from).max().unwrap_or(last);
            json!({
                "theme": theme.term,
                "count": theme.members.len(),
                "from": iso_date(span_first),
                "to": iso_date(span_last),
                "representative": ranked.first().map(|record| describe(record, &corpus)),
                "also": ranked.iter().skip(1).take(ALSO_PER_PERIOD)
                    .map(|record| describe(record, &corpus)).collect::<Vec<_>>(),
            })
        })
        .collect();

    let story = timeline_story(
        &label, &records, first, last, &themes, &decisions, &pitfalls, &busiest, &corpus,
    );

    Ok(json!({
        "period": {"label": label, "from": iso_date(start), "to": iso_date(end - 1)},
        "records": records.len(),
        "active_from": iso_date(first),
        "active_to": iso_date(last),
        "story": story,
        "themes": theme_values,
        "decisions": decisions.iter().map(|record| describe(record, &corpus)).collect::<Vec<_>>(),
        "pitfalls": pitfalls.iter().map(|record| describe(record, &corpus)).collect::<Vec<_>>(),
        "busiest_days": busiest.iter()
            .map(|(day, count)| json!({"date": day, "count": count}))
            .collect::<Vec<_>>(),
    }))
}

#[allow(clippy::too_many_arguments)]
fn timeline_story(
    label: &str,
    records: &[Record],
    first: i64,
    last: i64,
    themes: &[Theme],
    decisions: &[Record],
    pitfalls: &[Record],
    busiest: &[(String, usize)],
    corpus: &Corpus,
) -> String {
    let mut story = String::new();
    let _ = write!(
        story,
        "{label}: {}, с {} по {}.",
        records_phrase(records.len()),
        day_and_month(moment_of(first)),
        human_date(last)
    );

    if let Some((day, count)) = busiest.first()
        && *count * 3 > records.len()
    {
        let share = (*count as f64 * 100.0 / records.len().max(1) as f64).round();
        let _ = write!(
            story,
            " Самый насыщенный день — {}: {} ({share:.0}% периода); это плотность сохранённых событий, а не доказательство длительности работы.",
            human_date(
                parse_date(day)
                    .and_then(local_midnight)
                    .map(|moment| moment.timestamp())
                    .unwrap_or(first)
            ),
            records_phrase(*count),
        );
    }

    if !themes.is_empty() {
        let list = themes
            .iter()
            .take(5)
            .map(|theme| format!("{} ({})", theme.term, theme.members.len()))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = write!(story, "\nЧем занимались: {list}.");
        if let Some(theme) = themes.first()
            && let Some(best) = rank_records(
                &theme
                    .members
                    .iter()
                    .map(|index| records[*index].clone())
                    .collect::<Vec<_>>(),
                corpus,
            )
            .first()
        {
            let _ = write!(story, " Ярче всего — «{}».", title_of(best, corpus));
        }
    }

    if decisions.is_empty() {
        let _ = write!(story, "\nЗафиксированных решений за период нет.");
    } else {
        let _ = write!(
            story,
            "\nРешения ({}): {}.",
            decisions.len(),
            decisions
                .iter()
                .take(3)
                .map(|record| format!("«{}»", title_of(record, corpus)))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }

    if pitfalls.is_empty() {
        let _ = write!(story, "\nГраблей и ошибок не записано.");
    } else {
        let _ = write!(
            story,
            "\nГрабли ({}): {}.",
            pitfalls.len(),
            pitfalls
                .iter()
                .take(3)
                .map(|record| format!("«{}»", title_of(record, corpus)))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }

    story
}

// ---------- brain explain ----------

/// Relation names that carry a cause, mapped to how they read in an answer.
fn relation_ru(relation: &str, outgoing: bool) -> Option<&'static str> {
    match (relation, outgoing) {
        ("decided", false) => Some("принято в сессии"),
        ("decided", true) => Some("в этой сессии решили"),
        ("encountered", false) => Some("всплыло в сессии"),
        ("encountered", true) => Some("в этой сессии споткнулись о"),
        ("rationale_for", true) => Some("обосновывает"),
        ("rationale_for", false) => Some("обосновано решением"),
        ("acted_on", true) => Some("применено к"),
        ("acted_on", false) => Some("применено из"),
        ("relates_to", _) => Some("связано с"),
        ("references", _) => Some("ссылается на"),
        ("mentions", _) => Some("упоминает"),
        ("touched", true) => Some("затронуло"),
        ("touched", false) => Some("затронуто в"),
        ("part_of", _) => Some("часть"),
        _ => None,
    }
}

/// Drops records whose title names nothing: machine identifiers, and orders
/// addressed to an executor.
///
/// Applied over an already ranked list, so the worst case is a shorter section
/// rather than a fabricated one.
fn readable(records: Vec<Record>, corpus: &Corpus) -> Vec<Record> {
    records
        .into_iter()
        .filter(|record| {
            let title = title_candidate(record, corpus);
            !is_opaque_identifier(&title)
                && !looks_like_instruction(&title)
                && numeric_share(&title) < 0.3
        })
        .collect()
}

/// Picks the record a question is actually about.
///
/// Plain BM25 order returns whichever row repeats the words most, which for a
/// decision query is usually a log line; ranking restores intent.
fn find_subject(
    conn: &Connection,
    scope: Scope<'_>,
    query: &str,
    corpus: &Corpus,
) -> Result<Option<Record>, String> {
    let matches = search_topic(conn, scope, query)?;
    if matches.is_empty() {
        return Ok(None);
    }
    let words = terms(query);
    let rank_scale = matches
        .iter()
        .map(|record| record.rank)
        .fold(0.0_f64, f64::min);
    let best = matches.iter().max_by(|a, b| {
        subject_score(a, corpus, rank_scale, &words)
            .total_cmp(&subject_score(b, corpus, rank_scale, &words))
    });
    Ok(best.cloned())
}

/// Share of the question's content words a record actually carries.
///
/// Prefix matching rather than equality, because the index tokenises exactly
/// and Russian inflects by suffix: `пересобрать` in the question has to find
/// `пересобрал` in the record.
fn query_coverage(record: &Record, words: &[String]) -> f64 {
    if words.is_empty() {
        return 1.0;
    }
    let haystack = normalize(&format!("{} {}", record.label, record.body));
    let hay_words: Vec<&str> = haystack
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .collect();
    let matched = words
        .iter()
        .filter(|word| {
            let stem: String = word.chars().take(word.chars().count().max(4) - 1).collect();
            hay_words
                .iter()
                .any(|candidate| candidate.starts_with(&stem))
        })
        .count();
    matched as f64 / words.len() as f64
}

/// How well a record answers the question that was asked.
///
/// `when` ranks records that have already matched a topic, so merit alone is
/// the right yardstick there. `explain` chooses out of an OR-expansion where a
/// record can match one word of four, and merit alone picked «Установить Kitty
/// локально» for «почему graphify нельзя пересобрать локально» — one shared
/// word, a well-written title, a `decision` kind. Coverage squared makes a
/// partial match unable to win on presentation.
fn subject_score(record: &Record, corpus: &Corpus, rank_scale: f64, words: &[String]) -> f64 {
    let coverage = query_coverage(record, words);
    representativeness(record, corpus, rank_scale) * (0.10 + 0.90 * coverage * coverage)
}

/// Explains why a decision was taken, using the graph and the surrounding days.
pub fn explain(
    conn: &Connection,
    space_id: &str,
    as_of: AsOf,
    decision_query: &str,
) -> Result<Value, String> {
    let scope = Scope { space_id, as_of };
    let corpus = Corpus::load(conn, scope)?;
    let Some(subject) = find_subject(conn, scope, decision_query, &corpus)? else {
        return Ok(json!({
            "query": decision_query,
            "found": false,
            "story": format!("По запросу «{decision_query}» в памяти ничего не нашлось."),
        }));
    };

    // One hop out, plus a second hop through the session that owns the record:
    // in this schema a session is what ties a decision to the errors that
    // provoked it.
    let direct = neighbours(conn, scope, &subject.node_id)?;
    let mut wanted: HashSet<String> = direct
        .iter()
        .map(|neighbour| neighbour.node_id.clone())
        .collect();
    let mut sibling_links: Vec<Neighbour> = Vec::new();
    for neighbour in &direct {
        if matches!(neighbour.relation.as_str(), "decided" | "encountered") && !neighbour.outgoing {
            for sibling in neighbours(conn, scope, &neighbour.node_id)? {
                if sibling.node_id != subject.node_id
                    && matches!(sibling.relation.as_str(), "decided" | "encountered")
                {
                    wanted.insert(sibling.node_id.clone());
                    sibling_links.push(sibling);
                }
            }
        }
    }

    let ids: Vec<String> = wanted.into_iter().collect();
    let fetched = records_by_id(conn, scope, &ids)?;
    let by_id: HashMap<String, Record> = fetched
        .into_iter()
        .map(|record| (record.node_id.clone(), record))
        .collect();

    let mut links: Vec<(&Neighbour, &Record)> = Vec::new();
    for neighbour in direct.iter().chain(sibling_links.iter()) {
        if relation_ru(&neighbour.relation, neighbour.outgoing).is_none() {
            continue;
        }
        if let Some(record) = by_id.get(&neighbour.node_id) {
            links.push((neighbour, record));
        }
    }
    links.sort_by(|a, b| {
        representativeness(b.1, &corpus, 0.0).total_cmp(&representativeness(a.1, &corpus, 0.0))
    });

    let mut seen = HashSet::new();
    let graph: Vec<Value> = links
        .iter()
        .filter(|(_, record)| seen.insert(record.node_id.clone()))
        .take(MAX_CONTEXT * 2)
        .map(|(neighbour, record)| {
            let mut value = describe(record, &corpus);
            value["relation"] = json!(neighbour.relation);
            value["relation_ru"] =
                json!(relation_ru(&neighbour.relation, neighbour.outgoing).unwrap_or("связано с"));
            value
        })
        .collect();

    // Time context: what was recorded just before and just after.
    let window = EXPLAIN_WINDOW_DAYS * 86_400;
    let around = records_between(
        conn,
        scope,
        subject.valid_from - window,
        subject.valid_from + window,
    )?;
    let before: Vec<Record> = around
        .iter()
        .filter(|record| {
            record.node_id != subject.node_id && record.valid_from <= subject.valid_from
        })
        .cloned()
        .collect();
    let after: Vec<Record> = around
        .iter()
        .filter(|record| {
            record.node_id != subject.node_id && record.valid_from > subject.valid_from
        })
        .cloned()
        .collect();

    let preceding_errors: Vec<Record> = readable(
        rank_records(
            &before
                .iter()
                .filter(|record| record.kind == "error" && reads_as_problem(record))
                .cloned()
                .collect::<Vec<_>>(),
            &corpus,
        ),
        &corpus,
    )
    .into_iter()
    .take(MAX_CONTEXT)
    .collect();
    let before_top = readable(rank_records(&before, &corpus), &corpus)
        .into_iter()
        .take(MAX_CONTEXT)
        .collect::<Vec<_>>();
    let after_top = readable(rank_records(&after, &corpus), &corpus)
        .into_iter()
        .take(MAX_CONTEXT)
        .collect::<Vec<_>>();

    let coverage = query_coverage(&subject, &terms(decision_query));
    let story = explain_story(
        &subject,
        &graph,
        &preceding_errors,
        &before_top,
        &after_top,
        coverage,
        &corpus,
    );

    Ok(json!({
        "query": decision_query,
        "found": true,
        "match": coverage_phrase(coverage),
        "subject": {
            "title": title_of(&subject, &corpus),
            "kind": subject.kind,
            "kind_ru": kind_ru(&subject.kind),
            "node_id": subject.node_id,
            "date": human_date(subject.valid_from),
            "valid_from": iso_date(subject.valid_from),
            "recorded_at": iso_date(subject.recorded_at),
            "source": subject.source,
            "body": subject.body,
        },
        "story": story,
        "graph": graph,
        "preceding_errors": preceding_errors.iter()
            .map(|record| describe(record, &corpus)).collect::<Vec<_>>(),
        "before": before_top.iter()
            .map(|record| describe(record, &corpus)).collect::<Vec<_>>(),
        "after": after_top.iter()
            .map(|record| describe(record, &corpus)).collect::<Vec<_>>(),
    }))
}

/// Relations that carry a reason rather than a mere association.
const CAUSAL_RELATIONS: [&str; 3] = ["rationale_for", "acted_on", "encountered"];

/// How much of the question the chosen record covers, in words.
fn coverage_phrase(coverage: f64) -> String {
    match (coverage * 100.0).round() as i64 {
        100 => "совпало полностью".to_string(),
        percent => format!("совпало {percent}% слов запроса"),
    }
}

/// Titles of graph entries whose relation is in `wanted`, newest ranking first.
fn linked_titles(graph: &[Value], wanted: &[&str], limit: usize) -> Vec<String> {
    graph
        .iter()
        .filter(|value| wanted.contains(&value["relation"].as_str().unwrap_or("")))
        .filter_map(|value| {
            let title = value["title"].as_str()?;
            let relation = value["relation_ru"].as_str().unwrap_or("связано с");
            Some(format!("«{title}» ({relation})"))
        })
        .take(limit)
        .collect()
}

fn quoted_titles(records: &[Record], corpus: &Corpus, limit: usize) -> String {
    records
        .iter()
        .take(limit)
        .map(|record| format!("«{}»", title_of(record, corpus)))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Answers "why was this decided" in the order a person asks it: what it is,
/// what pushed it, what it rests on, what came of it.
///
/// The section that matters most is the one that has to stay empty honestly:
/// a graph that records no reason must say so instead of passing a list of
/// neighbours off as an explanation.
#[allow(clippy::too_many_arguments)]
fn explain_story(
    subject: &Record,
    graph: &[Value],
    preceding_errors: &[Record],
    before: &[Record],
    after: &[Record],
    coverage: f64,
    corpus: &Corpus,
) -> String {
    let mut story = format!(
        "«{}» — {}, относится к {}.",
        title_of(subject, corpus),
        kind_ru(&subject.kind),
        human_date(subject.valid_from)
    );
    // A partial match still deserves an answer, but the reader has to know the
    // answer is about a neighbouring subject, not the one asked for.
    if coverage < 0.6 {
        let _ = write!(
            story,
            "\nТочной записи по запросу нет — это ближайшая по словам ({}), так что читай её как соседнюю тему, а не как прямой ответ.",
            coverage_phrase(coverage)
        );
    }

    let session = graph
        .iter()
        .find(|value| value["relation"] == "decided" || value["relation"] == "encountered");
    if let Some(session) = session {
        let _ = write!(
            story,
            "\nГде принято: сессия «{}».",
            session["title"].as_str().unwrap_or("без названия")
        );
    }

    let reasons = linked_titles(graph, &CAUSAL_RELATIONS, 3);
    if reasons.is_empty() {
        let _ = write!(
            story,
            "\nПричина в памяти не зафиксирована — в графе нет обосновывающей связи. Контекст ниже не следует читать как доказанную причину."
        );
    } else {
        let _ = write!(story, "\nПочему: {}.", reasons.join("; "));
    }
    if !preceding_errors.is_empty() {
        let _ = write!(
            story,
            "\nРядом по времени до решения были ошибки (причинная связь не подтверждена): {}.",
            quoted_titles(preceding_errors, corpus, 3)
        );
    }

    let related = linked_titles(
        graph,
        &["relates_to", "references", "mentions", "part_of"],
        3,
    );
    if !related.is_empty() {
        let _ = write!(story, "\nПо графу связано с: {}.", related.join("; "));
    }

    if !before.is_empty() {
        let _ = write!(
            story,
            "\nЧто шло рядом до: {}.",
            quoted_titles(before, corpus, 3)
        );
    }
    if after.is_empty() {
        let _ = write!(story, "\nПосле этого записей в окне нет.");
    } else {
        let _ = write!(
            story,
            "\nЧем обернулось: {}.",
            quoted_titles(after, corpus, 3)
        );
    }

    story
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Agent, Attribution, NewNode, NodeKind, Session, Space, SpaceKind, SqliteStorage, Storage,
    };
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DATABASE: AtomicU64 = AtomicU64::new(0);

    struct TestDatabase {
        path: PathBuf,
    }

    impl Drop for TestDatabase {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let mut path = self.path.as_os_str().to_os_string();
                path.push(suffix);
                let _ = std::fs::remove_file(PathBuf::from(path));
            }
        }
    }

    fn test_database_path(name: &str) -> PathBuf {
        let sequence = NEXT_TEST_DATABASE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "brain-temporal-{name}-{}-{sequence}.sqlite",
            std::process::id()
        ))
    }

    // Test fixture: one argument per stored node column.
    #[allow(clippy::too_many_arguments)]
    fn stored_node(
        id: &str,
        space_id: &str,
        session_id: &str,
        kind: NodeKind,
        label: &str,
        valid_from: i64,
        valid_to: Option<i64>,
        recorded_at: i64,
    ) -> NewNode {
        NewNode {
            id: id.to_owned(),
            space_id: space_id.to_owned(),
            kind,
            label: label.to_owned(),
            body: label.to_owned(),
            source: None,
            metadata_json: "{}".to_owned(),
            valid_from,
            valid_to,
            attribution: Attribution {
                agent_id: "temporal-agent".to_owned(),
                session_id: session_id.to_owned(),
                recorded_at,
            },
            salience: 1.0,
            source_weight: 1.0,
        }
    }

    fn scoped_database(name: &str) -> (TestDatabase, Connection, AsOf, i64) {
        let path = test_database_path(name);
        let database = TestDatabase { path: path.clone() };
        let storage = SqliteStorage::open(&path).expect("open temporal database");
        let agent = Agent {
            id: "temporal-agent".to_owned(),
            display_name: None,
            metadata_json: "{}".to_owned(),
            created_at: 1,
        };
        for (space_id, session_id) in [
            ("temporal-space-a", "temporal-session-a"),
            ("temporal-space-b", "temporal-session-b"),
        ] {
            storage
                .ensure_context(
                    &agent,
                    &Space {
                        id: space_id.to_owned(),
                        kind: SpaceKind::Project,
                        label: space_id.to_owned(),
                        metadata_json: "{}".to_owned(),
                        created_by_agent_id: agent.id.clone(),
                        created_at: 1,
                    },
                    &Session {
                        id: session_id.to_owned(),
                        space_id: space_id.to_owned(),
                        agent_id: agent.id.clone(),
                        started_at: 1,
                        ended_at: None,
                        metadata_json: "{}".to_owned(),
                    },
                )
                .expect("create temporal context");
        }

        let event_at = month_bounds(2020, 7).expect("July bounds").0 + 12 * 3_600;
        let forgotten_at = event_at + 60;
        let decision_at = event_at + 120;
        let error_at = decision_at - 30;
        let recorded_at = month_bounds(2026, 8).expect("August bounds").0 + 12 * 3_600;
        let future_at = month_bounds(2030, 1).expect("future bounds").0 + 12 * 3_600;
        storage
            .insert_nodes(&[
                stored_node(
                    "current-a",
                    "temporal-space-a",
                    "temporal-session-a",
                    NodeKind::Memory,
                    "temporal sentinel unique",
                    event_at,
                    None,
                    recorded_at,
                ),
                stored_node(
                    "forgotten-a",
                    "temporal-space-a",
                    "temporal-session-a",
                    NodeKind::Memory,
                    "temporal sentinel unique",
                    forgotten_at,
                    Some(forgotten_at + 1),
                    recorded_at,
                ),
                stored_node(
                    "future-a",
                    "temporal-space-a",
                    "temporal-session-a",
                    NodeKind::Memory,
                    "future only temporal sentinel",
                    future_at,
                    None,
                    recorded_at,
                ),
                stored_node(
                    "other-space",
                    "temporal-space-b",
                    "temporal-session-b",
                    NodeKind::Memory,
                    "temporal sentinel unique",
                    event_at,
                    None,
                    recorded_at,
                ),
                stored_node(
                    "decision-a",
                    "temporal-space-a",
                    "temporal-session-a",
                    NodeKind::Decision,
                    "choose temporal isolation cache",
                    decision_at,
                    None,
                    recorded_at,
                ),
                stored_node(
                    "unrelated-error-a",
                    "temporal-space-a",
                    "temporal-session-a",
                    NodeKind::Error,
                    "unrelated nearby failure",
                    error_at,
                    None,
                    recorded_at,
                ),
            ])
            .expect("insert temporal fixtures");
        storage.flush().expect("flush temporal fixtures");
        drop(storage);
        let connection = Connection::open(&path).expect("open temporal read connection");
        (
            database,
            connection,
            AsOf::new(recorded_at + 86_400, recorded_at + 86_400),
            event_at,
        )
    }

    fn corpus_with(labels: &[(&str, usize)]) -> Corpus {
        let mut label_df = HashMap::new();
        let mut total = 0;
        for (label, count) in labels {
            total += count;
            *label_df.entry(normalize(label)).or_insert(0) += count;
        }
        let mut word_df = HashMap::new();
        for (label, count) in &label_df {
            for word in distinct_terms(label) {
                *word_df.entry(word).or_insert(0) += *count;
            }
        }
        Corpus {
            total: total.max(1),
            label_df,
            word_df,
            degree: HashMap::new(),
            slug_titles: HashMap::new(),
        }
    }

    fn record(kind: &str, label: &str, body: &str, at: i64) -> Record {
        Record {
            node_id: format!("n:{label}:{at}"),
            kind: kind.to_string(),
            label: label.to_string(),
            body: body.to_string(),
            source: None,
            valid_from: at,
            recorded_at: at,
            salience: 1.0,
            rank: 0.0,
        }
    }

    #[test]
    fn a_reused_heading_never_becomes_a_title() {
        let corpus = corpus_with(&[("Память", 58), ("Перегрев VS Code", 1)]);
        let entry = record(
            "memory",
            "Память",
            "[Перегрев VS Code (фикс)](vscode-thermal-fix.md) — почему камень шёл в 95°C",
            0,
        );
        assert_eq!(title_of(&entry, &corpus), "Перегрев VS Code (фикс)");
    }

    #[test]
    fn a_pasted_prompt_is_not_a_title() {
        let label = "SYSTEM: You are SAMPLEBOT auditing its own code. Cold, precise, ruthless. \
                     Output only the task. USER: analyse everything and report back now";
        let corpus = corpus_with(&[(label, 102)]);
        let entry = record("session", label, "Аудит собственного кода SAMPLEBOT", 0);
        assert_eq!(
            title_of(&entry, &corpus),
            "Аудит собственного кода SAMPLEBOT"
        );
    }

    #[test]
    fn a_specific_label_is_kept_as_written() {
        let corpus = corpus_with(&[("Swap 50 ГБ был прав, я нет", 1), ("Память", 58)]);
        let entry = record("memory", "Swap 50 ГБ был прав, я нет", "тело записи", 0);
        assert_eq!(title_of(&entry, &corpus), "Swap 50 ГБ был прав, я нет");
    }

    #[test]
    fn a_decision_outranks_a_session_of_equal_relevance() {
        let corpus = corpus_with(&[("Не устанавливать зависимости", 1), ("Промпт сессии", 1)]);
        let decision = record("decision", "Не устанавливать зависимости", "тело", 0);
        let session = record("session", "Промпт сессии", "тело", 0);
        assert!(
            representativeness(&decision, &corpus, 0.0)
                > representativeness(&session, &corpus, 0.0)
        );
    }

    #[test]
    fn seconds_apart_are_grouped_by_day_not_by_hour() {
        let base = 1_785_000_000;
        let granularity = choose_granularity(&[base, base + 2, base + 11, base + 30]);
        assert_eq!(granularity.name(), "day");
    }

    #[test]
    fn a_long_span_coarsens_until_it_fits_on_one_screen() {
        let day = 86_400;
        let two_weeks: Vec<i64> = (0..14).map(|index| index * day).collect();
        assert_eq!(choose_granularity(&two_weeks).name(), "day");
        let one_year: Vec<i64> = (0..400).map(|index| index * day).collect();
        assert_eq!(choose_granularity(&one_year).name(), "month");
        let three_years: Vec<i64> = (0..1_200).map(|index| index * day).collect();
        assert_eq!(choose_granularity(&three_years).name(), "quarter");
    }

    #[test]
    fn relative_periods_resolve_to_calendar_months() {
        let now = moment_of(1_787_000_000);
        let (start, end, label) = parse_period("2 месяца назад", now).unwrap();
        assert!(start < end);
        assert!(label.contains("назад"), "{label}");
        let (_, _, half) = parse_period("полгода назад", now).unwrap();
        assert!(half.contains("полгода"), "{half}");
        let (_, _, july) = parse_period("июль", now).unwrap();
        assert!(july.starts_with("июль"), "{july}");
        let (_, _, iso) = parse_period("2026-07", now).unwrap();
        assert_eq!(iso, "июль 2026");
    }

    #[test]
    fn relative_period_defaults_and_month_names_are_not_substrings() {
        let now = moment_of(1_787_000_000);
        let (week_start, week_end, _) = parse_period("последняя неделя", now).unwrap();
        let week_days = (week_end - week_start) / 86_400;
        assert!(
            (6..=8).contains(&week_days),
            "week span was {week_days} days"
        );

        let (month_start, month_end, _) = parse_period("последний месяц", now).unwrap();
        let month_days = (month_end - month_start) / 86_400;
        assert!(
            (29..=31).contains(&month_days),
            "month span was {month_days} days"
        );

        assert!(parse_period("мама", now).is_none());
        assert!(parse_period("команда", now).is_none());
        assert!(parse_period("маркер", now).is_none());
        assert_eq!(parse_period("в марте 2026", now).unwrap().2, "март 2026");
    }

    #[test]
    fn temporal_queries_are_space_isolated_and_bitemporal() {
        let (_database, connection, as_of, event_at) = scoped_database("scope");
        let answer = when(
            &connection,
            "temporal-space-a",
            as_of,
            "temporal sentinel unique",
        )
        .expect("run temporal when");
        assert_eq!(answer["found"], 1, "{answer:#}");
        assert_eq!(answer["first_seen"], iso_date(event_at));
        let encoded = answer.to_string();
        assert!(!encoded.contains("future-a"), "{answer:#}");
        assert!(!encoded.contains("forgotten-a"), "{answer:#}");
        assert!(!encoded.contains("other-space"), "{answer:#}");

        let future = when(
            &connection,
            "temporal-space-a",
            as_of,
            "future only temporal sentinel",
        )
        .expect("query future-only record");
        assert_eq!(future["found"], 0, "{future:#}");

        let missing = when(
            &connection,
            "space-that-does-not-exist",
            as_of,
            "temporal sentinel unique",
        )
        .expect("query missing space");
        assert_eq!(missing["found"], 0, "{missing:#}");

        let timeline = timeline(
            &connection,
            "temporal-space-a",
            as_of,
            TimelineArgs {
                month: Some("2020-07".to_owned()),
                from: None,
                to: None,
            },
        )
        .expect("run temporal timeline");
        assert_eq!(timeline["records"], 3, "{timeline:#}");
        let encoded = timeline.to_string();
        assert!(!encoded.contains("future-a"), "{timeline:#}");
        assert!(!encoded.contains("forgotten-a"), "{timeline:#}");
        assert!(!encoded.contains("other-space"), "{timeline:#}");
    }

    #[test]
    fn nearby_errors_are_context_not_invented_causes() {
        let (_database, connection, as_of, _) = scoped_database("causality");
        let answer = explain(
            &connection,
            "temporal-space-a",
            as_of,
            "choose temporal isolation cache",
        )
        .expect("explain temporal decision");
        let story = answer["story"].as_str().expect("story text");
        assert!(
            story.contains("Причина в памяти не зафиксирована"),
            "{story}"
        );
        assert!(story.contains("причинная связь не подтверждена"), "{story}");
        assert!(!story.contains("Этому предшествовали грабли"), "{story}");
    }

    #[test]
    fn russian_counts_agree_with_their_nouns() {
        assert_eq!(records_phrase(1), "1 запись");
        assert_eq!(records_phrase(3), "3 записи");
        assert_eq!(records_phrase(11), "11 записей");
        assert_eq!(records_phrase(21), "21 запись");
        assert_eq!(plural(4, "период", "периода", "периодов"), "4 периода");
        assert_eq!(plural(5, "период", "периода", "периодов"), "5 периодов");
    }

    #[test]
    fn a_word_used_everywhere_is_not_a_theme() {
        let corpus = corpus_with(&[("память", 5_000), ("graphify граф", 40)]);
        let suffixes = ["узлы", "рёбра", "кластеры", "экспорт", "слияние", "запросы"];
        let records: Vec<Record> = suffixes
            .iter()
            .enumerate()
            .map(|(index, suffix)| {
                record(
                    "memory",
                    &format!("память graphify граф {suffix}"),
                    "тело",
                    index as i64,
                )
            })
            .collect();
        let themes = themes_of(&records, &corpus);
        assert!(themes.iter().all(|theme| theme.term != "память"));
        assert!(themes.iter().any(|theme| theme.term.contains("graphify")));
    }

    #[test]
    fn one_heading_filled_in_many_times_is_not_a_theme() {
        let corpus = corpus_with(&[("Reusable knowledge", 68), ("прочее", 5_000)]);
        let records: Vec<Record> = (0..8)
            .map(|index| record("memory", "Reusable knowledge", "тело", index))
            .collect();
        let themes = themes_of(&records, &corpus);
        assert!(
            themes.is_empty(),
            "форма, заполненная восемь раз, выдана за тему: {:?}",
            themes.iter().map(|theme| &theme.term).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_file_stem_yields_to_the_words_the_corpus_uses_for_it() {
        let mut corpus = corpus_with(&[("asset-skills-graph-binding", 4)]);
        corpus.slug_titles.insert(
            "asset-skills-graph-binding".to_string(),
            "Asset-навыки в графе".to_string(),
        );
        let entry = record(
            "memory",
            "asset-skills-graph-binding",
            "**УСПЕХ (2026-07-07).** Все 13 asset-навыков теперь узлы в графе",
            0,
        );
        assert_eq!(title_of(&entry, &corpus), "Asset-навыки в графе");
    }

    #[test]
    fn markdown_links_recover_the_naming_table() {
        let body =
            "[Перегрев: git-скан дома + гейты](thermal-home-git-scan-storm.md) — ноут до 98°C";
        assert_eq!(
            markdown_links(body),
            vec![(
                "thermal-home-git-scan-storm".to_string(),
                "Перегрев: git-скан дома + гейты".to_string()
            )]
        );
    }

    #[test]
    fn an_overlong_title_is_cut_where_its_first_thought_ends() {
        let prompt = "Продолжаю Atlas Board по протоколу непрерывности (8.6): сначала \
                      STATE.md → git log → git status, чтобы понять, где оборвалась \
                      прошлая сессия";
        let cut = condense(prompt, TITLE_LIMIT);
        assert_eq!(
            cut,
            "Продолжаю Atlas Board по протоколу непрерывности (8.6)"
        );
        assert!(!cut.ends_with('…'), "{cut}");
    }

    #[test]
    fn a_briefing_header_marks_a_handover_not_a_topic() {
        assert!(looks_like_instruction(
            "Лейн: text-модель поверх локального CLI-агента Репозиторий: рабочее дерево"
        ));
        assert!(looks_like_instruction(
            "@\"/home/user/Рабочий стол/NOTES.md\" Итак, агент, привет"
        ));
        assert!(looks_like_instruction(
            "рабочее дерево, в котором ты работаешь, лежит рядом"
        ));
        assert!(!looks_like_instruction(
            "Swap 50 ГБ был прав, я нет: индексатор съел 11,5 ГБ и 30 ГБ свопа"
        ));
    }

    #[test]
    fn a_green_run_pasted_as_a_title_is_not_a_decision() {
        assert!(numeric_share("01 source-монолит: 189/189, MISSING 0, HASH MISMATCH 0") >= 0.3);
        assert!(numeric_share("Не устанавливать зависимости") < 0.3);
    }

    #[test]
    fn one_shared_word_out_of_four_cannot_win_a_question() {
        let corpus = corpus_with(&[
            ("Установить Kitty локально для фонового изображения", 1),
            ("Полную пересборку graphify локально не тянет память", 1),
        ]);
        let words = terms("почему graphify нельзя пересобрать локально");
        let stray = record(
            "decision",
            "Установить Kitty локально для фонового изображения",
            "терминал",
            0,
        );
        let answer = record(
            "memory",
            "Полную пересборку graphify локально не тянет память",
            "упирается в память после AST; собирать в CI, нельзя локально",
            0,
        );
        assert!(query_coverage(&answer, &words) > query_coverage(&stray, &words));
        assert!(
            subject_score(&answer, &corpus, 0.0, &words)
                > subject_score(&stray, &corpus, 0.0, &words)
        );
    }
}
