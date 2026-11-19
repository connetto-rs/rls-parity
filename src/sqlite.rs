use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use anyhow::{Context, Result, anyhow};
use diesel::{
    connection::SimpleConnection,
    prelude::*,
    result::Error as DieselError,
    sql_types::{Bool, Text},
};
use pg2sqlite::prelude::*;
use sql_traits::structs::ParserDB;
use sqlparser::ast::{Ident, ObjectName, Statement, TableObject};

use crate::{
    Outcome, Prepared,
    case::{Case, identifier},
    postgres::KeyRow,
};

struct Session {
    callers: Vec<(String, String)>,
    caller: AtomicUsize,
    exempt: AtomicBool,
}

#[diesel::declare_sql_function]
extern "SQL" {
    fn current_app_user() -> Text;
    fn current_subjects() -> Text;
    fn write_exempt() -> Bool;
}

pub(crate) struct Replica {
    connection: SqliteConnection,
    session: Arc<Session>,
    schema: ParserDB,
    options: Pg2SqliteOptions,
}

impl Replica {
    /// Load the script emitted for `role` and seed its published physical tables.
    pub fn create(case: &Case, role: &str) -> Result<Prepared<Self>> {
        let jsonb = JsonbFunctions::new("jsonb_canonical", "jsonb_order");
        let options = Pg2SqliteOptions::default()
            .with_deployment(case.deployment()?)
            .with_acting_role(role)
            .with_uuid_representation(UuidRepresentation::Blob)
            .with_uuid_function_name("uuid_blob")
            .with_rls_audit_table_name("rls_audit".to_owned())
            .with_write_exemption_function("write_exempt")
            .with_jsonb_functions(jsonb.clone())
            .with_session_variable(SessionVariableMapping::new(
                "app.user_id",
                "current_app_user",
            ))
            .with_session_variable(SessionVariableMapping::new(
                "app.subjects",
                "current_subjects",
            ));
        let translator = match Pg2Sqlite::default().sql(&case.schema) {
            Ok(translator) => translator,
            Err(error) => return Ok(Prepared::Refused(error.to_string())),
        };
        let script = match translator.translate(&options) {
            Ok(script) => script,
            Err(error) => return Ok(Prepared::Refused(error.to_string())),
        };
        let manifest = translator.translation_manifest(&options)?;
        let mut connection = SqliteConnection::establish(":memory:")?;
        jsonb.register(&mut connection)?;
        let session = Arc::new(Session {
            callers: case
                .config
                .callers
                .iter()
                .map(|caller| {
                    (
                        caller.settings["app.user_id"].clone(),
                        caller.settings["app.subjects"].clone(),
                    )
                })
                .collect(),
            caller: AtomicUsize::new(0),
            exempt: AtomicBool::new(false),
        });
        let user = Arc::clone(&session);
        current_app_user_utils::register_nondeterministic_impl(&mut connection, move || {
            user.callers[user.caller.load(Ordering::Relaxed)].0.clone()
        })?;
        let subjects = Arc::clone(&session);
        current_subjects_utils::register_nondeterministic_impl(&mut connection, move || {
            subjects.callers[subjects.caller.load(Ordering::Relaxed)]
                .1
                .clone()
        })?;
        let exempt = Arc::clone(&session);
        write_exempt_utils::register_nondeterministic_impl(&mut connection, move || {
            exempt.exempt.load(Ordering::Relaxed)
        })?;
        for statement in script.iter() {
            connection
                .batch_execute(&format!("{statement};"))
                .with_context(|| format!("Applying {statement}"))?;
        }
        let schema = translator.build_schema()?;
        let seed = Pg2Sqlite::default()
            .sql(&case.seed)?
            .translate_with_schema(&schema, &options)?;
        session.exempt.store(true, Ordering::Relaxed);
        for mut statement in seed.into_vec() {
            if let Statement::Insert(insert) = &mut statement {
                let TableObject::TableName(name) = &insert.table else {
                    return Err(anyhow!("Seed must insert into a named table"));
                };
                let logical = name.to_string();
                let Some(entry) = manifest
                    .iter()
                    .find(|entry| logical.trim_matches('"') == entry.logical)
                else {
                    continue;
                };
                insert.table = TableObject::TableName(ObjectName::from(vec![Ident::with_quote(
                    '"',
                    &entry.physical,
                )]));
            }
            // Seed statements describe runtime fixture schemas and retain translated value conversions.
            connection
                .batch_execute(&format!("{statement};"))
                .with_context(|| format!("Seeding {statement}"))?;
        }
        session.exempt.store(false, Ordering::Relaxed);
        // The physical table is known only once pg2sqlite has translated the case.
        let physical = manifest
            .iter()
            .find(|entry| entry.logical == case.config.table)
            .map(|entry| entry.physical.as_str());
        track_changes(&mut connection, physical, &case.config.key)?;
        Ok(Prepared::Ready(Self {
            connection,
            session,
            schema,
            options,
        }))
    }

