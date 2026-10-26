//! What the stored embedding mode is allowed to do right now.
//!
//! The mode used to be a value the settings window wrote and nothing read, so
//! moving the slider rewrote a file and changed no behaviour. Every decision
//! that was previously implicit — load the model, spawn the worker, warn the
//! user — is resolved here from the mode together with the real state of the
//! model cache. A mode that cannot be honoured says so; it never falls back to
//! BM25 while reporting that semantic search ran.

use std::path::Path;

use brain_core::embeddings::{
    DEFAULT_ENGLISH_MODEL2VEC_MODEL, DEFAULT_MODEL2VEC_MODEL, model_id_for_text,
};

use crate::background;
use crate::settings::{self, EmbeddingMode};

/// Whether semantics can actually run, and why not when they cannot.
///
/// The two failure states are kept apart on purpose: a missing model is the
/// user's to fix with one command, while `Disabled` is the user's own choice
/// and telling them to download half a gigabyte would be wrong.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SemanticState {
    /// The mode permits the model and the model is in the local cache.
    Ready,
    /// The mode asks for the model, but it is not installed.
    ModelMissing,
    /// The user set the mode to `off`.
    Disabled,
    /// This binary was built without an embedding backend.
    NotCompiled,
}

/// The mode, the model it needs, and whether that combination can run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SemanticPlan {
    mode: EmbeddingMode,
    state: SemanticState,
    model_id: &'static str,
}

impl SemanticPlan {
    /// Resolves the plan for one piece of text against the on-disk model cache.
    ///
    /// The substitution rule is the worker's: the multilingual model also covers
    /// English, so an English query must not lose semantic search merely because
    /// the compact English model was never downloaded. Without this the search
    /// path refused work the background worker would happily have done.
    pub(crate) fn decide(mode: EmbeddingMode, text: &str, models_dir: &Path) -> Self {
        let preferred = model_id_for_text(text);
        if mode == EmbeddingMode::Off || !cfg!(feature = "model2vec") {
            return Self::resolve(mode, preferred, cfg!(feature = "model2vec"), false);
        }
        match usable_model(preferred, models_dir) {
            Some(model_id) => Self::resolve(mode, model_id, cfg!(feature = "model2vec"), true),
            // Nothing is installed: name the model the user should install, not
            // a substitute that is equally absent.
            None => Self::resolve(mode, preferred, cfg!(feature = "model2vec"), false),
        }
    }

    /// Reads the persisted mode and resolves the plan in one step.
    pub(crate) fn current(brain_home: &Path, text: &str, models_dir: &Path) -> Self {
        Self::decide(stored_mode(brain_home), text, models_dir)
    }

    /// Resolves the plan for one named model rather than for a piece of text.
    ///
    /// The settings window reports every model the tool can use, not just the
    /// one some sample string would have picked.
    #[cfg(feature = "model2vec")]
    pub(crate) fn for_model(
        mode: EmbeddingMode,
        model_id: &'static str,
        models_dir: &Path,
    ) -> Self {
        if mode == EmbeddingMode::Off {
            return Self::resolve(mode, model_id, true, false);
        }
        Self::resolve(
            mode,
            model_id,
            cfg!(feature = "model2vec"),
            background::model_installed(model_id, models_dir),
        )
    }

    /// The decision itself, with both inputs injected.
    ///
    /// Kept free of the filesystem and of the feature gate so every mode can be
    /// tested in every state, including the states this build cannot reach.
    fn resolve(
        mode: EmbeddingMode,
        model_id: &'static str,
        compiled: bool,
        model_present: bool,
    ) -> Self {
        let state = match mode {
            EmbeddingMode::Off => SemanticState::Disabled,
            EmbeddingMode::Always | EmbeddingMode::OnDemand => {
                if !compiled {
                    SemanticState::NotCompiled
                } else if model_present {
                    SemanticState::Ready
                } else {
                    SemanticState::ModelMissing
                }
            }
        };
        Self {
            mode,
            state,
            model_id,
        }
    }

    /// Why semantics are or are not available.
    #[cfg(test)]
    pub(crate) const fn state(self) -> SemanticState {
        self.state
    }

