use crate::expr::short_circuit::ShortCircuit;
use crate::expr::Expr;
use crate::predicate::{Predicate, StreamingCompiledContentPredicateRef};
use crate::util::Done;
use regex_automata::dfa::Automaton;
use std::io::{self, Read};

pub mod fs;
pub mod structured;

pub fn run_contents_predicate(
    e: Expr<Predicate<Done, Done, StreamingCompiledContentPredicateRef<'_>>>,
    mut reader: impl Read,
) -> io::Result<Expr<Predicate<Done, Done, Done>>> {
    let config = regex_automata::util::start::Config::new();

    // Initialize state for DFA patterns
    let mut e: Expr<Predicate<Done, Done, _>> = e.map_predicate(|p| match p {
        Predicate::Content(pred) => {
            let dfa = pred.inner;
            let s = dfa
                .start_state(&config)
                .expect("DFA start_state failed: invalid regex configuration");
            Predicate::Content((dfa, s))
        }
        _ => unreachable!(),
    });

    let mut buf = vec![0; 64 * 1024];
    loop {
        // read the next chunk of bytes
        let bytes = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => &buf[..n],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };

        // advance each pattern appropriately
        e = e.reduce_predicate_and_short_circuit(|p| match p {
            Predicate::Content((dfa, state)) => {
                // DFA streaming processing
                let mut next_state = state;
                let mut iter = bytes.iter();

                loop {
                    if let Some(byte) = iter.next() {
                        next_state = dfa.next_state(next_state, *byte);

                        if dfa.is_match_state(next_state) {
                            break ShortCircuit::Known(true);
                        }

                        if dfa.is_dead_state(next_state) {
                            break ShortCircuit::Known(false);
                        }
                    } else {
                        break ShortCircuit::Unknown(Predicate::Content((dfa, next_state)));
                    }
                }
            }
            _ => unreachable!(),
        });
        if matches!(e, Expr::Literal(_)) {
            break;
        }
    }

    // Final evaluation
    let e = e.reduce_predicate_and_short_circuit(|p| match p {
        Predicate::Content((dfa, state)) => {
            let next_state = dfa.next_eoi_state(state);
            let matched = dfa.is_match_state(next_state);
            ShortCircuit::Known(matched)
        }
        _ => unreachable!(),
    });

    Ok(e)
}