    /// Run pg2sqlite's own translation of `statement` with the caller's session functions.
    pub fn visible(
        &mut self,
        case: &Case,
        caller: usize,
        statement: &str,
    ) -> Result<Prepared<BTreeSet<String>>> {
        let translated = match self.translate(caller, statement)? {
            Prepared::Ready(translated) => translated,
            Prepared::Refused(reason) => return Ok(Prepared::Refused(reason)),
        };
        let mut queries = translated
            .into_iter()
            .filter(|statement| matches!(statement, Statement::Query(_)));
        let query = queries
            .next()
            .filter(|_| queries.next().is_none())
            .with_context(|| format!("{statement} did not translate to one query"))?;
        // The case supplies the statement and row key at runtime.
        let rows = diesel::sql_query(format!(
            "SELECT CAST({} AS TEXT) AS key FROM ({query}) q",
            identifier(&case.config.key),
        ))
        .load::<KeyRow>(&mut self.connection)?;
        Ok(Prepared::Ready(
            rows.into_iter().map(|row| row.key).collect(),
        ))
    }

    /// Run pg2sqlite's translation of the write `statement` as the caller, rolled back.
    pub fn write(&mut self, caller: usize, statement: &str) -> Result<Prepared<Outcome>> {
        let translated = match self.translate(caller, statement)? {
            Prepared::Ready(translated) => translated,
            Prepared::Refused(reason) => return Ok(Prepared::Refused(reason)),
        };
        self.connection.batch_execute("SAVEPOINT probe")?;
        let result = (|| {
            for statement in &translated {
                // The case's write is runtime fixture data, as pg2sqlite translated it.
                if let Err(error) = self.connection.batch_execute(&format!("{statement};")) {
                    if let DieselError::DatabaseError(_, information) = &error
                        && let Some(refusal) = postgres_error(information.message())
                    {
                        return Ok(Outcome::Rejected(refusal.sqlstate().to_string()));
                    }
                    return Err(error.into());
                }
            }
            let changed = diesel::sql_query("SELECT key FROM probe_changes")
                .load::<KeyRow>(&mut self.connection)?;
            Ok(Outcome::Applied(
                changed.into_iter().map(|row| row.key).collect(),
            ))
        })();
        self.connection
            .batch_execute("ROLLBACK TO probe; RELEASE probe")?;
        result.map(Prepared::Ready)
    }

    /// Translate one statement against the replica's schema and select the caller.
    fn translate(&self, caller: usize, statement: &str) -> Result<Prepared<Vec<Statement>>> {
        anyhow::ensure!(
            caller < self.session.callers.len(),
            "Caller index is outside the case"
        );
        self.session.caller.store(caller, Ordering::Relaxed);
        Ok(
            match Pg2Sqlite::default()
                .sql(statement)
                .and_then(|source| source.translate_with_schema(&self.schema, &self.options))
            {
                Ok(script) => Prepared::Ready(script.into_vec()),
                Err(error) => Prepared::Refused(error.to_string()),
            },
        )
    }
}

/// Record the key of every row a statement writes to `physical` in `probe_changes`.
fn track_changes(
    connection: &mut SqliteConnection,
    physical: Option<&str>,
    key: &str,
) -> Result<()> {
    connection.batch_execute("CREATE TEMP TABLE probe_changes (key TEXT NOT NULL)")?;
    let Some(physical) = physical else {
        return Ok(());
    };
    let physical = identifier(physical);
    let old = format!("(CAST(OLD.{} AS TEXT))", identifier(key));
    let new = format!("(CAST(NEW.{} AS TEXT))", identifier(key));
    for (event, rows) in [
        ("INSERT", new.clone()),
        ("UPDATE", format!("{old}, {new}")),
        ("DELETE", old),
    ] {
        connection.batch_execute(&format!(
            "CREATE TEMP TRIGGER probe_{event} AFTER {event} ON main.{physical} \
             BEGIN INSERT INTO probe_changes VALUES {rows}; END"
        ))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::case::Shape;
    use std::path::Path;

    /// Registered session functions follow the caller and leave writes unexempted.
    #[test]
    fn caller_session_functions() -> Result<()> {
        let mut case =
            Case::read(&Path::new(env!("CARGO_MANIFEST_DIR")).join("cases/grant_without_update"))?;
        case.config.callers[0]
            .settings
            .insert("app.subjects".into(), "alpha,beta".into());
        let role = case.config.callers[0].role.clone();
        let Prepared::Ready(mut replica) = Replica::create(&case, &role)? else {
            panic!("ownership fixture must translate");
        };
        let select = case.statement(Shape::Select);
        for (index, caller) in case.config.callers.iter().enumerate() {
            replica.visible(&case, index, &select)?;
            let actual = diesel::select((current_app_user(), current_subjects(), write_exempt()))
                .get_result::<(String, String, bool)>(&mut replica.connection)?;
            assert_eq!(
                actual,
                (
                    caller.settings["app.user_id"].clone(),
                    caller.settings["app.subjects"].clone(),
                    false
                )
            );
        }
        let error = replica
            .visible(&case, case.config.callers.len(), &select)
            .expect_err("invalid caller");
        assert!(error.to_string().contains("outside the case"));
        assert!(matches!(
            replica.visible(&case, 0, "INVALID SQL")?,
            Prepared::Refused(_)
        ));
        let error = replica
            .visible(&case, 0, "DELETE FROM docs")
            .expect_err("a write is not a measured read");
        assert!(error.to_string().contains("one query"), "{error}");
        case.schema = "INVALID SQL".into();
        assert!(matches!(
            Replica::create(&case, &role)?,
            Prepared::Refused(_)
        ));
        Ok(())
    }
}
