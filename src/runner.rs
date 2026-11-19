use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    Answer, Known, Observation, Outcome, Prepared,
    case::{Case, Shape, Write, identifier, key_text},
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KnownFile {
    known: Vec<Known>,
}

/// Read the exact known-discrepancy list.
///
/// # Errors
/// Returns the path and cause when the file cannot be read or decoded.
pub fn read_known(path: &Path) -> std::result::Result<Vec<Known>, RunError> {
    let text = fs::read_to_string(path).map_err(|source| RunError::Io {
        path: path.into(),
        source,
    })?;
    let file: KnownFile = toml::from_str(&text).map_err(|source| RunError::InvalidKnown {
        path: path.into(),
        source,
    })?;
    Ok(file.known)
}

/// Collect live three-engine observations independently of the known list.
///
/// # Errors
/// Returns the case path and cause for invalid fixtures or engine setup failures.
pub fn observe_cases(
    root: &Path,
    database_url: &str,
    openfga_url: &str,
) -> std::result::Result<Vec<Observation>, RunError> {
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
    let mut observations = Vec::new();
    for directory in directories {
        observations.extend(
            run_case(&directory, database_url, openfga_url).map_err(|source| RunError::Case {
                path: directory,
                source,
            })?,
        );
    }
    Ok(observations)
}

/// Measure every caller and seeded row before evaluating the known list.
fn run_case(path: &Path, database_url: &str, openfga_url: &str) -> Result<Vec<Observation>> {
    let case = Case::read(path)?;
    let mut database =
        Database::create(database_url, &case).with_context(|| format!("Creating {}", case.name))?;
    let rows = database.rows(&identifier(&case.config.table))?;
    ensure!(!rows.is_empty(), "{} has no seeded rows", case.name);
    let mut replica =
        Replica::create(&case).with_context(|| format!("Loading SQLite for {}", case.name))?;
    let model = Model::create(openfga_url, &case, &mut database)
        .with_context(|| format!("Loading OpenFGA for {}", case.name))?;
    let mut observations = Vec::new();
    let read_columns = case.read_columns();
    let shapes: Vec<_> = case.shapes().collect();
    for shape in shapes {
        let statement = case.statement(shape);
        for (caller_index, caller) in case.config.callers.iter().enumerate() {
            let postgres = database.visible(&case, caller, &statement);
            let sqlite = on_replica(&mut replica, |replica| {
                replica.visible(&case, caller_index, &statement)
            });
            for row in &rows {
                let key = key_text(row.get(&case.config.key).context("Seeded row has no key")?);
                let sqlite_answer = match &sqlite {
                    Err(reason) => Answer::Refused(reason.clone()),
                    Ok(answer) => visibility(answer, &key),
                };
                let probe = Probe {
                    existing: Some(row),
                    resulting: None,
                    columns: read_columns.clone(),
                };
                let openfga_answer = match &model {
                    Prepared::Refused(reason) => Answer::Refused(reason.clone()),
                    Prepared::Ready(model) => {
                        match model.permits(&case, caller, shape.action(), &probe) {
                            Ok(true) => Answer::Visible,
                            Ok(false) => Answer::Hidden,
                            Err(error) => Answer::Error(format!("{error:#}")),
                        }
                    }
                };
                observations.push(Observation {
                    case: case.name.clone(),
                    caller: caller.name.clone(),
                    row: format!("{}:{key}", case.config.table),
                    statement: statement.clone(),
                    postgres: visibility(&postgres, &key),
                    sqlite: sqlite_answer,
                    openfga: openfga_answer,
                });
            }
        }
    }
    for write in case.writes() {
        let targets = targets(&case, write, &rows, &mut database)?;
        let columns = write.columns(&case.config.key);
        for (caller_index, caller) in case.config.callers.iter().enumerate() {
            for target in &targets {
                let postgres = database.write(&case, caller, &target.statement);
                let sqlite = on_replica(&mut replica, |replica| {
                    replica.write(caller_index, &target.statement)
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
                    let openfga_answer = match &model {
                        Prepared::Refused(reason) => Answer::Refused(reason.clone()),
                        Prepared::Ready(model) => {
                            match model.permits(&case, caller, write.shape.action(), &probe) {
                                Ok(true) => Answer::Changed,
                                Ok(false) => Answer::Denied,
                                Err(error) => Answer::Error(format!("{error:#}")),
                            }
                        }
                    };
                    observations.push(Observation {
                        case: case.name.clone(),
                        caller: caller.name.clone(),
                        row: format!("{}:{}", case.config.table, row.key),
                        statement: target.statement.clone(),
                        postgres: written(&postgres, &row.key),
                        sqlite: sqlite_answer,
                        openfga: openfga_answer,
                    });
                }
            }
        }
    }
    if let Prepared::Ready(mut model) = model {
        model.cleanup()?;
    }
    database.cleanup()?;
    Ok(observations)
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

/// Spell `write` per seeded row, or once for the table when it names no row.
fn targets<'a>(
    case: &Case,
    write: &Write,
    rows: &'a [Value],
    database: &mut Database,
) -> Result<Vec<Target<'a>>> {
    let key_of = |row: &'a Value| row.get(&case.config.key).context("Seeded row has no key");
    match write.shape {
        Shape::Insert => {
            let key = write
                .row
                .get(&case.config.key)
                .context("Inserted row has no key")?;
            Ok(vec![Target {
                statement: case.write_statement(write, None),
                rows: vec![TargetRow {
                    key: key_text(key),
                    existing: None,
                    resulting: database.resulting(&case.resulting_statement(write, None))?,
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
                    resulting: database.resulting(&case.resulting_statement(write, Some(key)))?,
                });
            }
            Ok(vec![Target {
                statement: case.write_statement(write, None),
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
                    database.resulting(&case.resulting_statement(write, Some(key)))?
                };
                Ok(Target {
                    statement: case.write_statement(write, Some(key)),
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
