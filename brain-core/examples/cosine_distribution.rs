//! Замер: как на самом деле распределены косинусные сходства в живой базе.
//!
//! Пороги min_cosine и strong_cosine раньше выбирались по аналогии с чужими
//! эмбеддингами. Аналогия — не факт: у статических моделей распределение
//! смещено вверх, и насколько именно, зависит от корпуса. Здесь мы сравниваем
//! два распределения на реальных данных — случайные пары записей против пар,
//! соединённых ребром графа, — и смотрим, где они расходятся.
//!
//! Запуск:
//!   cargo run -p brain-core --example cosine_distribution --release -- [путь-к-базе]

use std::collections::HashMap;

use brain_core::embeddings::{Embedder, Model2VecEmbedder};
use rusqlite::Connection;

const SAMPLE_PAIRS: usize = 4_000;
const MODEL_ID: &str = "minishlab/potion-multilingual-128M";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let home = std::env::var("HOME")?;
    let database = std::env::args()
        .nth(1)
        .unwrap_or_else(|| format!("{home}/.brain/brain.sqlite"));
    let models_dir =
        std::env::var("BRAIN_MODELS_DIR").unwrap_or_else(|_| format!("{home}/.brain/models"));

    let connection = Connection::open_with_flags(
        &database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let embedder = Model2VecEmbedder::from_pretrained_cached(MODEL_ID, &models_dir, true)?;

    // Re-embedding the text is cheaper than teaching this bench to read vec0
    // storage, and it measures exactly what a query would be compared against.
    let texts = load_texts(&connection)?;
    println!("записей с текстом: {}", texts.len());
    if texts.len() < 100 {
        println!("слишком мало данных для замера");
        return Ok(());
    }

    let ids: Vec<&String> = texts.keys().collect();
    let mut vectors: HashMap<&String, Vec<f32>> = HashMap::new();
    for id in &ids {
        let text = &texts[*id];
        if let Ok(vector) = embedder.embed(text) {
            vectors.insert(*id, vector);
        }
    }
    println!("векторов построено: {}", vectors.len());

    let linked = linked_pairs(&connection)?;
    println!("рёбер графа найдено: {}", linked.len());

    let mut random_scores = Vec::new();
    // A fixed stride instead of a random generator: the run must be repeatable,
    // otherwise two measurements cannot be compared.
    for step in 0..SAMPLE_PAIRS {
        let left = ids[(step * 7_919) % ids.len()];
        let right = ids[(step * 104_729 + 13) % ids.len()];
        if left == right {
            continue;
        }
        if let (Some(a), Some(b)) = (vectors.get(left), vectors.get(right)) {
            random_scores.push(cosine(a, b));
        }
    }

    let mut linked_scores = Vec::new();
    for (source, target) in &linked {
        if let (Some(a), Some(b)) = (vectors.get(source), vectors.get(target)) {
            linked_scores.push(cosine(a, b));
        }
    }

    report("случайные пары", &mut random_scores);
    report("пары, соединённые ребром", &mut linked_scores);
    separation(&mut random_scores, &mut linked_scores);
    Ok(())
}

fn load_texts(connection: &Connection) -> rusqlite::Result<HashMap<String, String>> {
    let mut statement = connection.prepare(
        "SELECT node_id, label, body FROM node_versions
         WHERE superseded_at IS NULL AND length(body) > 20
         ORDER BY row_id LIMIT 3000",
    )?;
    let rows = statement.query_map([], |row| {
        let id: String = row.get(0)?;
        let label: String = row.get(1)?;
        let body: String = row.get(2)?;
        Ok((id, format!("{label}\n{body}")))
    })?;
    rows.collect()
}

fn linked_pairs(connection: &Connection) -> rusqlite::Result<Vec<(String, String)>> {
    let mut statement = connection.prepare(
        "SELECT from_node_id, to_node_id FROM edges
         WHERE from_node_id <> to_node_id ORDER BY rowid LIMIT 4000",
    )?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

fn cosine(left: &[f32], right: &[f32]) -> f64 {
    let mut dot = 0.0_f64;
    let mut left_norm = 0.0_f64;
    let mut right_norm = 0.0_f64;
    for (a, b) in left.iter().zip(right) {
        dot += f64::from(*a) * f64::from(*b);
        left_norm += f64::from(*a) * f64::from(*a);
        right_norm += f64::from(*b) * f64::from(*b);
    }
    if left_norm == 0.0 || right_norm == 0.0 {
        return 0.0;
    }
    dot / (left_norm.sqrt() * right_norm.sqrt())
}

fn report(name: &str, scores: &mut [f64]) {
    if scores.is_empty() {
        println!("\n{name}: нет данных");
        return;
    }
    scores.sort_by(f64::total_cmp);
    let mean = scores.iter().sum::<f64>() / scores.len() as f64;
    println!("\n{name}: {} пар", scores.len());
    println!("  среднее {mean:.3}");
    for percentile in [5, 25, 50, 75, 90, 95, 99] {
        println!("  p{percentile:<3} {:.3}", quantile(scores, percentile));
    }
}

fn quantile(sorted: &[f64], percentile: usize) -> f64 {
    let index = (sorted.len() * percentile / 100).min(sorted.len() - 1);
    sorted[index]
}

/// Where a threshold would actually sit: the value that keeps most linked pairs
/// while rejecting most random ones.
fn separation(random: &mut [f64], linked: &mut [f64]) {
    if random.is_empty() || linked.is_empty() {
        return;
    }
    random.sort_by(f64::total_cmp);
    linked.sort_by(f64::total_cmp);
    println!("\nчто отсекает порог:");
    println!("  порог | случайных проходит | связанных проходит");
    let mut threshold = 0.30_f64;
    while threshold <= 0.90 {
        let random_share = share_at_or_above(random, threshold);
        let linked_share = share_at_or_above(linked, threshold);
        println!("  {threshold:.2}  | {random_share:>6.1}%            | {linked_share:>6.1}%");
        threshold += 0.05;
    }
}

fn share_at_or_above(sorted: &[f64], threshold: f64) -> f64 {
    let passing = sorted.iter().filter(|score| **score >= threshold).count();
    passing as f64 * 100.0 / sorted.len() as f64
}
