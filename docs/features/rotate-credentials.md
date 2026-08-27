# Feature: Rotate credentials

## Scope

Let the user invalidate and regenerate a leaked credential from the web UI:

- database owner role (`kind=db`) and extra-user role (`kind=user`)
- instance base URI user (the admin credential stored per instance)

Out of scope: rotating credentials for databases without a saved instance base
URI (there is no admin connection to run `ALTER ROLE`), and regenerating the
master password (separate existing feature).

## Confirmed Rules

- Rotation runs `ALTER ROLE <role> WITH PASSWORD '<new>'` connected with the
  instance base URI user; the previous password is invalidated immediately.
- The new password comes from `crypto::generate_password` (base64 of 24 random
  bytes).
- The new connection string is built exactly like provision does
  (`build_connection_string`), so the catalog and the emitted string stay
  consistent.
- After a successful `ALTER ROLE`, the catalog record is re-encrypted and
  updated; the new string is returned to the UI for the user to copy.
- Instance rotation updates the saved base `DATABASE_URL`, preserving host,
  port, database and query parameters.
- The blocking `ALTER ROLE` runs via `spawn_blocking` (same reason as the
  health checks: the sync `postgres` client cannot run on a tokio worker).
- The UI confirms before rotating (destructive) and shows the new string with a
  copy button afterwards.

## Local Decisions

- **Decision**: rotation connects as the instance base URI user. **Why**: the
  base URI is the admin credential cais already holds; a normal app role cannot
  alter another role. **Source**: user request (Aug 2026).
- **Decision**: expose rotation as a per-row "rotacionar" button (owner/extra
  user) and a "Rotacionar" button in the instance card header for the base URI.
  **Why**: matches the existing per-row and per-card action patterns.
  **Source**: user request (Aug 2026).

## Open Questions

- [ ] Should rotation verify connectivity with the new string before saving?
      Currently the `ALTER ROLE` success is considered sufficient.

## Dependencies

- The instance must have a saved base URI.
- The base URI user must have privileges to alter the target role (an owner or
  CREATEROLE member).