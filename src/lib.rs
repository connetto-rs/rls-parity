#![doc = include_str!("../README.md")]

mod case;
mod connetto;
mod generator;
mod postgres;
mod record;
mod runner;

pub use generator::{Construct, generate};
pub use record::{record, recorded, verify_recorded};
pub use runner::{RunError, observe_cases, observe_fixture, observe_suite, read_known};
pub use sql_traits::parity::{Answer, Engine};

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// What one write statement did on a database.
pub(crate) enum Outcome {
    /// The statement ran and changed the rows with these keys.
    Applied(BTreeSet<String>),
    /// The statement aborted with this SQLSTATE.
    Rejected(String),
}

impl Outcome {
    /// The answer this outcome gives for the row with `key`.
    fn answer(&self, key: &str) -> Answer {
        match self {
            Self::Applied(keys) if keys.contains(key) => Answer::Changed,
            Self::Applied(_) => Answer::Skipped,
            Self::Rejected(sqlstate) => Answer::Rejected(sqlstate.clone()),
        }
    }
}

/// A comparison with its live PostgreSQL oracle.
#[derive(Debug, Serialize)]
pub struct Observation {
    pub case: String,
    pub caller: String,
    pub row: String,
    pub statement: String,
    pub postgres: Answer,
    pub sqlite: Answer,
    pub openfga: Answer,
}

/// Each translator's verdict on a case's schema, `None` when it accepts and the refusal
/// otherwise.
#[derive(Debug, Serialize)]
pub struct Acceptance {
    pub case: String,
    pub sqlite: Option<String>,
    pub openfga: Option<String>,
}

/// Every live measurement of a suite.
#[derive(Debug, Default)]
pub struct Measurements {
    pub acceptances: Vec<Acceptance>,
    pub observations: Vec<Observation>,
    pub errors: Vec<String>,
}

impl Measurements {
    /// Append another run's measurements and failures.
    pub fn extend(&mut self, other: Self) {
        self.acceptances.extend(other.acceptances);
        self.observations.extend(other.observations);
        self.errors.extend(other.errors);
    }
}

/// A generator construct whose divergence is tracked, so the generator never produces it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnownConstruct {
    pub construct: Construct,
    pub engine: Engine,
    pub finding: String,
}

/// Generator constructs excluded by known findings.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnownList {
    #[serde(default)]
    pub construct: Vec<KnownConstruct>,
}
