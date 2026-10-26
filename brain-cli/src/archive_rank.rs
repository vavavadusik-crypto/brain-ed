//! Keep station knowledge ahead of incidental conversation word matches.

use serde_json::{Value, json};

use crate::{ArchiveSearch, archive_uri_component};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ArchiveRanking {
    CoreFirst,
    Conversation,
}

impl ArchiveRanking {
    /// A deliberately small, local heuristic, shared by CLI and MCP. A named
    /// period retains the existing temporal search behavior. Without a period,
    /// require conversation wording or personal history, not just past tense
    /// ("why was SQLite chosen?" is still a knowledge question).
    pub(crate) fn for_question(query: &str, has_period: bool) -> Self {
        let lower = query.to_lowercase();
        let words: Vec<_> = lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .collect();
        let conversation = words.iter().any(|word| {
            ["разговор", "обсуждал", "обсуд", "переписк"]
                .iter()
                .any(|prefix| word.starts_with(prefix))
                || matches!(
                    *word,
                    "чат"
                        | "чате"
                        | "чата"
                        | "чатах"
                        | "chat"
                        | "conversation"
                        | "conversations"
                        | "discussed"
                        | "said"
                        | "talked"
                        | "transcript"
                )
        });
        let message_lookup = words
            .iter()
            .any(|word| word.starts_with("сообщени") || matches!(*word, "message" | "messages"))
            && words.iter().any(|word| {
                matches!(
                    *word,
                    "первое"
                        | "последнее"
                        | "найди"
                        | "first"
                        | "last"
                        | "find"
                        | "sent"
                        | "wrote"
                        | "my"
                )
            });
        let personal = words
            .iter()
            .any(|word| matches!(*word, "мы" | "нам" | "нами" | "we" | "our" | "us"));
        let past_action = words.iter().any(|word| {
            [
                "делал",
                "отверг",
                "решил",
                "выбра",
                "отказ",
                "пробова",
                "договор",
            ]
            .iter()
            .any(|prefix| word.starts_with(prefix))
                || matches!(
                    *word,
                    "did" | "rejected" | "decided" | "chose" | "tried" | "agreed" | "stopped"
                )
        });
        let resuming = words
            .iter()
            .any(|word| matches!(*word, "остановились" | "остановилась" | "остановился"))
            || lower.contains("leave off");
        let evolution = (lower.contains("в начале") || words.contains(&"раньше"))
            && words.contains(&"сейчас")
            || words.contains(&"before") && words.contains(&"now");
        // Retrospective deployment and origin questions need conversation
        // evidence even without an explicit date or the pronoun "we".
        let past_state = words.contains(&"что")
            && words
                .iter()
                .any(|word| matches!(*word, "работало" | "стояло" | "запускалось" | "находилось"))
            || words.contains(&"what")
                && words.iter().any(|word| matches!(*word, "ran" | "worked"));
        let origin = words.contains(&"когда")
            && words
                .iter()
                .any(|word| word.starts_with("появил") || word.starts_with("введен"))
            || words.contains(&"when")
                && words
                    .iter()
                    .any(|word| matches!(*word, "introduced" | "appeared"));
        if has_period
            || conversation
            || message_lookup
            || personal && past_action
            || resuming
            || evolution
            || past_state
            || origin
        {
            Self::Conversation
        } else {
            Self::CoreFirst
        }
    }
}

