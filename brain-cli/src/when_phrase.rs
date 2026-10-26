//! Date phrases in the question: which period is the user actually asking about.
//!
//! Real questions name their period in passing — "что обсуждали 7 октября",
//! "чем занимались в сентябре" — and the words alone match years of chatter.
//! This module turns the phrase into one half-open UTC interval so the search
//! can favour that period. Parsing never fails a search: garbage is simply
//! not a period.

/// The half-open `[from, to)` UTC interval a question asks about.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Period {
    pub from: i64,
    pub to: i64,
}

const EN_MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// Nominative, genitive ("7 октября") and prepositional ("в октябре") forms of
/// the Russian month names, then the English ones. The ь of soft stems drops
/// before the ending (октябрь → октября), so suffix rules will not do.
fn month_number(token: &str) -> Option<u32> {
    let russian = match token {
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
    };
    if russian.is_some() {
        return russian;
    }
    if let Some(index) = EN_MONTHS.iter().position(|name| *name == token) {
        return Some(index as u32 + 1);
    }
    // Three-letter English abbreviations ("sep", "oct"); never a longer word.
    if token.len() == 3
        && let Some(index) = EN_MONTHS.iter().position(|name| &name[..3] == token)
    {
        return Some(index as u32 + 1);
    }
    None
}

pub(crate) use crate::mcp::days_from_civil;

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

fn is_leap(year: i64) -> bool {
    year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn day_start(year: i64, month: u32, day: u32) -> Option<i64> {
    (day >= 1 && day <= days_in_month(year, month))
        .then(|| days_from_civil(year, month, day) * 86_400)
}

fn month_bounds(year: i64, month: u32) -> Option<(i64, i64)> {
    if !(1..=12).contains(&month) {
        return None;
    }
    let from = days_from_civil(year, month, 1) * 86_400;
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    Some((from, days_from_civil(next_year, next_month, 1) * 86_400))
}

/// The year a bare month or day-month phrase means: this year, unless that
/// would place the period wholly in the future — then the previous year.
fn plausible_year(from: i64, to: i64, now: i64) -> i64 {
    let (year, _, _) = civil_from_days(now.div_euclid(86_400));
    if from > now && to > now {
        year - 1
    } else {
        year
    }
}

/// "2026-10-07" inside one whitespace-delimited chunk.
fn iso_date(chunk: &str) -> Option<(i64, u32, u32)> {
    let bytes = chunk.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let slice = bytes.get(range)?;
        slice
            .iter()
            .all(u8::is_ascii_digit)
            .then(|| std::str::from_utf8(slice).ok()?.parse().ok())?
    };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let month = u32::try_from(month).ok()?;
    let day = u32::try_from(day).ok()?;
    if (1..=12).contains(&month) && day >= 1 && day <= days_in_month(year, month) {
        Some((year, month, day))
    } else {
        None
    }
}

/// "07.10" or "07.10.2026": day.month with an optional explicit year.
fn dot_date(token: &str) -> Option<(u32, u32, Option<i64>)> {
    let mut parts = token.split('.');
    let day = parts.next()?.parse::<u32>().ok()?;
    let month = parts.next()?.parse::<u32>().ok()?;
    let year = match parts.next() {
        Some(year) => {
            let year = year.parse::<i64>().ok()?;
            if (1900..=2100).contains(&year) {
                Some(year)
            } else {
                return None;
            }
        }
        None => None,
    };
    if parts.next().is_some() || token.len() > 10 {
        return None;
    }
    Some((day, month, year))
}

