//! Conservative multi-language code indexer built on tree-sitter.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tree_sitter::{Node, Parser};
use walkdir::WalkDir;

/// A single source file larger than this is skipped instead of allowing a
/// generated artefact to monopolise memory during a repository scan.
pub const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;

// -----------------------------------------------------------------------------
// Data structures
// -----------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FunctionInfo {
    pub name: String,
    /// Stable lexical identity within a file, independent of line numbers.
    pub qualified_name: String,
    /// Owning type for a method, when tree-sitter proves one.
    pub container: Option<String>,
    /// Receiver spelling (`self`, `this`, `g`, ...), when known.
    pub receiver: Option<String>,
    pub line_start: usize,
    pub line_end: usize,
}

#[derive(Debug, Clone)]
pub struct ClassInfo {
    pub name: String,
    pub qualified_name: String,
    pub line_start: usize,
    pub line_end: usize,
    pub methods: Vec<MethodInfo>,
}

#[derive(Debug, Clone)]
pub struct MethodInfo {
    pub name: String,
    pub qualified_name: String,
    pub line_start: usize,
    pub line_end: usize,
    pub class_name: String,
    pub receiver: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ImportInfo {
    /// Name bound in the importing file.
    pub name: String,
    /// Module or package named by the import.
    pub module: Option<String>,
    /// Original exported symbol when `name` is an alias.
    pub imported_name: Option<String>,
    /// Whether `name` is a module/package namespace used as a qualifier.
    pub namespace: bool,
    pub line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Method,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerInfo {
    pub name: String,
    pub qualified_name: String,
    pub kind: SymbolKind,
    pub container: Option<String>,
    pub receiver: Option<String>,
    pub line_start: usize,
}

#[derive(Debug, Clone)]
pub struct CallInfo {
    /// Simple identifier at the end of the call target.
    pub name: String,
    /// Exact target spelling in the source (`self.run`, `pkg::run`, ...).
    pub raw_name: String,
    /// Everything before the final identifier, when the target is qualified.
    pub qualifier: Option<String>,
    /// Enclosing callable. `None` means a module/file-level call.
    pub caller: Option<CallerInfo>,
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionKind {
    SameContainer,
    SameFile,
    ExplicitImport,
    QualifiedType,
    UniqueGlobal,
}

#[derive(Debug, Clone)]
/// A call resolved to a definition, with the trust weight of that resolution.
pub struct ResolvedCall {
    /// File containing the call site.
    pub from_file: PathBuf,
    /// Line of the call site.
    pub from_line: usize,
    /// Column of the call site.
    ///
    /// Line alone does not identify a call: `outer(inner())` is two calls on one
    /// line, and in Rust chained expressions put several on a line routinely.
    /// Without the column they collide into one identifier.
    pub from_column: usize,
    /// Name of the resolved definition. This can differ from the source spelling
    /// when an imported alias is called.
    pub name: String,
    /// Original call-target spelling.
    pub raw_name: String,
    /// Enclosing caller symbol, for function-to-function persistence.
    pub caller: Option<CallerInfo>,
    /// File where the definition lives.
    pub to_file: PathBuf,
    /// Line of the definition.
    pub to_line: usize,
    /// Confidence of the resolution: same file 1.0, import 0.9, unique global 0.6.
    pub confidence: f64,
    pub resolution: ResolutionKind,
}

#[derive(Debug)]
pub struct ParsedFile {
    pub path: PathBuf,
    pub language: &'static str,
    pub hash: String,
    pub functions: Vec<FunctionInfo>,
    pub classes: Vec<ClassInfo>,
    pub imports: Vec<ImportInfo>,
    pub calls: Vec<CallInfo>,
    pub parse_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexIssueKind {
    InvalidUtf8,
    NonUtf8Path,
    Oversized,
    Io,
    Walk,
    Syntax,
}

#[derive(Debug, Clone)]
pub struct IndexIssue {
    pub path: PathBuf,
    pub kind: IndexIssueKind,
    pub message: String,
}

#[derive(Debug, Default, Clone)]
pub struct LanguageIndexStats {
    pub language: &'static str,
    pub files: usize,
    pub functions: usize,
    pub classes: usize,
    pub methods: usize,
    pub calls_observed: usize,
    pub calls_resolved: usize,
    pub parse_errors: usize,
    pub skipped_files: usize,
}

#[derive(Debug, Default)]
pub struct IndexStats {
    pub files: usize,
    pub functions: usize,
    pub classes: usize,
    pub methods: usize,
    pub edges: usize,
    pub calls_observed: usize,
    pub unresolved_calls: usize,
    pub ambiguous_skipped: usize,
    pub parse_errors: usize,
    pub skipped_files: usize,
    pub invalid_utf8_files: usize,
    pub non_utf8_paths: usize,
    pub oversized_files: usize,
    pub io_errors: usize,
    pub walk_errors: usize,
    pub languages: Vec<LanguageIndexStats>,
    pub issues: Vec<IndexIssue>,
}

// -----------------------------------------------------------------------------
// Internal resolution types
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DefinitionKind {
    Function,
    Class,
    Method,
}

#[derive(Debug, Clone)]
struct Definition {
    name: String,
    qualified_name: String,
    container: Option<String>,
    path: PathBuf,
    line: usize,
    kind: DefinitionKind,
}

// -----------------------------------------------------------------------------
// Errors
// -----------------------------------------------------------------------------

#[derive(Error, Debug)]
pub enum IndexError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("walkdir error: {0}")]
    Walkdir(#[from] walkdir::Error),
    #[error("tree-sitter language error: {0}")]
    Language(#[from] tree_sitter::LanguageError),
    #[error("UTF-8 error: {0}")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("source file {path} is {bytes} bytes; limit is {max_bytes}")]
    FileTooLarge {
        path: PathBuf,
        bytes: u64,
        max_bytes: u64,
    },
    #[error("parse error: {0}")]
    Parse(String),
}

// -----------------------------------------------------------------------------
// Public API
// -----------------------------------------------------------------------------

/// What one language calls the things this indexer looks for.
///
/// Node names differ per grammar and nothing else does, so a language is data
/// rather than code: adding one means adding an entry here, not threading a new
/// branch through every match in the file.
struct LanguageSpec {
    kind: LanguageKind,
    /// Human-readable name, used in reports.
    name: &'static str,
    /// File extensions that select this language.
    extensions: &'static [&'static str],
    /// Grammar to parse with.
    language: fn() -> tree_sitter::Language,
    /// Nodes that define a callable.
    functions: &'static [&'static str],
    /// Nodes that define a type: a class, struct, trait, interface, or enum.
    types: &'static [&'static str],
    /// Nodes that introduce a type scope without defining another type.
    type_scopes: &'static [&'static str],
    /// Nodes that add a lexical namespace.
    namespaces: &'static [&'static str],
    /// Nodes that invoke something.
    calls: &'static [&'static str],
    /// Field on a call node naming what is being invoked.
    call_target_field: &'static str,
    /// Nodes that bring in outside names.
    imports: &'static [&'static str],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LanguageKind {
    Python,
    Rust,
    Go,
    JavaScript,
    TypeScript,
    Tsx,
}

