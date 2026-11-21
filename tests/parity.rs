use std::{path::Path, time::Duration};

use rls_parity::{
    Answer, KnownList, Measurements, generate, observe_cases, observe_fixture, observe_suite,
    read_known, record, verify_recorded,
};
use sql_traits::parity::{Case, CaseKnown, Engine, EngineAnswer, Recorded, fixtures};

/// Embedded oracles match PostgreSQL and both engines satisfy shared judgements.
#[test]
fn live_known_entry_acceptance() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL for PostgreSQL 18");
    let openfga_url = std::env::var("OPENFGA_URL").expect("OPENFGA_URL for OpenFGA v1.11.6");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    omitted_table_seed_is_skipped(root, &database_url, &openfga_url)
        .expect("omitted replica table observations");
    let measurements =
        observe_suite(root, &database_url, &openfga_url).expect("live three-engine observations");
    assert_eq!(measurements.acceptances.len(), 27);
    assert!(measurements.errors.is_empty(), "{:?}", measurements.errors);
    recorded_fixtures_hold_each_case(root, &measurements).expect("recorded fixtures");
    removing_each_entry_reports_exactly_that_divergence(&measurements)
        .expect("shared known entries");
    changed_recording_fails_live_run(&database_url, &openfga_url).expect("live oracle mutation");
    fixture_edge_cases(root, &database_url, &openfga_url).expect("edge-case observations");
    let listed = read_known(&root.join("known.toml")).expect("known constructs");
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
    let local = directory.path().join("cases/connetto_deployment");
    std::fs::create_dir_all(&local)?;
    for file in ["seed.sql", "callers.toml"] {
        std::fs::copy(
            root.join("cases/connetto_deployment").join(file),
            local.join(file),
        )?;
    }
    let replayed = observe_suite(directory.path(), database_url, openfga_url)?;
    assert_eq!(replayed.acceptances.len(), 28);
    assert_ne!(replayed.errors, Vec::<String>::new());
    assert!(
        replayed
            .errors
            .iter()
            .all(|error| error.contains("seed0-0"))
    );
    let listed = directory.path().join("listed");
    assert_eq!(
        generate(0, one_draw, known, database_url, openfga_url, &listed)?,
        0
    );
    assert!(!listed.exists());
    Ok(())
}

/// Exported fixtures retain their cases, live answers and engine-known entries.
fn recorded_fixtures_hold_each_case(
    root: &Path,
    measurements: &Measurements,
) -> anyhow::Result<()> {
    let directory = tempfile::tempdir_in(root.join("target"))?;
    let out = directory.path().join("fixtures");
    assert_eq!(record(measurements, &out)?, 26);
    assert!(!out.join("connetto_deployment").exists());
    for fixture in fixtures() {
        let fixture = fixture?;
        let path = out.join(&fixture.case.name);
        let exported = Case::parse(
            &fixture.case.name,
            std::fs::read_to_string(path.join("schema.sql"))?,
            std::fs::read_to_string(path.join("seed.sql"))?,
            std::fs::read_to_string(path.join("callers.toml"))?,
        )?;
        assert_eq!(exported, fixture.case);
        let oracle: Recorded =
            toml::from_str(&std::fs::read_to_string(path.join("postgres.toml"))?)?;
        verify_recorded(&fixture.case.name, &fixture.recorded, &oracle)?;
        if fixture.known.is_empty() {
            assert!(!path.join("known.toml").exists());
        } else {
            let known: CaseKnown =
                toml::from_str(&std::fs::read_to_string(path.join("known.toml"))?)?;
            assert_eq!(known, fixture.known);
        }
    }
    Ok(())
}

