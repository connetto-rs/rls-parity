//! connetto's demo deployment, composed and translated the way connetto does.

use anyhow::Result;
use connetto_demo_deployment::{POLICIES_SQL, READER_ROLE, ROLES_SQL, SCHEMA_SQL};
use connetto_translate::{KindRoles, TextIds, UnsetSettings};
use pg2sqlite::prelude::Pg2SqliteOptions;
use sql_traits::prelude::Deployment;

/// The demo's catalog as connetto's server reads it: the schema with the bot registry
/// connetto appends, the roles, the policies, and the cap policies connetto adds, each
/// document ending in its own `;`.
pub(crate) fn schema() -> Result<String> {
    let cap = connetto_translate::cap_policies::<TextIds>(SCHEMA_SQL, POLICIES_SQL)?;
    let registry = connetto_translate::with_registry::<TextIds>(SCHEMA_SQL);
    Ok([registry.as_str(), ROLES_SQL, POLICIES_SQL, cap.as_str()].join("\n"))
}

/// The deployment connetto gives both translators, logging in as the reader role and
/// acting as the demo's kind roles, with both caller settings always bound as the demo
/// declares them.
pub(crate) fn deployment() -> Result<Deployment> {
    Ok(connetto_translate::deployment::<String>(
        READER_ROLE,
        KindRoles::defaults(),
        UnsetSettings::ALWAYS_BOUND,
    )?)
}

/// The options connetto translates the replica under, before choosing its acting role.
pub(crate) fn replica_options() -> Result<Pg2SqliteOptions> {
    Ok(connetto_translate::query_options::<String>(
        READER_ROLE,
        KindRoles::defaults(),
        UnsetSettings::ALWAYS_BOUND,
    )?)
}
