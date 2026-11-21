use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use rls2fga::types::ActionStatement;
use serde_json::Value;
use sql_traits::parity::{
    Caller, Case, CaseKnown, Engine, EngineAnswer, Fixture, Recorded, RecordedAnswer, Shape, Table,
    Write, fixtures, identifier,
};

use crate::{
    Acceptance, Answer, KnownList, Measurements, Observation, Outcome, Prepared, case, connetto,
    openfga::{Model, Probe, Verdict, action, columns},
    postgres::Database,
    verify_recorded,
};

/// A suite input or engine setup failure.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("Cannot read {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Invalid known list at {path}")]
    InvalidKnown {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("No case directories in {0}")]
    NoCases(PathBuf),
    #[error("Cannot measure case {path}")]
    Case {
        path: PathBuf,
        #[source]
        source: anyhow::Error,
    },
}

/// Read the exact known-discrepancy list.
///
/// # Errors
/// Returns the path and cause when the file cannot be read or decoded.
pub fn read_known(path: &Path) -> std::result::Result<KnownList, RunError> {
    let text = fs::read_to_string(path).map_err(|source| RunError::Io {
        path: path.into(),
        source,
    })?;
    toml::from_str(&text).map_err(|source| RunError::InvalidKnown {
        path: path.into(),
        source,
    })
}

/// Collect live three-engine observations independently of the known list.
///
/// # Errors
/// Returns the case path and cause for invalid fixtures or engine setup failures.
pub fn observe_cases(
    root: &Path,
    database_url: &str,
    openfga_url: &str,
) -> std::result::Result<Measurements, RunError> {
    let directories = fs::read_dir(root)
        .and_then(|entries| {
            entries
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<Vec<_>>>()
        })
        .map_err(|source| RunError::Io {
            path: root.into(),
            source,
        })?;
    if directories.is_empty() {
        return Err(RunError::NoCases(root.into()));
    }
    let mut directories = directories;
    directories.sort();
    let mut measurements = Measurements::default();
    for directory in directories {
        let case = case::read(&directory).map_err(|source| RunError::Case {
            path: directory.clone(),
            source,
        })?;
        let fixture = Fixture {
            case,
            recorded: Recorded::default(),
            known: CaseKnown::default(),
        };
        measure_fixture(fixture, false, database_url, openfga_url, &mut measurements).map_err(
            |source| RunError::Case {
                path: directory,
                source,
            },
        )?;
    }
    Ok(measurements)
}

/// Measure embedded fixtures and the local cases.
///
/// # Errors
/// Returns the case and cause when loading or measuring fails.
pub fn observe_suite(root: &Path, database_url: &str, openfga_url: &str) -> Result<Measurements> {
    let mut measurements = Measurements::default();
    for fixture in fixtures() {
        measure_fixture(fixture?, true, database_url, openfga_url, &mut measurements)?;
    }
    let local = observe_cases(root, database_url, openfga_url)?;
    measurements.acceptances.extend(local.acceptances);
    measurements.observations.extend(local.observations);
    measurements.errors.extend(local.errors);
    Ok(measurements)
}

/// Judge measured answers through the shared fixture contract.
fn measure_fixture(
    mut fixture: Fixture,
    has_recording: bool,
    database_url: &str,
    openfga_url: &str,
    measurements: &mut Measurements,
) -> Result<()> {
    let case = &fixture.case;
    let (mut acceptance, observations) = run_case(case, database_url, openfga_url)?;
    let (answer, openfga): (Vec<_>, Vec<_>) = observations.into_iter().unzip();
    let live = Recorded { answer };
    if has_recording && let Err(error) = verify_recorded(&case.name, &fixture.recorded, &live) {
        measurements.errors.push(error.to_string());
    }
    let owner = if case.name == "connetto_deployment" {
        connetto::replica_options()?
    } else {
        pg2sqlite::parity::case_options(case)?
    };
    let probes: Vec<_> = live
        .answer
        .iter()
        .map(|answer| pg2sqlite::parity::Probe {
            caller: &answer.caller,
            row: &answer.row,
            statement: &answer.statement,
        })
        .collect();
    let replay = pg2sqlite::parity::replay(case, &owner, &probes)
        .with_context(|| format!("Replaying SQLite for {}", case.name))?;
    fixture.recorded = live;
    let openfga: Vec<_> = fixture
        .recorded
        .answer
        .iter()
        .zip(openfga)
        .map(|(recorded, answer)| EngineAnswer {
            caller: recorded.caller.clone(),
            row: recorded.row.clone(),
            statement: recorded.statement.clone(),
            answer,
        })
        .collect();
    for discrepancy in fixture
        .judge(Engine::Sqlite, replay.refusal.as_deref(), &replay.answers)
        .into_iter()
        .chain(fixture.judge(Engine::Openfga, acceptance.openfga.as_deref(), &openfga))
    {
        measurements.errors.push(discrepancy.to_string());
    }
    acceptance.sqlite = replay.refusal;
    for ((recorded, sqlite), openfga) in fixture
        .recorded
        .answer
        .into_iter()
        .zip(replay.answers)
        .zip(openfga)
    {
        measurements.observations.push(Observation {
            case: fixture.case.name.clone(),
            caller: recorded.caller,
            row: recorded.row,
            statement: recorded.statement,
            postgres: recorded.postgres,
            sqlite: sqlite.answer,
            openfga: openfga.answer,
        });
    }
    measurements.acceptances.push(acceptance);
    Ok(())
}