/// For knowledge questions, retain every core hit in its original order and
/// budget; archive evidence can only use the remainder. Thus a top-N prefix
/// containing N core hits cannot lose recall just by enabling the archive.
/// For conversation/history questions, keep the original disjoint-list RRF
/// (k=60, core wins ties) and the archive's one-third token ceiling.
/// Both modes count UTF-8 bytes, bound excerpts at code point boundaries, and
/// preserve the archive's untrusted provenance independently of Brain spaces.
pub(crate) fn fuse_conversations(
    core: Vec<Value>,
    archive: ArchiveSearch,
    budget: usize,
    limit: usize,
    ranking: ArchiveRanking,
) -> (Vec<Value>, usize, usize) {
    let fused = archive.included;
    let core_first = ranking == ArchiveRanking::CoreFirst;
    // Respect MCP's result limit before reserving bytes for core: records that
    // cannot be returned must not prevent an archive-only answer from fitting.
    let core: Vec<_> = core.into_iter().take(limit).collect();
    let mut core_used = 0_usize;
    let mut retained_core = Vec::new();
    if core_first {
        for hit in &core {
            let cost = hit["token_len"].as_u64().unwrap_or(0) as usize;
            if cost <= budget.saturating_sub(core_used) {
                core_used += cost;
                retained_core.push(hit.clone());
            }
        }
    }
    let archive_budget = if core_first {
        (budget / 3).min(budget.saturating_sub(core_used))
    } else {
        budget / 3
    };
    let archive_offset = if core_first { retained_core.len() } else { 0 };
    let mut ranked = Vec::new();
    let mut archive_used = 0;
    for (rank, mut hit) in archive.results.into_iter().enumerate() {
        let remaining = archive_budget.saturating_sub(archive_used);
        if remaining <= 1 {
            break;
        }
        let text = hit["excerpt"].as_str().unwrap_or_default();
        let mut end = text.len().min(remaining.saturating_sub(1));
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            continue;
        }
        let excerpt = text[..end].to_owned();
        let truncated = end < text.len();
        let cost = excerpt.len() + 1;
        hit["excerpt"] = json!(excerpt);
        hit["excerpt_truncated_by_budget"] = json!(truncated);
        hit["kind"] = json!("conversation");
        hit["source"] = json!(format!(
            "archive://{}/{}/{}",
            archive_uri_component(hit["client"].as_str().unwrap_or_default()),
            archive_uri_component(hit["session_id"].as_str().unwrap_or_default()),
            hit["id"]
        ));
        hit["untrusted"] = json!(true);
        hit["score"] = json!(1.0 / (60.0 + (archive_offset + rank + 1) as f64));
        hit["token_len"] = json!(cost);
        archive_used += cost;
        ranked.push((archive_offset + rank, 1_u8, hit));
    }
    let core = if core_first { retained_core } else { core };
    core_used = 0;
    for (rank, mut hit) in core.into_iter().enumerate() {
        let cost = hit["token_len"].as_u64().unwrap_or(0) as usize;
        let available = if core_first {
            budget
        } else {
            budget.saturating_sub(archive_used)
        };
        if cost > available.saturating_sub(core_used) {
            continue;
        }
        if fused {
            hit["score"] = json!(1.0 / (60.0 + (rank + 1) as f64));
        }
        core_used += cost;
        ranked.push((rank, 0_u8, hit));
    }
    ranked.sort_by_key(|(rank, source, _)| (*rank, *source));
    let results: Vec<Value> = ranked
        .into_iter()
        .take(limit)
        .map(|(_, _, hit)| hit)
        .collect();
    let used = results
        .iter()
        .map(|hit| hit["token_len"].as_u64().unwrap_or(0) as usize)
        .sum();
    let archive_hits = results
        .iter()
        .filter(|hit| hit["kind"] == "conversation")
        .count();
    (results, used, archive_hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core(count: usize, cost: usize) -> Vec<Value> {
        (0..count).map(|id| json!({"id": format!("core-{id}"), "kind": "fact", "token_len": cost, "score": 0.5})).collect()
    }

    fn archive() -> ArchiveSearch {
        ArchiveSearch {
            included: true,
            results: (0..8)
                .map(|id| {
                    json!({
                        "id": id, "client": "codex", "session_id": "a/b", "role": "assistant",
                        "excerpt": "архив 🦀".repeat(30), "timestamp": "2026-01-01T00:00:00Z"
                    })
                })
                .collect(),
            ..ArchiveSearch::default()
        }
    }

    #[test]
    fn knowledge_keeps_all_core_even_when_archive_would_fill_its_third() {
        let input = core(6, 1900);
        let (hits, used, archives) = fuse_conversations(
            input.clone(),
            archive(),
            12000,
            usize::MAX,
            ArchiveRanking::CoreFirst,
        );
        assert_eq!(
            hits.iter().take(6).map(|h| &h["id"]).collect::<Vec<_>>(),
            input.iter().map(|h| &h["id"]).collect::<Vec<_>>()
        );
        assert!(archives > 0);
        assert!(used <= 12000);
        assert_eq!(hits[6]["untrusted"], true);
        assert_eq!(hits[6]["source"], "archive://codex/a%2Fb/0");
    }

    #[test]
    fn knowledge_never_loses_core_prefix_across_budgets_and_limits() {
        for budget in [0, 1, 2, 50, 100, 300, 12000, usize::MAX] {
            for limit in [0, 1, 3, 5, 10, usize::MAX] {
                let input = core(7, 50);
                let (without, _, _) = fuse_conversations(
                    input.clone(),
                    ArchiveSearch::default(),
                    budget,
                    limit,
                    ArchiveRanking::CoreFirst,
                );
                let (with, used, _) =
                    fuse_conversations(input, archive(), budget, limit, ArchiveRanking::CoreFirst);
                assert!(used <= budget);
                assert!(with.len() <= limit);
                assert_eq!(
                    with.iter()
                        .filter(|h| h["kind"] != "conversation")
                        .map(|h| &h["id"])
                        .collect::<Vec<_>>(),
                    without.iter().map(|h| &h["id"]).collect::<Vec<_>>()
                );
                assert_eq!(
                    with.iter()
                        .take(without.len())
                        .map(|h| &h["id"])
                        .collect::<Vec<_>>(),
                    without.iter().map(|h| &h["id"]).collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn history_retains_reciprocal_interleaving_and_archive_ceiling() {
        let (hits, used, archives) = fuse_conversations(
            core(6, 100),
            archive(),
            12000,
            5,
            ArchiveRanking::Conversation,
        );
        assert_eq!(
            hits.iter()
                .map(|h| h["kind"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["fact", "conversation", "fact", "conversation", "fact"]
        );
        assert_eq!(archives, 2);
        assert!(used <= 12000);
        assert!(
            hits.iter()
                .filter(|h| h["kind"] == "conversation")
                .map(|h| h["token_len"].as_u64().unwrap())
                .sum::<u64>()
                <= 4000
        );
        assert_eq!(hits[0]["score"], hits[1]["score"]);
    }

    #[test]
    fn archive_fallback_handles_empty_core_unicode_and_tiny_budgets() {
        for ranking in [ArchiveRanking::CoreFirst, ArchiveRanking::Conversation] {
            for budget in 0..100 {
                let (hits, used, count) =
                    fuse_conversations(Vec::new(), archive(), budget, 5, ranking);
                assert!(used <= budget);
                assert_eq!(count, hits.len());
                assert!(used <= budget / 3);
                for hit in hits {
                    assert_eq!(hit["token_len"], hit["excerpt"].as_str().unwrap().len() + 1);
                    assert_eq!(hit["untrusted"], true);
                }
            }
        }
    }

    #[test]
    fn classifier_uses_periods_conversation_and_personal_history_in_both_languages() {
        for question in [
            "Что обсуждали про компилятор?",
            "Найди сообщение в чате про кеш",
            "What did the chat say about caching?",
            "Find the first message about Rust",
            "Какие подходы мы отвергли?",
            "What did we decide about storage?",
            "Где остановились с реализацией?",
            "Where did we leave off?",
            "Как работало хранилище в начале и как сейчас?",
            "What worked before and now?",
            "Что работало на старом порту?",
            "What ran on the old server?",
            "Кто ввёл эти правила и когда они появились?",
            "When were these rules introduced?",
        ] {
            assert_eq!(
                ArchiveRanking::for_question(question, false),
                ArchiveRanking::Conversation,
                "{question}"
            );
        }
        assert_eq!(
            ArchiveRanking::for_question("cache 2026-10-07", true),
            ArchiveRanking::Conversation
        );
        for question in [
            "Почему выбрали SQLite?",
            "Why was SQLite chosen?",
            "Когда следует пересмотреть базу?",
            "When should we review the database?",
            "Как устроена история версий?",
            "Why keep valid and recorded time?",
            "Which cache protocol does the engine use?",
            "Какие сообщения об ошибках возвращает API?",
        ] {
            assert_eq!(
                ArchiveRanking::for_question(question, false),
                ArchiveRanking::CoreFirst,
                "{question}"
            );
        }
    }
}