/// A changed recording fails the same live measurement path as the default suite.
fn changed_recording_fails_live_run(database_url: &str, openfga_url: &str) -> anyhow::Result<()> {
    let mut fixture = fixtures().next().expect("embedded fixture")?;
    let name = fixture.case.name.clone();
    let expected = fixture.recorded.answer[0].clone();
    fixture.recorded.answer[0].postgres = Answer::Error("mutated recorded answer".into());
    let measurements = observe_fixture(fixture, database_url, openfga_url)?;
    assert_eq!(measurements.errors.len(), 1);
    let error = &measurements.errors[0];
    for witness in [
        &name,
        &expected.caller,
        &expected.row,
        "mutated recorded answer",
    ] {
        assert!(error.contains(witness), "{error}");
    }
    assert!(
        error.contains(&format!("{:?}", expected.statement)),
        "{error}"
    );
    assert!(
        error.contains(&format!("postgres: {:?}", expected.postgres)),
        "{error}"
    );
    assert_eq!(measurements.observations[0].postgres, expected.postgres);
    Ok(())
}

/// Removing each shared known entry exposes exactly its live disagreement.
fn removing_each_entry_reports_exactly_that_divergence(
    measurements: &Measurements,
) -> anyhow::Result<()> {
    for fixture in fixtures() {
        let mut fixture = fixture?;
        let acceptance = measurements
            .acceptances
            .iter()
            .find(|acceptance| acceptance.case == fixture.case.name)
            .expect("measured case");
        for engine in [Engine::Sqlite, Engine::Openfga] {
            let answers: Vec<_> = measurements
                .observations
                .iter()
                .filter(|observation| observation.case == fixture.case.name)
                .map(|observation| EngineAnswer {
                    caller: observation.caller.clone(),
                    row: observation.row.clone(),
                    statement: observation.statement.clone(),
                    answer: match engine {
                        Engine::Sqlite => observation.sqlite.clone(),
                        Engine::Openfga => observation.openfga.clone(),
                    },
                })
                .collect();
            let refusal = match engine {
                Engine::Sqlite => acceptance.sqlite.as_deref(),
                Engine::Openfga => acceptance.openfga.as_deref(),
            };
            let original = fixture.known.clone();
            let count = fixture.known.engine(engine).row.len();
            for index in 0..count {
                let known = match engine {
                    Engine::Sqlite => &mut fixture.known.sqlite,
                    Engine::Openfga => &mut fixture.known.openfga,
                };
                known.row.remove(index);
                assert_eq!(fixture.judge(engine, refusal, &answers).len(), 1);
                fixture.known = original.clone();
            }
            let known = match engine {
                Engine::Sqlite => &mut fixture.known.sqlite,
                Engine::Openfga => &mut fixture.known.openfga,
            };
            if known.refusal.take().is_some() {
                assert_eq!(fixture.judge(engine, refusal, &answers).len(), 1);
                fixture.known = original;
            }
        }
    }
    Ok(())
}

/// Find one embedded case for a local variation.
fn embedded_case(name: &str) -> anyhow::Result<Case> {
    for fixture in fixtures() {
        let fixture = fixture?;
        if fixture.case.name == name {
            return Ok(fixture.case);
        }
    }
    anyhow::bail!("No embedded case {name}")
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
    let source = embedded_case("grant_without_update")?;
    let schema = source.schema.as_str();
    let seed = source.seed.as_str();
    let callers = toml::to_string(&source.config)?;
    for (name, schema, seed, callers) in [
        (
            "text_key",
            schema.replace("id INTEGER", "id TEXT"),
            seed.replace("(1,", "('one',").replace("(2,", "('two',"),
            callers.clone(),
        ),
        (
            "missing_key",
            schema.replace(" PRIMARY KEY", ""),
            seed.into(),
            callers.clone(),
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
    let source = embedded_case("insert_only_grant")?;
    for (filename, contents) in [
        ("schema.sql", source.schema),
        ("seed.sql", source.seed),
        ("callers.toml", toml::to_string(&source.config)?),
    ] {
        std::fs::write(case.join(filename), contents)?;
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
