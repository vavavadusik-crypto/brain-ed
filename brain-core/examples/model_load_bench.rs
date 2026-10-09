//! Замер: сколько занимает загрузка модели и кодирование одного текста.
fn main() {
    let models_dir = std::env::var("BRAIN_MODELS_DIR")
        .unwrap_or_else(|_| format!("{}/.brain/models", std::env::var("HOME").unwrap()));
    let model_id = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "minishlab/potion-multilingual-128M".to_string());

    let t = std::time::Instant::now();
    match brain_core::embeddings::Model2VecEmbedder::from_pretrained_cached(
        &model_id,
        &models_dir,
        true,
    ) {
        Ok(embedder) => {
            println!("загрузка модели: {:?}", t.elapsed());
            let t2 = std::time::Instant::now();
            use brain_core::embeddings::Embedder;
            match embedder.embed("проверка скорости кодирования") {
                Ok(v) => println!(
                    "кодирование 1 текста: {:?}, размерность {}",
                    t2.elapsed(),
                    v.len()
                ),
                Err(e) => println!("ошибка кодирования: {e}"),
            }
            let t3 = std::time::Instant::now();
            for i in 0..100 {
                let _ = embedder.embed(&format!("текст номер {i}"));
            }
            println!("кодирование 100 текстов: {:?}", t3.elapsed());
        }
        Err(e) => println!("загрузка провалилась за {:?}: {e}", t.elapsed()),
    }
}
