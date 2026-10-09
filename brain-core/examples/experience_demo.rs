//! Runs `similar` and `template` against a real database without the CLI.
//!
//! The CLI owns the embedder, so this harness exercises the degraded path the
//! core takes on its own: full-text seeds plus graph expansion, no vector hits.
//!
//! Usage: `experience_demo <database> <space> <query> [similar|template]`

use std::time::Instant;

use brain_core::templates::{ExperienceOptions, similar, template};

fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let [database, space, query, rest @ ..] = arguments.as_slice() else {
        eprintln!("usage: experience_demo <database> <space> <query> [similar|template]");
        std::process::exit(2);
    };
    let mode = rest.first().map_or("similar", String::as_str);

    let connection = rusqlite::Connection::open(database).expect("open database");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before the unix epoch")
        .as_secs() as i64;
    let options = ExperienceOptions::at(now);

    let started = Instant::now();
    let rendered = match mode {
        "template" => template(&connection, space, query, &options)
            .expect("build template")
            .to_json(),
        _ => similar(&connection, space, query, &options)
            .expect("find similar work")
            .to_json(),
    };
    let elapsed = started.elapsed();

    println!(
        "{}",
        serde_json::to_string_pretty(&rendered).expect("render json")
    );
    eprintln!("[{mode}] {query:?} took {elapsed:?}");
}
