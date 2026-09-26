use crate::expr::short_circuit::ShortCircuit;
use crate::expr::Expr;
use crate::predicate::{
    MetadataPredicate, NamePredicate, Predicate, StreamingCompiledContentPredicateRef,
};
use crate::util::Done;
use slog::{debug, o, Logger};
use std::fs::{File, Metadata, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::eval::run_contents_predicate;
use crate::eval::structured::{eval_structured_predicate, ParsedDocuments};

/// multipass evaluation with short circuiting, runs, in order:
/// - file name matchers
/// - metadata matchers
/// - file content matchers
pub fn eval<'dfa>(
    logger: &Logger,
    e: &'dfa Expr<
        Predicate<NamePredicate, MetadataPredicate, StreamingCompiledContentPredicateRef<'dfa>>,
    >,
    path: &Path,
    base_path: Option<&Path>,
    follow_links: bool,
) -> std::io::Result<bool> {
    let logger = logger.new(o!("path" => format!("{:?}", path)));

    debug!(logger, "visit entity"; "expr" => %e);

    let e: Expr<Predicate<Done, MetadataPredicate, StreamingCompiledContentPredicateRef<'dfa>>> =
        e.reduce_predicate_and_short_circuit(|p| p.eval_name_predicate(path, base_path));

    if let Expr::Literal(b) = e {
        debug!(logger, "short circuit after path predicate eval"; "expr" => %e, "result" => %b);
        return Ok(b);
    }

    debug!(logger, "reduced expr after path predicate eval";  "expr" => %e);

    // stat without opening: opening a FIFO blocks, and metadata shouldn't need
    // read access. Only follow symlinks if the walker does.
    let metadata = entry_metadata(path, follow_links)?;

    let e: Expr<Predicate<Done, Done, StreamingCompiledContentPredicateRef<'dfa>>> =
        e.reduce_predicate_and_short_circuit(|p| p.eval_metadata_predicate(&metadata));

    if let Expr::Literal(b) = e {
        debug!(logger, "short circuit after metadata predicate eval";  "expr" => %e, "result" => %b);
        return Ok(b);
    }

    debug!(logger, "reduced expr after metadata predicate eval";  "expr" => %e);

    // Determine which predicates remain for optimized file reading
    let has_structured = e.contains_structured_predicates();
    let has_content = e.contains_content_predicates();

    let file = if metadata.is_file() {
        open_regular_file(path, follow_links)?
    } else {
        None
    };

    let Some(file) = file else {
        debug!(
            logger,
            "not a file, all structured/content predicates eval to false"
        );
        let e: Expr<Predicate<Done, Done, Done, Done>> =
            e.reduce_predicate_and_short_circuit(|p| match p {
                Predicate::Content(_) => ShortCircuit::Known(false),
                Predicate::Structured(_) => ShortCircuit::Known(false),
                _ => unreachable!(
                    "only Content and Structured predicates should remain after metadata phase"
                ),
            });

        if let Expr::Literal(b) = e {
            debug!(logger, "evaluation finished"; "result" => b);
            return Ok(b);
        }
        unreachable!("all predicates should be reduced to literals after evaluation")
    };

    match (has_structured, has_content) {
        (true, true) => {
            debug!(
                logger,
                "evaluating both structured and content predicates - single file read"
            );
            let bytes = read_all(file)?;

            if let Ok(contents) = std::str::from_utf8(&bytes) {
                // UTF-8: evaluate structured predicates first
                let mut cache = ParsedDocuments::new();
                let e = e.reduce_predicate_and_short_circuit(|p| match p {
                    Predicate::Structured(s) => {
                        match eval_structured_predicate(&s, contents, &mut cache) {
                            Ok(result) => ShortCircuit::Known(result),
                            Err(_) => ShortCircuit::Known(false),
                        }
                    }
                    Predicate::Content(c) => ShortCircuit::Unknown(Predicate::Content(c)),
                    _ => unreachable!("only Structured and Content predicates should remain"),
                });

                if let Expr::Literal(b) = e {
                    debug!(logger, "short circuit after structured predicates"; "result" => b);
                    return Ok(b);
                }

                // Evaluate content predicates against the bytes already read
                let e = run_contents_predicate(e, &bytes[..])?;

                if let Expr::Literal(b) = e {
                    debug!(logger, "evaluation finished"; "result" => b);
                    Ok(b)
                } else {
                    unreachable!(
                        "all content predicates should be reduced to literals after streaming"
                    )
                }
            } else {
                debug!(
                    logger,
                    "file is not UTF-8, structured predicates = false, using streaming content"
                );
                // Non-UTF-8: structured predicates fail, stream content
                let e = e.reduce_predicate_and_short_circuit(|p| match p {
                    Predicate::Structured(_) => ShortCircuit::Known(false),
                    Predicate::Content(c) => ShortCircuit::Unknown(Predicate::Content(c)),
                    _ => unreachable!("only Structured and Content predicates should remain"),
                });

                if let Expr::Literal(b) = e {
                    debug!(logger, "short circuit after structured=false"; "result" => b);
                    return Ok(b);
                }

                let e = run_contents_predicate(e, &bytes[..])?;

                if let Expr::Literal(b) = e {
                    debug!(logger, "evaluation finished"; "result" => b);
                    Ok(b)
                } else {
                    unreachable!(
                        "all content predicates should be reduced to literals after streaming"
                    )
                }
            }
        }
        (true, false) => {
            debug!(logger, "evaluating structured predicates only");
            let contents = read_all(file).map(String::from_utf8);
            let e = match contents {
                Ok(Ok(contents)) => {
                    let mut cache = ParsedDocuments::new();
                    e.reduce_predicate_and_short_circuit(|p| match p {
                        Predicate::Structured(s) => {
                            match eval_structured_predicate(&s, &contents, &mut cache) {
                                Ok(result) => {
                                    ShortCircuit::<Predicate<Done, Done, Done>>::Known(result)
                                }
                                Err(_) => ShortCircuit::<Predicate<Done, Done, Done>>::Known(false),
                            }
                        }
                        _ => unreachable!(
                            "only Structured predicates should remain when has_content is false"
                        ),
                    })
                }
                Ok(Err(_)) | Err(_) => {
                    // Non-UTF-8 or read error: all structured predicates = false
                    e.reduce_predicate_and_short_circuit(|p| match p {
                        Predicate::Structured(_) => {
                            ShortCircuit::<Predicate<Done, Done, Done>>::Known(false)
                        }
                        _ => unreachable!(
                            "only Structured predicates should remain when has_content is false"
                        ),
                    })
                }
            };

            if let Expr::Literal(b) = e {
                debug!(logger, "evaluation finished"; "result" => b);
                Ok(b)
            } else {
                unreachable!(
                    "all structured predicates should be reduced to literals after evaluation"
                )
            }
        }
        (false, true) => {
            debug!(logger, "evaluating content predicates only - streaming");
            let e = run_contents_predicate(e, file)?;

            if let Expr::Literal(b) = e {
                debug!(logger, "evaluation finished"; "result" => b);
                Ok(b)
            } else {
                unreachable!("all content predicates should be reduced to literals after streaming")
            }
        }
        (false, false) => {
            // No structured or content predicates remain (already short-circuited)
            unreachable!(
                "both has_structured and has_content are false - should have short-circuited"
            )
        }
    }
}

fn entry_metadata(path: &Path, follow_links: bool) -> std::io::Result<Metadata> {
    if follow_links {
        std::fs::metadata(path)
    } else {
        std::fs::symlink_metadata(path)
    }
}

/// Open `path` for reading only if it is (still) a regular file. O_NONBLOCK
/// keeps the open from hanging if the entry was swapped for a FIFO after it
/// was stat'ed; it has no effect on reads from regular files.
fn open_regular_file(path: &Path, follow_links: bool) -> std::io::Result<Option<File>> {
    let mut flags = libc::O_NONBLOCK | libc::O_NOCTTY;
    if !follow_links {
        flags |= libc::O_NOFOLLOW;
    }
    let file = match OpenOptions::new().read(true).custom_flags(flags).open(path) {
        Ok(file) => file,
        // swapped for a symlink since it was stat'ed
        // (Linux reports ELOOP, FreeBSD EMLINK)
        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::EMLINK)) => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(file.metadata()?.is_file().then_some(file))
}

fn read_all(mut file: File) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}
