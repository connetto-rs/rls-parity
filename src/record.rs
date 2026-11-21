//! Each files case written as a fixture that one engine replays against PostgreSQL's
//! recorded answers.

use std::{fs, path::Path};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    Answer, Direction, Engine, KnownList, Measurements,
    case::{Case, Source},
};

/// PostgreSQL's answer for every caller, row and statement of one case.
#[derive(Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Recorded {
    #[serde(default)]
    pub answer: Vec<RecordedAnswer>,
}

/// PostgreSQL's answer for one caller, row and statement.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedAnswer {
    pub caller: String,
    pub row: String,
    pub statement: String,
    pub postgres: Answer,
}

/// One case's known discrepancies, per engine.
#[derive(Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaseKnown {
    #[serde(default, skip_serializing_if = "EngineKnown::is_empty")]
    pub sqlite: EngineKnown,
    #[serde(default, skip_serializing_if = "EngineKnown::is_empty")]
    pub openfga: EngineKnown,
}

impl CaseKnown {
    /// Whether neither engine has a known discrepancy.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sqlite.is_empty() && self.openfga.is_empty()
    }

    /// The known discrepancies of `engine`.
    fn engine(&mut self, engine: Engine) -> &mut EngineKnown {
        match engine {
            Engine::Sqlite => &mut self.sqlite,
            Engine::Openfga => &mut self.openfga,
        }
    }
}

/// One engine's known discrepancies in one case.
#[derive(Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EngineKnown {
    /// The finding behind this engine's translator refusing a schema the other accepts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub row: Vec<KnownRow>,
}

impl EngineKnown {
    /// Whether the engine has no known discrepancy.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.refusal.is_none() && self.row.is_empty()
    }
}

/// A row on which one engine is known to answer differently from PostgreSQL.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KnownRow {
    pub caller: String,
    pub row: String,
    pub statement: String,
    pub direction: Direction,
    pub finding: String,
}

/// The fixture for case `name`, PostgreSQL's answers from `measurements` and the entries
/// of `known` naming the case.
#[must_use]
pub fn fixture(
    name: &str,
    measurements: &Measurements,
    known: &KnownList,
) -> (Recorded, CaseKnown) {
    let answer = measurements
        .observations
        .iter()
        .filter(|observation| observation.case == name)
        .map(|observation| RecordedAnswer {
            caller: observation.caller.clone(),
            row: observation.row.clone(),
            statement: observation.statement.clone(),
            postgres: observation.postgres.clone(),
        })
        .collect();
    let mut case_known = CaseKnown::default();
    for entry in known.known.iter().filter(|entry| entry.case == name) {
        case_known.engine(entry.engine).row.push(KnownRow {
            caller: entry.caller.clone(),
            row: entry.row.clone(),
            statement: entry.statement.clone(),
            direction: entry.direction,
            finding: entry.finding.clone(),
        });
    }
    for entry in known.refusal.iter().filter(|entry| entry.case == name) {
        case_known.engine(entry.engine).refusal = Some(entry.finding.clone());
    }
    (Recorded { answer }, case_known)
}

/// Write every files case under `cases` into `out`, beside its recorded answers in
/// `postgres.toml` and its known discrepancies in `known.toml`, and answer how many were
/// written. A case built from connetto stays out.
///
/// # Errors
/// Returns the case and cause when a case cannot be read or its fixture written.
pub fn record(
    cases: &Path,
    measurements: &Measurements,
    known: &KnownList,
    out: &Path,
) -> Result<usize> {
    let mut directories = fs::read_dir(cases)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    directories.sort();
    let mut written = 0;
    for directory in directories {
        let case =
            Case::read(&directory).with_context(|| format!("Reading {}", directory.display()))?;
        if case.config.source != Source::Files {
            continue;
        }
        let target = out.join(&case.name);
        fs::create_dir_all(&target)?;
        for file in ["schema.sql", "seed.sql", "callers.toml"] {
            fs::copy(directory.join(file), target.join(file))
                .with_context(|| format!("Copying {file} of {}", case.name))?;
        }
        let (recorded, case_known) = fixture(&case.name, measurements, known);
        fs::write(target.join("postgres.toml"), toml::to_string(&recorded)?)?;
        if !case_known.is_empty() {
            fs::write(target.join("known.toml"), toml::to_string(&case_known)?)?;
        }
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Known, KnownRefusal, Observation};

    fn observation(case: &str, caller: &str, postgres: Answer) -> Observation {
        Observation {
            case: case.into(),
            caller: caller.into(),
            row: "docs:1".into(),
            statement: "SELECT \"id\" FROM \"docs\"".into(),
            postgres,
            sqlite: Answer::Hidden,
            openfga: Answer::Visible,
        }
    }

    fn known_row(case: &str, engine: Engine, finding: &str) -> Known {
        Known {
            case: case.into(),
            caller: "alice".into(),
            row: "docs:1".into(),
            statement: "SELECT \"id\" FROM \"docs\"".into(),
            engine,
            direction: Direction::Narrower,
            finding: finding.into(),
        }
    }

    /// A case's fixture holds PostgreSQL's answers for that case alone, and its known
    /// entries split by the engine they name, and both survive a TOML round trip.
    #[test]
    fn fixture_keeps_one_case_split_by_engine() -> Result<()> {
        let measurements = Measurements {
            acceptances: Vec::new(),
            observations: vec![
                observation("docs", "alice", Answer::Visible),
                observation("other", "alice", Answer::Hidden),
                observation("docs", "bob", Answer::Rejected("42501".into())),
            ],
        };
        let known = KnownList {
            known: vec![
                known_row("docs", Engine::Sqlite, "sqlite finding"),
                known_row("other", Engine::Openfga, "other finding"),
                known_row("docs", Engine::Openfga, "openfga finding"),
            ],
            refusal: vec![KnownRefusal {
                case: "docs".into(),
                engine: Engine::Openfga,
                finding: "refusal finding".into(),
            }],
            construct: Vec::new(),
        };
        let (recorded, case_known) = fixture("docs", &measurements, &known);
        let row = |finding: &str| KnownRow {
            caller: "alice".into(),
            row: "docs:1".into(),
            statement: "SELECT \"id\" FROM \"docs\"".into(),
            direction: Direction::Narrower,
            finding: finding.into(),
        };
        let answer = |caller: &str, postgres| RecordedAnswer {
            caller: caller.into(),
            row: "docs:1".into(),
            statement: "SELECT \"id\" FROM \"docs\"".into(),
            postgres,
        };
        assert_eq!(
            recorded,
            Recorded {
                answer: vec![
                    answer("alice", Answer::Visible),
                    answer("bob", Answer::Rejected("42501".into())),
                ],
            }
        );
        assert_eq!(
            case_known,
            CaseKnown {
                sqlite: EngineKnown {
                    refusal: None,
                    row: vec![row("sqlite finding")],
                },
                openfga: EngineKnown {
                    refusal: Some("refusal finding".into()),
                    row: vec![row("openfga finding")],
                },
            }
        );
        assert_eq!(
            toml::from_str::<Recorded>(&toml::to_string(&recorded)?)?,
            recorded
        );
        assert_eq!(
            toml::from_str::<CaseKnown>(&toml::to_string(&case_known)?)?,
            case_known
        );
        Ok(())
    }
}