/// Every language this build understands.
static LANGUAGES: &[LanguageSpec] = &[
    LanguageSpec {
        kind: LanguageKind::Python,
        name: "Python",
        extensions: &["py"],
        language: || tree_sitter_python::LANGUAGE.into(),
        functions: &["function_definition"],
        types: &["class_definition"],
        type_scopes: &[],
        namespaces: &[],
        calls: &["call"],
        call_target_field: "function",
        imports: &["import_statement", "import_from_statement"],
    },
    LanguageSpec {
        kind: LanguageKind::Rust,
        name: "Rust",
        extensions: &["rs"],
        language: || tree_sitter_rust::LANGUAGE.into(),
        functions: &["function_item"],
        types: &["struct_item", "enum_item", "trait_item", "union_item"],
        type_scopes: &["impl_item"],
        namespaces: &["mod_item"],
        calls: &["call_expression"],
        call_target_field: "function",
        imports: &["use_declaration"],
    },
    LanguageSpec {
        kind: LanguageKind::Go,
        name: "Go",
        extensions: &["go"],
        language: || tree_sitter_go::LANGUAGE.into(),
        // A method carries its receiver, which is what ties it to its type.
        functions: &["function_declaration", "method_declaration"],
        types: &["type_declaration"],
        type_scopes: &[],
        namespaces: &[],
        calls: &["call_expression"],
        call_target_field: "function",
        imports: &["import_declaration"],
    },
    LanguageSpec {
        kind: LanguageKind::JavaScript,
        name: "JavaScript",
        extensions: &["js", "jsx", "mjs", "cjs"],
        language: || tree_sitter_javascript::LANGUAGE.into(),
        functions: &[
            "function_declaration",
            "method_definition",
            "generator_function_declaration",
        ],
        types: &["class_declaration"],
        type_scopes: &[],
        namespaces: &[],
        calls: &["call_expression"],
        call_target_field: "function",
        imports: &["import_statement"],
    },
    LanguageSpec {
        kind: LanguageKind::TypeScript,
        name: "TypeScript",
        extensions: &["ts", "mts", "cts"],
        language: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        functions: &[
            "function_declaration",
            "method_definition",
            "generator_function_declaration",
        ],
        types: &[
            "class_declaration",
            "interface_declaration",
            "type_alias_declaration",
            "enum_declaration",
        ],
        type_scopes: &[],
        namespaces: &["internal_module"],
        calls: &["call_expression"],
        call_target_field: "function",
        imports: &["import_statement"],
    },
    LanguageSpec {
        kind: LanguageKind::Tsx,
        name: "TSX",
        extensions: &["tsx"],
        language: || tree_sitter_typescript::LANGUAGE_TSX.into(),
        functions: &[
            "function_declaration",
            "method_definition",
            "generator_function_declaration",
        ],
        types: &[
            "class_declaration",
            "interface_declaration",
            "type_alias_declaration",
            "enum_declaration",
        ],
        type_scopes: &[],
        namespaces: &["internal_module"],
        calls: &["call_expression"],
        call_target_field: "function",
        imports: &["import_statement"],
    },
];

/// Picks the language for a path, or `None` for a file this build cannot read.
fn language_for(path: &Path) -> Option<&'static LanguageSpec> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())?
        .to_ascii_lowercase();
    LANGUAGES
        .iter()
        .find(|spec| spec.extensions.contains(&extension.as_str()))
}

/// Names every language this build can index, for reports and documentation.
#[must_use]
pub fn supported_languages() -> Vec<&'static str> {
    LANGUAGES.iter().map(|spec| spec.name).collect()
}

/// Parse a single source file and extract symbols.
pub fn parse_file(path: &Path) -> Result<ParsedFile, IndexError> {
    let spec = language_for(path).ok_or_else(|| {
        IndexError::Parse(format!(
            "no grammar for {}; supported: {}",
            path.display(),
            supported_languages().join(", ")
        ))
    })?;
    let bytes = fs::metadata(path)?.len();
    if bytes > MAX_SOURCE_BYTES {
        return Err(IndexError::FileTooLarge {
            path: path.to_path_buf(),
            bytes,
            max_bytes: MAX_SOURCE_BYTES,
        });
    }
    let content_bytes = read_source_bounded(path, MAX_SOURCE_BYTES)?;
    let content = std::str::from_utf8(&content_bytes)?;
    let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
    let mut parser = Parser::new();
    parser.set_language(&(spec.language)())?;
    let tree = parser
        .parse(content, None)
        .ok_or_else(|| IndexError::Parse("tree-sitter returned no tree".to_string()))?;

    let (functions, classes, imports, calls, parse_error) = extract_symbols(&tree, content, spec)?;

    Ok(ParsedFile {
        path: path.to_path_buf(),
        language: spec.name,
        hash,
        functions,
        classes,
        imports,
        calls,
        parse_error,
    })
}

/// Reads at most `max_bytes` from `path`.
///
/// The metadata check above is only a fast path: the file can grow between it
/// and the read, so the read itself is capped at `max_bytes + 1` and an
/// over-limit result is rejected instead of being buffered without bound.
fn read_source_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>, IndexError> {
    use std::io::Read;
    let file = fs::File::open(path)?;
    let mut content = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut content)?;
    let bytes = content.len() as u64;
    if bytes > max_bytes {
        return Err(IndexError::FileTooLarge {
            path: path.to_path_buf(),
            bytes,
            max_bytes,
        });
    }
    Ok(content)
}

/// Walk a directory, parse supported source files, and resolve only provable calls.
/// Parses a source tree and returns both the per-file details and the summary.
///
/// The caller persists the parsed files; keeping persistence out of the parser
/// lets the indexer stay usable without a database.
pub fn index_tree(
    root: &Path,
) -> Result<(Vec<ParsedFile>, Vec<ResolvedCall>, IndexStats), IndexError> {
    index_tree_inner(root)
}

pub fn index_directory(root: &Path) -> Result<IndexStats, IndexError> {
    Ok(index_tree_inner(root)?.2)
}

