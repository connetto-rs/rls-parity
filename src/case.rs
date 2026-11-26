use std::{fs, io::ErrorKind, path::Path};

use anyhow::{Context, Result};
use sql_traits::parity::{Case, CaseKnown, Config};

use crate::connetto;

/// Read a local case or a generated three-file case.
pub(crate) fn read(path: &Path) -> Result<Case> {
    let name = path
        .file_name()
        .context("Case directory has no name")?
        .to_str()
        .context("Case name is not UTF-8")?;
    let read_file = |file| {
        fs::read_to_string(path.join(file))
            .with_context(|| format!("Reading {file} in {}", path.display()))
    };
    let callers = read_file("callers.toml")?;
    let seed = read_file("seed.sql")?;
    if name == "connetto_deployment" {
        let config: Config = toml::from_str(&callers)?;
        Ok(Case {
            name: name.into(),
            schema: connetto::schema()?,
            seed,
            config,
        })
    } else {
        let schema = read_file("schema.sql")?;
        Case::parse(name, schema, seed, callers).map_err(Into::into)
    }
}

/// Read the discrepancies a local case's `known.toml` lists, none when it has no such file.
pub(crate) fn known(path: &Path) -> Result<CaseKnown> {
    let file = path.join("known.toml");
    match fs::read_to_string(&file) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("Parsing {}", file.display())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(CaseKnown::default()),
        Err(error) => Err(error).with_context(|| format!("Reading {}", file.display())),
    }
}
