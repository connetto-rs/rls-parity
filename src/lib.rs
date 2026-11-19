#![doc = include_str!("../README.md")]

mod case;
mod openfga;
mod postgres;
mod rows;
mod runner;
mod sqlite;

pub use runner::{RunError, observe_cases, read_known};

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub(crate) enum Prepared<T> {
    Ready(T),
    Refused(String),
}

/// What one write statement did on a database.
pub(crate) enum Outcome {
    /// The statement ran and changed the rows with these keys.
    Applied(BTreeSet<String>),
    /// The statement aborted with this SQLSTATE.
    Rejected(String),
}

impl Outcome {
    /// The answer this outcome gives for the row with `key`.
    fn answer(&self, key: &str) -> Answer {
        match self {
            Self::Applied(keys) if keys.contains(key) => Answer::Changed,
            Self::Applied(_) => Answer::Skipped,
            Self::Rejected(sqlstate) => Answer::Rejected(sqlstate.clone()),
        }
    }
}

/// An engine's answer for one row.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    Visible,
    Hidden,
    /// A write changed the row, or `OpenFGA` permits the write.
    Changed,
    /// A write ran and left the row alone because no policy let it reach the row.
    Skipped,
    /// A write aborted with this SQLSTATE.
    Rejected(String),
    /// `OpenFGA` refuses the write, which agrees with a skipped or a rejected row.
    Denied,
    Refused(String),
    Error(String),
}

/// A comparison with its live PostgreSQL oracle.
#[derive(Debug, Serialize)]
pub struct Observation {
    pub case: String,
    pub caller: String,
    pub row: String,
    pub statement: String,
    pub postgres: Answer,
    pub sqlite: Answer,
    pub openfga: Answer,
}

/// The translated engine being compared.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    Sqlite,
    Openfga,
}

/// The way an answer differs from PostgreSQL.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Wider,
    Narrower,
    /// Both refuse the write, one by skipping the row and the other by aborting, or with
    /// different SQLSTATEs.
    DifferentDenial,
    Refused,
    Error,
}

/// A tracked discrepancy without a stored PostgreSQL expectation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Known {
    pub case: String,
    pub caller: String,
    pub row: String,
    pub statement: String,
    pub engine: Engine,
    pub direction: Direction,
    pub finding: String,
}

/// Classify an engine answer against the live oracle.
fn direction(oracle: &Answer, actual: &Answer) -> Option<Direction> {
    if oracle == actual && !matches!(oracle, Answer::Error(_)) {
        return None;
    }
    Some(match (oracle, actual) {
        (_, Answer::Refused(_)) => Direction::Refused,
        (Answer::Skipped | Answer::Rejected(_), Answer::Denied) => return None,
        (Answer::Visible, Answer::Hidden)
        | (Answer::Changed, Answer::Skipped | Answer::Rejected(_) | Answer::Denied) => {
            Direction::Narrower
        }
        (Answer::Hidden, Answer::Visible)
        | (Answer::Skipped | Answer::Rejected(_), Answer::Changed) => Direction::Wider,
        (Answer::Skipped | Answer::Rejected(_), Answer::Skipped | Answer::Rejected(_)) => {
            Direction::DifferentDenial
        }
        _ => Direction::Error,
    })
}

