use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use diesel::{
    connection::SimpleConnection,
    prelude::*,
    result::Error as DieselError,
    sql_types::{Bool, Text},
};
use sql_traits::parity::{Caller, Table, identifier, text_of};

use crate::Outcome;

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
        let before = stored(connection, table)?;
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
            &stored(connection, table)?,
        )))
    })();
    connection.batch_execute("ROLLBACK")?;
    result
}

/// Read each stored row's transaction version under its declared key.
fn stored(connection: &mut PgConnection, table: &Table) -> Result<BTreeMap<String, String>> {
    // The case supplies the table and row key at runtime.
    Ok(diesel::sql_query(format!(
        "SELECT {} AS key, CAST(t.xmin AS TEXT) AS version FROM {} t",
        table.key_sql(|key| text_of("t", key)),
        identifier(&table.name)
    ))
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