/// Measure whether each translator accepts the schema, then every caller and seeded row
/// of every table, before evaluating the known list.
fn run_case(
    case: &Case,
    database_url: &str,
    openfga_url: &str,
) -> Result<(Acceptance, Vec<(RecordedAnswer, Answer)>)> {
    let mut database =
        Database::create(database_url, case).with_context(|| format!("Creating {}", case.name))?;
    let mut rows = BTreeMap::new();
    for table in &case.config.tables {
        let seeded = database.rows(&identifier(&table.name))?;
        ensure!(
            !seeded.is_empty(),
            "{} has no seeded rows in {}",
            case.name,
            table.name
        );
        rows.insert(table.name.clone(), seeded);
    }
    let model = Model::create(openfga_url, case, &mut database)
        .with_context(|| format!("Loading OpenFGA for {}", case.name))?;
    let acceptance = acceptance(case, &model);
    let mut engines = Engines { database, model };
    let mut observations = Vec::new();
    for table in &case.config.tables {
        observations.extend(measure_reads(
            case,
            table,
            &rows[&table.name],
            &mut engines,
        )?);
    }
    for write in case.writes() {
        let table = case
            .table(&write.table)
            .context("Write targets undeclared table")?;
        observations.extend(measure_write(
            case,
            table,
            write,
            &rows[&table.name],
            &mut engines,
        )?);
    }
    if let Prepared::Ready(mut model) = engines.model {
        model.cleanup()?;
    }
    engines.database.cleanup()?;
    Ok((acceptance, observations))
}

/// The live PostgreSQL database and `OpenFGA` model.
struct Engines {
    database: Database,
    model: Prepared<Model>,
}

/// Measure every read shape on `table` for every caller and seeded row.
fn measure_reads(
    case: &Case,
    table: &Table,
    rows: &[Value],
    engines: &mut Engines,
) -> Result<Vec<(RecordedAnswer, Answer)>> {
    let mut observations = Vec::new();
    let read_columns = columns(table.read_columns());
    for shape in case.reads(table) {
        let statement = table.statement(shape);
        for caller in &case.config.callers {
            let postgres = engines.database.visible(table, caller, &statement);
            for row in rows {
                let key = table.row_key(|column| row.get(column))?;
                let probe = Probe {
                    existing: Some(row),
                    resulting: None,
                    columns: read_columns.clone(),
                };
                let openfga_answer = match &engines.model {
                    Prepared::Refused(reason) => Answer::Refused(reason.clone()),
                    Prepared::Ready(model) => {
                        match model.permits(table, caller, action(shape), &probe) {
                            Ok(Verdict::Changed) => Answer::Visible,
                            Ok(Verdict::Skipped | Verdict::Aborted) => Answer::Hidden,
                            Err(error) => Answer::Error(format!("{error:#}")),
                        }
                    }
                };
                observations.push((
                    RecordedAnswer {
                        caller: caller.name.clone(),
                        row: format!("{}:{key}", table.name),
                        statement: statement.clone(),
                        postgres: visibility(&postgres, &key),
                    },
                    openfga_answer,
                ));
            }
        }
    }
    Ok(observations)
}

/// Measure `write` on `table` for every caller and the rows it reaches.
fn measure_write(
    case: &Case,
    table: &Table,
    write: &Write,
    rows: &[Value],
    engines: &mut Engines,
) -> Result<Vec<(RecordedAnswer, Answer)>> {
    let mut observations = Vec::new();
    let targets = targets(table, write, rows, &mut engines.database)?;
    let columns = columns(write.columns(&table.key));
    for caller in &case.config.callers {
        for target in &targets {
            let postgres = engines.database.write(table, caller, &target.statement);
            let probes: Vec<Probe> = target
                .rows
                .iter()
                .map(|row| Probe {
                    existing: row.existing,
                    resulting: row.resulting.as_ref(),
                    columns: columns.clone(),
                })
                .collect();
            let openfga =
                openfga_answers(&engines.model, table, caller, action(write.shape), &probes);
            for (row, openfga_answer) in target.rows.iter().zip(openfga) {
                observations.push((
                    RecordedAnswer {
                        caller: caller.name.clone(),
                        row: format!("{}:{}", table.name, row.key),
                        statement: target.statement.clone(),
                        postgres: written(&postgres, &row.key),
                    },
                    openfga_answer,
                ));
            }
        }
    }
    Ok(observations)
}