fn index_tree_inner(
    root: &Path,
) -> Result<(Vec<ParsedFile>, Vec<ResolvedCall>, IndexStats), IndexError> {
    fs::metadata(root)?;
    let mut parsed_files = Vec::new();
    let mut stats = IndexStats::default();

    for entry in WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| entry.depth() == 0 || !is_skipped_dir(entry))
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                stats.walk_errors += 1;
                stats.issues.push(IndexIssue {
                    path: error.path().map_or_else(PathBuf::new, Path::to_path_buf),
                    kind: IndexIssueKind::Walk,
                    message: error.to_string(),
                });
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Some(spec) = language_for(path) else {
            continue;
        };
        if path.to_str().is_none() {
            stats.skipped_files += 1;
            stats.non_utf8_paths += 1;
            language_stats_mut(&mut stats, spec.name).skipped_files += 1;
            stats.issues.push(IndexIssue {
                path: path.to_path_buf(),
                kind: IndexIssueKind::NonUtf8Path,
                message: "source path is not valid UTF-8".to_owned(),
            });
            continue;
        }

        let parsed = match parse_file(path) {
            Ok(parsed) => parsed,
            Err(IndexError::FileTooLarge {
                bytes, max_bytes, ..
            }) => {
                stats.skipped_files += 1;
                stats.oversized_files += 1;
                language_stats_mut(&mut stats, spec.name).skipped_files += 1;
                stats.issues.push(IndexIssue {
                    path: path.to_path_buf(),
                    kind: IndexIssueKind::Oversized,
                    message: format!("source is {bytes} bytes; limit is {max_bytes}"),
                });
                continue;
            }
            Err(IndexError::Utf8(error)) => {
                stats.skipped_files += 1;
                stats.invalid_utf8_files += 1;
                language_stats_mut(&mut stats, spec.name).skipped_files += 1;
                stats.issues.push(IndexIssue {
                    path: path.to_path_buf(),
                    kind: IndexIssueKind::InvalidUtf8,
                    message: error.to_string(),
                });
                continue;
            }
            Err(IndexError::Io(error)) => {
                stats.skipped_files += 1;
                stats.io_errors += 1;
                language_stats_mut(&mut stats, spec.name).skipped_files += 1;
                stats.issues.push(IndexIssue {
                    path: path.to_path_buf(),
                    kind: IndexIssueKind::Io,
                    message: error.to_string(),
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        let has_parse_error = parsed.parse_error.is_some();
        if has_parse_error {
            stats.parse_errors += 1;
            stats.issues.push(IndexIssue {
                path: path.to_path_buf(),
                kind: IndexIssueKind::Syntax,
                message: parsed
                    .parse_error
                    .clone()
                    .unwrap_or_else(|| "syntax error detected".to_owned()),
            });
        }
        stats.files += 1;
        stats.functions += parsed.functions.len();
        stats.classes += parsed.classes.len();
        let methods = parsed
            .functions
            .iter()
            .filter(|function| function.container.is_some())
            .count();
        stats.methods += methods;
        stats.calls_observed += parsed.calls.len();
        {
            let language_stats = language_stats_mut(&mut stats, parsed.language);
            language_stats.files += 1;
            language_stats.parse_errors += usize::from(has_parse_error);
            language_stats.functions += parsed.functions.len();
            language_stats.classes += parsed.classes.len();
            language_stats.methods += methods;
            language_stats.calls_observed += parsed.calls.len();
        }
        parsed_files.push(parsed);
    }

    parsed_files.sort_by(|left, right| left.path.cmp(&right.path));

    // Build a deterministic symbol table. A name can legitimately have many
    // definitions; resolution below must narrow it to exactly one.
    let mut definitions: HashMap<String, Vec<Definition>> = HashMap::new();
    let mut resolved: Vec<ResolvedCall> = Vec::new();

    for file in &parsed_files {
        for func in &file.functions {
            definitions
                .entry(func.name.clone())
                .or_default()
                .push(Definition {
                    name: func.name.clone(),
                    qualified_name: func.qualified_name.clone(),
                    container: func.container.clone(),
                    path: file.path.clone(),
                    line: func.line_start,
                    kind: if func.container.is_some() {
                        DefinitionKind::Method
                    } else {
                        DefinitionKind::Function
                    },
                });
        }
        for class in &file.classes {
            definitions
                .entry(class.name.clone())
                .or_default()
                .push(Definition {
                    name: class.name.clone(),
                    qualified_name: class.qualified_name.clone(),
                    container: None,
                    path: file.path.clone(),
                    line: class.line_start,
                    kind: DefinitionKind::Class,
                });
        }
    }
    for sites in definitions.values_mut() {
        sites.sort_by(|left, right| {
            (&left.path, left.line, &left.qualified_name).cmp(&(
                &right.path,
                right.line,
                &right.qualified_name,
            ))
        });
        sites.dedup_by(|left, right| {
            left.path == right.path
                && left.line == right.line
                && left.qualified_name == right.qualified_name
                && left.kind == right.kind
        });
    }

    for file in &parsed_files {
        for call in &file.calls {
            match resolve_call(root, file, call, &definitions) {
                ResolutionOutcome::Resolved(definition, confidence, resolution) => {
                    resolved.push(ResolvedCall {
                        from_file: file.path.clone(),
                        from_line: call.line,
                        from_column: call.column,
                        name: definition.name.clone(),
                        raw_name: call.raw_name.clone(),
                        caller: call.caller.clone(),
                        to_file: definition.path.clone(),
                        to_line: definition.line,
                        confidence,
                        resolution,
                    });
                    language_stats_mut(&mut stats, file.language).calls_resolved += 1;
                }
                ResolutionOutcome::Ambiguous => stats.ambiguous_skipped += 1,
                ResolutionOutcome::Unresolved => {}
            }
        }
    }

    stats.edges = resolved.len();
    stats.unresolved_calls = stats.calls_observed.saturating_sub(stats.edges);
    stats.languages.sort_by_key(|language| {
        LANGUAGES
            .iter()
            .position(|spec| spec.name == language.language)
            .unwrap_or(usize::MAX)
    });

    Ok((parsed_files, resolved, stats))
}

// -----------------------------------------------------------------------------
// Internal helpers
// -----------------------------------------------------------------------------

fn language_stats_mut<'a>(
    stats: &'a mut IndexStats,
    language: &'static str,
) -> &'a mut LanguageIndexStats {
    if let Some(index) = stats
        .languages
        .iter()
        .position(|item| item.language == language)
    {
        return &mut stats.languages[index];
    }
    stats.languages.push(LanguageIndexStats {
        language,
        ..LanguageIndexStats::default()
    });
    stats
        .languages
        .last_mut()
        .expect("a language statistic was just inserted")
}

enum ResolutionOutcome<'a> {
    Resolved(&'a Definition, f64, ResolutionKind),
    Ambiguous,
    Unresolved,
}

fn resolve_call<'a>(
    root: &Path,
    file: &ParsedFile,
    call: &CallInfo,
    definitions: &'a HashMap<String, Vec<Definition>>,
) -> ResolutionOutcome<'a> {
    if let Some(qualifier) = call.qualifier.as_deref() {
        return resolve_qualified_call(root, file, call, qualifier, definitions);
    }

    let candidates = definitions
        .get(&call.name)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let same_file = candidates
        .iter()
        .filter(|definition| definition.path == file.path)
        .collect::<Vec<_>>();

    // Search outward through the caller's lexical scopes. This resolves local
    // functions without treating an unrelated nested function elsewhere in
    // the file as visible. A type boundary is deliberately not searched for an
    // unqualified call: Python/Rust/JS methods require an explicit receiver.
    if let Some(caller) = &call.caller {
        let mut lexical_scope = caller.qualified_name.as_str();
        loop {
            let expected = format!("{lexical_scope}.{}", call.name);
            let lexical_candidates = same_file
                .iter()
                .copied()
                .filter(|definition| definition.qualified_name == expected)
                .collect::<Vec<_>>();
            match exactly_one(&lexical_candidates) {
                CandidateCount::One(definition) => {
                    return ResolutionOutcome::Resolved(definition, 1.0, ResolutionKind::SameFile);
                }
                CandidateCount::Many => return ResolutionOutcome::Ambiguous,
                CandidateCount::None => {}
            }
            let Some((parent, _)) = lexical_scope.rsplit_once('.') else {
                break;
            };
            if caller.container.as_deref() == Some(parent) {
                break;
            }
            lexical_scope = parent;
        }
    }

    let same_file_top_level = same_file
        .iter()
        .copied()
        .filter(|definition| definition.qualified_name == call.name)
        .collect::<Vec<_>>();
    match exactly_one(&same_file_top_level) {
        CandidateCount::One(definition) => {
            return ResolutionOutcome::Resolved(definition, 1.0, ResolutionKind::SameFile);
        }
        CandidateCount::Many => return ResolutionOutcome::Ambiguous,
        CandidateCount::None => {}
    }
    let matching_imports = file
        .imports
        .iter()
        .filter(|import| !import.namespace && import.name == call.name)
        .collect::<Vec<_>>();
    if !matching_imports.is_empty() {
        let mut imported_candidates = Vec::new();
        for import in matching_imports {
            let imported_name = import.imported_name.as_deref().unwrap_or(&call.name);
            if let Some(sites) = definitions.get(imported_name) {
                imported_candidates.extend(sites.iter().filter(|definition| {
                    definition.container.is_none()
                        && import
                            .module
                            .as_deref()
                            .is_some_and(|module| module_matches(root, &definition.path, module))
                }));
            }
        }
        imported_candidates.sort_by_key(|definition| (&definition.path, definition.line));
        imported_candidates.dedup_by_key(|definition| (&definition.path, definition.line));
        return match exactly_one(&imported_candidates) {
            CandidateCount::One(definition) => {
                ResolutionOutcome::Resolved(definition, 0.9, ResolutionKind::ExplicitImport)
            }
            CandidateCount::Many => ResolutionOutcome::Ambiguous,
            CandidateCount::None => ResolutionOutcome::Unresolved,
        };
    }

    // A repository-wide unique spelling is not proof that the language binds
    // this call to it: it may be an external dependency, builtin, field, or a
    // dynamically supplied name. Leave it unresolved unless scope/import data
    // above establishes the binding.
    ResolutionOutcome::Unresolved
}

