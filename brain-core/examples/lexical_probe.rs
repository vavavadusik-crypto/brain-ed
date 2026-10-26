//! Content-free exam helper: normalizes the supplied questions, never opens a DB.
use brain_core::lexical::{bm25_query, inflection_prefix, is_stop_word, terms};

fn main() {
    let rows: Vec<_> = std::env::args()
        .skip(1)
        .map(|query| {
            let meaningful: Vec<_> = terms(&query)
                .into_iter()
                .filter(|term| !is_stop_word(term))
                .collect();
            serde_json::json!({
                "terms": meaningful,
                "prefixes": meaningful.iter().map(|term| inflection_prefix(term)).collect::<Vec<_>>(),
                "fts": bm25_query(&query, false),
            })
        })
        .collect();
    println!("{}", serde_json::json!(rows));
}
