use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use diesel::{
    connection::SimpleConnection,
    dsl::exists,
    prelude::*,
    result::Error as DieselError,
    sql_types::{Bool, Nullable, Oid, Text},
};
use serde::Deserialize;
use sql_traits::parity::{Caller, Table, identifier, text_of};

use crate::Outcome;

diesel::table! {
    pg_catalog.pg_class (oid) {
        oid -> Oid,
        relkind -> CChar,
    }
}

#[derive(QueryableByName)]
pub(crate) struct KeyRow {
    #[diesel(sql_type = Text)]
    pub key: String,
}

/// One stored row as its key and the version the transaction that last wrote it gave it.
#[derive(QueryableByName)]
struct StoredRow {
    #[diesel(sql_type = Text)]
    key: String,
    #[diesel(sql_type = Text)]
    version: String,
}

/// The keys whose row version differs between two snapshots, rows added or removed included.
fn changed(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> BTreeSet<String> {
    before
        .keys()
        .chain(after.keys())
        .filter(|key| before.get(*key) != after.get(*key))
        .cloned()
        .collect()
}

#[diesel::declare_sql_function]
extern "SQL" {
    fn set_config(name: Text, value: Text, local: Bool) -> Text;
    fn to_regclass(name: Text) -> Nullable<Oid>;
}

/// `EXPLAIN (FORMAT JSON)` of an `UPDATE`, holding its one plan.
#[derive(Deserialize)]
struct ExplainedUpdate {
    #[serde(rename = "Plan")]
    plan: ModifyTable,
}

/// The plan node writing the relation an `UPDATE` lands on.
#[derive(Deserialize)]
struct ModifyTable {
    #[serde(rename = "Schema")]
    schema: String,
    #[serde(rename = "Relation Name")]
    relation: String,
    #[serde(rename = "Alias")]
    alias: String,
    /// The plan producing each updated row, its new values first.
    #[serde(rename = "Plans")]
    plans: [Subplan; 1],
}

/// A plan node with the expressions it outputs, deparsed.
#[derive(Deserialize)]
struct Subplan {
    #[serde(rename = "Output")]
    output: Vec<String>,
}

/// One row of `EXPLAIN` output.
#[derive(QueryableByName)]
struct Explained {
    #[diesel(sql_type = Text, column_name = "QUERY PLAN")]
    plan: String,
}

/// Run a caller's read and collect the returned row keys.
pub(crate) fn visible(
    connection: &mut PgConnection,
    table: &Table,
    caller: &Caller,
    statement: &str,
) -> Result<BTreeSet<String>> {
    connection.batch_execute("BEGIN")?;
    let result = (|| {
        act_as(connection, caller)?;
        // The case supplies its statement and row key at runtime.
        let rows = diesel::sql_query(format!(
            "SELECT {} AS key FROM ({statement}) q",
            table.key_sql(|key| text_of("q", key)),
        ))
        .load::<KeyRow>(connection)?;
        Ok(rows.into_iter().map(|row| row.key).collect())
    })();
    connection.batch_execute("ROLLBACK")?;
    result
}

/// Run a caller's write and measure changed row versions before rolling it back.
pub(crate) fn write(
    connection: &mut PgConnection,
    table: &Table,
    caller: &Caller,
    statement: &str,
) -> Result<Outcome> {
    connection.batch_execute("BEGIN")?;
    let result = (|| {
        let versions = versions_query(connection, table)?;
        let before = stored(connection, &versions)?;
        act_as(connection, caller)?;
        // The case supplies its write statement at runtime.
        if let Err(error) = diesel::sql_query(statement).execute(connection) {
            if let DieselError::DatabaseError(_, information) = &error
                && let Some(sqlstate) = information.sqlstate()
            {
                return Ok(Outcome::Rejected(sqlstate.to_string()));
            }
            return Err(error.into());
        }
        connection.batch_execute("RESET ROLE")?;
        Ok(Outcome::Applied(changed(
            &before,
            &stored(connection, &versions)?,
        )))
    })();
    connection.batch_execute("ROLLBACK")?;
    result
}

/// The query reading each row version of the relation a write on `table` lands on, named under
/// `table`'s key.
///
/// A view has no row versions, so the rows read are those of the table PostgreSQL rewrites a
/// write through it onto, rows the write moves out of the view included. PostgreSQL's own plan
/// of `UPDATE view SET column = column` names that table and the expression over it each key
/// column writes.
fn versions_query(connection: &mut PgConnection, table: &Table) -> Result<String> {
    let name = identifier(&table.name);
    let view = diesel::select(exists(
        pg_class::table
            .filter(pg_class::oid.nullable().eq(to_regclass(&name)))
            .filter(pg_class::relkind.eq(b'v')),
    ))
    .get_result::<bool>(connection)?;
    if !view {
        return Ok(format!(
            "SELECT {} AS key, CAST(t.xmin AS TEXT) AS version FROM {name} t",
            table.key_sql(|key| text_of("t", key)),
        ));
    }
    let mut written = None;
    let mut expressions = BTreeMap::new();
    for key in &table.key {
        let column = identifier(key);
        // The typed DSL cannot spell `EXPLAIN`, and the case supplies the view and key at runtime.
        let explained = diesel::sql_query(format!(
            "EXPLAIN (VERBOSE, COSTS OFF, FORMAT JSON) UPDATE {name} SET {column} = {column}"
        ))
        .get_result::<Explained>(connection)
        .with_context(|| format!("Resolving what key column {key} of view {name} writes"))?;
        let [ExplainedUpdate { plan }] =
            serde_json::from_str::<[ExplainedUpdate; 1]>(&explained.plan)?;
        let [Subplan { output }] = plan.plans;
        let expression = output
            .into_iter()
            .next()
            .context("The plan of a write through a view outputs no value")?;
        expressions.insert(key.as_str(), expression);
        written = Some((plan.schema, plan.relation, plan.alias));
    }
    let (schema, relation, alias) = written.context("A measured view declares no key")?;
    let alias = identifier(&alias);
    Ok(format!(
        "SELECT {} AS key, CAST({alias}.xmin AS TEXT) AS version FROM {}.{} {alias}",
        table.key_sql(|key| format!("CAST(({}) AS TEXT)", expressions[key])),
        identifier(&schema),
        identifier(&relation),
    ))
}

/// Read each stored row's transaction version under its declared key with `versions`.
fn stored(connection: &mut PgConnection, versions: &str) -> Result<BTreeMap<String, String>> {
    // The case supplies the relation and row key at runtime.
    Ok(diesel::sql_query(versions)
        .load::<StoredRow>(connection)?
        .into_iter()
        .map(|row| (row.key, row.version))
        .collect())
}

/// Take the caller's role and settings for the current transaction.
fn act_as(connection: &mut PgConnection, caller: &Caller) -> Result<()> {
    connection.batch_execute(&format!("SET LOCAL ROLE {}", identifier(&caller.role)))?;
    for (name, value) in &caller.settings {
        diesel::select(set_config(name, value, true)).get_result::<String>(connection)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connection holding temporary `resources` rows and two views over them, one renaming
    /// its key and one computing it.
    fn resources() -> Result<PgConnection> {
        let mut connection = PgConnection::establish(&std::env::var("DATABASE_URL")?)?;
        connection.batch_execute(
            "CREATE TEMP TABLE resources (id INTEGER PRIMARY KEY, code TEXT UNIQUE NOT NULL, parent_id INTEGER NOT NULL);
            CREATE TEMP VIEW resource_moves AS SELECT code AS resource, parent_id FROM resources WHERE parent_id > 0;
            CREATE TEMP VIEW resource_codes AS SELECT upper(code) AS resource, parent_id FROM resources;
            INSERT INTO resources VALUES (1, 'a', 1), (2, 'b', 1), (3, 'c', 0);",
        )?;
        Ok(connection)
    }

    /// The measured view keyed on `resource`, written by the table owner.
    fn probe(view: &str) -> (Table, Caller) {
        let table = Table {
            name: view.into(),
            key: vec!["resource".into()],
            columns: vec!["resource".into(), "parent_id".into()],
        };
        let caller = Caller {
            name: "owner".into(),
            role: "postgres".into(),
            settings: BTreeMap::new(),
        };
        (table, caller)
    }

    /// A write through a view renaming its key counts the row it moves out of the view as changed.
    #[test]
    fn view_write_counts_rows_moved_out() -> Result<()> {
        let mut connection = resources()?;
        let (table, caller) = probe("resource_moves");
        let outcome = write(
            &mut connection,
            &table,
            &caller,
            "UPDATE resource_moves SET parent_id = 0 WHERE resource = 'a'",
        )?;
        assert!(
            matches!(outcome, Outcome::Applied(keys) if keys == BTreeSet::from(["a".to_owned()]))
        );
        Ok(())
    }

    /// A view key no column of the written table holds fails the write instead of naming rows.
    #[test]
    fn computed_view_key_is_an_error() -> Result<()> {
        let mut connection = resources()?;
        let (table, caller) = probe("resource_codes");
        let error = write(
            &mut connection,
            &table,
            &caller,
            "UPDATE resource_codes SET parent_id = 0 WHERE resource = 'A'",
        )
        .err()
        .context("A computed view key named rows")?;
        let error = format!("{error:#}");
        assert!(
            error.contains(r#"Resolving what key column resource of view "resource_codes" writes"#),
            "{error}"
        );
        Ok(())
    }
}
