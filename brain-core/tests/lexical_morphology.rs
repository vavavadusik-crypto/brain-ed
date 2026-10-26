use brain_core::lexical::{bm25_query, terms};
use rusqlite::{Connection, params};

#[test]
fn cross_form_recall_and_prefix_collisions_on_an_unchanged_index() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE VIRTUAL TABLE corpus USING fts5(word, family UNINDEXED);")
        .unwrap();
    let groups: Vec<_> = include_str!("fixtures/inflections.tsv")
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .map(|line| {
            let (family, words) = line.split_once('\t').unwrap();
            (family, words.split_whitespace().collect::<Vec<_>>())
        })
        .collect();
    for (family, words) in &groups {
        for word in words {
            db.execute("INSERT INTO corpus VALUES (?1, ?2)", params![word, family])
                .unwrap();
        }
    }
    let mut metrics = [(0, 0, 0); 4]; // cross-form hits, possible pairs, off-family top-5
    for (family, words) in &groups {
        if family.starts_with("decoy_") {
            continue;
        }
        for query in words {
            let fixed4: String = query.chars().take(4).collect();
            let archive = if query.chars().count() >= 7 {
                format!("\"{}\"*", query.chars().take(5).collect::<String>())
            } else {
                format!("\"{query}\"")
            };
            let queries = [
                format!("\"{query}\""),
                format!("\"{fixed4}\"*"),
                archive,
                bm25_query(query, false),
            ];
            for (index, fts) in queries.iter().enumerate() {
                let mut statement = db.prepare(
                    "SELECT word, family FROM corpus WHERE corpus MATCH ?1 ORDER BY bm25(corpus), rowid LIMIT 5"
                ).unwrap();
                let found: Vec<(String, String)> = statement
                    .query_map([fts], |row| Ok((row.get(0)?, row.get(1)?)))
                    .unwrap()
                    .collect::<Result<_, _>>()
                    .unwrap();
                for other in words.iter().filter(|other| *other != query) {
                    let hit = found.iter().any(|(word, _)| word == other);
                    metrics[index].0 += usize::from(hit);
                    metrics[index].1 += 1;
                    if index == 3 {
                        assert!(hit, "{query} must find {other}: {fts}");
                    }
                }
                metrics[index].2 += found.iter().filter(|(_, group)| group != family).count();
            }
        }
    }
    for (name, (hits, pairs, noise)) in ["exact", "fixed4", "archive5", "inflection"]
        .iter()
        .zip(metrics)
    {
        println!("{name}: cross_form_hits={hits}/{pairs} off_family_top5={noise}");
    }
    assert!(metrics[3].0 > metrics[0].0);
    assert!(metrics[3].2 < metrics[1].2);
    // No index mutation, tokenizer change, or migration is needed for any run.
    assert_eq!(terms("parse_config_v2 8803"), ["parse_config_v2", "8803"]);
}
