//! connetto's demo deployment, composed and translated the way connetto does.

use anyhow::Result;
use connetto_demo_deployment::{POLICIES_SQL, READER_ROLE, ROLES_SQL, SCHEMA_SQL};
use connetto_schema::TextIds;
use pg2sqlite::prelude::Pg2SqliteOptions;
use sql_traits::prelude::Deployment;

/// The demo's catalog as connetto's server reads it: the schema with the bot registry
/// connetto appends, the roles, the policies, and the cap policies connetto adds, each
/// document ending in its own `;`.
pub(crate) fn schema() -> Result<String> {
    let cap = connetto_schema::cap_policies::<TextIds>(SCHEMA_SQL, POLICIES_SQL)?;
    let registry = connetto_schema::with_registry::<TextIds>(SCHEMA_SQL);
    Ok([registry.as_str(), ROLES_SQL, POLICIES_SQL, cap.as_str()].join("\n"))
}

/// The deployment connetto gives both translators, acting as the reader role.
pub(crate) fn deployment() -> Result<Deployment> {
    Ok(connetto_schema::deployment::<String>(READER_ROLE)?)
}

/// The options connetto translates the replica under, acting as `role`.
pub(crate) fn replica_options(role: &str) -> Result<Pg2SqliteOptions> {
    Ok(connetto_schema::query_options::<String>(READER_ROLE)?.with_acting_role(role))
}
