use std::{
    collections::BTreeSet,
    io::Write,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use diesel::{
    connection::SimpleConnection,
    prelude::*,
    sql_types::{Bool, Jsonb, Text},
};
use serde_json::Value;
use sqlparser::{ast::Statement, dialect::PostgreSqlDialect, parser::Parser};

use crate::case::{Caller, Case, identifier};

#[derive(QueryableByName)]
pub(crate) struct JsonRow {
    #[diesel(sql_type = Jsonb)]
    pub row: Value,
}

#[derive(QueryableByName)]
pub(crate) struct KeyRow {
    #[diesel(sql_type = Text)]
    pub key: String,
}

diesel::table! {
    pg_roles (rolname) {
        rolname -> Text,
    }
}

#[diesel::declare_sql_function]
extern "SQL" {
    fn current_setting(name: Text) -> Text;
    fn set_config(name: Text, value: Text, local: Bool) -> Text;
}

pub(crate) struct Database {
    admin: PgConnection,
    pub connection: PgConnection,
    cleanup_sql: Vec<String>,
}

impl Database {
    /// Create an isolated database and apply its source fixtures.
    pub fn create(url: &str, case: &Case) -> Result<Self> {
        let mut database_url = reqwest::Url::parse(url)?;
        database_url
            .query_pairs_mut()
            .append_pair("connect_timeout", "10");
        let mut admin = PgConnection::establish(database_url.as_str())?;
        admin.batch_execute("SET statement_timeout = '30s'; SET lock_timeout = '5s'")?;
        let version: String =
            diesel::select(current_setting("server_version_num")).get_result(&mut admin)?;
        ensure!(
            version.parse::<u32>()? / 10_000 == 18,
            "PostgreSQL 18 is required"
        );
        let mut roles = Vec::new();
        for statement in Parser::parse_sql(&PostgreSqlDialect {}, &case.schema)? {
            if let Statement::CreateRole(role) = statement {
                roles.extend(
                    role.names
                        .into_iter()
                        .map(|name| name.to_string().trim_matches('"').to_owned()),
                );
            }
        }
        let occupied = pg_roles::table
            .filter(pg_roles::rolname.eq_any(&roles))
            .count()
            .get_result::<i64>(&mut admin)?;
        ensure!(occupied == 0, "Case roles already exist in the cluster");
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let name = format!("rlspar_{}_{nonce}", std::process::id());
        admin.batch_execute(&format!("CREATE DATABASE {}", identifier(&name)))?;
        database_url.set_path(&format!("/{name}"));
        let connection = match PgConnection::establish(database_url.as_str()) {
            Ok(connection) => connection,
            Err(error) => {
                admin
                    .batch_execute(&format!("DROP DATABASE {} WITH (FORCE)", identifier(&name)))?;
                return Err(error.into());
            }
        };
        let mut cleanup_sql = vec![format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            identifier(&name)
        )];
        cleanup_sql.extend(
            roles
                .iter()
                .map(|role| format!("DROP ROLE IF EXISTS {}", identifier(role))),
        );
        let mut database = Self {
            admin,
            connection,
            cleanup_sql,
        };
        database.connection.batch_execute("SET statement_timeout = '30s'; SET lock_timeout = '5s'; SET TIME ZONE 'UTC'; SET DateStyle = 'ISO, MDY'; SET bytea_output = 'hex'")?;
        database
            .connection
            .batch_execute(&case.schema)
            .context("Applying schema.sql")?;
        database
            .connection
            .batch_execute(&case.seed)
            .context("Applying seed.sql")?;
        Ok(database)
    }

    /// Read runtime-declared table rows as the owner without policy filtering.
    pub fn rows(&mut self, table: &str) -> Result<Vec<Value>> {
        // The case supplies the table and its columns at runtime.
        Ok(
            diesel::sql_query(format!("SELECT to_jsonb(t) AS row FROM {table} t"))
                .load::<JsonRow>(&mut self.connection)?
                .into_iter()
                .map(|row| row.row)
                .collect(),
        )
    }

    /// Run `statement` under the caller's role and settings and collect the row keys it returns.
    pub fn visible(
        &mut self,
        case: &Case,
        caller: &Caller,
        statement: &str,
    ) -> Result<BTreeSet<String>> {
        self.connection.batch_execute("BEGIN")?;
        let result = (|| {
            self.connection
                .batch_execute(&format!("SET LOCAL ROLE {}", identifier(&caller.role)))?;
            for (name, value) in &caller.settings {
                diesel::select(set_config(name, value, true))
                    .get_result::<String>(&mut self.connection)?;
            }
            // The case's statement and row key are runtime fixture data.
            let rows = diesel::sql_query(format!(
                "SELECT CAST({} AS TEXT) AS key FROM ({statement}) q",
                identifier(&case.config.key),
            ))
            .load::<KeyRow>(&mut self.connection)?;
            Ok(rows.into_iter().map(|row| row.key).collect())
        })();
        self.connection.batch_execute("ROLLBACK")?;
        result
    }

    /// Remove only the database and roles created by this case.
    pub fn cleanup(&mut self) -> Result<()> {
        for statement in &self.cleanup_sql {
            self.admin.batch_execute(statement)?;
        }
        self.cleanup_sql.clear();
        Ok(())
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            let _ = writeln!(std::io::stderr(), "Database cleanup failed {error:#}");
        }
    }
}
