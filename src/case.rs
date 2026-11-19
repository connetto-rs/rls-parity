use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result, ensure};
use rls2fga::types::{ActionStatement, ColumnCommand};
use serde::Deserialize;
use serde_json::Value;
use sql_traits::prelude::{Deployment, SessionSetting, SetPacking, SettingValue};
use sqlparser::ast::DataType;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Caller {
    pub name: String,
    pub role: String,
    pub settings: BTreeMap<String, String>,
}

/// A statement shape a case measures, each asked of `OpenFGA` as its own action.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Shape {
    Select,
    SelectForUpdate,
    Update,
    UpdateWithoutWhere,
    Delete,
    Insert,
}

impl Shape {
    /// The action rls2fga answers for this shape.
    pub fn action(self) -> ActionStatement {
        match self {
            Self::Select => ActionStatement::Select,
            Self::SelectForUpdate => ActionStatement::SelectForUpdate,
            Self::Update => ActionStatement::Update,
            Self::UpdateWithoutWhere => ActionStatement::UpdateWithoutWhere,
            Self::Delete => ActionStatement::Delete,
            Self::Insert => ActionStatement::Insert,
        }
    }

    /// Whether the shape reads rows without writing any.
    pub fn reads(self) -> bool {
        matches!(self, Self::Select | Self::SelectForUpdate)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Write {
    pub shape: Shape,
    /// Columns an update assigns, with the values it assigns.
    #[serde(default)]
    pub set: BTreeMap<String, Value>,
    /// The row an insert writes.
    #[serde(default)]
    pub row: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub table: String,
    pub key: String,
    pub columns: Vec<String>,
    pub callers: Vec<Caller>,
    #[serde(default)]
    pub writes: Vec<Write>,
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
                caller.settings.len() == 2
                    && caller.settings.contains_key("app.user_id")
                    && caller.settings.contains_key("app.subjects"),
                "Declare both session settings"
            );
        }
        for write in &config.writes {
            ensure!(
                write.shape != Shape::Select,
                "Every case already measures SELECT"
            );
            let needs_set = matches!(write.shape, Shape::Update | Shape::UpdateWithoutWhere);
            let needs_row = write.shape == Shape::Insert;
            ensure!(
                write.set.is_empty() != needs_set,
                "{:?} {} `set`",
                write.shape,
                if needs_set { "requires" } else { "takes no" }
            );
            ensure!(
                write.row.is_empty() != needs_row,
                "{:?} {} `row`",
                write.shape,
                if needs_row { "requires" } else { "takes no" }
            );
            ensure!(
                !needs_row || write.row.contains_key(&config.key),
                "An inserted row must carry the row key"
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

    /// Every acting role the callers name, in the order they first appear.
    pub fn acting_roles(&self) -> Vec<&str> {
        let mut roles: Vec<&str> = Vec::new();
        for caller in &self.config.callers {
            if !roles.contains(&caller.role.as_str()) {
                roles.push(&caller.role);
            }
        }
        roles
    }

    /// Declare connetto's caller and packed subject inputs, logging in as the first caller's
    /// role and acting as every role a caller names.
    pub fn deployment(&self) -> Result<Deployment> {
        Ok(Deployment::new(
            &self.config.callers[0].role,
            self.acting_roles(),
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

    /// Every read shape this case measures, `SELECT` first.
    pub fn shapes(&self) -> impl Iterator<Item = Shape> + '_ {
        std::iter::once(Shape::Select).chain(
            self.config
                .writes
                .iter()
                .map(|write| write.shape)
                .filter(|shape| shape.reads()),
        )
    }

    /// Every write probe this case measures.
    pub fn writes(&self) -> impl Iterator<Item = &Write> {
        self.config
            .writes
            .iter()
            .filter(|write| !write.shape.reads())
    }

    /// Build the read measured under a read shape.
    pub fn statement(&self, shape: Shape) -> String {
        let read = format!(
            "SELECT {} FROM {}",
            self.config
                .columns
                .iter()
                .map(|column| identifier(column))
                .collect::<Vec<_>>()
                .join(", "),
            identifier(&self.config.table)
        );
        if shape == Shape::SelectForUpdate {
            format!("{read} FOR UPDATE")
        } else {
            read
        }
    }

    /// Build `write` as a caller runs it, on the row with `key` or, without one, as declared.
    pub fn write_statement(&self, write: &Write, key: Option<&Value>) -> String {
        self.write_sql(write, key, false)
    }

    /// Build `write` returning the row it produces, which the table owner runs to learn it.
    pub fn resulting_statement(&self, write: &Write, key: Option<&Value>) -> String {
        self.write_sql(write, key, true)
    }

    fn write_sql(&self, write: &Write, key: Option<&Value>, returning: bool) -> String {
        let table = identifier(&self.config.table);
        let alias = if returning { " AS t" } else { "" };
        let returning = if returning {
            " RETURNING to_jsonb(t) AS row"
        } else {
            ""
        };
        let filter = key.map_or_else(String::new, |key| {
            format!(" WHERE {} = {}", identifier(&self.config.key), literal(key))
        });
        match write.shape {
            Shape::Insert => format!(
                "INSERT INTO {table}{alias} ({}) VALUES ({}){returning}",
                write
                    .row
                    .keys()
                    .map(|column| identifier(column))
                    .collect::<Vec<_>>()
                    .join(", "),
                write
                    .row
                    .values()
                    .map(literal)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Shape::Delete => format!("DELETE FROM {table}{alias}{filter}{returning}"),
            _ => format!(
                "UPDATE {table}{alias} SET {}{filter}{returning}",
                write
                    .set
                    .iter()
                    .map(|(column, value)| format!("{} = {}", identifier(column), literal(value)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// The columns a read touches under each column command.
    pub fn read_columns(&self) -> BTreeMap<ColumnCommand, Vec<String>> {
        BTreeMap::from([(ColumnCommand::Select, self.config.columns.clone())])
    }
}

impl Write {
    /// The columns this write touches under each column command, `key` being the row key
    /// a keyed `WHERE` reads.
    pub fn columns(&self, key: &str) -> BTreeMap<ColumnCommand, Vec<String>> {
        let read = match self.shape {
            Shape::Update | Shape::Delete => vec![key.to_owned()],
            _ => Vec::new(),
        };
        let (command, assigned) = match self.shape {
            Shape::Insert => (ColumnCommand::Insert, &self.row),
            _ => (ColumnCommand::Update, &self.set),
        };
        BTreeMap::from([
            (ColumnCommand::Select, read),
            (command, assigned.keys().cloned().collect()),
        ])
    }
}

/// The text a row key compares as, the way `CAST(key AS TEXT)` prints it.
pub(crate) fn key_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Spell a declared or seeded value as a PostgreSQL literal.
fn literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".into(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => format!("'{}'", text.replace('\'', "''")),
        other => format!("'{}'", other.to_string().replace('\'', "''")),
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
                valid.replace(", \"app.subjects\" = \"\"", ""),
                "both session settings",
            ),
            (
                valid.replace("\"app.subjects\"", "\"app.other\""),
                "both session settings",
            ),
            (format!("unknown = true\n{valid}"), "unknown field"),
            (
                format!("{valid}\n[[writes]]\nshape = \"select\"\n"),
                "already measures SELECT",
            ),
            (
                format!("{valid}\n[[writes]]\nshape = \"update\"\n"),
                "requires `set`",
            ),
            (
                format!("{valid}\n[[writes]]\nshape = \"delete\"\nset = {{ body = \"x\" }}\n"),
                "takes no `set`",
            ),
            (
                format!("{valid}\n[[writes]]\nshape = \"insert\"\n"),
                "requires `row`",
            ),
            (
                format!("{valid}\n[[writes]]\nshape = \"delete\"\nrow = {{ id = 9 }}\n"),
                "takes no `row`",
            ),
            (
                format!("{valid}\n[[writes]]\nshape = \"insert\"\nrow = {{ owner = \"alice\" }}\n"),
                "must carry the row key",
            ),
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
            case.statement(Shape::Select),
            "SELECT \"id\", \"odd\"\"column\" FROM \"odd\"\"table\""
        );
        Ok(())
    }

    /// Write values reach PostgreSQL as literals of their JSON type, quotes escaped.
    #[test]
    fn write_literals() -> Result<()> {
        let case =
            Case::read(&Path::new(env!("CARGO_MANIFEST_DIR")).join("cases/grant_without_update"))?;
        let write: Write = serde_json::from_value(serde_json::json!({
            "shape": "update",
            "set": { "a": null, "b": true, "c": "it's", "d": ["x'y"] },
        }))?;
        assert_eq!(
            case.write_statement(&write, Some(&"o'k".into())),
            "UPDATE \"docs\" SET \"a\" = NULL, \"b\" = true, \"c\" = 'it''s', \"d\" = '[\"x''y\"]' \
             WHERE \"id\" = 'o''k'"
        );
        Ok(())
    }
}
