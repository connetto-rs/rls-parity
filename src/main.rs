use std::{env, io::Write, path::Path};

use anyhow::{Context, Result, bail, ensure};
use rls_parity::{Known, Observation, compare, observe_cases, read_known};

/// Compare every case against a live PostgreSQL oracle.
fn main() -> Result<()> {
    println!("{}", env!("DEPENDENCY_REVISIONS"));
    let args: Vec<String> = env::args().skip(1).collect();
    let known = read_known(known_path(&args)?)?;
    let database_url = env::var("DATABASE_URL")
        .context("Set DATABASE_URL to a dedicated PostgreSQL 18 cluster")?;
    let openfga_url = env::var("OPENFGA_URL").context("Set OPENFGA_URL to OpenFGA v1.11.6")?;
    let observations = observe_cases(Path::new("cases"), &database_url, &openfga_url)?;
    report(
        &observations,
        &known,
        std::io::stdout().lock(),
        std::io::stderr().lock(),
    )
}

/// Read the optional alternate known-list path.
fn known_path(args: &[String]) -> Result<&Path> {
    match args {
        [] => Ok(Path::new("known.toml")),
        [flag, path] if flag == "--known" => Ok(Path::new(path)),
        _ => bail!("Usage cargo run -- [--known PATH]"),
    }
}

/// Emit every observation and fail after emitting every comparison diagnostic.
fn report(
    observations: &[Observation],
    known: &[Known],
    mut stdout: impl Write,
    mut stderr: impl Write,
) -> Result<()> {
    let errors = compare(observations, known);
    for observation in observations {
        serde_json::to_writer(&mut stdout, observation)?;
        writeln!(stdout)?;
    }
    for error in &errors {
        writeln!(stderr, "{error}")?;
    }
    ensure!(errors.is_empty(), "{} parity failures", errors.len());
    writeln!(
        stdout,
        "Compared {} caller-row statements across three engines",
        observations.len()
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rls_parity::Answer;

    /// Argument parsing accepts only an optional known-list path.
    #[test]
    fn known_list_arguments() -> Result<()> {
        assert_eq!(known_path(&[])?, Path::new("known.toml"));
        assert_eq!(
            known_path(&["--known".into(), "other.toml".into()])?,
            Path::new("other.toml")
        );
        for args in [
            vec!["--known".into()],
            vec!["--unknown".into(), "other.toml".into()],
        ] {
            assert!(known_path(&args).is_err());
        }
        Ok(())
    }

    /// Report output retains the full witness when a comparison fails.
    #[test]
    fn discrepancy_report() {
        let observation = Observation {
            case: "fixture".into(),
            caller: "alice".into(),
            row: "docs:1".into(),
            statement: "SELECT id FROM docs".into(),
            postgres: Answer::Visible,
            sqlite: Answer::Hidden,
            openfga: Answer::Visible,
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let error = report(
            std::slice::from_ref(&observation),
            &[],
            &mut stdout,
            &mut stderr,
        )
        .expect_err("unlisted divergence");
        assert_eq!(error.to_string(), "1 parity failures");
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            format!("{}\n", compare(std::slice::from_ref(&observation), &[])[0])
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&stdout).unwrap(),
            serde_json::to_value(observation).unwrap()
        );
    }
}