    /// The model this text would be embedded with.
    #[cfg(any(feature = "model2vec", test))]
    pub(crate) const fn model_id(self) -> &'static str {
        self.model_id
    }

    /// Whether a caller may load the model in this process.
    ///
    /// `off` returns false before any filesystem work, which is what makes the
    /// mode observable: the search never pays the load and never claims a
    /// vector signal it did not have.
    pub(crate) const fn may_load_model(self) -> bool {
        matches!(self.state, SemanticState::Ready)
    }

    /// Whether a write should start the background worker immediately.
    ///
    /// This is the whole difference between `always` and `on-demand` on a
    /// process-per-command tool: `always` embeds new memories as they arrive,
    /// `on-demand` leaves them queued until a request needs them.
    pub(crate) const fn indexes_writes_eagerly(self) -> bool {
        self.may_load_model() && matches!(self.mode, EmbeddingMode::Always)
    }

    /// Whether a search may start the worker to clear a waiting backlog.
    ///
    /// For `on-demand` this is the design: a search is the request the mode
    /// names. For `always` it is repair — a backlog can only exist there because
    /// the mode was `off` when those memories were written, and without this a
    /// user who turns semantics back on never gets the old records embedded.
    /// `off` is excluded because it is the state that produced the backlog.
    #[cfg(any(feature = "model2vec", test))]
    pub(crate) const fn drains_backlog_on_search(self) -> bool {
        self.may_load_model()
    }

    /// The degradation notice, or `None` when nothing is degraded.
    pub(crate) fn warning(self) -> Option<String> {
        match self.state {
            SemanticState::Ready => None,
            SemanticState::Disabled => Some(
                "semantic search is off because the embedding mode is set to off; BM25 and exact \
                 matches still run. Set the mode to always or on-demand to turn vectors back on"
                    .to_owned(),
            ),
            SemanticState::ModelMissing => Some(format!(
                "embedding mode is {}, but the model it needs is not installed: {}",
                self.mode,
                background::semantic_unavailable_message_for(self.model_id)
            )),
            SemanticState::NotCompiled => Some(format!(
                "embedding mode is {} but this binary was built without the model2vec feature, so \
                 {} cannot be loaded; BM25 and exact matches only",
                self.mode, self.model_id
            )),
        }
    }

    /// What happens to the vector of a memory being written right now.
    ///
    /// Always a sentence, because a write always has an outcome worth stating:
    /// the record is stored either way, and only the vector is in question.
    pub(crate) fn write_note(self) -> String {
        match self.state {
            SemanticState::Ready => match self.mode {
                EmbeddingMode::Always => {
                    "vector indexing queued for the background worker; BM25 + exact matches are \
                     available now"
                        .to_owned()
                }
                EmbeddingMode::OnDemand | EmbeddingMode::Off => {
                    "vector indexing queued; embedding mode is on-demand, so it runs on the next \
                     search or `brain reindex`. BM25 + exact matches are available now"
                        .to_owned()
                }
            },
            SemanticState::Disabled => {
                "embedding mode is off: the memory is stored and stays queued, but no vector is \
                 produced until the mode is set back to always or on-demand. BM25 + exact matches \
                 are available now"
                    .to_owned()
            }
            SemanticState::ModelMissing | SemanticState::NotCompiled => format!(
                "{}; memory stored now with BM25 + exact matches and vector indexing stays queued",
                self.warning()
                    .unwrap_or_else(|| "semantic indexing unavailable".to_owned())
            ),
        }
    }
}

