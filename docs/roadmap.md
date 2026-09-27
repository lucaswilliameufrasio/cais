# Cais Roadmap

This roadmap is the complete current scope for generic PostgreSQL discovery and
backup integration in Cais. It is organized by implementation dependency. No
personal hostname, IP address, project name, database/container name, credential,
or infrastructure inventory belongs in Cais code, fixtures, or documentation.

## 1. Discovery Model

- Define a normalized, versioned inventory shared by all discovery sources.
- Record source, stable-in-this-inventory identifier, server/container label,
  discovered databases, endpoint, route/context, and access status.
- Distinguish `discovered`, `reachable`, and `authenticated`; finding an
  endpoint must not imply that the current process can connect to it.
- Keep errors attached to the source/result and sanitize them before display or
  serialization.

## 2. Local Host Discovery

- Detect PostgreSQL available on the local host using the current user's
  PostgreSQL client configuration and operating-system defaults. Enumerate
  clusters reported by `pg_lsclusters` on Debian/Ubuntu; probe the default local
  PostgreSQL client endpoint on other systems.
- Query database names only when local authentication permits it.
- Report whether the connection uses a local socket or TCP, and the effective
  endpoint where available.
- If a server is detected but authentication or client tools are unavailable,
  report that state and the missing requirement instead of failing silently.

## 3. Docker Discovery

- Discover running PostgreSQL-compatible containers through the selected local
  Docker daemon/context: PostgreSQL, PostGIS, and TimescaleDB images.
- Collect container identity, image, network attachments, addresses, published
  ports, and database names when querying the container succeeds.
- Distinguish Docker-network-only addresses from host-published endpoints and
  state which execution context can use each address.
- Do not assume container names, image tags, fixed ports, or a particular
  Docker network. Treat container IP addresses as temporary.
- Do not inspect or print environment values containing credentials by
  default.

## 4. SSH Remote-Host Discovery

- Discover local PostgreSQL services and Docker containers visible on a
  user-selected SSH host.
- Reuse OpenSSH configuration and the user's agent where possible; never accept
  private keys as command-line values or place them in inventory output.
- Report whether the discovered endpoint is reachable directly from the caller
  or requires an SSH tunnel, including the remote address and port needed to
  construct that tunnel.
- Do not open tunnels, publish ports, change firewall rules, or modify remote
  services automatically.
- Safely handle remote command arguments and explain unavailable SSH, Docker,
  PostgreSQL, or permission prerequisites.

## 5. `cais discover` CLI

- Add `cais discover` with source selection for `local`, `docker`, and `ssh`.
- Support selecting one or more SSH hosts and choosing human-readable or
  versioned JSON output.
- Give each source a useful partial result when another source is unavailable.
- Keep the JSON schema stable and machine-readable for automation.
- Redact credentials in text and JSON output. Do not persist inventory unless
  the user explicitly supplies an output path.

## 6. Backup Integration

- Preserve the existing `cais backup --database-uri ...` and
  `--database-uri-env ...` interfaces.
- Allow a user to select a discovered server/inventory for backup, reusing
  explicitly supplied credentials rather than trying to recover passwords.
- Verify that the selected URI connects to the selected inventory server by
  comparing the URI and server-reported endpoint against discovered routes.
- Verify connectivity before starting database dumps and reject a URI that
  reaches a different server.
- Allow selecting databases from the discovery result; retain the existing
  behavior of discovering all connectable databases when no selection is given.
- Preserve encryption-before-upload for local and S3-compatible destinations.
- Report partial success/failure per server and database without exposing
  connection strings or secrets.

## 7. Documentation and Verification

- Document generic usage, prerequisites, reachability context, credential
  handling, and troubleshooting for local, Docker, and SSH discovery.
- Unit-test normalization, schema versioning, filtering, partial failures, and
  credential redaction.
- Test local and Docker discovery against disposable PostgreSQL instances.
- Test SSH discovery against a controlled SSH host; do not depend on personal
  infrastructure in routine CI.
- Test backup integration against real local PostgreSQL and an S3-compatible
  test service.
- Run formatting, Clippy, the full test suite, and build before declaring the
  implementation complete.
