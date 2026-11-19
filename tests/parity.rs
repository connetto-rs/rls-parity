use std::path::Path;

use rls_parity::{
    Answer, Direction, Engine, Known, Observation, compare, observe_cases, read_known,
};

/// Every recorded discrepancy must be necessary and must remain a discrepancy.
#[test]
fn live_known_entry_acceptance() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL for PostgreSQL 18");
    let openfga_url = std::env::var("OPENFGA_URL").expect("OPENFGA_URL for OpenFGA v1.11.6");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    omitted_table_seed_is_skipped(root, &database_url, &openfga_url)
        .expect("omitted replica table observations");
    let observations = observe_cases(&root.join("cases"), &database_url, &openfga_url)
        .expect("live three-engine observations");
    let mut known = read_known(&root.join("known.toml")).expect("known discrepancies");
    assert!(!known.is_empty(), "removal proof needs a known discrepancy");
    assert_eq!(compare(&observations, &known), Vec::<String>::new());
    removing_each_entry_reports_exactly_that_divergence(&observations, &mut known);
    stale_agreement_reports_three_answers(&observations, &mut known);
    fixture_edge_cases(root, &database_url, &openfga_url).expect("edge-case observations");
}

/// Removing a known entry exposes exactly its measured engine disagreement.
fn removing_each_entry_reports_exactly_that_divergence(
    observations: &[Observation],
    known: &mut Vec<Known>,
) {
    for index in 0..known.len() {
        let entry = known.remove(index);
        let witness = observations
            .iter()
            .find(|observation| {
                observation.case == entry.case
                    && observation.caller == entry.caller
                    && observation.row == entry.row
                    && observation.statement == entry.statement
            })
            .expect("known entry has a live witness");
        assert_eq!(
            compare(observations, known),
            [format!(
                "Unlisted {:?} {:?} {witness:?}",
                entry.engine, entry.direction
            )],
            "removed {entry:?}"
        );
        known.insert(index, entry);
    }
}

/// A stale entry includes the complete agreeing observation in its diagnostic.
fn stale_agreement_reports_three_answers(observations: &[Observation], known: &mut Vec<Known>) {
    let witness = observations
        .iter()
        .find(|observation| {
            observation.case == "grant_without_update"
                && observation.caller == "alice"
                && observation.row == "docs:1"
        })
        .expect("agreeing case witness");
    assert_eq!(witness.postgres, Answer::Visible);
    assert_eq!(witness.sqlite, Answer::Visible);
    assert_eq!(witness.openfga, Answer::Visible);
    known.push(Known {
        case: witness.case.clone(),
        caller: witness.caller.clone(),
        row: witness.row.clone(),
        statement: witness.statement.clone(),
        engine: Engine::Sqlite,
        direction: Direction::Narrower,
        finding: "deliberately stale acceptance witness".into(),
    });
    let errors = compare(observations, known);
    assert_eq!(errors.len(), 1);
    for required in [
        "Stale known entry",
        &witness.case,
        &witness.caller,
        &witness.row,
        &format!("{:?}", witness.statement),
        "postgres: Visible",
        "sqlite: Visible",
        "openfga: Visible",
    ] {
        assert!(
            errors[0].contains(required),
            "missing {required} in {}",
            errors[0]
        );
    }
    known.pop();
}

/// Empty suites and unreadable or malformed inputs retain their path in errors.
#[test]
fn invalid_suite_inputs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    let directory = tempfile::tempdir_in(root).expect("isolated fixture directory");
    let error = observe_cases(directory.path(), "", "").expect_err("empty suite");
    assert!(error.to_string().contains("No case directories"));
    let missing = directory.path().join("missing");
    let error = observe_cases(&missing, "", "").expect_err("missing suite");
    assert!(error.to_string().contains(&missing.display().to_string()));
    let error = read_known(&missing).expect_err("missing known list");
    assert!(error.to_string().contains(&missing.display().to_string()));
    let known_path = directory.path().join("known.toml");
    std::fs::write(&known_path, "known = false").expect("malformed known file");
    let error = read_known(&known_path).expect_err("invalid known list");
    assert!(
        error
            .to_string()
            .contains(&known_path.display().to_string())
    );
    let error = observe_cases(directory.path(), "", "").expect_err("invalid case directory");
    assert!(
        error
            .to_string()
            .contains(&known_path.display().to_string())
    );
}

/// String keys, missing row identity and undeclared roles retain distinct outcomes.
fn fixture_edge_cases(root: &Path, database_url: &str, openfga_url: &str) -> anyhow::Result<()> {
    let schema = include_str!("../cases/grant_without_update/schema.sql");
    let seed = include_str!("../cases/grant_without_update/seed.sql");
    let callers = include_str!("../cases/grant_without_update/callers.toml");
    for (name, schema, seed, callers) in [
        (
            "text_key",
            schema.replace("id INTEGER", "id TEXT"),
            seed.replace("(1,", "('one',").replace("(2,", "('two',"),
            callers.to_owned(),
        ),
        (
            "missing_key",
            schema.replace(" PRIMARY KEY", ""),
            seed.into(),
            callers.into(),
        ),
        (
            "missing_role",
            schema.into(),
            seed.into(),
            callers.replace("app_reader", "undeclared"),
        ),
    ] {
        let directory = tempfile::tempdir_in(root.join("target"))?;
        let case = directory.path().join(name);
        std::fs::create_dir(&case)?;
        for (file, content) in [
            ("schema.sql", schema),
            ("seed.sql", seed),
            ("callers.toml", callers),
        ] {
            std::fs::write(case.join(file), content)?;
        }
        let observations = observe_cases(directory.path(), database_url, openfga_url)?;
        assert_eq!(observations.len(), 8);
        for observation in &observations {
            let read = observation.statement.starts_with("SELECT");
            if name == "text_key" {
                assert!(["docs:one", "docs:two"].contains(&observation.row.as_str()));
                if read {
                    assert_eq!(observation.postgres, observation.sqlite);
                    assert_eq!(observation.postgres, observation.openfga);
                } else {
                    assert_eq!(observation.postgres, Answer::Rejected("42501".into()));
                }
            } else if read && name == "missing_key" {
                assert!(matches!(observation.openfga, Answer::Error(_)));
            } else if read {
                assert!(matches!(observation.postgres, Answer::Error(_)));
                assert!(matches!(observation.openfga, Answer::Refused(_)));
            }
        }
    }
    Ok(())
}

/// A table omitted by the replica remains an error for every oracle row.
fn omitted_table_seed_is_skipped(
    root: &Path,
    database_url: &str,
    openfga_url: &str,
) -> anyhow::Result<()> {
    let directory = tempfile::tempdir_in(root.join("target"))?;
    let case = directory.path().join("insert_only_grant");
    std::fs::create_dir(&case)?;
    for filename in ["schema.sql", "seed.sql", "callers.toml"] {
        std::fs::copy(
            root.join("cases/insert_only_grant").join(filename),
            case.join(filename),
        )?;
    }
    let observations = observe_cases(directory.path(), database_url, openfga_url)?;
    let reads: Vec<_> = observations
        .iter()
        .filter(|observation| observation.statement.starts_with("SELECT"))
        .collect();
    assert_eq!(reads.len(), 4);
    for observation in reads {
        assert!(
            matches!(&observation.postgres, Answer::Error(reason) if reason.contains("permission denied")),
            "{observation:?}"
        );
        assert!(
            matches!(&observation.sqlite, Answer::Error(reason) if reason.contains("no such table")),
            "{observation:?}"
        );
    }
    Ok(())
}
