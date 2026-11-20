use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result, ensure};
use rls2fga::types::{ActionStatement, ColumnCommand};
use serde::Deserialize;
use serde_json::Value;
use sql_traits::prelude::{Deployment, SessionSetting, SetPacking, SettingValue};
use sqlparser::ast::DataType;

use crate::connetto;

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
    /// The declared table the write targets.
    pub table: String,
    pub shape: Shape,
    /// Columns an update assigns, with the values it assigns.
    #[serde(default)]
    pub set: BTreeMap<String, Value>,
    /// The row an insert writes.
    #[serde(default)]
    pub row: BTreeMap<String, Value>,
}

/// A measured table, the columns its row key spans and the columns a read projects.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Table {
    pub name: String,
    pub key: Vec<String>,
    pub columns: Vec<String>,
}

/// Where a case's schema, deployment and replica options come from.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Source {
    /// The case directory's own `schema.sql`, under rls-parity's deployment.
    #[default]
    Files,
    /// connetto's demo deployment, composed and translated the way connetto does.
    Connetto,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    #[serde(default)]
    pub source: Source,
    pub tables: Vec<Table>,
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
        ensure!(!config.tables.is_empty(), "No tables in {}", path.display());
        for (index, table) in config.tables.iter().enumerate() {
            ensure!(
                !table.key.is_empty() && table.key.iter().all(|key| table.columns.contains(key)),
                "Projection of {} must include the row key",
                table.name
            );
            ensure!(
                config.tables[..index]
                    .iter()
                    .all(|earlier| earlier.name != table.name),
                "Table {} is declared twice",
                table.name
            );
        }
        let deployment = config.deployment()?;
        let declared: BTreeSet<&str> = deployment
            .settings()
            .iter()
            .map(SessionSetting::name)
            .collect();
        for caller in &config.callers {
            ensure!(
                deployment.acting_roles().any(|role| role == caller.role),
                "Caller {} acts as {}, which the deployment does not",
                caller.name,
                caller.role
            );
            ensure!(
                caller
                    .settings
                    .keys()
                    .map(String::as_str)
                    .eq(declared.iter().copied()),
                "Declare every session setting the deployment names: {}",
                declared.iter().copied().collect::<Vec<_>>().join(", ")
            );
        }
        for write in &config.writes {
            let table = config
                .tables
                .iter()
                .find(|table| table.name == write.table)
                .with_context(|| format!("A write targets undeclared table {}", write.table))?;
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
                !needs_row || table.key.iter().all(|key| write.row.contains_key(key)),
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
            schema: match config.source {
                Source::Files => fs::read_to_string(path.join("schema.sql"))?,
                Source::Connetto => connetto::schema()?,
            },
            seed: fs::read_to_string(path.join("seed.sql"))?,
            config,
        })
    }

    /// The declared table named `name`.
    pub fn table(&self, name: &str) -> Result<&Table> {
        self.config
            .tables
            .iter()
            .find(|table| table.name == name)
            .with_context(|| format!("No declared table {name}"))
    }

    /// Every read shape this case measures on `table`, `SELECT` first.
    pub fn reads<'case>(&'case self, table: &'case Table) -> impl Iterator<Item = Shape> + 'case {
        std::iter::once(Shape::Select).chain(
            self.config
                .writes
                .iter()
                .filter(move |write| write.shape.reads() && write.table == table.name)
                .map(|write| write.shape),
        )
    }

    /// Every write probe this case measures.
    pub fn writes(&self) -> impl Iterator<Item = &Write> {
        self.config
            .writes
            .iter()
            .filter(|write| !write.shape.reads())
    }
}

impl Config {
    /// Every acting role the callers name, in the order they first appear.
    pub fn acting_roles(&self) -> Vec<&str> {
        let mut roles: Vec<&str> = Vec::new();
        for caller in &self.callers {
            if !roles.contains(&caller.role.as_str()) {
                roles.push(&caller.role);
            }
        }
        roles
    }

