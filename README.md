# rls-parity

[![CI](https://github.com/connetto-rs/rls-parity/actions/workflows/ci.yml/badge.svg)](https://github.com/connetto-rs/rls-parity/actions/workflows/ci.yml)

Compare live PostgreSQL answers with the SQLite replica from `pg2sqlite` and the `OpenFGA` model from `rls2fga`. The runner prints resolved dependency commits, schema refusals and row answers.

## Cases

`sql_traits::parity::fixtures()` supplies 26 embedded cases with recorded PostgreSQL answers and per-engine known discrepancies. Every run measures PostgreSQL live and fails on the first differing answer in each recording. Both engines are judged through `Fixture::judge`, which rejects unlisted discrepancies and known discrepancies that disappear.

`cases/connetto_deployment` holds the demo's seed and callers. Its schema, deployment and replica options come from connetto, and its callers declare `app.user_id`, `app.subjects` and `app.bot_list`. This case and generated cases use live PostgreSQL answers with no known discrepancies.

Each case declares tables, callers and write probes in `callers.toml`. Reads measure each seeded row, and writes run in rolled-back savepoints. SQLite runs `pg2sqlite::parity::replay`, while the runner loads the `OpenFGA` model and checks existing and resulting rows.

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

`Cargo.lock` remains local. The runner creates and removes each case database and its roles, and also replays case directories under `failures/`.

## Record

`cargo run --release -- record --out PATH` exports the 26 embedded cases with fresh live answers in `postgres.toml` and their shared per-engine `known.toml` entries. It allows recordings to be regenerated when PostgreSQL's answers change.

## Generate

`cargo run --release -- generate --seed N --minutes M` draws random cases for `M` minutes and shrinks discrepancies into `failures/seed{N}-{draw}/`. Each directory contains `schema.sql`, `seed.sql` and `callers.toml`, parsed by `Case::parse` and replayed by the default run.

A `[[construct]]` entry in `known.toml` names a construct tied to a known finding, with its `engine` and `finding`. The generator never produces a listed construct, and removing the entry turns it back on.

The `Nightly` workflow generates for 30 minutes with the UTC date as seed, such as `20261120`, and uploads any failures as the `failures-{seed}` artifact.