/// `OpenFGA`'s answer for each row one statement reaches, one probe per row. PostgreSQL
/// aborts the whole statement when a row it reaches fails a check on the row it would
/// produce, so such a row denies every row of the statement.
fn openfga_answers(
    model: &Prepared<Model>,
    table: &Table,
    caller: &Caller,
    action: ActionStatement,
    probes: &[Probe],
) -> Vec<Answer> {
    let model = match model {
        Prepared::Refused(reason) => return vec![Answer::Refused(reason.clone()); probes.len()],
        Prepared::Ready(model) => model,
    };
    let verdicts: Vec<Result<Verdict>> = probes
        .iter()
        .map(|probe| model.permits(table, caller, action, probe))
        .collect();
    let aborted = verdicts
        .iter()
        .any(|verdict| matches!(verdict, Ok(Verdict::Aborted)));
    verdicts
        .into_iter()
        .map(|verdict| match verdict {
            Ok(Verdict::Changed) if !aborted => Answer::Changed,
            Ok(_) => Answer::Denied,
            Err(error) => Answer::Error(format!("{error:#}")),
        })
        .collect()
}

/// The `OpenFGA` translator's schema verdict.
fn acceptance(case: &Case, model: &Prepared<Model>) -> Acceptance {
    Acceptance {
        case: case.name.clone(),
        sqlite: None,
        openfga: match model {
            Prepared::Ready(model) => model.divergence().map(ToOwned::to_owned),
            Prepared::Refused(reason) => Some(reason.clone()),
        },
    }
}

/// One write statement a caller runs and the rows it answers for.
struct Target<'a> {
    statement: String,
    rows: Vec<TargetRow<'a>>,
}

struct TargetRow<'a> {
    key: String,
    /// The seeded row the statement reaches, absent for an insert.
    existing: Option<&'a Value>,
    /// The row the table owner's run of the statement produces, absent for a delete.
    resulting: Option<Value>,
}

/// Spell `write` on `table` per seeded row, or once for the table when it names no row.
fn targets<'a>(
    table: &Table,
    write: &Write,
    rows: &'a [Value],
    database: &mut Database,
) -> Result<Vec<Target<'a>>> {
    let key_of = |row: &Value| table.row_key(|column| row.get(column));
    match write.shape {
        Shape::Insert => {
            let key = table.row_key(|column| write.row.get(column))?;
            Ok(vec![Target {
                statement: table.write_statement(write, None),
                rows: vec![TargetRow {
                    key,
                    existing: None,
                    resulting: database.resulting(&table.resulting_statement(write, None))?,
                }],
            }])
        }
        Shape::UpdateWithoutWhere => {
            let mut target_rows = Vec::new();
            for row in rows {
                target_rows.push(TargetRow {
                    key: key_of(row)?,
                    existing: Some(row),
                    resulting: database.resulting(&table.resulting_statement(write, Some(row)))?,
                });
            }
            Ok(vec![Target {
                statement: table.write_statement(write, None),
                rows: target_rows,
            }])
        }
        _ => rows
            .iter()
            .map(|row| {
                let key = key_of(row)?;
                let resulting = if write.shape == Shape::Delete {
                    None
                } else {
                    database.resulting(&table.resulting_statement(write, Some(row)))?
                };
                Ok(Target {
                    statement: table.write_statement(write, Some(row)),
                    rows: vec![TargetRow {
                        key,
                        existing: Some(row),
                        resulting,
                    }],
                })
            })
            .collect(),
    }
}

/// Preserve an execution error independently of a write the policies refused.
fn written(outcome: &Result<Outcome>, key: &str) -> Answer {
    match outcome {
        Ok(outcome) => outcome.answer(key),
        Err(error) => Answer::Error(format!("{error:#}")),
    }
}

/// Preserve an execution error independently of a policy denying visibility.
fn visibility(rows: &Result<BTreeSet<String>>, key: &str) -> Answer {
    match rows {
        Ok(rows) if rows.contains(key) => Answer::Visible,
        Ok(_) => Answer::Hidden,
        Err(error) => Answer::Error(format!("{error:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Failed reads retain their cause instead of becoming hidden rows.
    #[test]
    fn failed_read_is_an_error() {
        assert_eq!(
            visibility(&Err(anyhow::anyhow!("denied")), "1"),
            Answer::Error("denied".into())
        );
    }
}