fn resolve_qualified_call<'a>(
    root: &Path,
    file: &ParsedFile,
    call: &CallInfo,
    qualifier: &str,
    definitions: &'a HashMap<String, Vec<Definition>>,
) -> ResolutionOutcome<'a> {
    let candidates = definitions
        .get(&call.name)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let qualifier_tail = qualifier_tail(qualifier);

    if let Some(caller) = &call.caller {
        let is_receiver =
            qualifier_tail == "Self" || caller.receiver.as_deref() == Some(qualifier_tail);
        if is_receiver {
            let Some(container) = caller.container.as_deref() else {
                return ResolutionOutcome::Unresolved;
            };
            let receiver_candidates = candidates
                .iter()
                .filter(|definition| {
                    definition.path == file.path
                        && definition.container.as_deref() == Some(container)
                })
                .collect::<Vec<_>>();
            return resolution_from_candidates(
                &receiver_candidates,
                1.0,
                ResolutionKind::SameContainer,
            );
        }
    }

    let qualifier_root = qualifier_head(qualifier);
    let namespace_imports = file
        .imports
        .iter()
        .filter(|import| import.namespace && import.name == qualifier_root)
        .collect::<Vec<_>>();
    if !namespace_imports.is_empty() {
        let qualified_parts = qualified_parts(qualifier);
        let container_hint = (qualified_parts.len() > 1).then(|| qualified_parts[1..].join("."));
        let mut imported_candidates = Vec::new();
        for import in namespace_imports {
            if let Some(module) = import.module.as_deref() {
                imported_candidates.extend(candidates.iter().filter(|definition| {
                    module_matches(root, &definition.path, module)
                        && match container_hint.as_deref() {
                            Some(container) => definition
                                .container
                                .as_deref()
                                .is_some_and(|value| qualified_tail_matches(value, container)),
                            None => definition.container.is_none(),
                        }
                }));
            }
        }
        imported_candidates.sort_by_key(|definition| (&definition.path, definition.line));
        imported_candidates.dedup_by_key(|definition| (&definition.path, definition.line));
        return resolution_from_candidates(
            &imported_candidates,
            0.9,
            ResolutionKind::ExplicitImport,
        );
    }

    if qualifier == "self" {
        let module_candidates = candidates
            .iter()
            .filter(|definition| {
                definition.path == file.path
                    && definition.container.is_none()
                    && definition.qualified_name == call.name
            })
            .collect::<Vec<_>>();
        return resolution_from_candidates(&module_candidates, 1.0, ResolutionKind::SameFile);
    }

    if qualifier == "crate" {
        let crate_candidates = candidates
            .iter()
            .filter(|definition| {
                definition.container.is_none() && is_crate_root(root, &definition.path)
            })
            .collect::<Vec<_>>();
        return resolution_from_candidates(&crate_candidates, 0.9, ResolutionKind::ExplicitImport);
    }

    if qualifier.starts_with("crate::") || qualifier.starts_with("self::") {
        let module_candidates = candidates
            .iter()
            .filter(|definition| {
                definition.container.is_none() && module_matches(root, &definition.path, qualifier)
            })
            .collect::<Vec<_>>();
        return resolution_from_candidates(&module_candidates, 0.9, ResolutionKind::ExplicitImport);
    }

    // `Type.method()` is accepted only when `Type` is demonstrably in scope:
    // declared in this file or brought in by a named import. A same-named type
    // elsewhere in the repository is not enough.
    let normalized_qualifier = qualified_parts(qualifier).join(".");
    let local_types = definitions
        .get(qualifier_tail)
        .into_iter()
        .flatten()
        .filter(|definition| {
            definition.kind == DefinitionKind::Class
                && definition.path == file.path
                && qualified_tail_matches(&definition.qualified_name, &normalized_qualifier)
        })
        .collect::<Vec<_>>();
    match exactly_one(&local_types) {
        CandidateCount::One(class) => {
            let type_candidates = candidates
                .iter()
                .filter(|definition| {
                    definition.path == class.path
                        && definition.container.as_deref() == Some(class.qualified_name.as_str())
                })
                .collect::<Vec<_>>();
            return resolution_from_candidates(
                &type_candidates,
                1.0,
                ResolutionKind::QualifiedType,
            );
        }
        CandidateCount::Many => return ResolutionOutcome::Ambiguous,
        CandidateCount::None => {}
    }

    let imported_types = file
        .imports
        .iter()
        .filter(|import| !import.namespace && import.name == qualifier_head(qualifier))
        .collect::<Vec<_>>();
    if !imported_types.is_empty() {
        let mut classes = Vec::new();
        for import in imported_types {
            let imported_name = import.imported_name.as_deref().unwrap_or(qualifier_tail);
            if let Some(definitions_for_name) = definitions.get(imported_name) {
                classes.extend(definitions_for_name.iter().filter(|definition| {
                    definition.kind == DefinitionKind::Class
                        && import
                            .module
                            .as_deref()
                            .is_some_and(|module| module_matches(root, &definition.path, module))
                }));
            }
        }
        classes.sort_by_key(|definition| (&definition.path, definition.line));
        classes.dedup_by_key(|definition| (&definition.path, definition.line));
        return match exactly_one(&classes) {
            CandidateCount::One(class) => {
                let type_candidates = candidates
                    .iter()
                    .filter(|definition| {
                        definition.path == class.path
                            && definition.container.as_deref()
                                == Some(class.qualified_name.as_str())
                    })
                    .collect::<Vec<_>>();
                resolution_from_candidates(&type_candidates, 0.9, ResolutionKind::QualifiedType)
            }
            CandidateCount::Many => ResolutionOutcome::Ambiguous,
            CandidateCount::None => ResolutionOutcome::Unresolved,
        };
    }

    // An arbitrary object expression does not prove its runtime type. Refusing
    // it is safer than attaching a high-confidence edge to a same-named method.
    ResolutionOutcome::Unresolved
}

enum CandidateCount<'a> {
    None,
    One(&'a Definition),
    Many,
}

fn exactly_one<'a>(candidates: &[&'a Definition]) -> CandidateCount<'a> {
    match candidates {
        [] => CandidateCount::None,
        [definition] => CandidateCount::One(definition),
        _ => CandidateCount::Many,
    }
}

fn resolution_from_candidates<'a>(
    candidates: &[&'a Definition],
    confidence: f64,
    resolution: ResolutionKind,
) -> ResolutionOutcome<'a> {
    match exactly_one(candidates) {
        CandidateCount::One(definition) => {
            ResolutionOutcome::Resolved(definition, confidence, resolution)
        }
        CandidateCount::Many => ResolutionOutcome::Ambiguous,
        CandidateCount::None => ResolutionOutcome::Unresolved,
    }
}

