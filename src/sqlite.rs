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
    sql_types::{Bool, Text},
};
use pg2sqlite::prelude::*;
use sqlparser::ast::{Ident, ObjectName, Statement, TableObject};

use crate::{
    Prepared,
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
}

impl Replica {
    /// Load the emitted script and seed its published physical tables.
    pub fn create(case: &Case) -> Result<Prepared<Self>> {
        let jsonb = JsonbFunctions::new("jsonb_canonical", "jsonb_order");
        let options = Pg2SqliteOptions::default()
            .with_deployment(case.deployment()?)
            .with_acting_role(&case.config.callers[0].role)
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
        Ok(Prepared::Ready(Self {
            connection,
            session,
        }))
    }

    /// Read the same projection with the caller's session functions.
    pub fn visible(&mut self, case: &Case, caller: usize) -> Result<BTreeSet<String>> {
        anyhow::ensure!(
            caller < self.session.callers.len(),
            "Caller index is outside the case"
        );
        self.session.caller.store(caller, Ordering::Relaxed);
        // The case supplies the projection and row key at runtime.
        let rows = diesel::sql_query(format!(
            "SELECT CAST({} AS TEXT) AS key FROM ({}) q",
            identifier(&case.config.key),
            case.statement()
        ))
        .load::<KeyRow>(&mut self.connection)?;
        Ok(rows.into_iter().map(|row| row.key).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Registered session functions follow the caller and leave writes unexempted.
    #[test]
    fn caller_session_functions() -> Result<()> {
        let mut case =
            Case::read(&Path::new(env!("CARGO_MANIFEST_DIR")).join("cases/grant_without_update"))?;
        case.config.callers[0]
            .settings
            .insert("app.subjects".into(), "alpha,beta".into());
        let Prepared::Ready(mut replica) = Replica::create(&case)? else {
            panic!("ownership fixture must translate");
        };
        for (index, caller) in case.config.callers.iter().enumerate() {
            replica.visible(&case, index)?;
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
            .visible(&case, case.config.callers.len())
            .expect_err("invalid caller");
        assert!(error.to_string().contains("outside the case"));
        case.schema = "INVALID SQL".into();
        assert!(matches!(Replica::create(&case)?, Prepared::Refused(_)));
        Ok(())
    }
}