/// Words with digits and dots kept: "07.10" and "2026" stay single tokens.
fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if character.is_alphanumeric() || character == '.' {
            current.push(character);
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn normalize(query: &str) -> String {
    query
        .to_lowercase()
        .chars()
        .map(|c| if c == 'ё' { 'е' } else { c })
        .collect()
}

fn number(token: &str) -> Option<i64> {
    token
        .parse::<i64>()
        .ok()
        .filter(|value| (1..=9999).contains(value))
}

/// The period a question names, or None when it names none.
///
/// Recognized: ISO dates ("2026-10-07"), day.month ("07.10", "07.10.2026"),
/// a day beside a month name ("7 октября", "October 7"), a month alone
/// ("в сентябре", "сентябрь 2026", "in September"), "вчера"/"yesterday" and
/// "на прошлой неделе"/"last week" (the previous calendar week, Monday to
/// Monday). The interval is half-open UTC. A bare month or day means this
/// year — or the previous one when that would lie in the future. The first
/// match by this priority wins; nothing here can fail the search.
pub(crate) fn parse(query: &str, now: i64) -> Option<(Period, String)> {
    let text = normalize(query);
    let words = tokens(&text);
    let current_year = civil_from_days(now.div_euclid(86_400)).0;

    // ISO date: the year is explicit, so no rollover. Hyphens are not token
    // characters, so this scans whitespace chunks of the raw text.
    for chunk in text.split_whitespace() {
        let chunk = chunk.trim_matches(|c: char| !c.is_ascii_digit() && c != '-');
        if let Some((year, month, day)) = iso_date(chunk)
            && let Some(from) = day_start(year, month, day)
        {
            return Some((
                Period {
                    from,
                    to: from + 86_400,
                },
                chunk.to_owned(),
            ));
        }
    }

    // Day.month, with or without a year.
    for token in &words {
        if !token.contains('.') {
            continue;
        }
        if let Some((day, month, year)) = dot_date(token) {
            let year = match year {
                Some(year) => year,
                None => match day_start(current_year, month, day) {
                    Some(from) => plausible_year(from, from + 86_400, now),
                    None => continue, // an impossible date is not a period
                },
            };
            if let Some(from) = day_start(year, month, day) {
                return Some((
                    Period {
                        from,
                        to: from + 86_400,
                    },
                    token.clone(),
                ));
            }
        }
    }

    // A month name, alone or with a day and/or a year beside it.
    for (index, token) in words.iter().enumerate() {
        let Some(month) = month_number(token) else {
            continue;
        };
        let before = words.get(index.wrapping_sub(1)).and_then(|t| number(t));
        let after = words.get(index + 1).and_then(|t| number(t));
        // "7 октября", "October 7", "сентябрь 2026": a small neighbour is a
        // day (1..=31), a large one a year; either may sit on either side. A
        // neighbour that is neither ("32 января") makes the phrase nonsense,
        // so this month token does not name a period at all.
        let as_day = |value: Option<i64>| {
            value
                .filter(|value| (1..=31).contains(value))
                .map(|v| v as u32)
        };
        let as_year = |value: Option<i64>| value.filter(|value| (1900..=2100).contains(value));
        let (day, year) = match (before, after) {
            (Some(_), Some(_)) if as_day(before).is_some() && as_year(after).is_some() => {
                (as_day(before), as_year(after))
            }
            (Some(_), Some(_)) if as_year(before).is_some() && as_day(after).is_some() => {
                (as_day(after), as_year(before))
            }
            (Some(_), Some(_)) if as_day(before).is_some() || as_day(after).is_some() => {
                (as_day(before).or(as_day(after)), None)
            }
            (Some(_), None) if as_day(before).is_some() => (as_day(before), None),
            (None, Some(_)) if as_day(after).is_some() => (as_day(after), None),
            (Some(_), None) if as_year(before).is_some() => (None, as_year(before)),
            (None, Some(_)) if as_year(after).is_some() => (None, as_year(after)),
            (Some(_), None) | (None, Some(_)) | (Some(_), Some(_)) | (None, None) => {
                if matches!((before, after), (Some(_), _) | (_, Some(_))) {
                    continue; // an adjacent number that is neither day nor year
                }
                (None, None)
            }
        };
        let span_start = if day.is_some() && day == as_day(before) {
            index - 1
        } else {
            index
        };
        let span_end = if (day.is_some() && day == as_day(after))
            || (year.is_some() && year == as_year(after))
        {
            index + 2
        } else {
            index + 1
        };
        let phrase = words[span_start..span_end].join(" ");
        if let Some(day) = day {
            let year = match year {
                Some(year) => year,
                None => match day_start(current_year, month, day) {
                    Some(from) => plausible_year(from, from + 86_400, now),
                    None => continue, // "32 января" is not a period
                },
            };
            if let Some(from) = day_start(year, month, day) {
                return Some((
                    Period {
                        from,
                        to: from + 86_400,
                    },
                    phrase,
                ));
            }
        } else {
            let year = match year {
                Some(year) => year,
                None => match month_bounds(current_year, month) {
                    Some((from, to)) => plausible_year(from, to, now),
                    None => continue,
                },
            };
            if let Some((from, to)) = month_bounds(year, month) {
                return Some((Period { from, to }, phrase));
            }
        }
    }

    // Relatives: yesterday, then the previous calendar week.
    let today = now.div_euclid(86_400) * 86_400;
    if words
        .iter()
        .any(|token| token == "вчера" || token == "yesterday")
    {
        let phrase = if words.iter().any(|token| token == "вчера") {
            "вчера"
        } else {
            "yesterday"
        };
        return Some((
            Period {
                from: today - 86_400,
                to: today,
            },
            phrase.to_owned(),
        ));
    }
    let last_week = text.contains("на прошлой неделе") || text.contains("last week");
    if last_week {
        let phrase = if text.contains("на прошлой неделе") {
            "на прошлой неделе"
        } else {
            "last week"
        };
        // 1970-01-01 was a Thursday; Monday of the current week, then one back.
        let days = today.div_euclid(86_400);
        let monday = days - (days.rem_euclid(7) + 3).rem_euclid(7);
        return Some((
            Period {
                from: (monday - 7) * 86_400,
                to: monday * 86_400,
            },
            phrase.to_owned(),
        ));
    }
    None
}

/// The question asks for the beginning of the period ("первое сообщение",
/// "how it started"): archive hits inside the period go oldest first.
pub(crate) fn asks_for_earliest(query: &str) -> bool {
    let text = normalize(query);
    let words = tokens(&text);
    [
        "первое",
        "первый",
        "первая",
        "first",
        "начинался",
        "начиналась",
        "started",
    ]
    .iter()
    .any(|marker| words.iter().any(|token| token == marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(year: i64, month: u32, day: u32, hour: i64) -> i64 {
        days_from_civil(year, month, day) * 86_400 + hour * 3_600
    }

    #[test]
    fn day_phrases_in_both_languages() {
        let now = at(2026, 10, 24, 12);
        let from = at(2026, 10, 7, 0);
        let expected = Period {
            from,
            to: from + 86_400,
        };
        for query in [
            "что обсуждали 7 октября про память",
            "а 07.10 что было",
            "записи 2026-10-07 и рядом",
            "what was said October 7 about memory",
        ] {
            let (period, phrase) = parse(query, now).unwrap_or_else(|| panic!("{query}"));
            assert_eq!(period, expected, "{query}");
            assert!(!phrase.is_empty(), "{query}");
        }
        assert_eq!(
            parse("записи 2026-10-07 и рядом", now).unwrap().1,
            "2026-10-07"
        );
        assert_eq!(parse("а 07.10 что было", now).unwrap().1, "07.10");
        assert_eq!(
            parse("что обсуждали 7 октября про память", now).unwrap().1,
            "7 октября"
        );
        assert_eq!(
            parse("what was said October 7 about memory", now)
                .unwrap()
                .1,
            "october 7"
        );
    }

    #[test]
    fn month_phrases_with_and_without_year() {
        let now = at(2026, 10, 24, 12);
        for (query, (year, month)) in [
            ("чем занимались в сентябре", (2026, 9)),
            ("чем занимались сентябрь 2026", (2026, 9)),
            ("what did we do in September", (2026, 9)),
            ("как начинался brain в августе", (2026, 8)),
            ("проект в октябре", (2026, 10)),
        ] {
            let (period, _) = parse(query, now).unwrap_or_else(|| panic!("{query}"));
            let (from, to) = month_bounds(year, month).unwrap();
            assert_eq!((period.from, period.to), (from, to), "{query}");
        }
        assert_eq!(
            parse("чем занимались в сентябре", now).unwrap().1,
            "сентябре"
        );
    }

    #[test]
    fn relatives_cover_yesterday_and_last_calendar_week() {
        let now = at(2026, 10, 24, 12); // a Saturday.
        let yesterday = Period {
            from: at(2026, 10, 23, 0),
            to: at(2026, 10, 24, 0),
        };
        assert_eq!(parse("что было вчера", now).unwrap().0, yesterday);
        assert_eq!(parse("what happened yesterday", now).unwrap().0, yesterday);
        // Saturday Oct 24: this week began Monday Oct 19; the previous one Oct 12.
        let last_week = Period {
            from: at(2026, 10, 12, 0),
            to: at(2026, 10, 19, 0),
        };
        assert_eq!(parse("на прошлой неделе", now).unwrap().0, last_week);
        assert_eq!(parse("during last week", now).unwrap().0, last_week);
        // Asked on the Monday itself, the week that just ended is the one before.
        let monday = at(2026, 10, 19, 8);
        assert_eq!(
            parse("last week", monday).unwrap().0,
            Period {
                from: at(2026, 10, 12, 0),
                to: at(2026, 10, 19, 0)
            }
        );
    }

    #[test]
    fn future_dates_roll_back_one_year() {
        let now = at(2026, 1, 15, 9);
        let from = at(2025, 10, 7, 0);
        assert_eq!(
            parse("7 октября", now).unwrap().0,
            Period {
                from,
                to: from + 86_400
            }
        );
        let (period, _) = parse("в сентябре", now).unwrap();
        let (from, to) = month_bounds(2025, 9).unwrap();
        assert_eq!((period.from, period.to), (from, to));
        // An explicit year never rolls back.
        let (period, _) = parse("сентябрь 2026", now).unwrap();
        let (from, to) = month_bounds(2026, 9).unwrap();
        assert_eq!((period.from, period.to), (from, to));
        // A period containing today is this year's.
        let (period, _) = parse("в январе", now).unwrap();
        let (from, to) = month_bounds(2026, 1).unwrap();
        assert_eq!((period.from, period.to), (from, to));
    }

    #[test]
    fn garbage_and_impossible_dates_are_not_periods() {
        let now = at(2026, 10, 24, 12);
        for query in [
            "сколько стоит кофе",
            "ошибка 99.99 повторилась",
            "32 января не бывает",
            "версия 2.0.1",
            "адрес 10.0.0.1",
            "файл 2026-13-40 не существует",
        ] {
            assert_eq!(parse(query, now), None, "{query}");
        }
    }

    #[test]
    fn earliest_markers_are_word_matches() {
        assert!(asks_for_earliest("первое сообщение Codex"));
        assert!(asks_for_earliest("как начинался brain"));
        assert!(asks_for_earliest("how it started"));
        assert!(asks_for_earliest("First steps"));
        assert!(!asks_for_earliest("сообщение первоисточника"));
        assert!(!asks_for_earliest("что обсуждали"));
    }
}