fn qualified_parts(value: &str) -> Vec<&str> {
    value
        .split(['.', ':'])
        .filter(|part| !part.is_empty())
        .collect()
}

fn qualified_tail_matches(value: &str, expected: &str) -> bool {
    value == expected || value.ends_with(&format!(".{expected}"))
}

fn qualifier_head(qualifier: &str) -> &str {
    qualifier
        .split(['.', ':'])
        .find(|part| !part.is_empty())
        .unwrap_or(qualifier)
}

fn qualifier_tail(qualifier: &str) -> &str {
    qualifier
        .rsplit(['.', ':'])
        .find(|part| !part.is_empty())
        .unwrap_or(qualifier)
}

fn module_matches(root: &Path, target: &Path, module: &str) -> bool {
    let module_parts = normalized_module_parts(module);
    if module_parts.is_empty() {
        return false;
    }
    let relative = target.strip_prefix(root).unwrap_or(target);
    let mut file_parts = relative
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .filter_map(|component| component.as_os_str().to_str())
        .filter(|part| *part != "src")
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let parent_parts = file_parts.clone();
    if let Some(stem) = relative.file_stem().and_then(|value| value.to_str())
        && !matches!(stem, "__init__" | "index" | "mod" | "lib" | "main")
    {
        file_parts.push(stem.to_owned());
    }
    path_parts_match(&file_parts, &module_parts) || path_parts_match(&parent_parts, &module_parts)
}

fn path_parts_match(path: &[String], module: &[String]) -> bool {
    path.ends_with(module) || module.ends_with(path)
}

fn is_crate_root(root: &Path, target: &Path) -> bool {
    let relative = target.strip_prefix(root).unwrap_or(target);
    matches!(
        relative.to_str(),
        Some("lib.rs" | "main.rs" | "src/lib.rs" | "src/main.rs")
    )
}

fn normalized_module_parts(module: &str) -> Vec<String> {
    let normalized = module
        .trim()
        .trim_matches(['\'', '"'])
        .trim_start_matches("./")
        .replace("::", "/")
        .replace('.', "/");
    normalized
        .split('/')
        .filter(|part| !part.is_empty() && !matches!(*part, "crate" | "self" | "super" | "src"))
        .map(strip_source_extension)
        .collect()
}

fn strip_source_extension(part: &str) -> String {
    for extension in [".tsx", ".py", ".rs", ".js", ".ts"] {
        if let Some(stem) = part.strip_suffix(extension) {
            return stem.to_owned();
        }
    }
    part.to_owned()
}

fn is_skipped_dir(entry: &walkdir::DirEntry) -> bool {
    entry.file_type().is_dir()
        && entry
            .file_name()
            .to_str()
            .map(|name| {
                matches!(
                    name,
                    ".git"
                        | ".hg"
                        | ".svn"
                        | ".venv"
                        | "venv"
                        | ".tox"
                        | ".mypy_cache"
                        | ".pytest_cache"
                        | "__pycache__"
                        | "node_modules"
                        | "target"
                        | "dist"
                        | "build"
                        | "out"
                        | "coverage"
                        | ".next"
                        | ".turbo"
                )
            })
            .unwrap_or(false)
}

type ExtractedSymbols = (
    Vec<FunctionInfo>,
    Vec<ClassInfo>,
    Vec<ImportInfo>,
    Vec<CallInfo>,
    Option<String>,
);

fn extract_symbols(
    tree: &tree_sitter::Tree,
    source: &str,
    spec: &LanguageSpec,
) -> Result<ExtractedSymbols, IndexError> {
    let root = tree.root_node();
    if has_error_node(root) {
        // Tree-sitter recovers aggressively and can manufacture plausible
        // declarations below ERROR nodes. Persisting those as trusted symbols
        // is worse than omitting one malformed file.
        return Ok((
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Some("syntax error detected; symbols withheld".to_owned()),
        ));
    }

    let mut functions = Vec::new();
    let mut classes = Vec::new();
    let mut imports = Vec::new();
    let mut calls = Vec::new();
    let mut scope = Vec::new();
    let mut callers = Vec::new();

    traverse(
        root,
        source,
        spec,
        &mut scope,
        &mut callers,
        &mut functions,
        &mut classes,
        &mut imports,
        &mut calls,
    )?;
    attach_methods(&functions, &mut classes);

    Ok((functions, classes, imports, calls, None))
}

fn has_error_node(node: Node) -> bool {
    if node.is_error() || node.is_missing() {
        return true;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if has_error_node(child) {
            return true;
        }
    }
    false
}

/// The name a type declaration introduces, when it has one.
///
/// Most grammars put it in a `name` field. Rust's `impl` blocks do not — the
/// type being implemented sits in `type` — and Go wraps its declaration around
/// a spec node. An anonymous form returns `None` rather than a placeholder,
/// because a made-up name would resolve calls that were never written.
fn type_name(node: Node, source: &str) -> Result<Option<String>, IndexError> {
    if let Some(name) = node.child_by_field_name("name") {
        return Ok(Some(name.utf8_text(source.as_bytes())?.to_string()));
    }
    if let Some(implemented) = node.child_by_field_name("type") {
        return first_identifier(implemented, source);
    }
    Ok(None)
}

fn type_names(node: Node, source: &str, spec: &LanguageSpec) -> Result<Vec<String>, IndexError> {
    if spec.kind != LanguageKind::Go {
        return Ok(type_name(node, source)?.into_iter().collect());
    }
    let mut names = Vec::new();
    collect_type_spec_names(node, source, &mut names)?;
    Ok(names)
}