/// The installed model that can serve this text, preferring the exact match.
///
/// Mirrors the background worker's substitution so a query and the write it is
/// looking for cannot disagree about whether a vector is obtainable.
fn usable_model(preferred: &'static str, models_dir: &Path) -> Option<&'static str> {
    if background::model_installed(preferred, models_dir) {
        return Some(preferred);
    }
    // The multilingual model can serve English, but the compact English model
    // cannot safely stand in for Russian or another non-English query.
    (preferred == DEFAULT_ENGLISH_MODEL2VEC_MODEL
        && background::model_installed(DEFAULT_MODEL2VEC_MODEL, models_dir))
    .then_some(DEFAULT_MODEL2VEC_MODEL)
}

/// The persisted mode, or the shipped default when nothing is stored yet.
pub(crate) fn stored_mode(brain_home: &Path) -> EmbeddingMode {
    settings::load(brain_home).embedding_mode
}

/// Memories waiting for a vector, counted across both queue files.
///
/// The number is what makes `on-demand` and `off` auditable rather than a
/// promise: it is the backlog the user is choosing to accumulate.
#[cfg(any(feature = "model2vec", test))]
pub(crate) fn pending_vector_jobs(brain_home: &Path, database: &Path) -> usize {
    background::pending_embedding_jobs(brain_home, database)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn plan(mode: EmbeddingMode, compiled: bool, present: bool) -> SemanticPlan {
        SemanticPlan::resolve(mode, DEFAULT_ENGLISH_MODEL2VEC_MODEL, compiled, present)
    }

    #[test]
    fn off_never_loads_the_model_even_when_it_is_installed() {
        let off = plan(EmbeddingMode::Off, true, true);
        assert_eq!(off.state(), SemanticState::Disabled);
        assert!(!off.may_load_model());
        assert!(!off.indexes_writes_eagerly());
        assert!(!off.drains_backlog_on_search());
    }

    #[test]
    fn off_is_reported_as_a_choice_and_never_asks_for_a_download() {
        let warning = plan(EmbeddingMode::Off, true, false)
            .warning()
            .expect("off must state itself");
        assert!(warning.contains("embedding mode is set to off"));
        assert!(
            !warning.contains("install"),
            "a mode the user turned off is not a missing model: {warning}"
        );
    }

    #[test]
    fn always_and_on_demand_differ_only_in_when_work_happens() {
        let always = plan(EmbeddingMode::Always, true, true);
        let on_demand = plan(EmbeddingMode::OnDemand, true, true);
        assert!(always.may_load_model() && on_demand.may_load_model());
        // The whole distinction lives at the write.
        assert!(always.indexes_writes_eagerly());
        assert!(!on_demand.indexes_writes_eagerly());
        // Both clear a waiting backlog on a search: `on-demand` because that is
        // its design, `always` because a backlog there is leftover `off` work.
        assert!(always.drains_backlog_on_search());
        assert!(on_demand.drains_backlog_on_search());
    }

    #[test]
    fn a_mode_that_needs_a_missing_model_names_the_mode_and_the_fix() {
        for mode in [EmbeddingMode::Always, EmbeddingMode::OnDemand] {
            let current = plan(mode, true, false);
            assert_eq!(current.state(), SemanticState::ModelMissing);
            assert!(!current.may_load_model(), "must not pretend to be working");
            let warning = current.warning().expect("missing model must be stated");
            assert!(warning.contains(&mode.to_string()));
            assert!(warning.contains("brain model install"));
        }
    }

    #[test]
    fn a_build_without_the_backend_says_so_instead_of_blaming_the_cache() {
        let current = plan(EmbeddingMode::Always, false, true);
        assert_eq!(current.state(), SemanticState::NotCompiled);
        let warning = current.warning().expect("build gap must be stated");
        assert!(warning.contains("model2vec"));
    }

    #[test]
    fn a_ready_plan_raises_no_warning() {
        assert!(plan(EmbeddingMode::Always, true, true).warning().is_none());
        assert!(
            plan(EmbeddingMode::OnDemand, true, true)
                .warning()
                .is_none()
        );
    }

    #[test]
    fn write_notes_tell_the_user_which_mode_produced_them() {
        assert!(
            plan(EmbeddingMode::Always, true, true)
                .write_note()
                .contains("background worker")
        );
        assert!(
            plan(EmbeddingMode::OnDemand, true, true)
                .write_note()
                .contains("on-demand")
        );
        assert!(
            plan(EmbeddingMode::Off, true, true)
                .write_note()
                .contains("embedding mode is off")
        );
    }

    #[test]
    fn text_selects_the_same_model_the_worker_would_use() {
        let english = SemanticPlan::decide(EmbeddingMode::Off, "local memory", Path::new("/none"));
        let russian =
            SemanticPlan::decide(EmbeddingMode::Off, "локальная память", Path::new("/none"));
        assert_eq!(english.model_id(), model_id_for_text("local memory"));
        assert_eq!(russian.model_id(), model_id_for_text("локальная память"));
        assert_ne!(english.model_id(), russian.model_id());
    }

    /// Builds a cache directory that looks like one model is installed.
    ///
    /// `hf-hub` calls a model present when the three files it needs resolve, so
    /// staging empty ones is enough to exercise the decision without a download.
    #[cfg(feature = "model2vec")]
    fn cache_with_only(model_id: &str) -> std::path::PathBuf {
        let root = crate::test_support::temp_path("brain-models");
        let repository = root.join(format!("models--{}", model_id.replace('/', "--")));
        let snapshot = repository.join("snapshots").join("test");
        std::fs::create_dir_all(&snapshot).expect("create model snapshot");
        std::fs::create_dir_all(repository.join("refs")).expect("create refs");
        std::fs::write(repository.join("refs").join("main"), "test").expect("write ref");
        for file in ["tokenizer.json", "model.safetensors", "config.json"] {
            std::fs::write(snapshot.join(file), b"").expect("stage model file");
        }
        root
    }

    #[test]
    #[cfg(feature = "model2vec")]
    fn an_english_query_uses_the_multilingual_model_when_only_that_one_is_installed() {
        // This asymmetry once made an ASCII query report "model missing" while
        // the worker embedded the same text without complaint. The test used to
        // read the developer's own model cache, which meant it silently checked
        // nothing on every other machine — including CI.
        let models_dir = cache_with_only(DEFAULT_MODEL2VEC_MODEL);
        assert!(
            background::model_installed(DEFAULT_MODEL2VEC_MODEL, &models_dir),
            "the staged cache must look installed, or this test proves nothing"
        );
        assert!(!background::model_installed(
            DEFAULT_ENGLISH_MODEL2VEC_MODEL,
            &models_dir
        ));

        let plan = SemanticPlan::decide(EmbeddingMode::Always, "ffmpeg overheating", &models_dir);
        assert_eq!(plan.model_id(), DEFAULT_MODEL2VEC_MODEL);
        assert_eq!(plan.state(), SemanticState::Ready);
        std::fs::remove_dir_all(&models_dir).ok();
    }

    #[test]
    #[cfg(feature = "model2vec")]
    fn the_english_model_does_not_substitute_for_multilingual_text() {
        let models_dir = cache_with_only(DEFAULT_ENGLISH_MODEL2VEC_MODEL);
        let plan = SemanticPlan::decide(EmbeddingMode::Always, "перегрев ноутбука", &models_dir);
        assert_eq!(plan.model_id(), DEFAULT_MODEL2VEC_MODEL);
        assert_eq!(plan.state(), SemanticState::ModelMissing);
        std::fs::remove_dir_all(&models_dir).ok();
    }

    #[test]
    fn a_missing_queue_is_an_empty_backlog_not_an_error() {
        assert_eq!(
            pending_vector_jobs(
                Path::new("/definitely/missing/brain-home"),
                Path::new("/definitely/missing/brain.sqlite")
            ),
            0
        );
    }

    #[test]
    fn backlog_counts_both_queue_files_and_ignores_blank_lines() {
        let home = crate::test_support::temp_path("brain-embedding-runtime");
        fs::create_dir_all(&home).expect("create temp brain home");
        let mine = home.join("mine.sqlite");
        let other = home.join("other.sqlite");
        let job = |database: &Path, node: &str| {
            format!(
                r#"{{"database":"{}","models_dir":"/models","space":"default","agent":"a","node_id":"{node}","model_id":"m","queued_at":1}}"#,
                database.display()
            )
        };
        fs::write(
            home.join(background::QUEUE_FILE),
            format!("{}\n\n{}\n", job(&mine, "one"), job(&mine, "two")),
        )
        .expect("write queue");
        // A job for a different database must not be counted here: telling
        // someone that vectors are pending for a database they are not using
        // sends them looking for a problem that is not theirs.
        fs::write(
            home.join(background::PROCESSING_FILE),
            format!("{}\n{}\n", job(&mine, "three"), job(&other, "elsewhere")),
        )
        .expect("write processing");
        assert_eq!(pending_vector_jobs(&home, &mine), 3);
        assert_eq!(pending_vector_jobs(&home, &other), 1);
        fs::remove_dir_all(home).expect("remove temp brain home");
    }
}
