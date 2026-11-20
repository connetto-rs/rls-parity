use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

use anyhow::{Context, Result, anyhow};
use diesel::{
    connection::SimpleConnection,
    prelude::*,
    result::Error as DieselError,
    sql_types::{Binary, Bool, Nullable, Text},
};
use pg2sqlite::prelude::*;
use sql_traits::structs::ParserDB;
use sqlparser::ast::{Ident, ObjectName, Statement, TableObject};

use crate::{
    Outcome, Prepared,
    case::{Case, Source, Table, identifier, text_of},
    connetto,
    postgres::KeyRow,
};

/// The caller each replica function answers for, whether writes are exempt, and how many
/// UUIDs the replica has minted.
struct Session {
    callers: Vec<BTreeMap<String, String>>,
    caller: AtomicUsize,
    exempt: AtomicBool,
    minted: AtomicU64,
}

impl Session {
    /// The current caller's value of `setting`, `NULL` when the caller sets none, as
    /// `current_setting(setting, true)` reads it.
    fn setting(&self, setting: &str) -> Option<String> {
        self.callers[self.caller.load(Ordering::Relaxed)]
            .get(setting)
            .cloned()
    }

    /// A fresh version 4 UUID as sixteen bytes, numbered from a counter so every run
    /// mints the same sequence.
    fn mint_uuid(&self) -> Vec<u8> {
        let number = self.minted.fetch_add(1, Ordering::Relaxed) + 1;
        let mut bytes = [0; 16];
        bytes[8..].copy_from_slice(&number.to_be_bytes());
        bytes[6] = 0x40;
        bytes[8] |= 0x80;
        bytes.to_vec()
    }
}

// The names connetto's replica functions carry, which every case's options map to.
#[diesel::declare_sql_function]
extern "SQL" {
    fn current_app_user() -> Nullable<Text>;
    fn current_app_subjects() -> Nullable<Text>;
    fn current_bot_list() -> Nullable<Text>;
    fn connetto_write_exempt() -> Bool;
    fn uuidv4() -> Binary;
}

/// The options the replica of a files case translates under, before choosing its acting role.
fn files_options(case: &Case, jsonb: JsonbFunctions) -> Result<Pg2SqliteOptions> {
    Ok(Pg2SqliteOptions::default()
        .with_deployment(case.config.deployment()?)
        .with_uuid_representation(UuidRepresentation::Blob)
        .with_uuid_function_name("uuidv4")
        .with_rls_audit_table_name("rls_audit".to_owned())
        .with_write_exemption_function("connetto_write_exempt")
        .with_jsonb_functions(jsonb)
        .with_session_variable(SessionVariableMapping::new(
            "app.user_id",
            "current_app_user",
        ))
        .with_session_variable(SessionVariableMapping::new(
            "app.subjects",
            "current_app_subjects",
        )))
}

pub(crate) struct Replica {
    connection: SqliteConnection,
    session: Arc<Session>,
    schema: ParserDB,
    options: Pg2SqliteOptions,
    /// How the replica stores each published table, read to spell its keys as text.
    manifest: Vec<TableManifestEntry>,
}

