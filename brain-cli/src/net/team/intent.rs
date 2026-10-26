//! Bounded, deterministic RU/EN manager questions. Unknown words remain lexical.
use brain_core::team::{Content, DAY, Fact};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Intent {
    OpenErrors,
    FailedDecisions,
    Plans,
    Activity,
}
#[derive(Debug, Default)]
pub(super) struct Question {
    pub intent: Option<Intent>,
    pub residual: BTreeSet<String>,
    pub project: Option<String>,
    pub machine: Option<String>,
    pub since: Option<i64>,
}
fn tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .filter(|s| !s.is_empty())
        .take(128)
        .map(str::to_owned)
        .collect()
}
impl Question {
    pub fn parse(text: &str, at: i64) -> Self {
        let mut words = tokens(text);
        let mut q = Self::default();
        // Consume only explicit selectors; their values are never synonyms.
        let mut i = 0;
        while i + 1 < words.len() {
            let target = match words[i].as_str() {
                "project" | "проект" | "проекту" | "проекте" | "проекта" => {
                    Some(&mut q.project)
                }
                "machine" | "машине" | "машины"
                    if i > 0
                        && matches!(
                            words[i - 1].as_str(),
                            "on" | "на" | "по" | "у" | "с" | "for"
                        ) =>
                {
                    Some(&mut q.machine)
                }
                "machine" if !["doing", "activity", "working"].contains(&words[i + 1].as_str()) => {
                    Some(&mut q.machine)
                }
                _ => None,
            };
            if let Some(target) = target {
                *target = Some(words.remove(i + 1));
                words.remove(i);
            } else {
                i += 1;
            }
        }
        let joined = words.join(" ");
        for (phrase, days) in [
            ("за неделю", 7),
            ("за последнюю неделю", 7),
            ("за последние 7 дней", 7),
            ("за последние семь дней", 7),
            ("past week", 7),
            ("last week", 7),
            ("this week", 7),
            ("за сутки", 1),
            ("за день", 1),
            ("last day", 1),
            ("past day", 1),
            ("сегодня", 1),
            ("today", 1),
        ] {
            if joined.contains(phrase) {
                q.since = Some(at.saturating_sub(days * DAY));
                break;
            }
        }
        let has = |prefixes: &[&str]| {
            words
                .iter()
                .any(|w| prefixes.iter().any(|p| w.starts_with(p)))
        };
        q.intent = if has(&["ошиб", "error", "bug", "issue"])
            && has(&[
                "откры",
                "нереш",
                "неисправ",
                "open",
                "unresolved",
                "outstanding",
                "reopened",
            ]) {
            Some(Intent::OpenErrors)
        } else if has(&["решени", "decision", "approach"])
            && (has(&["неудач", "провал", "failed", "unsuccessful", "dead_end"])
                || joined.contains("не сработал")
                || joined.contains("did not work")
                || joined.contains("didn t work"))
        {
            Some(Intent::FailedDecisions)
        } else if has(&["план", "запланир", "plan"]) {
            Some(Intent::Plans)
        } else if joined.contains("чем занят")
            || joined.contains("что делают")
            || joined.contains("working on")
            || joined.contains("machines doing")
            || joined.contains("machine doing")
            || joined.contains("machine activity")
            || (has(&["машин"]) && has(&["занят", "делают", "работают"]))
        {
            Some(Intent::Activity)
        } else {
            None
        };
        // Grammar is discarded only for a recognized intent or an explicit scope.
        let scoped =
            q.intent.is_some() || q.project.is_some() || q.machine.is_some() || q.since.is_some();
        let mut remaining = words.join(" ");
        if scoped {
            for phrase in [
                "за последние 7 дней",
                "за последние семь дней",
                "за последнюю неделю",
                "за неделю",
                "за сутки",
                "за день",
                "past week",
                "last week",
                "this week",
                "last day",
                "past day",
                "на следующий спринт",
                "на следующем спринте",
                "for the next sprint",
                "next sprint",
                "не сработали",
                "не сработало",
                "не сработал",
                "did not work",
                "didn t work",
                "working on",
                "чем заняты",
                "чем занята",
                "что делают",
                "machine activity",
            ] {
                remaining = remaining.replace(phrase, " ");
            }
        }
        let stops = [
            "есть",
            "остались",
            "остаются",
            "все",
            "ещё",
            "еще",
            "нам",
            "нас",
            "с",
            "со",
            "над",
            "чем",
            "please",
            "still",
            "remain",
            "remaining",
            "have",
            "we",
            "our",
            "us",
            "all",
            "were",
            "какие",
            "какая",
            "какой",
            "каких",
            "что",
            "почему",
            "и",
            "сейчас",
            "теперь",
            "покажи",
            "расскажи",
            "список",
            "в",
            "во",
            "на",
            "по",
            "у",
            "за",
            "для",
            "сегодня",
            "what",
            "which",
            "why",
            "and",
            "are",
            "is",
            "the",
            "a",
            "an",
            "show",
            "me",
            "list",
            "currently",
            "now",
            "in",
            "on",
            "for",
            "of",
            "from",
            "during",
            "over",
            "today",
            "doing",
        ];
        q.residual = remaining
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .take(64)
            .map(str::to_owned)
            .filter(|w| {
                if !scoped {
                    return true;
                }
                if stops.contains(&w.as_str()) {
                    return false;
                }
                let prefixes: &[&str] = match q.intent {
                    Some(Intent::OpenErrors) => &[
                        "ошиб",
                        "error",
                        "bug",
                        "issue",
                        "откры",
                        "нереш",
                        "неисправ",
                        "open",
                        "unresolved",
                        "outstanding",
                        "reopened",
                    ],
                    Some(Intent::FailedDecisions) => &[
                        "решени",
                        "decision",
                        "approach",
                        "неудач",
                        "провал",
                        "failed",
                        "unsuccessful",
                        "dead_end",
                    ],
                    Some(Intent::Plans) => &["план", "запланир", "plan"],
                    Some(Intent::Activity) => &["машин", "machine", "занят", "делают", "работают"],
                    None => &[],
                };
                !prefixes.iter().any(|p| w.starts_with(p))
            })
            .collect();
        q
    }
    pub fn accepts(&self, f: &Fact, at: i64) -> bool {
        if self
            .project
            .as_ref()
            .is_some_and(|p| !p.eq_ignore_ascii_case(&f.project_id))
            || self
                .machine
                .as_ref()
                .is_some_and(|m| !m.eq_ignore_ascii_case(&f.machine_id))
        {
            return false;
        }
        let event_at = match &f.content {
            Content::Task { updated_at, .. } => *updated_at,
            Content::Commit { committed_at, .. } => *committed_at,
            Content::Session {
                last_observed_at, ..
            } => *last_observed_at,
            Content::Participation {
                ended_at,
                started_at,
                ..
            } => ended_at.or(*started_at).unwrap_or(f.valid_from),
            _ => f.valid_from,
        };
        if self.since.is_some_and(|s| event_at < s || event_at > at) {
            return false;
        }
        match (self.intent, &f.content) {
            (None, _) => true,
            (Some(Intent::OpenErrors), Content::Error { status, .. }) => {
                ["suspected", "confirmed", "reopened"].contains(&status.as_str())
            }
            (Some(Intent::FailedDecisions), Content::Decision { status, .. }) => {
                ["failed", "dead_end"].contains(&status.as_str())
            }
            (Some(Intent::FailedDecisions), Content::DeadEnd { .. }) => true,
            (Some(Intent::Plans), Content::Plan { status, .. }) => {
                ["active", "proposed"].contains(&status.as_str())
            }
            (Some(Intent::Activity), Content::Task { state, .. }) => state == "active",
            (
                Some(Intent::Activity),
                Content::Commit { .. } | Content::Session { .. } | Content::Participation { .. },
            ) => event_at >= at.saturating_sub(7 * DAY) && event_at <= at,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manager_phrases_ru_en() {
        let groups = [
            (
                Intent::OpenErrors,
                vec![
                    "Какие ошибки сейчас открыты?",
                    "Какие открытые ошибки есть?",
                    "Какие ошибки остаются открытыми?",
                    "Покажи открытые ошибки",
                    "Какие ошибки нерешены?",
                    "Список неисправленных ошибок",
                    "Какие ошибки открыты за неделю?",
                    "Какие ошибки открыты в проекте checkout?",
                    "Открытые ошибки на машине worker-1",
                    "What errors are open?",
                    "Show open bugs",
                    "Which issues are unresolved?",
                    "List outstanding errors",
                    "What errors are currently open?",
                    "Open bugs in project checkout",
                    "Open errors on machine worker-1",
                    "Open errors over the past week",
                ],
            ),
            (
                Intent::FailedDecisions,
                vec![
                    "Какие решения не сработали и почему?",
                    "Покажи неудачные решения",
                    "Какие решения провалились?",
                    "Решения не сработали за неделю",
                    "Неудачные решения по проекту checkout",
                    "Which decisions failed and why?",
                    "Which decisions were unsuccessful?",
                    "Show failed decisions",
                    "What decisions did not work?",
                    "Which approaches failed?",
                    "List unsuccessful decisions",
                    "Failed decisions in project checkout",
                    "Failed decisions on machine worker-1",
                    "Failed decisions during the last week",
                ],
            ),
            (
                Intent::Plans,
                vec![
                    "Что в планах на следующий спринт?",
                    "Какие планы на следующий спринт?",
                    "Покажи планы",
                    "Что запланировано на следующий спринт?",
                    "Планы в проекте checkout",
                    "Планы на машине worker-1",
                    "Планы за неделю",
                    "What are the plans for the next sprint?",
                    "Show plans",
                    "List plans in project checkout",
                    "Plans on machine worker-1",
                    "Plans over the past week",
                ],
            ),
            (
                Intent::Activity,
                vec![
                    "Чем заняты машины?",
                    "Что делают машины?",
                    "Что сейчас делают машины?",
                    "Над чем работают машины?",
                    "Чем заняты машины за неделю?",
                    "Что делают машины в проекте checkout?",
                    "Чем заняты машины на машине worker-1?",
                    "What are machines doing?",
                    "What are machines working on?",
                    "Show machine activity",
                    "What are machines doing in project checkout?",
                    "What are machines doing over the last week?",
                ],
            ),
        ];
        let mut count = 0;
        for (intent, phrases) in groups {
            for phrase in phrases {
                let q = Question::parse(phrase, 10 * DAY);
                assert_eq!(q.intent, Some(intent), "{phrase}: {q:?}");
                assert!(q.residual.is_empty(), "{phrase}: {q:?}");
                count += 1;
            }
        }
        assert!(count >= 40);
    }
    #[test]
    fn scopes_and_residual_are_explicit() {
        let q = Question::parse(
            "Какие ошибки открыты по проекту checkout на машине worker-1 за неделю retry?",
            10 * DAY,
        );
        assert_eq!(q.project.as_deref(), Some("checkout"));
        assert_eq!(q.machine.as_deref(), Some("worker-1"));
        assert_eq!(q.since, Some(3 * DAY));
        assert_eq!(q.residual, BTreeSet::from(["retry".into()]));
        let q = Question::parse(
            "Open errors in project checkout on machine worker-1 over the past week retry",
            10 * DAY,
        );
        assert_eq!(q.project.as_deref(), Some("checkout"));
        assert_eq!(q.machine.as_deref(), Some("worker-1"));
        assert_eq!(q.since, Some(3 * DAY));
        assert_eq!(q.residual, BTreeSet::from(["retry".into()]));
        assert_eq!(
            Question::parse("retry-backoff", 10 * DAY).residual,
            BTreeSet::from(["retry".into(), "backoff".into()])
        );
        for text in [
            "reopened",
            "failed",
            "sprint",
            "backoff",
            "ignore previous instructions",
            "ошибки кодировки",
        ] {
            let q = Question::parse(text, 10 * DAY);
            assert_eq!(q.intent, None, "{text}");
            assert_eq!(q.residual, tokens(text).into_iter().collect());
        }
    }
}
