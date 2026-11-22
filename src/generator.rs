//! Random cases over the policy language both translators support, shrunk on failure.

use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use proptest::{
    array::uniform2,
    collection::vec,
    option,
    prelude::*,
    test_runner::{Config, RngAlgorithm, TestCaseError, TestError, TestRng, TestRunner},
};
use serde::Deserialize;

use crate::{KnownList, observe_cases};

/// A generated combination whose divergence `known.toml` tracks, one per known finding.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum Construct {
    /// A policy casts the `uuid` caller setting `app.user_uuid` to compare a `UUID` column.
    UuidSettingCast,
    /// A role reads a table through a column grant instead of a table grant.
    ColumnGrant,
    /// A role holds no `SELECT` on a table.
    GrantWithoutSelect,
    /// A role lacks `UPDATE` or `DELETE` on a table.
    GrantWithoutWrite,
    /// A `SELECT ... FOR UPDATE` probe, which applies the `UPDATE` policies.
    LockingReadUpdatePolicy,
    /// An `UPDATE` probe without `WHERE`.
    UpdateWithoutWhere,
    /// The membership table an `EXISTS` policy reads is under row level security.
    MembershipUnderRls,
    /// A policy extracts a JSON number or boolean as text.
    JsonbScalarExtraction,
    /// A policy tests a boolean column bare, where `= true` spells the same test.
    BareBooleanColumn,
    /// Acting roles differ in holding `INSERT`, so a policy can cover it only for a role
    /// without the privilege.
    PolicyForRoleWithoutPrivilege,
}

/// Distinct caller identities, spelled as text a `UUID` cast accepts.
const USERS: u8 = 4;
const TEAMS: [&str; 2] = ["red", "blue"];
const ROLES: [&str; 2] = ["gen_r0", "gen_r1"];

#[derive(Clone, Copy, Debug)]
enum ColumnType {
    Integer,
    Text,
    Uuid,
    Jsonb,
    Boolean,
    Timestamptz,
}

impl ColumnType {
    fn sql(self) -> &'static str {
        match self {
            Self::Integer => "INTEGER",
            Self::Text => "TEXT",
            Self::Uuid => "UUID",
            Self::Jsonb => "JSONB",
            Self::Boolean => "BOOLEAN",
            Self::Timestamptz => "TIMESTAMPTZ",
        }
    }
}

#[derive(Clone, Debug)]
enum Predicate {
    /// The row's owner is the caller.
    Owner,
    /// The row's team is one of the caller's subjects.
    Subjects,
    /// The caller is a member of the row's team.
    Membership,
    Flag,
    Constant(bool),
    /// A `UUID` column holds the caller, read from the `uuid` setting.
    UuidOwner,
    /// A JSON column's `owner` field is the caller.
    JsonOwner,
    And(Box<Predicate>, Box<Predicate>),
    Or(Box<Predicate>, Box<Predicate>),
}

#[derive(Clone, Copy, Debug)]
enum Command {
    All,
    Select,
    Insert,
    Update,
    Delete,
}

#[derive(Clone, Debug)]
struct Policy {
    restrictive: bool,
    command: Command,
    /// The role the policy applies to, `PUBLIC` when absent.
    to: Option<usize>,
    using: Predicate,
    check: Option<Predicate>,
}

#[derive(Clone, Copy, Debug)]
#[expect(clippy::struct_excessive_bools, reason = "one flag per privilege")]
struct Grant {
    select: bool,
    column_select: bool,
    insert: bool,
    update: bool,
    delete: bool,
}

#[derive(Clone, Debug)]
struct Row {
    owner: u8,
    team: usize,
    flag: bool,
    /// One raw value per extra column, spelled by the column's type.
    extras: [u8; 2],
}

#[derive(Clone, Debug)]
struct Table {
    extras: Vec<ColumnType>,
    rows: Vec<Row>,
    grants: [Grant; 2],
    policies: Vec<Policy>,
}

#[derive(Clone, Copy, Debug)]
enum WriteShape {
    Update,
    UpdateWithoutWhere,
    Delete,
    Insert,
    SelectForUpdate,
}

#[derive(Clone, Debug)]
struct Write {
    table: usize,
    shape: WriteShape,
    /// The owner an update assigns, or the flag it sets when absent.
    owner: Option<u8>,
    /// The row an insert writes.
    row: Row,
}

#[derive(Clone, Debug)]
struct Caller {
    role: usize,
    user: u8,
    subjects: [bool; 2],
}