    /// The deployment the translators read. A files case declares connetto's caller and
    /// packed subject inputs, logging in as the first caller's role and acting as every role
    /// a caller names, and the connetto case takes connetto's own.
    pub fn deployment(&self) -> Result<Deployment> {
        match self.source {
            Source::Files => Deployment::new(
                &self.callers[0].role,
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
            )
            .map_err(Into::into),
            Source::Connetto => connetto::deployment(),
        }
    }
}

impl Table {
    /// Build the read measured under a read shape.
    pub fn statement(&self, shape: Shape) -> String {
        let read = format!(
            "SELECT {} FROM {}",
            self.columns
                .iter()
                .map(|column| identifier(column))
                .collect::<Vec<_>>()
                .join(", "),
            identifier(&self.name)
        );
        if shape == Shape::SelectForUpdate {
            format!("{read} FOR UPDATE")
        } else {
            read
        }
    }

    /// Build `write` as a caller runs it, on the key of `row` or, without one, as declared.
    pub fn write_statement(&self, write: &Write, row: Option<&Value>) -> String {
        self.write_sql(write, row, false)
    }

    /// Build `write` returning the row it produces, which the table owner runs to learn it.
    pub fn resulting_statement(&self, write: &Write, row: Option<&Value>) -> String {
        self.write_sql(write, row, true)
    }

