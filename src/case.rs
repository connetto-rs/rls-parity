use std::{fs, path::Path};

use anyhow::{Context, Result};
use sql_traits::parity::{Case, Config};

use crate::connetto;

/// Read a local case or a generated three-file case.
pub(crate) fn read(path: &Path) -> Result<Case> {
    let name = path
        .file_name()
        .context("Case directory has no name")?
        .to_str()
        .context("Case name is not UTF-8")?;
    let callers = fs::read_to_string(path.join("callers.toml"))
        .with_context(|| format!("Reading callers of {name}"))?;
    let seed = fs::read_to_string(path.join("seed.sql"))
        .with_context(|| format!("Reading seed of {name}"))?;
    if name == "connetto_deployment" {
        let config: Config = toml::from_str(&callers)?;
        Ok(Case {
            name: name.into(),
            schema: connetto::schema()?,
            seed,
            config,
        })
    } else {
        let schema = fs::read_to_string(path.join("schema.sql"))
            .with_context(|| format!("Reading schema of {name}"))?;
        Case::parse(name, schema, seed, callers).map_err(Into::into)
    }
}