/// Report unexpected discrepancies and known discrepancies that disappeared.
#[must_use]
pub fn compare(observations: &[Observation], known: &[Known]) -> Vec<String> {
    let mut seen = vec![false; known.len()];
    let mut errors = Vec::new();
    for observation in observations {
        for (engine, answer) in [
            (Engine::Sqlite, &observation.sqlite),
            (Engine::Openfga, &observation.openfga),
        ] {
            let Some(direction) = direction(&observation.postgres, answer) else {
                continue;
            };
            let matching = known.iter().position(|entry| {
                entry.case == observation.case
                    && entry.caller == observation.caller
                    && entry.row == observation.row
                    && entry.statement == observation.statement
                    && entry.engine == engine
                    && entry.direction == direction
            });
            if let Some(index) = matching {
                seen[index] = true;
            } else {
                errors.push(format!("Unlisted {engine:?} {direction:?} {observation:?}"));
            }
        }
    }
    for (entry, seen) in known.iter().zip(seen) {
        if !seen {
            let witness = observations.iter().find(|row| {
                row.case == entry.case
                    && row.caller == entry.caller
                    && row.row == entry.row
                    && row.statement == entry.statement
            });
            errors.push(format!("Stale known entry {entry:?} witness={witness:?}"));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A known divergence must still disagree in its declared direction.
    #[test]
    fn known_entries_must_be_observed() {
        let known = Known {
            case: "fixture".into(),
            caller: "alice".into(),
            row: "docs:1".into(),
            statement: "SELECT id FROM docs".into(),
            engine: Engine::Sqlite,
            direction: Direction::Narrower,
            finding: "test".into(),
        };
        let mut observation = Observation {
            case: "fixture".into(),
            caller: "alice".into(),
            row: "docs:1".into(),
            statement: "SELECT id FROM docs".into(),
            postgres: Answer::Visible,
            sqlite: Answer::Hidden,
            openfga: Answer::Visible,
        };
        assert_eq!(
            compare(
                std::slice::from_ref(&observation),
                std::slice::from_ref(&known)
            ),
            Vec::<String>::new()
        );
        assert_eq!(compare(std::slice::from_ref(&observation), &[]).len(), 1);
        let mut unrelated = Observation {
            case: observation.case.clone(),
            caller: "bob".into(),
            row: observation.row.clone(),
            statement: observation.statement.clone(),
            postgres: Answer::Visible,
            sqlite: Answer::Hidden,
            openfga: Answer::Visible,
        };
        assert_eq!(
            compare(
                std::slice::from_ref(&unrelated),
                std::slice::from_ref(&known)
            )
            .len(),
            2
        );
        unrelated.postgres = Answer::Error("broken query".into());
        unrelated.sqlite = Answer::Error("broken query".into());
        unrelated.openfga = Answer::Error("broken query".into());
        assert_eq!(compare(&[unrelated], &[]).len(), 2);
        observation.sqlite = Answer::Visible;
        let errors = compare(&[observation], &[known]);
        assert_eq!(errors.len(), 1);
        for required in [
            "fixture", "alice", "docs:1", "SELECT", "postgres", "sqlite", "openfga",
        ] {
            assert!(errors[0].contains(required), "{required}");
        }
    }

    /// Visibility, write outcomes, translation refusals and execution failures have distinct directions.
    #[test]
    fn answer_directions() {
        for (oracle, actual, expected) in [
            (Answer::Hidden, Answer::Visible, Some(Direction::Wider)),
            (Answer::Visible, Answer::Hidden, Some(Direction::Narrower)),
            (
                Answer::Visible,
                Answer::Refused("unsupported".into()),
                Some(Direction::Refused),
            ),
            (
                Answer::Visible,
                Answer::Error("failed".into()),
                Some(Direction::Error),
            ),
            (Answer::Hidden, Answer::Hidden, None),
            (Answer::Skipped, Answer::Denied, None),
            (Answer::Rejected("42501".into()), Answer::Denied, None),
            (
                Answer::Rejected("42501".into()),
                Answer::Rejected("42501".into()),
                None,
            ),
            (Answer::Changed, Answer::Denied, Some(Direction::Narrower)),
            (Answer::Changed, Answer::Skipped, Some(Direction::Narrower)),
            (Answer::Skipped, Answer::Changed, Some(Direction::Wider)),
            (
                Answer::Rejected("42501".into()),
                Answer::Changed,
                Some(Direction::Wider),
            ),
            (
                Answer::Rejected("42501".into()),
                Answer::Skipped,
                Some(Direction::DifferentDenial),
            ),
            (
                Answer::Rejected("42501".into()),
                Answer::Rejected("23505".into()),
                Some(Direction::DifferentDenial),
            ),
            (Answer::Visible, Answer::Changed, Some(Direction::Error)),
        ] {
            assert_eq!(direction(&oracle, &actual), expected);
        }
    }
}
