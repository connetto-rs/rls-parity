use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde_json::Value;

use crate::{
    Acceptance, Answer, KnownList, Measurements, Observation, Outcome, Prepared,
    case::{Caller, Case, Shape, Table, Write, identifier, key_text},
    openfga::{Model, Probe},
    postgres::Database,
    sqlite::Replica,
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
        let (acceptance, observations) =
            run_case(&directory, database_url, openfga_url).map_err(|source| RunError::Case {
                path: directory,
                source,
            })?;
        measurements.acceptances.push(acceptance);
        measurements.observations.extend(observations);
    }
    Ok(measurements)
}

/// Measure whether each translator accepts the schema, then every caller and seeded row
/// of every table, before evaluating the known list.
fn run_case(
    path: &Path,
    database_url: &str,
    openfga_url: &str,
) -> Result<(Acceptance, Vec<Observation>)> {
    let case = Case::read(path)?;
    let mut database =
        Database::create(database_url, &case).with_context(|| format!("Creating {}", case.name))?;
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
    let replicas = replicas(&case)?;
    let model = Model::create(openfga_url, &case, &mut database)
        .with_context(|| format!("Loading OpenFGA for {}", case.name))?;
    let acceptance = acceptance(&case, &replicas, &model);
    let mut engines = Engines {
        database,
        replicas,
        model,
    };
    let mut observations = Vec::new();
    for table in &case.config.tables {
        observations.extend(measure_reads(
            &case,
            table,
            &rows[&table.name],
            &mut engines,
        )?);
    }
    for write in case.writes() {
        let table = case.table(&write.table)?;
        observations.extend(measure_write(
            &case,
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

/// The three engines one case is measured on.
struct Engines {
    database: Database,
    replicas: BTreeMap<String, Prepared<Replica>>,
    model: Prepared<Model>,
}

/// Measure every read shape on `table` for every caller and seeded row.
fn measure_reads(
    case: &Case,
    table: &Table,
    rows: &[Value],
    engines: &mut Engines,
) -> Result<Vec<Observation>> {
    let mut observations = Vec::new();
    let read_columns = table.read_columns();
    for shape in case.reads(table) {
        let statement = table.statement(shape);
        for (caller_index, caller) in case.config.callers.iter().enumerate() {
            let postgres = engines.database.visible(table, caller, &statement);
            let sqlite = on_replica(role_replica(&mut engines.replicas, caller)?, |replica| {
                replica.visible(table, caller_index, &statement)
            });
            for row in rows {
                let key = key_text(row.get(&table.key).context("Seeded row has no key")?);
                let sqlite_answer = match &sqlite {
                    Err(reason) => Answer::Refused(reason.clone()),
                    Ok(answer) => visibility(answer, &key),
                };
                let probe = Probe {
                    existing: Some(row),
                    resulting: None,
                    columns: read_columns.clone(),
                };
                let openfga_answer = match &engines.model {
                    Prepared::Refused(reason) => Answer::Refused(reason.clone()),
                    Prepared::Ready(model) => {
                        match model.permits(table, caller, shape.action(), &probe) {
                            Ok(true) => Answer::Visible,
                            Ok(false) => Answer::Hidden,
                            Err(error) => Answer::Error(format!("{error:#}")),
                        }
                    }
                };
                observations.push(Observation {
                    case: case.name.clone(),
                    caller: caller.name.clone(),
                    row: format!("{}:{key}", table.name),
                    statement: statement.clone(),
                    postgres: visibility(&postgres, &key),
                    sqlite: sqlite_answer,
                    openfga: openfga_answer,
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
    engines: &mut Engines,
) -> Result<Vec<Observation>> {
    let mut observations = Vec::new();
    let targets = targets(table, write, rows, &mut engines.database)?;
    let columns = write.columns(&table.key);
    for (caller_index, caller) in case.config.callers.iter().enumerate() {
        for target in &targets {
            let postgres = engines.database.write(table, caller, &target.statement);
            let sqlite = on_replica(role_replica(&mut engines.replicas, caller)?, |replica| {
                replica.write(table, caller_index, &target.statement)
            });
            for row in &target.rows {
                let sqlite_answer = match &sqlite {
                    Err(reason) => Answer::Refused(reason.clone()),
                    Ok(outcome) => written(outcome, &row.key),
                };
                let probe = Probe {
                    existing: row.existing,
                    resulting: row.resulting.as_ref(),
                    columns: columns.clone(),
                };
                let openfga_answer = match &engines.model {
                    Prepared::Refused(reason) => Answer::Refused(reason.clone()),
                    Prepared::Ready(model) => {
                        match model.permits(table, caller, write.shape.action(), &probe) {
                            Ok(true) => Answer::Changed,
                            Ok(false) => Answer::Denied,
                            Err(error) => Answer::Error(format!("{error:#}")),
                        }
                    }
                };
                observations.push(Observation {
                    case: case.name.clone(),
                    caller: caller.name.clone(),
                    row: format!("{}:{}", table.name, row.key),
                    statement: target.statement.clone(),
                    postgres: written(&postgres, &row.key),
                    sqlite: sqlite_answer,
                    openfga: openfga_answer,
                });
            }
        }
    }
    Ok(observations)
}

/// Each translator's verdict on the case's schema, the way connetto gates it. `pg2sqlite`
/// refuses the schema when it refuses any acting role's translation.
fn acceptance(
    case: &Case,
    replicas: &BTreeMap<String, Prepared<Replica>>,
    model: &Prepared<Model>,
) -> Acceptance {
    let refusals: Vec<String> = replicas
        .iter()
        .filter_map(|(role, replica)| match replica {
            Prepared::Ready(_) => None,
            Prepared::Refused(reason) => Some(format!("{role}: {reason}")),
        })
        .collect();
    Acceptance {
        case: case.name.clone(),
        sqlite: (!refusals.is_empty()).then(|| refusals.join("\n")),
        openfga: match model {
            Prepared::Ready(model) => model.divergence().map(ToOwned::to_owned),
            Prepared::Refused(reason) => Some(reason.clone()),
        },
    }
}

/// The replica `pg2sqlite` translated for the caller's acting role.
fn role_replica<'replicas>(
    replicas: &'replicas mut BTreeMap<String, Prepared<Replica>>,
    caller: &Caller,
) -> Result<&'replicas mut Prepared<Replica>> {
    replicas
        .get_mut(&caller.role)
        .context("No replica for the caller's acting role")
}

/// Translate one replica per acting role the callers name.
fn replicas(case: &Case) -> Result<BTreeMap<String, Prepared<Replica>>> {
    case.acting_roles()
        .into_iter()
        .map(|role| {
            let replica = Replica::create(case, role)
                .with_context(|| format!("Loading SQLite for {} acting as {role}", case.name))?;
            Ok((role.to_owned(), replica))
        })
        .collect()
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
    let key_of = |row: &'a Value| row.get(&table.key).context("Seeded row has no key");
    match write.shape {
        Shape::Insert => {
            let key = write
                .row
                .get(&table.key)
                .context("Inserted row has no key")?;
            Ok(vec![Target {
                statement: table.write_statement(write, None),
                rows: vec![TargetRow {
                    key: key_text(key),
                    existing: None,
                    resulting: database.resulting(&table.resulting_statement(write, None))?,
                }],
            }])
        }
        Shape::UpdateWithoutWhere => {
            let mut target_rows = Vec::new();
            for row in rows {
                let key = key_of(row)?;
                target_rows.push(TargetRow {
                    key: key_text(key),
                    existing: Some(row),
                    resulting: database.resulting(&table.resulting_statement(write, Some(key)))?,
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
                    database.resulting(&table.resulting_statement(write, Some(key)))?
                };
                Ok(Target {
                    statement: table.write_statement(write, Some(key)),
                    rows: vec![TargetRow {
                        key: key_text(key),
                        existing: Some(row),
                        resulting,
                    }],
                })
            })
            .collect(),
    }
}

/// Run `measure` on a loaded replica, `Err` holding why pg2sqlite refused the schema or
/// the statement and the inner error an execution failure.
fn on_replica<T>(
    replica: &mut Prepared<Replica>,
    measure: impl FnOnce(&mut Replica) -> Result<Prepared<T>>,
) -> std::result::Result<Result<T>, String> {
    match replica {
        Prepared::Ready(replica) => match measure(replica) {
            Ok(Prepared::Ready(value)) => Ok(Ok(value)),
            Ok(Prepared::Refused(reason)) => Err(reason),
            Err(error) => Ok(Err(error)),
        },
        Prepared::Refused(reason) => Err(reason.clone()),
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
