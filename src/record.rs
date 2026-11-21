//! Live PostgreSQL answers and exported fixtures.

use std::{fs, path::Path};

use anyhow::{Context, Result, bail};
use sql_traits::parity::{Recorded, RecordedAnswer, fixtures};

use crate::{Measurements, Observation};

/// Collect PostgreSQL's answers from live observations.
#[must_use]
pub fn recorded<'observation>(
    observations: impl IntoIterator<Item = &'observation Observation>,
) -> Recorded {
    Recorded {
        answer: observations
            .into_iter()
            .map(|observation| RecordedAnswer {
                caller: observation.caller.clone(),
                row: observation.row.clone(),
                statement: observation.statement.clone(),
                postgres: observation.postgres.clone(),
            })
            .collect(),
    }
}

/// Check the recorded oracle against live PostgreSQL.
///
/// # Errors
/// Returns the case and first differing answer.
pub fn verify_recorded(name: &str, expected: &Recorded, live: &Recorded) -> Result<()> {
    let count = expected.answer.len().max(live.answer.len());
    let expected = expected
        .answer
        .iter()
        .map(Some)
        .chain(std::iter::repeat(None));
    let actual = live.answer.iter().map(Some).chain(std::iter::repeat(None));
    for (index, (expected, actual)) in expected.zip(actual).take(count).enumerate() {
        if expected != actual {
            bail!("{name} recorded answer {index} differs, recorded={expected:?}, live={actual:?}");
        }
    }
    Ok(())
}

/// Write embedded cases with their live PostgreSQL answers.
///
/// # Errors
/// Returns the case and cause when an answer is missing or a file cannot be written.
pub fn record(measurements: &Measurements, out: &Path) -> Result<usize> {
    let mut written = 0;
    for fixture in fixtures() {
        let fixture = fixture?;
        let case = fixture.case;
        let observations = recorded(
            measurements
                .observations
                .iter()
                .filter(|observation| observation.case == case.name),
        );
        anyhow::ensure!(
            !observations.answer.is_empty(),
            "No live answers for {}",
            case.name
        );
        let target = out.join(&case.name);
        fs::create_dir_all(&target).with_context(|| format!("Creating {}", target.display()))?;
        for (file, contents) in [
            ("schema.sql", case.schema),
            ("seed.sql", case.seed),
            ("callers.toml", toml::to_string(&case.config)?),
            ("postgres.toml", toml::to_string(&observations)?),
        ] {
            fs::write(target.join(file), contents)
                .with_context(|| format!("Writing {file} of {}", case.name))?;
        }
        if !fixture.known.is_empty() {
            fs::write(target.join("known.toml"), toml::to_string(&fixture.known)?)?;
        }
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Answer;

    /// Changed, missing and extra recorded answers identify the first mismatch.
    #[test]
    fn recorded_answer_mutation_is_rejected() -> Result<()> {
        let fixture = fixtures().next().context("No embedded fixture")??;
        let live = &fixture.recorded;
        verify_recorded(&fixture.case.name, live, live)?;
        let mut mutated = live.clone();
        mutated.answer[0].postgres = Answer::Error("mutated oracle".into());
        let error = verify_recorded(&fixture.case.name, &mutated, live)
            .expect_err("changed recorded answer must fail");
        let message = error.to_string();
        assert!(message.contains(&fixture.case.name));
        assert!(message.contains(&live.answer[0].caller));
        assert!(message.contains(&live.answer[0].row));
        assert!(message.contains("mutated oracle"));
        mutated = live.clone();
        mutated.answer.pop();
        assert!(verify_recorded(&fixture.case.name, &mutated, live).is_err());
        assert!(verify_recorded(&fixture.case.name, live, &mutated).is_err());
        Ok(())
    }
}