/// One generated case before the disabled constructs are taken out of it.
#[derive(Clone, Debug)]
struct Draft {
    tables: Vec<Table>,
    /// Whether the first role is a member of the second.
    inherit: bool,
    callers: Vec<Caller>,
    members: Vec<(usize, u8)>,
    members_rls: bool,
    writes: Vec<Write>,
}

/// A case directory's three files.
struct Files {
    schema: String,
    seed: String,
    callers: String,
}

fn column_type() -> impl Strategy<Value = ColumnType> {
    prop_oneof![
        Just(ColumnType::Integer),
        Just(ColumnType::Text),
        Just(ColumnType::Uuid),
        Just(ColumnType::Jsonb),
        Just(ColumnType::Boolean),
        Just(ColumnType::Timestamptz),
    ]
}

fn predicate() -> impl Strategy<Value = Predicate> {
    let leaf = prop_oneof![
        Just(Predicate::Owner),
        Just(Predicate::Subjects),
        Just(Predicate::Membership),
        Just(Predicate::Flag),
        any::<bool>().prop_map(Predicate::Constant),
        Just(Predicate::UuidOwner),
        Just(Predicate::JsonOwner),
    ];
    leaf.prop_recursive(2, 6, 2, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone())
                .prop_map(|(left, right)| Predicate::And(Box::new(left), Box::new(right))),
            (inner.clone(), inner)
                .prop_map(|(left, right)| Predicate::Or(Box::new(left), Box::new(right))),
        ]
    })
}

fn command() -> impl Strategy<Value = Command> {
    prop_oneof![
        Just(Command::All),
        Just(Command::Select),
        Just(Command::Insert),
        Just(Command::Update),
        Just(Command::Delete),
    ]
}

fn policy() -> impl Strategy<Value = Policy> {
    (
        any::<bool>(),
        command(),
        option::of(0..ROLES.len()),
        predicate(),
        option::of(predicate()),
    )
        .prop_map(|(restrictive, command, to, using, check)| Policy {
            restrictive,
            command,
            to,
            using,
            check,
        })
}

fn grant() -> impl Strategy<Value = Grant> {
    uniform2(uniform2(any::<bool>())).prop_flat_map(
        |[[select, column_select], [insert, update]]| {
            any::<bool>().prop_map(move |delete| Grant {
                select,
                column_select,
                insert,
                update,
                delete,
            })
        },
    )
}

fn row() -> impl Strategy<Value = Row> {
    (
        0..USERS,
        0..TEAMS.len(),
        any::<bool>(),
        uniform2(any::<u8>()),
    )
        .prop_map(|(owner, team, flag, extras)| Row {
            owner,
            team,
            flag,
            extras,
        })
}

fn table() -> impl Strategy<Value = Table> {
    (
        vec(column_type(), 0..=2),
        vec(row(), 1..=3),
        uniform2(grant()),
        vec(policy(), 0..=3),
    )
        .prop_map(|(extras, rows, grants, policies)| Table {
            extras,
            rows,
            grants,
            policies,
        })
}

fn write() -> impl Strategy<Value = Write> {
    let shape = prop_oneof![
        Just(WriteShape::Update),
        Just(WriteShape::Delete),
        Just(WriteShape::Insert),
        Just(WriteShape::UpdateWithoutWhere),
        Just(WriteShape::SelectForUpdate),
    ];
    (0..3usize, shape, option::of(0..USERS), row()).prop_map(|(table, shape, owner, row)| Write {
        table,
        shape,
        owner,
        row,
    })
}

fn caller() -> impl Strategy<Value = Caller> {
    (0..ROLES.len(), 0..USERS, uniform2(any::<bool>())).prop_map(|(role, user, subjects)| Caller {
        role,
        user,
        subjects,
    })
}

fn draft() -> impl Strategy<Value = Draft> {
    (
        vec(table(), 1..=3),
        any::<bool>(),
        vec(caller(), 1..=3),
        vec((0..TEAMS.len(), 0..USERS), 0..=4),
        any::<bool>(),
        vec(write(), 0..=2),
    )
        .prop_map(
            |(tables, inherit, callers, members, members_rls, writes)| Draft {
                tables,
                inherit,
                callers,
                members,
                members_rls,
                writes,
            },
        )
}

/// The text identity of user `index`, which also parses as a `UUID`.
fn user(index: u8) -> String {
    format!(
        "00000000-0000-0000-0000-{:012x}",
        u32::from(index % USERS) + 1
    )
}

/// Quote a string as an SQL literal.
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