fn collect_type_spec_names(
    node: Node,
    source: &str,
    names: &mut Vec<String>,
) -> Result<(), IndexError> {
    if node.kind().ends_with("_spec")
        && let Some(name) = node.child_by_field_name("name")
    {
        names.push(name.utf8_text(source.as_bytes())?.to_owned());
        return Ok(());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_type_spec_names(child, source, names)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Namespace,
    Type,
    Function,
}

#[derive(Debug, Clone)]
struct ScopeFrame {
    name: String,
    kind: ScopeKind,
}

// One output vector per extracted kind keeps this recursive walker a single
// pass; bundling them would be a refactor of the parser for lint's sake.
#[allow(clippy::too_many_arguments)]
fn traverse(
    node: Node,
    source: &str,
    spec: &LanguageSpec,
    scope: &mut Vec<ScopeFrame>,
    callers: &mut Vec<CallerInfo>,
    functions: &mut Vec<FunctionInfo>,
    classes: &mut Vec<ClassInfo>,
    imports: &mut Vec<ImportInfo>,
    calls: &mut Vec<CallInfo>,
) -> Result<(), IndexError> {
    let kind = node.kind();

    if spec.namespaces.contains(&kind)
        && let Some(name_node) = node.child_by_field_name("name")
    {
        let name = name_node.utf8_text(source.as_bytes())?.to_owned();
        scope.push(ScopeFrame {
            name,
            kind: ScopeKind::Namespace,
        });
        traverse_children(
            node, source, spec, scope, callers, functions, classes, imports, calls,
        )?;
        scope.pop();
        return Ok(());
    }

    if spec.type_scopes.contains(&kind)
        && let Some(name) = type_name(node, source)?
    {
        scope.push(ScopeFrame {
            name,
            kind: ScopeKind::Type,
        });
        traverse_children(
            node, source, spec, scope, callers, functions, classes, imports, calls,
        )?;
        scope.pop();
        return Ok(());
    }

    if spec.types.contains(&kind) {
        let names = type_names(node, source, spec)?;
        for name in &names {
            classes.push(ClassInfo {
                name: name.clone(),
                qualified_name: qualify(scope, name),
                line_start: node.start_position().row + 1,
                line_end: node.end_position().row + 1,
                methods: Vec::new(),
            });
        }
        if names.len() == 1 && spec.kind != LanguageKind::Go {
            scope.push(ScopeFrame {
                name: names[0].clone(),
                kind: ScopeKind::Type,
            });
            traverse_children(
                node, source, spec, scope, callers, functions, classes, imports, calls,
            )?;
            scope.pop();
            return Ok(());
        }
    }

    let named_function = if spec.functions.contains(&kind) {
        node.child_by_field_name("name")
            .map(|name| name.utf8_text(source.as_bytes()).map(str::to_owned))
            .transpose()?
            .map(|name| (name, node))
    } else {
        binding_function(node, source, spec)?
    };
    if let Some((name, body_node)) = named_function {
        let (container, receiver) = callable_container(node, source, spec, scope)?;
        let qualified_name = if container.is_some()
            && !scope.iter().any(|frame| {
                frame.kind == ScopeKind::Type && Some(frame.name.as_str()) == container.as_deref()
            }) {
            qualify_with_container(scope, container.as_deref(), &name)
        } else {
            qualify(scope, &name)
        };
        let caller = CallerInfo {
            name: name.clone(),
            qualified_name: qualified_name.clone(),
            kind: if container.is_some() {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            },
            container: container.clone(),
            receiver: receiver.clone(),
            line_start: node.start_position().row + 1,
        };
        functions.push(FunctionInfo {
            name: name.clone(),
            qualified_name,
            container,
            receiver,
            line_start: node.start_position().row + 1,
            line_end: body_node.end_position().row + 1,
        });
        scope.push(ScopeFrame {
            name,
            kind: ScopeKind::Function,
        });
        callers.push(caller);
        traverse_children(
            node, source, spec, scope, callers, functions, classes, imports, calls,
        )?;
        callers.pop();
        scope.pop();
        return Ok(());
    }

    if spec.imports.contains(&kind) {
        imports.extend(extract_imports(node, source, spec)?);
        return Ok(());
    }

    if spec.calls.contains(&kind)
        && let Some(target) = node.child_by_field_name(spec.call_target_field)
    {
        let raw_name = target.utf8_text(source.as_bytes())?.to_owned();
        let (name, qualifier) = normalize_call_target(&raw_name);
        if !name.is_empty() {
            calls.push(CallInfo {
                name,
                raw_name,
                qualifier,
                caller: callers.last().cloned(),
                line: node.start_position().row + 1,
                column: node.start_position().column + 1,
            });
        }
    }

    traverse_children(
        node, source, spec, scope, callers, functions, classes, imports, calls,
    )
}

#[allow(clippy::too_many_arguments)]
fn traverse_children(
    node: Node,
    source: &str,
    spec: &LanguageSpec,
    scope: &mut Vec<ScopeFrame>,
    callers: &mut Vec<CallerInfo>,
    functions: &mut Vec<FunctionInfo>,
    classes: &mut Vec<ClassInfo>,
    imports: &mut Vec<ImportInfo>,
    calls: &mut Vec<CallInfo>,
) -> Result<(), IndexError> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        traverse(
            child, source, spec, scope, callers, functions, classes, imports, calls,
        )?;
    }
    Ok(())
}

fn attach_methods(functions: &[FunctionInfo], classes: &mut [ClassInfo]) {
    for function in functions {
        let Some(container) = function.container.as_deref() else {
            continue;
        };
        for class in classes.iter_mut().filter(|class| class.name == container) {
            class.methods.push(MethodInfo {
                name: function.name.clone(),
                qualified_name: function.qualified_name.clone(),
                line_start: function.line_start,
                line_end: function.line_end,
                class_name: container.to_owned(),
                receiver: function.receiver.clone(),
            });
        }
    }
}

fn qualify(scope: &[ScopeFrame], name: &str) -> String {
    let mut parts = scope
        .iter()
        .map(|frame| frame.name.as_str())
        .collect::<Vec<_>>();
    parts.push(name);
    parts.join(".")
}

fn qualify_with_container(scope: &[ScopeFrame], container: Option<&str>, name: &str) -> String {
    let mut parts = scope
        .iter()
        .filter(|frame| frame.kind == ScopeKind::Namespace)
        .map(|frame| frame.name.as_str())
        .collect::<Vec<_>>();
    if let Some(container) = container {
        parts.push(container);
    }
    parts.push(name);
    parts.join(".")
}

fn binding_function<'tree>(
    node: Node<'tree>,
    source: &str,
    spec: &LanguageSpec,
) -> Result<Option<(String, Node<'tree>)>, IndexError> {
    if !matches!(
        spec.kind,
        LanguageKind::JavaScript | LanguageKind::TypeScript | LanguageKind::Tsx
    ) || node.kind() != "variable_declarator"
    {
        return Ok(None);
    }
    let Some(value) = node.child_by_field_name("value") else {
        return Ok(None);
    };
    if !matches!(value.kind(), "arrow_function" | "function_expression") {
        return Ok(None);
    }
    let Some(name) = node.child_by_field_name("name") else {
        return Ok(None);
    };
    Ok(Some((name.utf8_text(source.as_bytes())?.to_owned(), value)))
}

fn callable_container(
    node: Node,
    source: &str,
    spec: &LanguageSpec,
    scope: &[ScopeFrame],
) -> Result<(Option<String>, Option<String>), IndexError> {
    if spec.kind == LanguageKind::Go
        && node.kind() == "method_declaration"
        && let Some(receiver) = node.child_by_field_name("receiver")
    {
        let mut identifiers = Vec::new();
        collect_identifiers(receiver, source, &mut identifiers)?;
        if identifiers.len() >= 2 {
            let receiver_name = identifiers.first().cloned();
            let container = identifiers.last().cloned();
            return Ok((container, receiver_name));
        }
    }

    let container = scope
        .iter()
        .rev()
        .find(|frame| frame.kind == ScopeKind::Type)
        .map(|frame| frame.name.clone());
    let receiver = match (spec.kind, container.as_ref()) {
        (LanguageKind::Python, Some(_)) => {
            if let Some(parameters) = node.child_by_field_name("parameters") {
                first_identifier(parameters, source)?
            } else {
                None
            }
        }
        (LanguageKind::Rust, Some(_)) => {
            node.child_by_field_name("parameters")
                .and_then(|parameters| {
                    parameters
                        .utf8_text(source.as_bytes())
                        .ok()
                        .filter(|text| text.contains("self"))
                        .map(|_| "self".to_owned())
                })
        }
        (LanguageKind::JavaScript | LanguageKind::TypeScript | LanguageKind::Tsx, Some(_)) => {
            Some("this".to_owned())
        }
        _ => None,
    };
    Ok((container, receiver))
}

fn first_identifier(node: Node, source: &str) -> Result<Option<String>, IndexError> {
    if matches!(node.kind(), "identifier" | "type_identifier") {
        return Ok(Some(node.utf8_text(source.as_bytes())?.to_owned()));
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(identifier) = first_identifier(child, source)? {
            return Ok(Some(identifier));
        }
    }
    Ok(None)
}

fn collect_identifiers(
    node: Node,
    source: &str,
    identifiers: &mut Vec<String>,
) -> Result<(), IndexError> {
    if matches!(node.kind(), "identifier" | "type_identifier") {
        identifiers.push(node.utf8_text(source.as_bytes())?.to_owned());
        return Ok(());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifiers(child, source, identifiers)?;
    }
    Ok(())
}