impl Replica {
    /// Load the script emitted for `role` and seed its published physical tables as the
    /// table owner, as PostgreSQL loads the seed.
    pub fn create(case: &Case, role: &str) -> Result<Prepared<Self>> {
        let jsonb = JsonbFunctions::new("jsonb_canonical", "jsonb_text", "jsonb_order");
        let owner = match case.config.source {
            Source::Files => files_options(case, jsonb.clone())?,
            Source::Connetto => connetto::replica_options()?,
        };
        let options = owner.clone().with_acting_role(role);
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
                .map(|caller| caller.settings.clone())
                .collect(),
            caller: AtomicUsize::new(0),
            exempt: AtomicBool::new(false),
            minted: AtomicU64::new(0),
        });
        let user = Arc::clone(&session);
        current_app_user_utils::register_nondeterministic_impl(&mut connection, move || {
            user.setting("app.user_id")
        })?;
        let subjects = Arc::clone(&session);
        current_app_subjects_utils::register_nondeterministic_impl(&mut connection, move || {
            subjects.setting("app.subjects")
        })?;
        let bot_list = Arc::clone(&session);
        current_bot_list_utils::register_nondeterministic_impl(&mut connection, move || {
            bot_list.setting("app.bot_list")
        })?;
        let exempt = Arc::clone(&session);
        connetto_write_exempt_utils::register_nondeterministic_impl(&mut connection, move || {
            exempt.exempt.load(Ordering::Relaxed)
        })?;
        let minted = Arc::clone(&session);
        uuidv4_utils::register_nondeterministic_impl(&mut connection, move || minted.mint_uuid())?;
        for statement in script.iter() {
            connection
                .batch_execute(&format!("{statement};"))
                .with_context(|| format!("Applying {statement}"))?;
        }
        let schema = translator.build_schema()?;
        let seed = Pg2Sqlite::default()
            .sql(&case.seed)?
            .translate_with_schema(&schema, &owner)?;
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
        track_changes(&mut connection, &case.config.tables, &manifest)?;
        Ok(Prepared::Ready(Self {
            connection,
            session,
            schema,
            options,
            manifest,
        }))
    }

    /// Run pg2sqlite's own translation of `statement` with the caller's session functions,
    /// collecting the keys of the `table` rows it returns.
    pub fn visible(
        &mut self,
        table: &Table,
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
        let entry = self
            .manifest
            .iter()
            .find(|entry| entry.logical == table.name);
        // The case supplies the statement and row key at runtime.
        let rows = diesel::sql_query(format!(
            "SELECT {} AS key FROM ({query}) q",
            table.key_sql(|key| replica_text(entry, "q", key)),
        ))
        .load::<KeyRow>(&mut self.connection)?;
        Ok(Prepared::Ready(
            rows.into_iter().map(|row| row.key).collect(),
        ))
    }

    /// Run pg2sqlite's translation of the write `statement` as the caller, rolled back,
    /// collecting the keys of the rows of `table` it wrote.
    pub fn write(
        &mut self,
        table: &Table,
        caller: usize,
        statement: &str,
    ) -> Result<Prepared<Outcome>> {
        let translated = match self.translate(caller, statement)? {
            Prepared::Ready(translated) => translated,
            Prepared::Refused(reason) => return Ok(refused_write(reason)),
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
            let changed = diesel::sql_query("SELECT key FROM probe_changes WHERE logical = ?")
                .bind::<Text, _>(&table.name)
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

/// A write pg2sqlite refuses to translate, read as the SQLSTATE PostgreSQL raises when the
/// refusal is one of its errors.
fn refused_write(reason: String) -> Prepared<Outcome> {
    match postgres_error(&reason).map(|error| error.sqlstate().to_string()) {
        Some(sqlstate) => Prepared::Ready(Outcome::Rejected(sqlstate)),
        None => Prepared::Refused(reason),
    }
}

/// Record in `probe_changes` the logical table and key of every row a statement writes to
/// each of `tables` that the replica publishes.
fn track_changes(
    connection: &mut SqliteConnection,
    tables: &[Table],
    manifest: &[TableManifestEntry],
) -> Result<()> {
    connection.batch_execute(
        "CREATE TEMP TABLE probe_changes (logical TEXT NOT NULL, key TEXT NOT NULL)",
    )?;
    for (index, table) in tables.iter().enumerate() {
        // A table the translation omits has no physical counterpart to watch.
        let Some(entry) = manifest.iter().find(|entry| entry.logical == table.name) else {
            continue;
        };
        let logical = format!("'{}'", table.name.replace('\'', "''"));
        let physical = identifier(&entry.physical);
        let old = format!(
            "({logical}, {})",
            table.key_sql(|key| replica_text(Some(entry), "OLD", key))
        );
        let new = format!(
            "({logical}, {})",
            table.key_sql(|key| replica_text(Some(entry), "NEW", key))
        );
        for (event, rows) in [
            ("INSERT", new.clone()),
            ("UPDATE", format!("{old}, {new}")),
            ("DELETE", old),
        ] {
            connection.batch_execute(&format!(
                "CREATE TEMP TRIGGER probe_{index}_{event} AFTER {event} ON main.{physical} \
                 BEGIN INSERT INTO probe_changes VALUES {rows}; END"
            ))?;
        }
    }
    Ok(())
}

/// A replica column read through `qualifier` as the text PostgreSQL prints for it, a UUID
/// stored as sixteen bytes spelled in its canonical lowercase hyphenated form.
fn replica_text(entry: Option<&TableManifestEntry>, qualifier: &str, column: &str) -> String {
    let stored_as_blob = entry.is_some_and(|entry| {
        entry
            .columns
            .iter()
            .any(|stored| stored.name == column && stored.storage == ColumnStorage::UuidBlob)
    });
    if !stored_as_blob {
        return text_of(qualifier, column);
    }
    let hex = format!("hex({qualifier}.{})", identifier(column));
    let groups: Vec<String> = [(1, 8), (9, 4), (13, 4), (17, 4), (21, 12)]
        .iter()
        .map(|(start, length)| format!("substr({hex}, {start}, {length})"))
        .collect();
    format!("lower({})", groups.join(" || '-' || "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::case::Shape;
    use std::path::Path;

    /// Registered session functions follow the caller, read a setting the caller leaves
    /// unset as `NULL`, leave writes unexempted, and mint version 4 UUIDs numbered from one.
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
        let table = &case.config.tables[0];
        let select = table.statement(Shape::Select);
        for (index, caller) in case.config.callers.iter().enumerate() {
            replica.visible(table, index, &select)?;
            let actual = diesel::select((
                current_app_user(),
                current_app_subjects(),
                current_bot_list(),
                connetto_write_exempt(),
            ))
            .get_result::<(Option<String>, Option<String>, Option<String>, bool)>(
                &mut replica.connection,
            )?;
            assert_eq!(
                actual,
                (
                    Some(caller.settings["app.user_id"].clone()),
                    Some(caller.settings["app.subjects"].clone()),
                    None,
                    false
                )
            );
        }
        let error = replica
            .visible(table, case.config.callers.len(), &select)
            .expect_err("invalid caller");
        assert!(error.to_string().contains("outside the case"));
        assert!(matches!(
            replica.visible(table, 0, "INVALID SQL")?,
            Prepared::Refused(_)
        ));
        let error = replica
            .visible(table, 0, "DELETE FROM docs")
            .expect_err("a write is not a measured read");
        assert!(error.to_string().contains("one query"), "{error}");
        case.schema = "INVALID SQL".into();
        assert!(matches!(
            Replica::create(&case, &role)?,
            Prepared::Refused(_)
        ));
        let minted = diesel::select((uuidv4(), uuidv4()))
            .get_result::<(Vec<u8>, Vec<u8>)>(&mut replica.connection)?;
        let expected = |number: u8| {
            let mut bytes = vec![0; 16];
            bytes[6] = 0x40;
            bytes[8] = 0x80;
            bytes[15] = number;
            bytes
        };
        assert_eq!(minted, (expected(1), expected(2)));
        Ok(())
    }

    /// A write's translation refusal carrying a PostgreSQL error answers its SQLSTATE, and
    /// any other refusal stays a refusal.
    #[test]
    fn refused_write_answers_postgres_errors() {
        assert!(matches!(
            refused_write("permission denied for table docs".into()),
            Prepared::Ready(Outcome::Rejected(sqlstate)) if sqlstate == "42501"
        ));
        assert!(matches!(
            refused_write("Unsupported SQLite feature: write-only column".into()),
            Prepared::Refused(reason) if reason == "Unsupported SQLite feature: write-only column"
        ));
    }
}
