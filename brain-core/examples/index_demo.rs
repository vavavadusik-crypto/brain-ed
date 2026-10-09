use std::process::ExitCode;

fn main() -> ExitCode {
    let root = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let t = std::time::Instant::now();
    match brain_core::indexer::index_directory(&root) {
        Ok(stats) => {
            println!("{stats:?} in {:?}", t.elapsed());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("cannot index {}: {error}", root.display());
            ExitCode::FAILURE
        }
    }
}
