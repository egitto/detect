#![doc = include_str!("../README.md")]
#![warn(clippy::all)]
#![warn(clippy::cargo)]

pub mod eval;
pub mod expr;
pub mod parser;
pub mod predicate;
mod predicate_error;
pub mod util;

use std::{
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

use expr::Expr;
use ignore::{WalkBuilder, WalkState};
use parser::{error::DetectError, RawParser, Typechecker};
use predicate::{
    MetadataPredicate, NamePredicate, Predicate, StreamingCompiledContentPredicateRef,
};
use slog::{debug, info, warn, Logger};

/// Runtime configuration for detect operations
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Maximum file size (in bytes) for structured data parsing (YAML/JSON/TOML)
    /// Files larger than this will skip structured data evaluation
    pub max_structured_size: u64,
    /// Follow symbolic links while walking and evaluate predicates against
    /// their targets (like `find -L`)
    pub follow_links: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_structured_size: 10 * 1024 * 1024, // 10MB default
            follow_links: false,
        }
    }
}

/// Walks `root` in parallel, calling `on_match` (on the calling thread) for
/// each matching entry, in no particular order. Blocks while walking.
pub async fn parse_and_run_fs<F: FnMut(&Path)>(
    logger: Logger,
    root: &Path,
    respect_gitignore: bool,
    expr: String,
    config: RuntimeConfig,
    mut on_match: F,
) -> Result<usize, DetectError> {
    let original_query = expr.clone();
    let parse_result = RawParser::parse_raw_expr(&expr)
        .and_then(|raw_expr| Typechecker::typecheck(raw_expr, &expr, &config));

    match parse_result {
        Ok(parsed_expr) => {
            if !root.exists() {
                return Err(DetectError::DirectoryNotFound {
                    path: root.display().to_string(),
                });
            }
            if !root.is_dir() {
                return Err(DetectError::NotADirectory {
                    path: root.display().to_string(),
                });
            }

            let walker = WalkBuilder::new(root)
                .hidden(false)
                .git_ignore(respect_gitignore)
                .follow_links(config.follow_links)
                .filter_entry(|entry| {
                    // Always exclude VCS directories, regardless of gitignore settings
                    // This matches ripgrep's behavior
                    !entry
                        .file_name()
                        .to_str()
                        .is_some_and(|s| s == ".git" || s == ".hg" || s == ".svn")
                })
                .build_parallel();

            let expr = parsed_expr.map_predicate_ref(|p| match p {
                Predicate::Name(n) => Predicate::Name(Arc::clone(n)),
                Predicate::Metadata(m) => Predicate::Metadata(Arc::clone(m)),
                Predicate::Content(c) => Predicate::Content(c.as_ref()),
                Predicate::Structured(s) => Predicate::Structured(s.clone()),
            });

            info!(logger, "parsed expression"; "expr" => %expr);

            // Walk and evaluate in parallel, funnelling matches back to this
            // thread so `on_match` needn't be Send
            let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
            let worker_panic = Mutex::new(None);
            let mut match_count = 0;
            std::thread::scope(|scope| {
                let (expr, logger, worker_panic) = (&expr, &logger, &worker_panic);
                scope.spawn(move || {
                    walker.run(|| {
                        let tx = tx.clone();
                        Box::new(move |entry| {
                            let path = match entry {
                                Ok(e) => e.into_path(),
                                Err(e) => {
                                    // Skip entries we can't access (permission denied, etc.)
                                    warn!(logger, "skipping entry due to walker error"; "error" => %e);
                                    return WalkState::Continue;
                                }
                            };
                            if path == root {
                                return WalkState::Continue;
                            }
                            // a panicking worker would leave the others waiting on it
                            // forever, so stop the walk and re-raise it afterwards
                            let visited = panic::catch_unwind(AssertUnwindSafe(|| {
                                visit(logger, expr, &path, root, config.follow_links)
                            }));
                            match visited {
                                Ok(true) if tx.send(path).is_err() => WalkState::Quit,
                                Ok(_) => WalkState::Continue,
                                Err(payload) => {
                                    worker_panic.lock().unwrap().get_or_insert(payload);
                                    WalkState::Quit
                                }
                            }
                        })
                    });
                });

                for path in rx {
                    match_count += 1;
                    on_match(&path);
                }
            });
            if let Some(payload) = worker_panic.into_inner().unwrap() {
                panic::resume_unwind(payload);
            }

            if match_count == 0 {
                eprintln!("No files matched the query: {original_query}");
                eprintln!("Searched in: {}", root.display());
                if respect_gitignore {
                    eprintln!("Hint: Use -i flag to include gitignored files, or try broadening your search criteria");
                } else {
                    eprintln!("Hint: Try broadening your search criteria or check the path/expression syntax");
                }
            }

            Ok(match_count)
        }
        Err(err) => Err(err),
    }
}

/// Evaluate `expr` against one entry, treating I/O errors as a non-match
fn visit(
    logger: &Logger,
    expr: &Expr<Predicate<NamePredicate, MetadataPredicate, StreamingCompiledContentPredicateRef>>,
    path: &Path,
    root: &Path,
    follow_links: bool,
) -> bool {
    let start = Instant::now();

    let is_match = match eval::fs::eval(logger, expr, path, Some(root), follow_links) {
        Ok(result) => result,
        Err(e) => {
            // Handle I/O errors gracefully - skip files we can't access
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                debug!(logger, "skipping file due to permission denied"; "path" => #?path);
            } else {
                // For other I/O errors, also skip but log at warning level
                warn!(logger, "skipping file due to I/O error"; "path" => #?path, "error" => %e);
            }
            return false;
        }
    };

    let duration = start.elapsed();
    debug!(logger, "visited entity"; "path" => #?path, "duration" => #?duration, "result" => is_match);
    is_match
}
