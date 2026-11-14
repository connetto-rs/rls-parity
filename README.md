# rls-parity

[![CI](https://github.com/connetto-rs/rls-parity/actions/workflows/ci.yml/badge.svg)](https://github.com/connetto-rs/rls-parity/actions/workflows/ci.yml)

Compare PostgreSQL row visibility with the SQLite replica from `pg2sqlite` and the `OpenFGA` model from `rls2fga`. PostgreSQL computes the oracle on every run. The runner prints resolved dependency commits and reports refusals alongside visibility answers.

## Cases

Each directory in `cases/` contains standalone `schema.sql`, `seed.sql` and `callers.toml`. The latter declares `table`, `key`, `columns`, and `[[callers]]` with `name`, `role`, and `settings` containing `app.user_id` and `app.subjects`. All callers use the declared reader role, with text identity and comma-delimited subjects.

`known.toml` identifies each discrepancy by `case`, `caller`, `row`, `statement`, `engine`, `direction` and `finding`. Directions are `wider`, `narrower`, `refused` and `error`. Unexpected discrepancies and entries that no longer occur fail the run.

Engine errors count as discrepancies even when both engines reject the read. An absent replica table reports its SQLite error for every oracle row.

Cases cover role inheritance, caller settings, grants, policy interactions, and table layouts. Only ordinary `SELECT` visibility is compared, including the read side of write and locking probes. Both `to_role_member` variants use the declared acting role.

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
