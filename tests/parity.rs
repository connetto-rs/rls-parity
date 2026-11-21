use std::{path::Path, time::Duration};

use rls_parity::{
    Answer, CaseKnown, Direction, Engine, Known, KnownList, Measurements, Recorded, compare,
    generate, observe_cases, read_known, record,
};

/// Every recorded discrepancy must be necessary and must remain a discrepancy.
#[test]
fn live_known_entry_acceptance() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL for PostgreSQL 18");
    let openfga_url = std::env::var("OPENFGA_URL").expect("OPENFGA_URL for OpenFGA v1.11.6");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    omitted_table_seed_is_skipped(root, &database_url, &openfga_url)
        .expect("omitted replica table observations");
    let measurements = observe_cases(&root.join("cases"), &database_url, &openfga_url)
        .expect("live three-engine observations");
    let mut known = read_known(&root.join("known.toml")).expect("known discrepancies");
    assert!(
        !known.known.is_empty() && !known.refusal.is_empty(),
        "removal proof needs a known discrepancy of each kind"
    );
    assert_eq!(compare(&measurements, &known), Vec::<String>::new());
    removing_each_entry_reports_exactly_that_divergence(&measurements, &mut known);
    stale_agreement_reports_three_answers(&measurements, &mut known);
    recorded_fixtures_hold_each_case(root, &measurements, &known).expect("recorded fixtures");
    fixture_edge_cases(root, &database_url, &openfga_url).expect("edge-case observations");
    let listed = read_known(&root.join("known.toml")).expect("known discrepancies");
    generated_failure_replays(root, &database_url, &openfga_url, &listed)
        .expect("generated observations");
}

/// The first draw of seed 0 diverges with every construct generated, its written case replays
/// the divergence, and the listed constructs keep the same draw from diverging.
fn generated_failure_replays(
    root: &Path,
    database_url: &str,
    openfga_url: &str,
    known: &KnownList,
) -> anyhow::Result<()> {
    let one_draw = Duration::from_millis(1);
    let directory = tempfile::tempdir_in(root.join("target"))?;
    let failures = directory.path().join("failures");
    let unlisted = KnownList::default();
    let failed = generate(0, one_draw, &unlisted, database_url, openfga_url, &failures)?;
    assert_eq!(failed, 1);
    let replayed = observe_cases(&failures, database_url, openfga_url)?;
    assert_ne!(compare(&replayed, &unlisted), Vec::<String>::new());
    let listed = directory.path().join("listed");
    assert_eq!(
        generate(0, one_draw, known, database_url, openfga_url, &listed)?,
        0
    );
    assert!(!listed.exists());
    Ok(())
}

