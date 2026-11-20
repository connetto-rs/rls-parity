use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use diesel::prelude::*;
use reqwest::{Method, blocking::Client};
use rls2fga::{
    generator::tuple_generator::{TupleCondition, TupleRow},
    parser::sql_parser::parse_schema,
    translator::{Outputs, TranslatorBuilder},
    types::{
        ActionAnswer, ActionStatement, ColumnCommand, ConditionParameterName, ConfidenceLevel,
        Record, RowVersion, records_from_row,
    },
};
use serde_json::{Map, Value, json};
use sql_traits::prelude::{SetPacking, SettingContent, SettingValue};

use crate::{
    Prepared,
    case::{Caller, Case, Table},
    postgres::{Database, JsonRow},
    rows::Row,
};

/// The rows and columns one check about one statement names.
pub(crate) struct Probe<'a> {
    /// The stored row the statement reaches, absent for an insert.
    pub existing: Option<&'a Value>,
    /// The row the statement produces, absent for a read or a delete.
    pub resulting: Option<&'a Value>,
    /// The columns the statement touches under each column command.
    pub columns: BTreeMap<ColumnCommand, Vec<String>>,
}

/// What `OpenFGA` answers for one row of a statement, split the way PostgreSQL acts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Every judgement holds, so the statement changes or reads the row.
    Changed,
    /// A judgement about the stored row fails, so the statement leaves the row alone,
    /// as a `USING` clause filters it.
    Skipped,
    /// The stored row passes but a judgement about the row the statement produces, or a
    /// column privilege, fails, so PostgreSQL aborts the whole statement.
    Aborted,
}

/// A delimited set setting and the condition parameter its values travel under.
struct SetParameter {
    setting: String,
    parameter: String,
    delimiter: String,
}

pub(crate) struct Model {
    client: Client,
    store_url: String,
    id: String,
    outputs: Outputs,
    /// Why connetto's gate, `Translation::outputs`, refuses the model, absent when it accepts.
    divergence: Option<String>,
    /// Every delimited set setting the deployment declares, sent split in each check.
    sets: Vec<SetParameter>,
    cleaned: bool,
}

impl Model {
    /// Translate the schema and load the model's own records into a fresh store.
    pub fn create(url: &str, case: &Case, database: &mut Database) -> Result<Prepared<Self>> {
        let parsed = match parse_schema(&case.schema) {
            Ok(parsed) => parsed,
            Err(error) => return Ok(Prepared::Refused(error.to_string())),
        };
        let deployment = case.config.deployment()?;
        let sets = deployment
            .settings()
            .iter()
            .filter_map(|setting| match setting.content() {
                SettingContent::Value(SettingValue::Set {
                    packing: SetPacking::Delimited(delimiter),
                    ..
                }) => Some((setting.name(), delimiter)),
                _ => None,
            })
            .map(|(setting, delimiter)| SetParameter {
                setting: setting.to_owned(),
                parameter: ConditionParameterName::derived(setting).into(),
                delimiter: delimiter.clone(),
            })
            .collect::<Vec<_>>();
        let translation = match TranslatorBuilder::new(deployment)
            .with_min_confidence(ConfidenceLevel::B)
            .build()
            .translate(&parsed)
        {
            Ok(translation) => translation,
            Err(error) => return Ok(Prepared::Refused(error.to_string())),
        };
        for note in translation.notes() {
            println!("{} OpenFGA note {note:?}", case.name);
        }
        let divergence = translation
            .clone()
            .outputs()
            .err()
            .map(|error| error.to_string());
        let outputs = translation.outputs_accepting_gaps();
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        let store = request(
            &client,
            Method::POST,
            &format!("{url}/stores"),
            &json!({"name":case.name}),
        )?;
        let store_id = store["id"].as_str().context("Store response has no id")?;
        let mut model = Self {
            client,
            store_url: format!("{url}/stores/{store_id}"),
            id: String::new(),
            outputs,
            divergence,
            sets,
            cleaned: false,
        };
        let created = request(
            &model.client,
            Method::POST,
            &format!("{}/authorization-models", model.store_url),
            &serde_json::to_value(model.outputs.json_model())?,
        )?;
        created["authorization_model_id"]
            .as_str()
            .context("Model response has no id")?
            .clone_into(&mut model.id);
        let records = load_records(&model.outputs, database)?;
        for chunk in records.chunks(100) {
            let tuples: Vec<Value> = chunk.iter().map(tuple).collect();
            model.post(
                "write",
                &json!({"authorization_model_id":model.id,"writes":{"tuple_keys":tuples}}),
            )?;
        }
        Ok(Prepared::Ready(model))
    }

