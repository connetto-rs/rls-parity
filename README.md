# rls-parity

[![CI](https://github.com/connetto-rs/rls-parity/actions/workflows/ci.yml/badge.svg)](https://github.com/connetto-rs/rls-parity/actions/workflows/ci.yml)

Compare PostgreSQL row visibility with the SQLite replica from `pg2sqlite` and the `OpenFGA` model from `rls2fga`. PostgreSQL computes the oracle on every run. The runner prints resolved dependency commits and reports refusals alongside visibility answers.

## Cases

Each directory in `cases/` contains standalone `schema.sql`, `seed.sql` and `callers.toml`. The latter declares `table`, `key`, `columns`, and `[[callers]]` with `name`, `role`, and `settings` containing `app.user_id` and `app.subjects`, with text identity and comma-delimited subjects. Every role a caller names is an acting role of the deployment, which logs in as the first caller's role. `pg2sqlite` translates one replica per acting role, and each caller is measured on its own role's replica. Every case measures `SELECT`. A `[[writes]]` entry adds a probe of shape `select_for_update`, `update` or `update_without_where` with a `set` table, `delete`, or `insert` with a `row` table carrying the row key. `OpenFGA` answers the locking read as its own action.

An `update` or `delete` probe runs once per seeded row, filtered by its key, while `update_without_where` and `insert` run once. Every write is rolled back. PostgreSQL and the replica answer `changed` or `skipped` per row, or `rejected` with the SQLSTATE that aborted the statement. `OpenFGA` answers `changed` or `denied` from the existing row's tuples and, for the row an `INSERT` or `UPDATE` would leave, from the tuples `rls2fga` derives for it.

The SQLite replica runs the statement as `pg2sqlite` translates it, so a clause the translation drops is dropped in the measurement too.

A `[[known]]` entry in `known.toml` identifies each row discrepancy by `case`, `caller`, `row`, `statement`, `engine`, `direction` and `finding`. Directions are `wider`, `narrower`, `different_denial`, `refused` and `error`. `different_denial` marks a write both engines refuse, one by skipping the row and the other by aborting, or with different SQLSTATEs. Unexpected discrepancies and entries that no longer occur fail the run.

Engine errors count as discrepancies even when both engines reject the read. An absent replica table reports its SQLite error for every oracle row.

Each case also records whether each translator accepts its schema, the way connetto gates it. `pg2sqlite` accepts when it translates the schema for every acting role. `rls2fga` accepts when `Translation::outputs` succeeds, so a note that diverges from the database refuses it, while row comparisons keep running on `outputs_accepting_gaps`. A schema exactly one of them refuses is a discrepancy unless a `[[refusal]]` entry in `known.toml` lists its `case`, the refusing `engine` and the `finding`.

Cases cover role inheritance, several acting roles, caller settings, grants, policy interactions, writes, and table layouts. Both `to_role_member` variants use the declared acting role.

## Run

Start PostgreSQL 18 and `OpenFGA` `v1.11.6` with its `run` command, then use a dedicated PostgreSQL cluster and a superuser connection.

```sh
cargo update
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres
export OPENFGA_URL=http://localhost:8080
cargo run --release
cargo llvm-cov --all-targets --no-report
cargo llvm-cov run --no-report
cargo llvm-cov report --include-build-script --lcov --output-path lcov.info
```

`Cargo.lock` remains local. The runner creates and removes each case database and its roles. Pass `-- --known PATH` to test an alternate known list.
