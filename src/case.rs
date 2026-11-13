use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sql_traits::prelude::{Deployment, SessionSetting, SetPacking, SettingValue};
use sqlparser::ast::DataType;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Caller {
    pub name: String,
    pub role: String,
    pub settings: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub table: String,
    pub key: String,
    pub columns: Vec<String>,
    pub callers: Vec<Caller>,
}

pub(crate) struct Case {
    pub name: String,
    pub schema: String,
    pub seed: String,
    pub config: Config,
}

impl Case {
    /// Read one self-contained case directory.
    pub fn read(path: &Path) -> Result<Self> {
        let config: Config = toml::from_str(&fs::read_to_string(path.join("callers.toml"))?)?;
        ensure!(
            !config.callers.is_empty(),
            "No callers in {}",
            path.display()
        );
        ensure!(
            config.columns.contains(&config.key),
            "Projection must include the row key"
        );
        for caller in &config.callers {
            ensure!(
                caller.role == config.callers[0].role,
                "A case uses one acting role"
            );
            ensure!(
                caller.settings.len() == 2
                    && caller.settings.contains_key("app.user_id")
                    && caller.settings.contains_key("app.subjects"),
                "Declare both session settings"
            );
        }
        Ok(Self {
            name: path
                .file_name()
                .context("Case directory has no name")?
                .to_str()
                .context("Case name is not UTF-8")?
                .to_owned(),
            schema: fs::read_to_string(path.join("schema.sql"))?,
            seed: fs::read_to_string(path.join("seed.sql"))?,
            config,
        })
    }

    /// Declare connetto's caller and packed subject inputs.
    pub fn deployment(&self) -> Result<Deployment> {
        let role = &self.config.callers[0].role;
        Ok(Deployment::new(
            role,
            [role.as_str()],
            [
                SessionSetting::value("app.user_id", SettingValue::Caller(DataType::Text))?,
                SessionSetting::value(
                    "app.subjects",
                    SettingValue::Set {
                        element: DataType::Text,
                        packing: SetPacking::Delimited(",".into()),
                    },
                )?,
            ],
        )?)
    }

    /// Build the read whose visibility is measured.
    pub fn statement(&self) -> String {
        format!(
            "SELECT {} FROM {}",
            self.config
                .columns
                .iter()
                .map(|column| identifier(column))
                .collect::<Vec<_>>()
                .join(", "),
            identifier(&self.config.table)
        )
    }
}

/// Quote an SQL identifier supplied by a case.
pub(crate) fn identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Invalid case declarations fail before contacting an engine.
    #[test]
    fn invalid_case_configuration() -> Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        let directory = tempfile::tempdir_in(root)?;
        let valid = include_str!("../cases/grant_without_update/callers.toml");
        for (config, expected) in [
            (
                "table='docs'\nkey='id'\ncolumns=['id']\ncallers=[]".to_owned(),
                "No callers",
            ),
            (
                valid.replace("columns = [\"id\"]", "columns = []"),
                "Projection must include",
            ),
            (
                valid.replacen("app_reader", "other_reader", 1),
                "one acting role",
            ),
            (
                valid.replace(", \"app.subjects\" = \"\"", ""),
                "both session settings",
            ),
            (
                valid.replace("\"app.subjects\"", "\"app.other\""),
                "both session settings",
            ),
            (format!("unknown = true\n{valid}"), "unknown field"),
        ] {
            fs::write(directory.path().join("callers.toml"), config)?;
            let error = Case::read(directory.path())
                .err()
                .context("Expected invalid configuration")?;
            assert!(error.to_string().contains(expected), "{error}");
        }
        Ok(())
    }

    /// SQL identifiers escape embedded quotes in the projection and table.
    #[test]
    fn quoted_projection() -> Result<()> {
        let mut case =
            Case::read(&Path::new(env!("CARGO_MANIFEST_DIR")).join("cases/grant_without_update"))?;
        case.config.table = "odd\"table".into();
        case.config.columns = vec!["id".into(), "odd\"column".into()];
        assert_eq!(
            case.statement(),
            "SELECT \"id\", \"odd\"\"column\" FROM \"odd\"\"table\""
        );
        Ok(())
    }
}