fn normalize_call_target(raw: &str) -> (String, Option<String>) {
    let without_generics = strip_angle_groups(raw.trim()).replace("?.", ".");
    let normalized = without_generics
        .trim()
        .trim_matches(|character| matches!(character, '(' | ')' | '&' | '*'))
        .trim_end_matches("::")
        .trim();
    let separator = normalized
        .rfind("::")
        .map(|index| (index, 2))
        .into_iter()
        .chain(normalized.rfind('.').map(|index| (index, 1)))
        .max_by_key(|(index, _)| *index);
    if let Some((index, width)) = separator {
        let qualifier = normalized[..index].trim().to_owned();
        let name = normalized[index + width..].trim().to_owned();
        return (name, (!qualifier.is_empty()).then_some(qualifier));
    }
    (normalized.to_owned(), None)
}

fn strip_angle_groups(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut depth = 0_usize;
    for character in value.chars() {
        match character {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => result.push(character),
            _ => {}
        }
    }
    result
}

fn extract_imports(
    node: Node,
    source: &str,
    spec: &LanguageSpec,
) -> Result<Vec<ImportInfo>, IndexError> {
    let raw = node.utf8_text(source.as_bytes())?;
    let line = node.start_position().row + 1;
    Ok(match spec.kind {
        LanguageKind::Python => parse_python_import(raw, line),
        LanguageKind::Rust => parse_rust_import(raw, line),
        LanguageKind::Go => parse_go_import(node, source, line)?,
        LanguageKind::JavaScript | LanguageKind::TypeScript | LanguageKind::Tsx => {
            parse_javascript_import(raw, line)
        }
    })
}

fn parse_python_import(raw: &str, line: usize) -> Vec<ImportInfo> {
    let normalized = collapse_whitespace(raw);
    if let Some(rest) = normalized.strip_prefix("from ")
        && let Some((module, names)) = rest.split_once(" import ")
    {
        return split_top_level_commas(names.trim_matches(['(', ')']))
            .into_iter()
            .filter_map(|item| {
                let (original, alias) = split_alias(item, " as ");
                (original != "*").then(|| ImportInfo {
                    name: alias.unwrap_or(original).to_owned(),
                    module: Some(module.trim().to_owned()),
                    imported_name: Some(original.to_owned()),
                    namespace: false,
                    line,
                })
            })
            .collect();
    }
    let Some(rest) = normalized.strip_prefix("import ") else {
        return Vec::new();
    };
    split_top_level_commas(rest)
        .into_iter()
        .filter_map(|item| {
            let (module, alias) = split_alias(item, " as ");
            let name = alias
                .map(str::to_owned)
                .or_else(|| module.split('.').next().map(str::to_owned))?;
            Some(ImportInfo {
                name,
                module: Some(module.to_owned()),
                imported_name: None,
                namespace: true,
                line,
            })
        })
        .collect()
}

fn parse_rust_import(raw: &str, line: usize) -> Vec<ImportInfo> {
    let Some(use_index) = raw.find("use ") else {
        return Vec::new();
    };
    let tree = raw[use_index + 4..].trim().trim_end_matches(';');
    let mut imports = Vec::new();
    expand_rust_use("", tree, line, &mut imports);
    imports
}

fn expand_rust_use(prefix: &str, tree: &str, line: usize, imports: &mut Vec<ImportInfo>) {
    if let (Some(open), Some(close)) = (tree.find('{'), tree.rfind('}'))
        && close > open
    {
        let head = tree[..open].trim().trim_end_matches("::");
        let combined = join_rust_path(prefix, head);
        for item in split_top_level_commas(&tree[open + 1..close]) {
            expand_rust_use(&combined, item, line, imports);
        }
        return;
    }

    let (path_part, alias) = split_alias(tree.trim(), " as ");
    let full_path = join_rust_path(prefix, path_part);
    if full_path.ends_with("::*") {
        return;
    }
    let mut parts = full_path
        .split("::")
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let Some(imported) = parts.pop() else {
        return;
    };
    if imported == "self" {
        let Some(module_name) = parts.last().copied() else {
            return;
        };
        imports.push(ImportInfo {
            name: alias.unwrap_or(module_name).to_owned(),
            module: Some(parts.join("::")),
            imported_name: None,
            namespace: true,
            line,
        });
        return;
    }
    let local_name = alias.unwrap_or(imported).to_owned();
    imports.push(ImportInfo {
        name: local_name.clone(),
        module: Some(parts.join("::")),
        imported_name: Some(imported.to_owned()),
        namespace: false,
        line,
    });
    if alias.is_some() {
        imports.push(ImportInfo {
            name: local_name,
            module: Some(full_path),
            imported_name: None,
            namespace: true,
            line,
        });
    }
}

fn join_rust_path(prefix: &str, suffix: &str) -> String {
    match (prefix.is_empty(), suffix.is_empty()) {
        (true, _) => suffix.to_owned(),
        (_, true) => prefix.to_owned(),
        _ => format!("{prefix}::{suffix}"),
    }
}

fn parse_go_import(node: Node, source: &str, _line: usize) -> Result<Vec<ImportInfo>, IndexError> {
    let mut specs = Vec::new();
    collect_nodes_of_kind(node, "import_spec", &mut specs);
    if specs.is_empty() {
        specs.push(node);
    }
    let mut imports = Vec::new();
    for spec_node in specs {
        let raw = spec_node.utf8_text(source.as_bytes())?.trim();
        let Some((prefix, module)) = quoted_module(raw) else {
            continue;
        };
        let alias = prefix
            .trim()
            .trim_start_matches("import")
            .split_whitespace()
            .last();
        if alias == Some("_") {
            continue;
        }
        let default_name = module.rsplit('/').next().unwrap_or(module.as_str());
        imports.push(ImportInfo {
            name: alias.unwrap_or(default_name).to_owned(),
            module: Some(module),
            imported_name: None,
            namespace: true,
            line: spec_node.start_position().row + 1,
        });
    }
    Ok(imports)
}

fn collect_nodes_of_kind<'tree>(node: Node<'tree>, kind: &str, nodes: &mut Vec<Node<'tree>>) {
    if node.kind() == kind {
        nodes.push(node);
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_nodes_of_kind(child, kind, nodes);
    }
}

fn parse_javascript_import(raw: &str, line: usize) -> Vec<ImportInfo> {
    let normalized = collapse_whitespace(raw);
    let Some((clause, module_text)) = normalized
        .strip_prefix("import ")
        .and_then(|rest| rest.rsplit_once(" from "))
    else {
        return Vec::new();
    };
    let Some((_, module)) = quoted_module(module_text) else {
        return Vec::new();
    };
    let mut imports = Vec::new();
    if let Some(star) = clause.find("* as ") {
        let alias = clause[star + 5..]
            .split([',', ' '])
            .find(|part| !part.is_empty());
        if let Some(alias) = alias {
            imports.push(ImportInfo {
                name: alias.to_owned(),
                module: Some(module.clone()),
                imported_name: None,
                namespace: true,
                line,
            });
        }
    }
    if let (Some(open), Some(close)) = (clause.find('{'), clause.rfind('}')) {
        for item in split_top_level_commas(&clause[open + 1..close]) {
            let (original, alias) = split_alias(item, " as ");
            imports.push(ImportInfo {
                name: alias.unwrap_or(original).to_owned(),
                module: Some(module.clone()),
                imported_name: Some(original.to_owned()),
                namespace: false,
                line,
            });
        }
    }
    let default = clause.split(',').next().unwrap_or_default().trim();
    if !default.is_empty() && !default.starts_with(['{', '*']) {
        imports.push(ImportInfo {
            name: default.to_owned(),
            module: Some(module),
            imported_name: Some("default".to_owned()),
            namespace: false,
            line,
        });
    }
    imports
}

