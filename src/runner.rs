use std::{
    borrow::Cow,
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    Answer, Known, Observation, Prepared,
    case::{Case, identifier},
    openfga::Model,
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
    let shapes: Vec<_> = case.shapes().collect();
    for shape in shapes {
        let statement = case.statement(shape);
        for (caller_index, caller) in case.config.callers.iter().enumerate() {
            let postgres = database.visible(&case, caller, &statement);
            // The outer error is a refusal and the inner one an execution failure.
            let sqlite = match &mut replica {
                Prepared::Ready(replica) => {
                    match replica.visible(&case, caller_index, &statement) {
                        Ok(Prepared::Ready(keys)) => Ok(Ok(keys)),
                        Ok(Prepared::Refused(reason)) => Err(reason),
                        Err(error) => Ok(Err(error)),
                    }
                }
                Prepared::Refused(reason) => Err(reason.clone()),
            };
            for row in &rows {
                let key = row.get(&case.config.key).context("Seeded row has no key")?;
                let key = match key {
                    Value::String(key) => Cow::Borrowed(key.as_str()),
                    other => Cow::Owned(other.to_string()),
                };
                let sqlite_answer = match &sqlite {
                    Err(reason) => Answer::Refused(reason.clone()),
                    Ok(answer) => visibility(answer, &key),
                };
                let openfga_answer = match &model {
                    Prepared::Refused(reason) => Answer::Refused(reason.clone()),
                    Prepared::Ready(model) => {
                        match model.visible(&case, caller, row, shape.action()) {
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
    if let Prepared::Ready(mut model) = model {
        model.cleanup()?;
    }
    database.cleanup()?;
    Ok(observations)
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
