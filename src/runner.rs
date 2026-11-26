use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use anyhow::{Context, Result, ensure};
use rls2fga::parity::Database;
use serde_json::Value;
use sql_traits::parity::{
    Case, Engine, Fixture, Recorded, RecordedAnswer, Shape, Table, Write, fixtures, identifier,
};

use crate::{
    Acceptance, Answer, KnownList, Measurements, Observation, Outcome, case, connetto, postgres,
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
        case::read(&directory)
            .and_then(|case| {
                let fixture = Fixture {
                    known: case::known(&directory)?,
                    case,
                    recorded: Recorded::default(),
                };
                measure_fixture(fixture, false, database_url, openfga_url, &mut measurements)
            })
            .map_err(|source| RunError::Case {
                path: directory,
                source,
            })?;
    }
    Ok(measurements)
}

/// Measure embedded fixtures, local cases and saved failures under `root`.
///
/// # Errors
/// Returns the case and cause when loading or measuring fails.
pub fn observe_suite(root: &Path, database_url: &str, openfga_url: &str) -> Result<Measurements> {
    let mut measurements = Measurements::default();
    for fixture in fixtures() {
        measurements.extend(observe_fixture(fixture?, database_url, openfga_url)?);
    }
    measurements.extend(observe_cases(
        &root.join("cases"),
        database_url,
        openfga_url,
    )?);
    let failures = root.join("failures");
    if failures.exists() {
        measurements.extend(observe_cases(&failures, database_url, openfga_url)?);
    }
    Ok(measurements)
}

/// Measure and judge one fixture against live PostgreSQL.
///
/// # Errors
/// Returns the cause when engine setup or measurement fails.
pub fn observe_fixture(
    fixture: Fixture,
    database_url: &str,
    openfga_url: &str,
) -> Result<Measurements> {
    let mut measurements = Measurements::default();
    measure_fixture(fixture, true, database_url, openfga_url, &mut measurements)?;
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
    static RUNTIME: LazyLock<std::io::Result<tokio::runtime::Runtime>> = LazyLock::new(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
    });
    let case = &fixture.case;
    let mut database =
        Database::create(database_url, case).with_context(|| format!("Creating {}", case.name))?;
    let live = Recorded {
        answer: run_case(case, &mut database)?,
    };
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
    let deployment = if case.name == "connetto_deployment" {
        connetto::deployment()?
    } else {
        case.config.deployment()?
    };
    let runtime = RUNTIME
        .as_ref()
        .map_err(|error| anyhow::anyhow!("Creating the replay runtime failed {error}"))?;
    let openfga = runtime.block_on(rls2fga::parity::replay(
        case,
        &deployment,
        live.answer.iter().map(rls2fga::parity::Probe::from),
        &mut database,
        openfga_url,
    ))?;
    database.cleanup()?;
    fixture.recorded = live;
    for discrepancy in fixture
        .judge(Engine::Sqlite, replay.refusal.as_deref(), &replay.answers)
        .into_iter()
        .chain(fixture.judge(
            Engine::Openfga,
            openfga.refusal.as_deref(),
            &openfga.answers,
        ))
    {
        measurements.errors.push(discrepancy.to_string());
    }
    let acceptance = Acceptance {
        case: fixture.case.name.clone(),
        sqlite: replay.refusal,
        openfga: openfga.refusal,
    };
    for ((recorded, sqlite), openfga) in fixture
        .recorded
        .answer
        .into_iter()
        .zip(replay.answers)
        .zip(openfga.answers)
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

/// Measure each caller's statements against the live PostgreSQL oracle.
fn run_case(case: &Case, database: &mut Database) -> Result<Vec<RecordedAnswer>> {
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
    let mut observations = Vec::new();
    for table in &case.config.tables {
        observations.extend(measure_reads(case, table, &rows[&table.name], database)?);
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
            database,
        )?);
    }
    Ok(observations)
}

/// Measure every read shape on `table` for every caller and seeded row.
fn measure_reads(
    case: &Case,
    table: &Table,
    rows: &[Value],
    database: &mut Database,
) -> Result<Vec<RecordedAnswer>> {
    let mut observations = Vec::new();
    for shape in case.reads(table) {
        let statement = table.statement(shape);
        for caller in &case.config.callers {
            let postgres = postgres::visible(database.connection(), table, caller, &statement);
            for row in rows {
                let key = table.row_key(|column| row.get(column))?;
                observations.push(RecordedAnswer {
                    caller: caller.name.clone(),
                    row: format!("{}:{key}", table.name),
                    statement: statement.clone(),
                    postgres: visibility(&postgres, &key),
                });
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
    database: &mut Database,
) -> Result<Vec<RecordedAnswer>> {
    let mut observations = Vec::new();
    let targets = targets(table, write, rows)?;
    for caller in &case.config.callers {
        for target in &targets {
            let postgres = postgres::write(database.connection(), table, caller, &target.statement);
            for key in &target.keys {
                observations.push(RecordedAnswer {
                    caller: caller.name.clone(),
                    row: format!("{}:{key}", table.name),
                    statement: target.statement.clone(),
                    postgres: written(&postgres, key),
                });
            }
        }
    }
    Ok(observations)
}

/// One write statement a caller runs and the rows it answers for.
struct Target {
    statement: String,
    keys: Vec<String>,
}

/// Spell a write for each row it reaches or once for a whole-table statement.
fn targets(table: &Table, write: &Write, rows: &[Value]) -> Result<Vec<Target>> {
    let key_of = |row: &Value| table.row_key(|column| row.get(column));
    match write.shape {
        Shape::Insert => Ok(vec![Target {
            statement: table.write_statement(write, None),
            keys: vec![table.row_key(|column| write.row.get(column))?],
        }]),
        Shape::UpdateWithoutWhere => Ok(vec![Target {
            statement: table.write_statement(write, None),
            keys: rows.iter().map(key_of).collect::<Result<_, _>>()?,
        }]),
        _ => rows
            .iter()
            .map(|row| {
                Ok(Target {
                    statement: table.write_statement(write, Some(row)),
                    keys: vec![key_of(row)?],
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