fn quoted_module(value: &str) -> Option<(String, String)> {
    let quote_index = value.find(['\'', '"'])?;
    let quote = value.as_bytes()[quote_index] as char;
    let rest = &value[quote_index + 1..];
    let end = rest.find(quote)?;
    Some((value[..quote_index].to_owned(), rest[..end].to_owned()))
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn split_alias<'a>(value: &'a str, separator: &str) -> (&'a str, Option<&'a str>) {
    value
        .trim()
        .split_once(separator)
        .map_or((value.trim(), None), |(name, alias)| {
            (name.trim(), Some(alias.trim()))
        })
}

fn split_top_level_commas(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0_usize;
    let mut start = 0_usize;
    for (index, character) in value.char_indices() {
        match character {
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                let part = value[start..index].trim();
                if !part.is_empty() {
                    parts.push(part);
                }
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    let tail = value[start..].trim();
    if !tail.is_empty() {
        parts.push(tail);
    }

    parts
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn create_temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("brain_indexer_test_{}", nanos));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn test_parse_function() {
        let dir = create_temp_dir();
        let path = write_file(&dir, "test.py", "def foo():\n    return 42\n");
        let parsed = parse_file(&path).unwrap();
        assert_eq!(parsed.functions.len(), 1);
        assert_eq!(parsed.functions[0].name, "foo");
        assert_eq!(parsed.functions[0].line_start, 1);
        assert_eq!(parsed.functions[0].line_end, 2);
        assert!(parsed.parse_error.is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_parse_class_with_methods() {
        let dir = create_temp_dir();
        let path = write_file(
            &dir,
            "test.py",
            "class MyClass:\n    def method(self):\n        pass\n",
        );
        let parsed = parse_file(&path).unwrap();
        assert_eq!(parsed.classes.len(), 1);
        assert_eq!(parsed.classes[0].name, "MyClass");
        assert_eq!(parsed.classes[0].methods.len(), 1);
        assert_eq!(parsed.classes[0].methods[0].name, "method");
        assert_eq!(parsed.classes[0].methods[0].class_name, "MyClass");
        assert!(parsed.parse_error.is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_broken_syntax() {
        let dir = create_temp_dir();
        let path = write_file(&dir, "broken.py", "def foo(:\n    pass\n");
        let parsed = parse_file(&path).unwrap();
        assert!(parsed.parse_error.is_some());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A call the language never bound to a definition must not become an edge.
    ///
    /// Two files define `helper` and a third calls it with no import, so nothing
    /// in the source says which one runs. Guessing would put a false edge in the
    /// graph, which is worse than a missing one: a wrong edge is indistinguishable
    /// from a real relationship once it is stored.
    #[test]
    fn a_call_with_no_binding_evidence_does_not_create_an_edge() {
        let dir = create_temp_dir();
        write_file(&dir, "a.py", "def helper():\n    pass\n");
        write_file(&dir, "b.py", "def helper():\n    pass\n");
        write_file(&dir, "c.py", "def caller():\n    helper()\n");

        let stats = index_directory(&dir).unwrap();
        assert_eq!(stats.files, 3);
        assert_eq!(stats.functions, 3); // helper in a, helper in b, caller in c
        assert_eq!(stats.edges, 0);
        assert!(
            stats.unresolved_calls >= 1,
            "the skipped call has to be counted somewhere, or the gap is invisible"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Every supported language must yield functions, a type, and a call. A
    /// grammar wired up with the wrong node names parses without complaint and
    /// silently finds nothing, which reads exactly like a file with no code.
    #[test]
    fn each_language_yields_functions_types_and_calls() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "sample.rs",
                "struct Engine { power: u32 }\n\
                 impl Engine {\n    fn start(&self) -> u32 { helper() }\n}\n\
                 fn helper() -> u32 { 1 }\n",
                "Engine",
            ),
            (
                "sample.go",
                "package main\n\
                 type Engine struct { Power int }\n\
                 func helper() int { return 1 }\n\
                 func Start() int { return helper() }\n",
                "Engine",
            ),
            (
                "sample.ts",
                "interface Engine { power: number }\n\
                 function helper(): number { return 1; }\n\
                 function start(): number { return helper(); }\n",
                "Engine",
            ),
            (
                "sample.js",
                "class Engine { start() { return helper(); } }\n\
                 function helper() { return 1; }\n",
                "Engine",
            ),
        ];

        for (name, source, expected_type) in cases {
            let dir = create_temp_dir();
            let path = write_file(&dir, name, source);
            let parsed =
                parse_file(&path).unwrap_or_else(|error| panic!("{name} must parse: {error}"));
            assert!(
                parsed.parse_error.is_none(),
                "{name} parsed with an error: {:?}",
                parsed.parse_error
            );
            assert!(
                !parsed.functions.is_empty()
                    || parsed.classes.iter().any(|c| !c.methods.is_empty()),
                "{name} yielded no callables"
            );
            assert!(
                parsed.classes.iter().any(|c| c.name == *expected_type),
                "{name} did not yield type {expected_type}; got {:?}",
                parsed.classes.iter().map(|c| &c.name).collect::<Vec<_>>()
            );
            assert!(
                parsed.calls.iter().any(|call| call.name.contains("helper")),
                "{name} did not record the call to helper; got {:?}",
                parsed.calls.iter().map(|c| &c.name).collect::<Vec<_>>()
            );
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// Broken syntax in any language must be reported, not thrown.
    #[test]
    fn broken_syntax_is_reported_for_every_language() {
        for (name, source) in [
            ("broken.rs", "fn oops( {\n"),
            ("broken.go", "func oops( {\n"),
            ("broken.ts", "function oops( {\n"),
        ] {
            let dir = create_temp_dir();
            let path = write_file(&dir, name, source);
            let parsed = parse_file(&path).unwrap_or_else(|error| panic!("{name}: {error}"));
            assert!(
                parsed.parse_error.is_some(),
                "{name} should report its syntax error"
            );
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn an_unsupported_extension_says_what_is_supported() {
        let dir = create_temp_dir();
        let path = write_file(&dir, "notes.txt", "nothing to parse\n");
        let error = parse_file(&path).expect_err("a text file has no grammar");
        assert!(
            error.to_string().contains("Python") && error.to_string().contains("Rust"),
            "the error should list supported languages: {error}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bounded_read_rejects_actual_overlimit_bytes_and_keeps_unicode() {
        let dir = std::env::temp_dir().join(format!(
            "brain-indexer-bounded-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        fs::create_dir_all(&dir).expect("dir");
        let source = dir.join("unicode.py");
        let text = "def привет():\n    return \"日本語 ✓\"\n";
        fs::write(&source, text).expect("write");
        let limit = text.len() as u64;
        assert_eq!(
            read_source_bounded(&source, limit).expect("at limit"),
            text.as_bytes()
        );
        match read_source_bounded(&source, limit - 1) {
            Err(IndexError::FileTooLarge {
                bytes, max_bytes, ..
            }) => {
                assert_eq!(max_bytes, limit - 1);
                assert_eq!(bytes, limit, "reads only limit + 1 bytes");
            }
            other => panic!("expected FileTooLarge, got {other:?}"),
        }
        let parsed = parse_file(&source).expect("unicode source parses");
        assert!(parsed.functions.iter().any(|f| f.name == "привет"));
        assert!(matches!(
            read_source_bounded(&dir.join("missing.py"), limit),
            Err(IndexError::Io(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }
}