    /// Why `Translation::outputs` refuses the model, absent when it accepts.
    pub fn divergence(&self) -> Option<&str> {
        self.divergence.as_deref()
    }

    /// Ask the emitted answer for `action` on `table` about the rows and columns `probe` names.
    pub fn permits(
        &self,
        table: &Table,
        caller: &Caller,
        action: ActionStatement,
        probe: &Probe,
    ) -> Result<Verdict> {
        let naming = self
            .outputs
            .translation()
            .row_naming()
            .iter()
            .find(|naming| naming.table.name() == table.name)
            .with_context(|| format!("Translation has no row naming for {}", table.name))?;
        let existing = match probe.existing {
            Some(row) => Some(
                naming
                    .render(&Row(row))?
                    .context("Row has no OpenFGA object name")?,
            ),
            None => None,
        };
        let entry = self
            .outputs
            .translation()
            .action_relations()
            .iter()
            .find(|entry| entry.type_name == naming.type_name && entry.statement == action)
            .context(format!("Translation has no {action:?} answer"))?;
        self.answer(&entry.answer, existing.as_deref(), probe, caller)
    }

    /// Interpret the translation's action answer without inferring relations. The stored
    /// row's judgements are asked first, since a row they filter is never checked further.
    fn answer(
        &self,
        answer: &ActionAnswer,
        existing: Option<&str>,
        probe: &Probe,
        caller: &Caller,
    ) -> Result<Verdict> {
        match answer {
            ActionAnswer::Unrestricted => Ok(Verdict::Changed),
            ActionAnswer::Denied => Ok(Verdict::Skipped),
            ActionAnswer::Judged(judgements) => {
                let (stored, produced): (Vec<_>, Vec<_>) = judgements
                    .iter()
                    .partition(|judgement| judgement.version == RowVersion::Existing);
                for judgement in stored {
                    let object = existing.context("The statement reaches no stored row")?;
                    if !self.check(judgement.relation.as_str(), object, caller, &[])? {
                        return Ok(Verdict::Skipped);
                    }
                }
                for judgement in produced {
                    let version = judgement.version;
                    ensure!(
                        version == RowVersion::Resulting,
                        "Unknown row version {version:?}"
                    );
                    let row = probe
                        .resulting
                        .context("The statement produces no resulting row")?;
                    let resulting = self
                        .outputs
                        .translation()
                        .resulting_rows()
                        .iter()
                        .find(|resulting| resulting.type_name == judgement.type_name)
                        .context("Translation names no resulting row for the judgement")?;
                    let object = resulting
                        .object(&Row(row))?
                        .context("Resulting row has no OpenFGA object name")?;
                    let records: Vec<Value> =
                        resulting.records(&Row(row))?.iter().map(tuple).collect();
                    if !self.check(judgement.relation.as_str(), &object, caller, &records)? {
                        return Ok(Verdict::Aborted);
                    }
                }
                Ok(Verdict::Changed)
            }
            ActionAnswer::ByColumn(columns) => {
                let rows = self.answer(&columns.rows, existing, probe, caller)?;
                if rows != Verdict::Changed {
                    return Ok(rows);
                }
                for command in &columns.commands {
                    let touched = probe.columns.get(&command.command).with_context(|| {
                        format!("The statement touches no {:?} columns", command.command)
                    })?;
                    for column in touched {
                        let column = rls2fga::types::ColumnName::from_stored(column);
                        if !self.check(
                            command.relation(&column).as_str(),
                            &columns.gate,
                            caller,
                            &[],
                        )? {
                            return Ok(Verdict::Aborted);
                        }
                    }
                }
                Ok(Verdict::Changed)
            }
            other => Err(anyhow!("Unsupported action answer {other:?}")),
        }
    }

    /// Carry the acting role and `records` as contextual tuples on the check.
    fn check(
        &self,
        relation: &str,
        object: &str,
        caller: &Caller,
        records: &[Value],
    ) -> Result<bool> {
        let user = format!("user:{}", caller.settings["app.user_id"]);
        let mut contextual = vec![
            json!({"user":user,"relation":"usage","object":format!("pg_role:{}",caller.role)}),
        ];
        contextual.extend_from_slice(records);
        let mut context = Map::new();
        context.insert("settings".into(), json!(caller.settings));
        for set in &self.sets {
            let values: Vec<&str> = caller.settings[&set.setting]
                .split(set.delimiter.as_str())
                .filter(|item| !item.is_empty())
                .collect();
            context.insert(set.parameter.clone(), json!(values));
        }
        let result = self.post(
            "check",
            &json!({
                "authorization_model_id":self.id,
                "tuple_key":{"user":user,"relation":relation,"object":object},
                "contextual_tuples":{"tuple_keys":contextual},
                "context":context,
                "consistency":"HIGHER_CONSISTENCY"
            }),
        )?;
        result["allowed"]
            .as_bool()
            .context("Check response has no allowed answer")
    }