/// `record` writes every files case, leaves out the connetto case, copies each case's files
/// unchanged, and writes exactly that case's PostgreSQL answers and known entries.
fn recorded_fixtures_hold_each_case(
    root: &Path,
    measurements: &Measurements,
    known: &KnownList,
) -> anyhow::Result<()> {
    let directory = tempfile::tempdir_in(root.join("target"))?;
    let out = directory.path().join("fixtures");
    let cases = root.join("cases");
    let written = record(&cases, measurements, known, &out)?;
    assert_eq!(written, measurements.acceptances.len() - 1);
    assert!(!out.join("connetto_deployment").exists());
    for acceptance in &measurements.acceptances {
        let name = &acceptance.case;
        if name == "connetto_deployment" {
            continue;
        }
        let case = out.join(name);
        for file in ["schema.sql", "seed.sql", "callers.toml"] {
            assert_eq!(
                std::fs::read(case.join(file))?,
                std::fs::read(cases.join(name).join(file))?,
                "{name}/{file}"
            );
        }
        let recorded: Recorded =
            toml::from_str(&std::fs::read_to_string(case.join("postgres.toml"))?)?;
        let expected: Vec<_> = measurements
            .observations
            .iter()
            .filter(|observation| &observation.case == name)
            .map(|observation| {
                (
                    observation.caller.as_str(),
                    observation.row.as_str(),
                    observation.statement.as_str(),
                    &observation.postgres,
                )
            })
            .collect();
        let answers: Vec<_> = recorded
            .answer
            .iter()
            .map(|answer| {
                (
                    answer.caller.as_str(),
                    answer.row.as_str(),
                    answer.statement.as_str(),
                    &answer.postgres,
                )
            })
            .collect();
        assert_eq!(answers, expected, "{name}/postgres.toml");
        let listed = known
            .known
            .iter()
            .filter(|entry| &entry.case == name)
            .count();
        let refused = known
            .refusal
            .iter()
            .filter(|entry| &entry.case == name)
            .count();
        let path = case.join("known.toml");
        if listed + refused == 0 {
            assert!(!path.exists(), "{name} has no known entries");
            continue;
        }
        let case_known: CaseKnown = toml::from_str(&std::fs::read_to_string(path)?)?;
        let engines = [&case_known.sqlite, &case_known.openfga];
        assert_eq!(
            engines.iter().map(|engine| engine.row.len()).sum::<usize>(),
            listed,
            "{name}/known.toml rows"
        );
        assert_eq!(
            engines
                .iter()
                .filter(|engine| engine.refusal.is_some())
                .count(),
            refused,
            "{name}/known.toml refusals"
        );
    }
    Ok(())
}

/// Removing a known entry exposes exactly its measured engine disagreement.
fn removing_each_entry_reports_exactly_that_divergence(
    measurements: &Measurements,
    known: &mut KnownList,
) {
    for index in 0..known.known.len() {
        let entry = known.known.remove(index);
        let witness = measurements
            .observations
            .iter()
            .find(|observation| {
                observation.case == entry.case
                    && observation.caller == entry.caller
                    && observation.row == entry.row
                    && observation.statement == entry.statement
            })
            .expect("known entry has a live witness");
        assert_eq!(
            compare(measurements, known),
            [format!(
                "Unlisted {:?} {:?} {witness:?}",
                entry.engine, entry.direction
            )],
            "removed {entry:?}"
        );
        known.known.insert(index, entry);
    }
    for index in 0..known.refusal.len() {
        let entry = known.refusal.remove(index);
        let witness = measurements
            .acceptances
            .iter()
            .find(|acceptance| acceptance.case == entry.case)
            .expect("known refusal has a live witness");
        assert_eq!(
            compare(measurements, known),
            [format!("Unlisted {:?} refusal {witness:?}", entry.engine)],
            "removed {entry:?}"
        );
        known.refusal.insert(index, entry);
    }
}

/// A stale entry includes the complete agreeing observation in its diagnostic.
fn stale_agreement_reports_three_answers(measurements: &Measurements, known: &mut KnownList) {
    let witness = measurements
        .observations
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
    known.known.push(Known {
        case: witness.case.clone(),
        caller: witness.caller.clone(),
        row: witness.row.clone(),
        statement: witness.statement.clone(),
        engine: Engine::Sqlite,
        direction: Direction::Narrower,
        finding: "deliberately stale acceptance witness".into(),
    });
    let errors = compare(measurements, known);
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
    known.known.pop();
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
        let observations = observe_cases(directory.path(), database_url, openfga_url)?.observations;
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

/// A table omitted by the replica answers PostgreSQL's privilege error for every oracle row.
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
    let observations = observe_cases(directory.path(), database_url, openfga_url)?.observations;
    let reads: Vec<_> = observations
        .iter()
        .filter(|observation| observation.statement.starts_with("SELECT"))
        .collect();
    assert_eq!(reads.len(), 4);
    for observation in reads {
        assert!(
            matches!(&observation.postgres, Answer::Error(reason) if reason == "permission denied for table docs"),
            "{observation:?}"
        );
        assert!(
            matches!(&observation.sqlite, Answer::Refused(reason) if reason == "permission denied for table docs"),
            "{observation:?}"
        );
    }
    Ok(())
}
