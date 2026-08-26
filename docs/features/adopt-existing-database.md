# Feature: Adopt existing databases

## Scope

Allow a database that already exists on a managed instance (created outside
cais) to be imported into the catalog so it appears on the web dashboard and
can be used as a source/destination in the same way as provisioned databases.

Out of scope: adopting databases on instances without a saved base URI,
creating owner/extra roles for adopted databases, and relaxing the
`validate_database_name` naming convention.

## Confirmed Rules

- Adopting reuses the instance base URI credentials, swapping only the database
  name in the path (see `database_connection_string` in `src/postgres.rs`).
- The database must exist and accept a connection; `/api/adopt` verifies this
  with a health check before saving anything.
- Saved as a provisioned database record with `database_created = false` and
  `role_created = false`, so later operations treat it as pre-existing.
- The catalog `role_name` is the base URI username, matching the role in the
  stored connection string (same invariant provision already follows).

## Local Decisions

- **Decision**: `/api/adopt` accepts `{ instance_name, database_name,
  application_name? }` and validates the database name with
  `validate_database_name`. **Why**: keeps the catalog naming convention
  consistent with provisioning; adopted databases are expected to be normal
  application databases. **Source**: user request (Aug 2026).
- **Decision**: the web UI offers a per-instance "Adicionar banco existente"
  button that lists databases discovered by `/api/discover`, minus the ones
  already in the catalog, and lets the user select several to adopt at once.
  **Why**: discovery already existed; filtering by catalog keeps the list
  actionable. **Source**: user request (Aug 2026).
- **Decision**: adoption is a synchronous catalog write (no operation polling),
  like "Add instance". **Why**: the action is fast and does not need progress
  logs.
- **Decision**: the connectivity probe (and `post_discover`'s database listing)
  runs through `tokio::task::spawn_blocking`. **Why**: the `postgres` crate's
  sync `Client::connect` builds a blocking runtime internally and panics with
  "Cannot start a runtime from within a runtime" when called on a tokio worker
  thread; the probe must leave the async handler. **Source**: runtime panic
  observed on the web discover flow (Aug 2026).

## Open Questions

- [ ] Should non-conforming database names (uppercase, hyphens) be adoptable?
      Currently blocked by `validate_database_name`.
- [ ] Should adopted databases report their actual owner instead of the base
      URI username in the dashboard "Role" column?

## Dependencies

- Instances must have a saved base URI (they do — that is how `Add instance`
  works).
- The base URI user must have `CONNECT` privilege on the adopted database.