    /// Send a bounded request to this store.
    fn post(&self, path: &str, body: &Value) -> Result<Value> {
        request(
            &self.client,
            Method::POST,
            &format!("{}/{path}", self.store_url),
            body,
        )
    }

    /// Delete the temporary store and surface cleanup failures.
    pub fn cleanup(&mut self) -> Result<()> {
        if !self.cleaned {
            self.client
                .delete(&self.store_url)
                .send()?
                .error_for_status()?;
            self.cleaned = true;
        }
        Ok(())
    }
}

impl Drop for Model {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            let _ = writeln!(std::io::stderr(), "OpenFGA cleanup failed {error:#}");
        }
    }
}

/// Send one bounded request and preserve any server refusal body.
fn request(client: &Client, method: Method, url: &str, body: &Value) -> Result<Value> {
    let response = client.request(method, url).json(body).send()?;
    let status = response.status();
    let text = response.text()?;
    if !status.is_success() {
        bail!("OpenFGA {url} returned {status} {text}");
    }
    Ok(serde_json::from_str(&text)?)
}

/// Load pure row descriptions and execute the remaining tuple queries.
fn load_records(outputs: &Outputs, database: &mut Database) -> Result<Vec<Record>> {
    let mut records = BTreeSet::new();
    let mut rows = BTreeMap::new();
    for query in outputs.tuple_queries() {
        if query.skipped.is_some() {
            continue;
        }
        if let Some(description) = &query.description
            && let Some(table) = description.row_table()
        {
            let table = table.sql_name();
            let table_rows = match rows.entry(table) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    let loaded = database.rows(entry.key())?;
                    entry.insert(loaded)
                }
            };
            for row in table_rows {
                records.extend(records_from_row(description, &Row(row))?);
            }
            continue;
        }
        // The translator supplies the tuple query and its runtime result shape.
        let tuples = diesel::sql_query(format!(
            "SELECT to_jsonb(t) AS row FROM ({}) t",
            query.sql.trim().trim_end_matches(';')
        ))
        .load::<JsonRow>(&mut database.connection)?;
        for JsonRow { row } in tuples {
            let context = row.get("context").map(Value::to_string);
            let condition = if query.condition.is_some() {
                Some(TupleCondition {
                    name: row["condition"]
                        .as_str()
                        .context("Missing tuple condition")?,
                    context: context.as_deref().context("Missing tuple context")?,
                })
            } else {
                None
            };
            records.insert(outputs.record_from_tuple_row(TupleRow {
                object: row["object"].as_str().context("Missing tuple object")?,
                relation: row["relation"].as_str().context("Missing tuple relation")?,
                subject: row["subject"].as_str().context("Missing tuple subject")?,
                condition,
            })?);
        }
    }
    Ok(records.into_iter().collect())
}

/// Serialize the record without changing its relation or subject.
fn tuple(record: &Record) -> Value {
    let mut tuple =
        json!({"object":record.object,"relation":record.relation.as_str(),"user":record.subject});
    if let Some(context) = &record.context {
        tuple["condition"] = json!({"name":context.condition.as_str(),"context":context.values});
    }
    tuple
}

#[cfg(test)]
mod tests {
    use super::*;
    use rls2fga::types::{ConditionName, RecordContextValue, RelationName, TypeName};

    /// Tuple serialization preserves emitted names and optional condition context.
    #[test]
    fn record_context_is_preserved() {
        let mut record = Record {
            object: "doc:1".into(),
            relation: RelationName::canonicalized("owner"),
            subject: "user:alice".into(),
            context: None,
        };
        assert_eq!(
            tuple(&record),
            json!({"object":"doc:1","relation":"owner","user":"user:alice"})
        );
        record.context = Some(RecordContextValue {
            condition: ConditionName::canonicalized("owner_matches"),
            values: BTreeMap::from([("owner".into(), "alice".into())]),
        });
        assert_eq!(
            tuple(&record),
            json!({
                "object":"doc:1","relation":"owner","user":"user:alice",
                "condition":{"name":"owner_matches","context":{"owner":"alice"}}
            })
        );
    }