    fn write_sql(&self, write: &Write, row: Option<&Value>, returning: bool) -> String {
        let table = identifier(&self.name);
        let alias = if returning { " AS t" } else { "" };
        let returning = if returning {
            " RETURNING to_jsonb(t) AS row"
        } else {
            ""
        };
        let filter = row.map_or_else(String::new, |row| {
            let matches: Vec<String> = self
                .key
                .iter()
                .map(|key| format!("{} = {}", identifier(key), literal(&row[key.as_str()])))
                .collect();
            format!(" WHERE {}", matches.join(" AND "))
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
        BTreeMap::from([(ColumnCommand::Select, self.columns.clone())])
    }

    /// The text a row's key compares as, each key column as `CAST(column AS TEXT)` prints
    /// it, joined by commas, `column` looking up the row's value of a column.
    pub fn row_key<'row>(&self, column: impl Fn(&str) -> Option<&'row Value>) -> Result<String> {
        let parts = self
            .key
            .iter()
            .map(|key| {
                column(key)
                    .map(key_text)
                    .with_context(|| format!("A row of {} has no key column {key}", self.name))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(parts.join(","))
    }

    /// The SQL expression spelling a row's key as [`Self::row_key`] does, `column` spelling
    /// one key column's text.
    pub fn key_sql(&self, column: impl Fn(&str) -> String) -> String {
        self.key
            .iter()
            .map(|key| column(key))
            .collect::<Vec<_>>()
            .join(" || ',' || ")
    }
}

impl Write {
    /// The columns this write touches under each column command, `key` being the row key
    /// columns a keyed `WHERE` reads.
    pub fn columns(&self, key: &[String]) -> BTreeMap<ColumnCommand, Vec<String>> {
        let read = match self.shape {
            Shape::Update | Shape::Delete => key.to_vec(),
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

/// The text a row key column compares as, the way `CAST(column AS TEXT)` prints it.
fn key_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// A column read through `qualifier` as text, the way PostgreSQL prints it.
pub(crate) fn text_of(qualifier: &str, column: &str) -> String {
    format!("CAST({qualifier}.{} AS TEXT)", identifier(column))
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
            ("tables=[]\ncallers=[]".to_owned(), "No callers"),
            (
                format!(
                    "tables = []\n{}",
                    &valid[valid.find("[[callers]]").unwrap_or(0)..]
                ),
                "No tables",
            ),
            (
                valid.replacen("[[tables]]", "[[unused]]", 1),
                "unknown field",
            ),
            (
                valid.replace("columns = [\"id\"]", "columns = []"),
                "Projection of docs must include",
            ),
            (
                format!(
                    "{valid}\n[[tables]]\nname = \"docs\"\nkey = [\"id\"]\ncolumns = [\"id\"]\n"
                ),
                "declared twice",
            ),
            (
                format!("{valid}\n[[writes]]\ntable = \"other\"\nshape = \"delete\"\n"),
                "undeclared table other",
            ),
            (
                valid.replace(", \"app.subjects\" = \"\"", ""),
                "every session setting",
            ),
            (
                valid.replace("\"app.subjects\"", "\"app.other\""),
                "every session setting",
            ),
            (format!("unknown = true\n{valid}"), "unknown field"),
            (
                format!("{valid}\n[[writes]]\ntable = \"docs\"\nshape = \"select\"\n"),
                "already measures SELECT",
            ),
            (
                format!("{valid}\n[[writes]]\ntable = \"docs\"\nshape = \"update\"\n"),
                "requires `set`",
            ),
            (
                format!(
                    "{valid}\n[[writes]]\ntable = \"docs\"\nshape = \"delete\"\nset = {{ body = \"x\" }}\n"
                ),
                "takes no `set`",
            ),
            (
                format!("{valid}\n[[writes]]\ntable = \"docs\"\nshape = \"insert\"\n"),
                "requires `row`",
            ),
            (
                format!(
                    "{valid}\n[[writes]]\ntable = \"docs\"\nshape = \"delete\"\nrow = {{ id = 9 }}\n"
                ),
                "takes no `row`",
            ),
            (
                format!(
                    "{valid}\n[[writes]]\ntable = \"docs\"\nshape = \"insert\"\nrow = {{ owner = \"alice\" }}\n"
                ),
                "must carry the row key",
            ),
            (
                include_str!("../cases/connetto_deployment/callers.toml").replacen(
                    "\"connetto_reader\"",
                    "\"intruder\"",
                    1,
                ),
                "Caller alice acts as intruder, which the deployment does not",
            ),
            (
                include_str!("../cases/connetto_deployment/callers.toml").replacen(
                    ", \"app.bot_list\" = \"*\"",
                    "",
                    1,
                ),
                "app.bot_list, app.subjects, app.user_id",
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
        let table = &mut case.config.tables[0];
        table.name = "odd\"table".into();
        table.columns = vec!["id".into(), "odd\"column".into()];
        assert_eq!(
            table.statement(Shape::Select),
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
            "table": "docs",
            "shape": "update",
            "set": { "a": null, "b": true, "c": "it's", "d": ["x'y"] },
        }))?;
        assert_eq!(
            case.table("docs")?
                .write_statement(&write, Some(&serde_json::json!({ "id": "o'k", "body": 1 }))),
            "UPDATE \"docs\" SET \"a\" = NULL, \"b\" = true, \"c\" = 'it''s', \"d\" = '[\"x''y\"]' \
             WHERE \"id\" = 'o''k'"
        );
        Ok(())
    }

    /// A composite key filters a write on every key column, names a row by all of them, and
    /// refuses a row missing one.
    #[test]
    fn composite_key() -> Result<()> {
        let case = Case::read(&Path::new(env!("CARGO_MANIFEST_DIR")).join("cases/composite_key"))?;
        let table = case.table("lines")?;
        let delete: Write = serde_json::from_value(serde_json::json!({
            "table": "lines",
            "shape": "delete",
        }))?;
        let row = serde_json::json!({ "order_id": 1, "line_no": "2", "owner": "bob" });
        assert_eq!(
            table.write_statement(&delete, Some(&row)),
            "DELETE FROM \"lines\" WHERE \"order_id\" = 1 AND \"line_no\" = '2'"
        );
        assert_eq!(table.row_key(|column| row.get(column))?, "1,2");
        assert_eq!(
            table.key_sql(|key| text_of("q", key)),
            "CAST(q.\"order_id\" AS TEXT) || ',' || CAST(q.\"line_no\" AS TEXT)"
        );
        let error = table
            .row_key(|column| row.get(column).filter(|_| column == "order_id"))
            .err()
            .context("A row without line_no has no key")?;
        assert!(
            error.to_string().contains("no key column line_no"),
            "{error}"
        );
        Ok(())
    }
}