const CALLER: &str = "current_setting('app.user_id', true)";
const UUID_CALLER: &str = "current_setting('app.user_uuid', true)";

const SETTINGS: &str = "[[settings]]\nname = \"app.user_id\"\nvalue = { caller = \"text\" }\n\n\
    [[settings]]\nname = \"app.user_uuid\"\nvalue = { caller = \"uuid\" }\n\n\
    [[settings]]\nname = \"app.subjects\"\n\
    value = { set = { element = \"text\", packing = { delimited = \",\" } } }\n";

impl Draft {
    /// Spell the case without any construct `disabled` lists.
    fn render(&self, disabled: &BTreeSet<Construct>) -> Files {
        let enabled = |construct| !disabled.contains(&construct);
        let mut schema: Vec<String> = ROLES
            .iter()
            .map(|role| format!("CREATE ROLE {role} NOLOGIN;"))
            .collect();
        if self.inherit {
            schema.push(format!("GRANT {} TO {};", ROLES[1], ROLES[0]));
        }
        schema.push(
            "CREATE TABLE members (team TEXT NOT NULL, user_id TEXT NOT NULL, \
             PRIMARY KEY (team, user_id));"
                .into(),
        );
        schema.push(format!("GRANT SELECT ON members TO {};", ROLES.join(", ")));
        if self.members_rls && enabled(Construct::MembershipUnderRls) {
            schema.push("ALTER TABLE members ENABLE ROW LEVEL SECURITY;".into());
            schema.push(format!(
                "CREATE POLICY members_self ON members USING (user_id = {CALLER});"
            ));
        }
        let mut seed = Vec::new();
        let members: BTreeSet<_> = self.members.iter().collect();
        if !members.is_empty() {
            let values: Vec<_> = members
                .iter()
                .map(|(team, member)| {
                    format!("({}, {})", quoted(TEAMS[*team]), quoted(&user(*member)))
                })
                .collect();
            seed.push(format!(
                "INSERT INTO members (team, user_id) VALUES {};",
                values.join(", ")
            ));
        }
        let uuid_caller = enabled(Construct::UuidSettingCast);
        let mut callers = Vec::new();
        if uuid_caller {
            callers.push(SETTINGS.to_owned());
        }
        for (index, table) in self.tables.iter().enumerate() {
            let name = format!("t{index}");
            schema.extend(table.schema(&name, &enabled));
            let columns = table.columns();
            let rows: Vec<_> = table
                .rows
                .iter()
                .enumerate()
                .map(|(row_index, row)| {
                    let mut values = vec![
                        (row_index + 1).to_string(),
                        quoted(&user(row.owner)),
                        quoted(TEAMS[row.team]),
                        row.flag.to_string(),
                    ];
                    values.extend(
                        table
                            .extras
                            .iter()
                            .zip(row.extras)
                            .map(|(column, raw)| extra_value(*column, raw, &enabled)),
                    );
                    format!("({})", values.join(", "))
                })
                .collect();
            seed.push(format!(
                "INSERT INTO {name} ({}) VALUES {};",
                columns.join(", "),
                rows.join(", ")
            ));
            callers.push(format!(
                "[[tables]]\nname = \"{name}\"\nkey = [\"id\"]\ncolumns = [{}]\n",
                columns
                    .iter()
                    .map(|column| format!("\"{column}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        callers.extend(
            self.callers
                .iter()
                .enumerate()
                .map(|(index, caller)| caller.render(index, uuid_caller)),
        );
        callers.extend(
            self.writes
                .iter()
                .enumerate()
                .filter_map(|(index, write)| write.render(index, self.tables.len(), &enabled)),
        );
        Files {
            schema: schema.join("\n") + "\n",
            seed: seed.join("\n") + "\n",
            callers: callers.join("\n"),
        }
    }

    /// Write the case directory `directory` without any construct `disabled` lists.
    fn write(&self, directory: &Path, disabled: &BTreeSet<Construct>) -> Result<()> {
        let files = self.render(disabled);
        fs::create_dir_all(directory)?;
        fs::write(directory.join("schema.sql"), files.schema)?;
        fs::write(directory.join("seed.sql"), files.seed)?;
        fs::write(directory.join("callers.toml"), files.callers)?;
        Ok(())
    }
}

/// Spell the raw value of an extra column of `column` type.
fn extra_value(column: ColumnType, raw: u8, enabled: &impl Fn(Construct) -> bool) -> String {
    match column {
        ColumnType::Integer => raw.to_string(),
        ColumnType::Text => quoted(&format!("v{raw}")),
        ColumnType::Uuid => quoted(&user(raw)),
        ColumnType::Jsonb => {
            let owner = match raw % 3 {
                1 if enabled(Construct::JsonbScalarExtraction) => raw.to_string(),
                2 if enabled(Construct::JsonbScalarExtraction) => raw.is_multiple_of(2).to_string(),
                _ => format!("\"{}\"", user(raw)),
            };
            quoted(&format!("{{\"owner\": {owner}}}"))
        }
        ColumnType::Boolean => raw.is_multiple_of(2).to_string(),
        ColumnType::Timestamptz => quoted(&format!("2026-01-{:02}T00:00:00Z", 1 + raw % 28)),
    }
}

impl Table {
    fn columns(&self) -> Vec<String> {
        let mut columns: Vec<String> = ["id", "owner", "team", "flag"].map(String::from).into();
        columns.extend((0..self.extras.len()).map(|index| format!("c{index}")));
        columns
    }

    /// The table's definition, grants and policies.
    fn schema(&self, name: &str, enabled: &impl Fn(Construct) -> bool) -> Vec<String> {
        let columns: Vec<_> = [
            "id INTEGER PRIMARY KEY",
            "owner TEXT NOT NULL",
            "team TEXT NOT NULL",
            "flag BOOLEAN NOT NULL",
        ]
        .map(String::from)
        .into_iter()
        .chain(
            self.extras
                .iter()
                .enumerate()
                .map(|(index, column)| format!("c{index} {}", column.sql())),
        )
        .collect();
        let mut schema = vec![
            format!("CREATE TABLE {name} ({});", columns.join(", ")),
            format!("ALTER TABLE {name} ENABLE ROW LEVEL SECURITY;"),
        ];
        for (role, grant) in ROLES.iter().zip(self.grants) {
            let column_grant = enabled(Construct::ColumnGrant) && grant.column_select;
            let privileges: Vec<_> = [
                (
                    "SELECT",
                    !column_grant && (grant.select || !enabled(Construct::GrantWithoutSelect)),
                ),
                (
                    "INSERT",
                    if enabled(Construct::PolicyForRoleWithoutPrivilege) {
                        grant.insert
                    } else {
                        self.grants[0].insert
                    },
                ),
                (
                    "UPDATE",
                    grant.update || !enabled(Construct::GrantWithoutWrite),
                ),
                (
                    "DELETE",
                    grant.delete || !enabled(Construct::GrantWithoutWrite),
                ),
            ]
            .into_iter()
            .filter_map(|(privilege, held)| held.then_some(privilege))
            .collect();
            if !privileges.is_empty() {
                schema.push(format!(
                    "GRANT {} ON {name} TO {role};",
                    privileges.join(", ")
                ));
            }
            if column_grant {
                schema.push(format!("GRANT SELECT (id, owner) ON {name} TO {role};"));
            }
        }
        for (index, policy) in self.policies.iter().enumerate() {
            let using = self.predicate(name, &policy.using, enabled);
            let clauses = match (policy.command, &policy.check) {
                (Command::Insert, _) => format!("WITH CHECK ({using})"),
                (Command::Select | Command::Delete, _) | (_, None) => format!("USING ({using})"),
                (Command::All | Command::Update, Some(check)) => format!(
                    "USING ({using}) WITH CHECK ({})",
                    self.predicate(name, check, enabled)
                ),
            };
            schema.push(format!(
                "CREATE POLICY {name}_p{index} ON {name}{} FOR {} TO {} {clauses};",
                if policy.restrictive {
                    " AS RESTRICTIVE"
                } else {
                    ""
                },
                match policy.command {
                    Command::All => "ALL",
                    Command::Select => "SELECT",
                    Command::Insert => "INSERT",
                    Command::Update => "UPDATE",
                    Command::Delete => "DELETE",
                },
                policy.to.map_or("PUBLIC", |role| ROLES[role]),
            ));
        }
        schema
    }

    /// Spell `predicate` over this table, comparing the owner where the column it needs is
    /// absent or its construct disabled.
    fn predicate(
        &self,
        name: &str,
        predicate: &Predicate,
        enabled: &impl Fn(Construct) -> bool,
    ) -> String {
        let column = |wanted: fn(ColumnType) -> bool| {
            self.extras
                .iter()
                .position(|column| wanted(*column))
                .map(|index| format!("c{index}"))
        };
        let owner = format!("owner = {CALLER}");
        match predicate {
            Predicate::Owner => owner,
            Predicate::Subjects => {
                "team = ANY (string_to_array(current_setting('app.subjects', true), ','))".into()
            }
            Predicate::Membership => format!(
                "EXISTS (SELECT 1 FROM members m WHERE m.team = {name}.team AND m.user_id = {CALLER})"
            ),
            Predicate::Flag if enabled(Construct::BareBooleanColumn) => "flag".into(),
            Predicate::Flag => "flag = true".into(),
            Predicate::Constant(value) => value.to_string(),
            Predicate::UuidOwner => match column(|column| matches!(column, ColumnType::Uuid)) {
                Some(column) if enabled(Construct::UuidSettingCast) => {
                    format!("{column} = {UUID_CALLER}::uuid")
                }
                _ => owner,
            },
            Predicate::JsonOwner => match column(|column| matches!(column, ColumnType::Jsonb)) {
                Some(column) => format!("({column} ->> 'owner') = {CALLER}"),
                None => owner,
            },
            Predicate::And(left, right) => format!(
                "({}) AND ({})",
                self.predicate(name, left, enabled),
                self.predicate(name, right, enabled)
            ),
            Predicate::Or(left, right) => format!(
                "({}) OR ({})",
                self.predicate(name, left, enabled),
                self.predicate(name, right, enabled)
            ),
        }
    }
}

impl Caller {
    /// The `[[callers]]` entry, setting `app.user_uuid` when `uuid_caller` declares it.
    fn render(&self, index: usize, uuid_caller: bool) -> String {
        let subjects: Vec<_> = TEAMS
            .iter()
            .zip(self.subjects)
            .filter_map(|(team, held)| held.then_some(*team))
            .collect();
        let uuid_setting = if uuid_caller {
            format!("\"app.user_uuid\" = \"{}\", ", user(self.user))
        } else {
            String::new()
        };
        format!(
            "[[callers]]\nname = \"c{index}\"\nrole = \"{}\"\n\
             settings = {{ \"app.user_id\" = \"{}\", {uuid_setting}\"app.subjects\" = \"{}\" }}\n",
            ROLES[self.role],
            user(self.user),
            subjects.join(",")
        )
    }
}

impl Write {
    /// The `[[writes]]` entry, or `None` when its shape's construct is disabled.
    fn render(
        &self,
        index: usize,
        tables: usize,
        enabled: &impl Fn(Construct) -> bool,
    ) -> Option<String> {
        let (shape, values) = match self.shape {
            WriteShape::Update => ("update", Some(self.assignment())),
            WriteShape::UpdateWithoutWhere if enabled(Construct::UpdateWithoutWhere) => {
                ("update_without_where", Some(self.assignment()))
            }
            WriteShape::Delete => ("delete", None),
            WriteShape::Insert => (
                "insert",
                Some(format!(
                    "row = {{ id = {}, owner = \"{}\", team = \"{}\", flag = {} }}\n",
                    100 + index,
                    user(self.row.owner),
                    TEAMS[self.row.team],
                    self.row.flag
                )),
            ),
            WriteShape::SelectForUpdate if enabled(Construct::LockingReadUpdatePolicy) => {
                ("select_for_update", None)
            }
            WriteShape::UpdateWithoutWhere | WriteShape::SelectForUpdate => return None,
        };
        Some(format!(
            "[[writes]]\ntable = \"t{}\"\nshape = \"{shape}\"\n{}\n",
            self.table % tables,
            values.unwrap_or_default()
        ))
    }

    fn assignment(&self) -> String {
        match self.owner {
            Some(owner) => format!("set = {{ owner = \"{}\" }}\n", user(owner)),
            None => "set = { flag = true }\n".into(),
        }
    }
}

/// Measure one draft on the three engines, failing on any discrepancy.
fn check(
    draft: &Draft,
    disabled: &BTreeSet<Construct>,
    database_url: &str,
    openfga_url: &str,
) -> Result<(), TestCaseError> {
    let fail = |error: anyhow::Error| TestCaseError::fail(format!("{error:#}"));
    let directory = tempfile::tempdir().map_err(|error| fail(error.into()))?;
    draft
        .write(&directory.path().join("generated"), disabled)
        .map_err(fail)?;
    let measurements = observe_cases(directory.path(), database_url, openfga_url)
        .map_err(|error| fail(error.into()))?;
    let errors = measurements.errors;
    if errors.is_empty() {
        Ok(())
    } else {
        Err(TestCaseError::fail(errors.join("\n")))
    }
}

/// Generate cases from `seed` for `budget`, skipping every construct `known` lists, and
/// write each shrunk failure under `failures` as a case directory. Returns how many failed.
///
/// # Errors
/// Returns an error when a failure cannot be written or proptest cannot draw a case.
pub fn generate(
    seed: u64,
    budget: Duration,
    known: &KnownList,
    database_url: &str,
    openfga_url: &str,
    failures: &Path,
) -> Result<usize> {
    let disabled: BTreeSet<Construct> = known
        .construct
        .iter()
        .map(|entry| entry.construct)
        .collect();
    let started = Instant::now();
    let strategy = draft();
    let mut failed = 0;
    for iteration in 0u64.. {
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        // Each draw is reproducible from the seed and its iteration alone.
        let mut bytes = [0; 32];
        bytes[..8].copy_from_slice(&seed.to_le_bytes());
        bytes[8..16].copy_from_slice(&iteration.to_le_bytes());
        let config = Config {
            failure_persistence: None,
            // Zero would let shrinking run without bound.
            max_shrink_time: u32::try_from(remaining.as_millis())
                .unwrap_or(u32::MAX)
                .max(1),
            ..Config::default()
        };
        let mut runner =
            TestRunner::new_with_rng(config, TestRng::from_seed(RngAlgorithm::ChaCha, &bytes));
        let tree = strategy
            .new_tree(&mut runner)
            .map_err(|reason| anyhow!("Cannot draw a case: {reason}"))?;
        match runner.run_one(tree, |draft| {
            check(&draft, &disabled, database_url, openfga_url)
        }) {
            Ok(_) => {}
            Err(TestError::Fail(reason, draft)) => {
                let directory = failures.join(format!("seed{seed}-{iteration}"));
                draft.write(&directory, &disabled)?;
                eprintln!("{} {reason}", directory.display());
                failed += 1;
            }
            Err(TestError::Abort(reason)) => bail!("Generation aborted: {reason}"),
        }
    }
    Ok(failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::case;

    const EVERY: [Construct; 10] = [
        Construct::UuidSettingCast,
        Construct::ColumnGrant,
        Construct::GrantWithoutSelect,
        Construct::GrantWithoutWrite,
        Construct::LockingReadUpdatePolicy,
        Construct::UpdateWithoutWhere,
        Construct::MembershipUnderRls,
        Construct::JsonbScalarExtraction,
        Construct::BareBooleanColumn,
        Construct::PolicyForRoleWithoutPrivilege,
    ];

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Every draft spells a valid case, and a disabled construct never reaches it.
        #[test]
        fn disabled_constructs_stay_out(draft in draft()) {
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
            let directory = tempfile::tempdir_in(root).map_err(|error| TestCaseError::fail(error.to_string()))?;
            for disabled in [BTreeSet::new(), BTreeSet::from(EVERY)] {
                let path = directory.path().join("generated");
                draft.write(&path, &disabled).map_err(|error| TestCaseError::fail(error.to_string()))?;
                let case = case::read(&path).map_err(|error| TestCaseError::fail(error.to_string()))?;
                if disabled.is_empty() {
                    continue;
                }
                prop_assert!(!case.schema.contains("::uuid"));
                prop_assert!(!case.schema.contains("app.user_uuid"));
                prop_assert!(case.config.settings.is_none());
                prop_assert!(!case.schema.contains("SELECT (id, owner)"));
                prop_assert!(!case.schema.contains("ON members USING"));
                prop_assert!(!case.schema.contains("(flag)"));
                prop_assert!(case.seed.match_indices("\"owner\": ").all(|(at, field)| case.seed[at + field.len()..].starts_with('"')));
                for table in &case.config.tables {
                    let mut inserts = Vec::new();
                    for role in ROLES {
                        let grant = format!("ON {} TO {role};", table.name);
                        let line = case.schema.lines().find(|line| line.starts_with("GRANT S") && line.ends_with(&grant));
                        prop_assert!(line.is_some_and(|line| ["SELECT", "UPDATE", "DELETE"].iter().all(|privilege| line.contains(privilege))), "{}", case.schema);
                        inserts.push(line.is_some_and(|line| line.contains("INSERT")));
                    }
                    prop_assert!(inserts.iter().all(|held| *held == inserts[0]), "{}", case.schema);
                }
                prop_assert!(case.config.writes.iter().all(|write| !matches!(write.shape, sql_traits::parity::Shape::SelectForUpdate | sql_traits::parity::Shape::UpdateWithoutWhere)));
            }
        }
    }
}