    /// Live column checks enforce both row ownership and projection privileges.
    #[test]
    fn live_column_answers() -> Result<()> {
        let mut case = Case::read(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("cases/grant_without_update"),
        )?;
        case.schema = case
            .schema
            .replace("GRANT SELECT, INSERT", "GRANT SELECT (id), INSERT");
        let database_url = std::env::var("DATABASE_URL")?;
        let openfga_url = std::env::var("OPENFGA_URL")?;
        let mut database = Database::create(&database_url, &case)?;
        let Prepared::Ready(mut model) = Model::create(&openfga_url, &case, &mut database)? else {
            panic!("column-grant fixture must translate");
        };
        let row = json!({"id":1,"owner":"alice","body":"a"});
        let caller = &case.config.callers[0];
        let select = ActionStatement::Select;
        let read = |case: &Case| Probe {
            existing: Some(&row),
            resulting: None,
            columns: case.config.tables[0].read_columns(),
        };
        let table = &case.config.tables[0];
        assert_eq!(
            model.permits(table, caller, select, &read(&case))?,
            Verdict::Changed
        );
        assert_eq!(
            model.permits(table, &case.config.callers[1], select, &read(&case))?,
            Verdict::Skipped
        );
        case.config.tables[0].columns.push("body".into());
        let probe = read(&case);
        assert_eq!(
            model.permits(&case.config.tables[0], caller, select, &probe)?,
            Verdict::Aborted
        );
        assert_eq!(
            model.answer(&ActionAnswer::Unrestricted, Some("docs:1"), &probe, caller)?,
            Verdict::Changed
        );
        assert_eq!(
            model.answer(&ActionAnswer::Denied, Some("docs:1"), &probe, caller)?,
            Verdict::Skipped
        );
        let resulting = ActionAnswer::Judged(vec![rls2fga::types::ActionJudgement::new(
            RelationName::canonicalized("owner"),
            RowVersion::Resulting,
            TypeName::canonicalized("docs"),
        )]);
        let error = model
            .answer(&resulting, Some("docs:1"), &probe, caller)
            .expect_err("a read produces no resulting row");
        assert!(error.to_string().contains("no resulting row"), "{error}");
        let existing = ActionAnswer::Judged(vec![rls2fga::types::ActionJudgement::new(
            RelationName::canonicalized("owner"),
            RowVersion::Existing,
            TypeName::canonicalized("docs"),
        )]);
        let error = model
            .answer(&existing, None, &probe, caller)
            .expect_err("an insert reaches no stored row");
        assert!(error.to_string().contains("no stored row"), "{error}");
        let mut answer = model
            .outputs
            .translation()
            .action_relations()
            .iter()
            .find(|entry| entry.statement == ActionStatement::Select)
            .context("SELECT action")?
            .answer
            .clone();
        let ActionAnswer::ByColumn(columns) = &mut answer else {
            panic!("column grant must emit column checks");
        };
        columns
            .commands
            .first_mut()
            .context("column command")?
            .command = ColumnCommand::Insert;
        let error = model
            .answer(&answer, Some("docs:1"), &probe, caller)
            .expect_err("a read touches no inserted column");
        assert!(error.to_string().contains("Insert"), "{error}");
        let error = model
            .post("check", &json!({}))
            .expect_err("incomplete check");
        assert!(error.to_string().contains("400 Bad Request"), "{error}");
        model.cleanup()?;
        refused_and_keyless_models(&mut case, &mut database, &openfga_url)
    }

    /// Checks that a schema the parser rejects is refused, and that a table without a row
    /// key translates but cannot name the row a check asks about.
    ///
    /// Runs on the caller's database because case roles are cluster-wide.
    fn refused_and_keyless_models(
        case: &mut Case,
        database: &mut Database,
        openfga_url: &str,
    ) -> Result<()> {
        let schema = std::mem::replace(&mut case.schema, "INVALID SQL".into());
        ensure!(
            matches!(
                Model::create(openfga_url, case, database)?,
                Prepared::Refused(_)
            ),
            "an unparsable schema must be refused"
        );
        case.schema = schema.replace("PRIMARY KEY", "");
        let Prepared::Ready(model) = Model::create(openfga_url, case, database)? else {
            bail!("missing row key is an emitted gap");
        };
        let row = json!({"id":1,"owner":"alice","body":"a"});
        let probe = Probe {
            existing: Some(&row),
            resulting: None,
            columns: case.config.tables[0].read_columns(),
        };
        let table = &case.config.tables[0];
        let caller = &case.config.callers[0];
        ensure!(
            model
                .permits(table, caller, ActionStatement::Select, &probe)
                .is_err(),
            "a keyless table cannot name the checked row"
        );
        Ok(())
    }
}
