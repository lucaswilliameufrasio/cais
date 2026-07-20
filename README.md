# cais

PostgreSQL operations TUI — provision, migrate, backup, and restore from the terminal.

Secrets are encrypted with AES-256-GCM (Argon2id key derivation) and stored in a local SQLite database.

## Features

- **Provision** a database + owner role + optional extra user in one screen
- **Migrate** a database from one cluster to another via `pg_dump` + `pg_restore`
- **Backup** a database to an encrypted file (`pg_dump` → AES-256-GCM → `.pgdump.enc`)
- **Restore** a database from an encrypted backup file (decrypt → `pg_restore`)
- **Instances** — named PostgreSQL clusters with an optionally saved base URI
- **Saved connections** — view, copy, rename, or delete previously generated connection strings
- **Change master password** — transactional re-encryption of all secrets
- **Docker fallback** — if `pg_dump`/`pg_restore` aren't installed, the app uses Docker automatically
- **Version check** — detects pg_dump/server version mismatch and warns before migration/backup
- **Unicode-aware cursor navigation** in text fields (Left/Right/Home/End)
- **Background worker** — provisioning, migrations, backup, and restore run in a thread; UI stays responsive

## Prerequisites

### PostgreSQL client tools (optional)

One of:

```bash
brew install postgresql              # macOS
sudo apt install postgresql-client   # Debian/Ubuntu
sudo dnf install postgresql          # Fedora
mise use -g postgres                 # mise
```

### Docker (optional fallback)

If neither `pg_dump` nor `pg_restore` are found in `$PATH`, the app checks for Docker and uses:

```bash
docker run --rm -i --network host postgres:<version>-alpine pg_dump ...
```

The Docker image tag is chosen automatically based on the source server version (e.g., `postgres:16-alpine` for PostgreSQL 16).

At least one of (`pg_dump` + `pg_restore`) **or** Docker is required for Migrate, Backup, and Restore screens.

## Quick start

```bash
cargo run
```

On first run you will be prompted to create a master password. This password is never stored — only a verification blob derived from it is saved.

## Makefile targets

```bash
make all       # lint → format → test (default)
make lint      # cargo clippy --all-targets -- -D warnings
make format    # cargo fmt --check
make test      # cargo test --lib + integration tests (requires Docker)
```

## Keybindings

### Global

| Key | Action |
|-----|--------|
| `q` | Quit (from Home) |
| `Esc` | Go back |

### First run / Unlock

| Key | Action |
|-----|--------|
| `Enter` | Submit |
| `Tab` / `Up` / `Down` | Switch fields |

### Home

| Key | Action |
|-----|--------|
| `Up` / `Down` / `PageUp` / `PageDown` / `Home` / `End` | Move selection |
| `Enter` | Open selected item |

### Provision Full

| Key | Action |
|-----|--------|
| `Tab` / `Up` / `Down` | Switch fields |
| `Enter` | Start provisioning |
| `F5` | Load saved base URI from the selected instance |
| `Esc` | Back to Home |

### Migrate Database

| Phase | Key | Action |
|-------|-----|--------|
| Select source | `Enter` | Confirm source connection |
| Select destination instance | `Enter` | Confirm destination instance |
| Enter DB name | `Enter` | Start migration |
| Running | `Esc` | Back to Home |

`Up`/`Down` to navigate lists. `Esc` goes back one phase.

### Backup Database

| Key | Action |
|-----|--------|
| `Up` / `Down` | Select source connection |
| `Enter` | Start backup |

Backups are saved to `backups/{db_name}_{timestamp}.pgdump.enc`.

### Restore Database

| Phase | Key | Action |
|-------|-----|--------|
| Enter file path | `Enter` | Confirm backup file path |
| Select instance | `Enter` | Confirm destination instance |
| Enter DB name | `Enter` | Start restore |
| Running | `Esc` | Back to Home |

### View Saved Connections

| Key | Action |
|-----|--------|
| `Up` / `Down` / `PageUp` / `PageDown` / `Home` / `End` | Change selection |
| `Enter` | Reveal decrypted connection string |
| `e` | Edit application name |
| `Delete` | Delete record (confirm with `y`) |

### Manage Instances

| Key | Action |
|-----|--------|
| `Enter` | Select/deselect current instance |
| `a` | Add a new instance |
| `Delete` | Delete instance (confirm with `y`) |

### Settings / Change Password

| Key | Action |
|-----|--------|
| `Tab` / `Up` / `Down` | Switch fields |
| `Enter` | Submit |

About screen shows tool versions (native pg_dump or Docker image).

## Storage

The SQLite database is stored in the platform app data directory:

- **macOS**: `~/Library/Application Support/com.lucaseufrasiojcpm.cais/data.sqlite`
- **Linux**: `~/.local/share/com.lucaseufrasiojcpm.cais/data.sqlite`

### Stored data

- Argon2 salt and KDF parameters
- Encrypted password verification blob
- Encrypted instance base URIs
- Encrypted provisioned connection strings (database + extra user)

The master password is **never stored**.

## Testing

```bash
# Unit tests
cargo test --lib

# Integration tests (requires Docker)
RUN_DOCKER_TESTS=1 cargo test --test postgres_integration

# All tests via Makefile (requires Docker)
make test
```

Integration tests spin up ephemeral PostgreSQL containers and verify provisioning, migration, and tool detection.
