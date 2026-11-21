use std::{env, io::Write, path::Path, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use rls_parity::{KnownList, Measurements, compare, generate, observe_cases, read_known, record};

/// Compare every case against a live PostgreSQL oracle, record the cases as fixtures, or
/// generate cases for a while.
fn main() -> Result<()> {
    println!("{}", env!("DEPENDENCY_REVISIONS"));
    let args: Vec<String> = env::args().skip(1).collect();
    let command = command(&args)?;
    let known = read_known(command.known())?;
    let database_url = env::var("DATABASE_URL")
        .context("Set DATABASE_URL to a dedicated PostgreSQL 18 cluster")?;
    let openfga_url = env::var("OPENFGA_URL").context("Set OPENFGA_URL to OpenFGA v1.11.6")?;
    match command {
        Command::Compare(_) => {
            let measurements = observe_cases(Path::new("cases"), &database_url, &openfga_url)?;
            report(
                &measurements,
                &known,
                std::io::stdout().lock(),
                std::io::stderr().lock(),
            )
        }
        Command::Generate { seed, minutes } => {
            let failures = Path::new("failures");
            let budget = Duration::from_secs(minutes.saturating_mul(60));
            let failed = generate(seed, budget, &known, &database_url, &openfga_url, failures)?;
            ensure!(
                failed == 0,
                "{failed} generated cases failed, written under {}",
                failures.display()
            );
            Ok(())
        }
        Command::Record(out) => {
            let cases = Path::new("cases");
            let measurements = observe_cases(cases, &database_url, &openfga_url)?;
            report(
                &measurements,
                &known,
                std::io::stdout().lock(),
                std::io::stderr().lock(),
            )?;
            let written = record(cases, &measurements, &known, out)?;
            println!("Recorded {written} cases under {}", out.display());
            Ok(())
        }
    }
}

#[derive(Debug, PartialEq)]
enum Command<'a> {
    /// Compare the cases against the known list at this path.
    Compare(&'a Path),
    /// Generate cases from `seed` for `minutes`.
    Generate { seed: u64, minutes: u64 },
    /// Record every files case with PostgreSQL's answers and its known entries under this
    /// directory.
    Record(&'a Path),
}

impl Command<'_> {
    /// The known list the command reads.
    fn known(&self) -> &Path {
        match self {
            Self::Compare(path) => path,
            Self::Generate { .. } | Self::Record(_) => Path::new("known.toml"),
        }
    }
}

/// Read the command and its arguments.
fn command(args: &[String]) -> Result<Command<'_>> {
    match args {
        [] => Ok(Command::Compare(Path::new("known.toml"))),
        [flag, path] if flag == "--known" => Ok(Command::Compare(Path::new(path))),
        [generate, seed_flag, seed, minutes_flag, minutes]
            if generate == "generate" && seed_flag == "--seed" && minutes_flag == "--minutes" =>
        {
            Ok(Command::Generate {
                seed: seed.parse().context("--seed takes an unsigned integer")?,
                minutes: minutes
                    .parse()
                    .context("--minutes takes an unsigned integer")?,
            })
        }
        [record, out_flag, out] if record == "record" && out_flag == "--out" => {
            Ok(Command::Record(Path::new(out)))
        }
        _ => bail!(
            "Usage cargo run -- [--known PATH] | generate --seed N --minutes M | record --out PATH"
        ),
    }
}

/// Emit every measurement and fail after emitting every comparison diagnostic.
fn report(
    measurements: &Measurements,
    known: &KnownList,
    mut stdout: impl Write,
    mut stderr: impl Write,
) -> Result<()> {
    let errors = compare(measurements, known);
    for acceptance in &measurements.acceptances {
        serde_json::to_writer(&mut stdout, acceptance)?;
        writeln!(stdout)?;
    }
    for observation in &measurements.observations {
        serde_json::to_writer(&mut stdout, observation)?;
        writeln!(stdout)?;
    }
    for error in &errors {
        writeln!(stderr, "{error}")?;
    }
    ensure!(errors.is_empty(), "{} parity failures", errors.len());
    writeln!(
        stdout,
        "Compared {} schemas and {} caller-row statements across three engines",
        measurements.acceptances.len(),
        measurements.observations.len()
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rls_parity::{Answer, Observation};

    /// Argument parsing accepts an optional known-list path or the generate command.
    #[test]
    fn command_arguments() -> Result<()> {
        let args = |words: &[&str]| {
            words
                .iter()
                .map(|word| (*word).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(command(&[])?, Command::Compare(Path::new("known.toml")));
        let other = args(&["--known", "other.toml"]);
        assert_eq!(command(&other)?, Command::Compare(Path::new("other.toml")));
        let generate = args(&["generate", "--seed", "7", "--minutes", "30"]);
        assert_eq!(
            command(&generate)?,
            Command::Generate {
                seed: 7,
                minutes: 30
            }
        );
        let record = args(&["record", "--out", "fixtures"]);
        assert_eq!(command(&record)?, Command::Record(Path::new("fixtures")));
        for words in [
            &["--known"][..],
            &["--unknown", "other.toml"],
            &["generate", "--seed", "-1", "--minutes", "30"],
            &["generate", "--minutes", "30", "--seed", "7"],
            &["record", "fixtures"],
        ] {
            assert!(command(&args(words)).is_err());
        }
        Ok(())
    }

    /// Report output retains the full witness when a comparison fails.
    #[test]
    fn discrepancy_report() {
        let measurements = Measurements {
            acceptances: Vec::new(),
            observations: vec![Observation {
                case: "fixture".into(),
                caller: "alice".into(),
                row: "docs:1".into(),
                statement: "SELECT id FROM docs".into(),
                postgres: Answer::Visible,
                sqlite: Answer::Hidden,
                openfga: Answer::Visible,
            }],
        };
        let known = KnownList::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let error = report(&measurements, &known, &mut stdout, &mut stderr)
            .expect_err("unlisted divergence");
        assert_eq!(error.to_string(), "1 parity failures");
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            format!("{}\n", compare(&measurements, &known)[0])
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&stdout).unwrap(),
            serde_json::to_value(&measurements.observations[0]).unwrap()
        );
    }
}
