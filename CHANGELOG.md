# Changelog

All notable changes to this project will be documented in this file.

## [0.4.0] - 2026-11-03

### Bug Fixes

- Edit instances and list workspaces on first serve

### CI / Build

- Correct download-artifact version comment

### Chores

- Refresh compatible Rust dependencies
- Bump actions/checkout from 4 to 7
- Bump actions/upload-artifact from 4 to 7
- Bump actions/download-artifact from 4.1.9 to 8.0.2
- Bump rusqlite from 0.40.2 to 0.41.0
- Bump argon2 from 0.5.3 to 0.6.0
- Bump base64 from 0.22.1 to 0.23.1

### Features

- Reshape the tui and web around daily use

## [0.3.0] - 2026-09-27

### Bug Fixes

- Report Docker image pull failures
- Verify backups match discovered servers

### Features

- Add generic PostgreSQL discovery
## [0.2.0] - 2026-09-15

### Bug Fixes

- Skip status, preview metadata, tablespace flatten; test: replace integration
- Make Docker unit tests portable and fix prepare-release YAML
- Prepare-release.yml YAML block scalar and portable Docker tests
- Match Docker PG version to native pg_dump in integration tests
- Use timescaledb image for docker backup/restore
- Stop operation polling when modal closes and add fetch timeout
- Repair backup picker, dashboard refresh, and reprovision credentials
- Enforce connect timeouts in migration, restore, and discovery
- Exit immediately on shutdown instead of draining idle connections
- Legacy vault migration on web startup and searchable workspace list
- Skip TimescaleDB internal catalog on cross-version restore
- Also filter _timescaledb_cache and any _timescaledb* schema creation
- Verify restored tables and refuse self-replacing migration
- Inspect dump TOC through Docker backend
- Cover all remote PostgreSQL postgres targets

### CI / Build

- Add GitHub Actions workflow
- Improve workflow and add release pipeline
- Fix workflow SHAs and add comprehensive Makefile
- Run web API tests in build job
- Add guarded remote PostgreSQL backup workflow
- Install AWS CLI on Ubuntu runners
- Use official AWS CLI installer
- Match PostgreSQL client to integration server
- Fix release artifact action reference
- Install NASM for Windows release builds
- Expose NASM to Windows release builds
- Export release manifest from host job

### Chores

- Ignore backups and document insecure web binding
- Prepare for v0.2.0

### Documentation

- Add cais naming explanation to README

### Features

- Add initial features
- DBP2 instance backup with selection, globals, and configuration
- Restore conflict policies and DBP2 integration tests
- Replace conflict policy and restore bundle preview
- Add local web interface with per-instance dashboard
- Add favicon and loading feedback for unlock/dashboard
- Show health check result states in a modal
- Rotate database and instance credentials
- Add 5s safety countdown to destructive confirmations
- Paginated instance list, removal, and optional dedicated owner
- Per-instance backup, source instance picker, and database filter
- Cancellable health checks
- Add SQL query console with table browser
- Replace existing target database option
- Add cais reset command for forgotten master password
- Multiple workspaces as independent vaults
- Table selection and TimescaleDB metadata exclusion
- Add headless backup and restore

### Refactor

- Rename project to cais

### Testing

- Live Docker integration tests for 17→18 and DBP2
- Isolate integration tests with testcontainers
- Cover first-run/unlock, instance CRUD, wizard URL building
- Add cross-TimescaleDB migration and workspace isolation integration tests
- Cover headless S3 backup restore
