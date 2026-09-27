use std::process::{Command, Output};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const DATABASE_QUERY: &str =
    "SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate ORDER BY datname";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryReport {
    pub schema_version: u32,
    pub sources: Vec<SourceStatus>,
    pub servers: Vec<DiscoveredServer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceStatus {
    pub source: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredServer {
    pub id: String,
    pub name: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    pub status: String,
    pub databases: Vec<String>,
    pub endpoints: Vec<DiscoveredEndpoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredEndpoint {
    pub host: String,
    pub port: u16,
    /// `tcp`, `docker-network`, or `ssh-tunnel`.
    pub route: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoverySource {
    Local,
    Docker,
    Ssh,
}

impl DiscoverySource {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "local" => Ok(Self::Local),
            "docker" => Ok(Self::Docker),
            "ssh" => Ok(Self::Ssh),
            _ => anyhow::bail!("discovery source must be local, docker, or ssh"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Docker => "docker",
            Self::Ssh => "ssh",
        }
    }
}

pub fn discover(sources: &[DiscoverySource], ssh_hosts: &[String]) -> DiscoveryReport {
    let mut report = DiscoveryReport {
        schema_version: 1,
        sources: Vec::new(),
        servers: Vec::new(),
    };

    for source in sources {
        match source {
            DiscoverySource::Local => discover_local(&mut report),
            DiscoverySource::Docker => discover_docker(&mut report, None),
            DiscoverySource::Ssh => {
                if ssh_hosts.is_empty() {
                    report.sources.push(SourceStatus {
                        source: source.as_str().to_owned(),
                        status: "unavailable".to_owned(),
                        message: Some("SSH discovery requires at least one --host".to_owned()),
                    });
                } else {
                    for host in ssh_hosts {
                        discover_ssh(&mut report, host);
                    }
                }
            }
        }
    }
    report
}

pub fn load_server(path: &std::path::Path, server_id: &str) -> Result<DiscoveredServer> {
    let contents = std::fs::read(path)
        .with_context(|| format!("failed to read discovery inventory {}", path.display()))?;
    let report: DiscoveryReport = serde_json::from_slice(&contents)
        .context("discovery inventory is not valid versioned Cais JSON")?;
    if report.schema_version != 1 {
        anyhow::bail!(
            "unsupported discovery inventory schema version {}",
            report.schema_version
        )
    }
    let server = report
        .servers
        .iter()
        .find(|server| server.id == server_id)
        .with_context(|| {
            format!("server '{server_id}' was not found in the discovery inventory")
        })?;
    if server.status != "authenticated" {
        anyhow::bail!("server '{server_id}' has no authenticated database inventory")
    }
    if server.databases.is_empty() {
        anyhow::bail!("server '{server_id}' has no enumerated databases in the inventory")
    }
    Ok(server.clone())
}

pub fn load_server_databases(path: &std::path::Path, server_id: &str) -> Result<Vec<String>> {
    Ok(load_server(path, server_id)?.databases)
}

pub fn inventory_matches_connection(
    server: &DiscoveredServer,
    uri_host: &str,
    uri_port: u16,
    connected_host: Option<&str>,
    connected_port: u16,
) -> bool {
    server.endpoints.iter().any(|endpoint| {
        (endpoint.port == uri_port && hosts_match(&endpoint.host, uri_host))
            || (endpoint.port == connected_port
                && connected_host.is_some_and(|host| hosts_match(&endpoint.host, host)))
            || (endpoint.port == connected_port
                && connected_host.is_none()
                && endpoint.route.contains("socket"))
    })
}

fn hosts_match(left: &str, right: &str) -> bool {
    let left = normalize_host(left);
    let right = normalize_host(right);
    if left.eq_ignore_ascii_case(right) {
        return true;
    }
    let is_loopback = |host: &str| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    };
    is_loopback(left) && is_loopback(right)
}

fn normalize_host(host: &str) -> &str {
    host.trim().trim_start_matches('[').trim_end_matches(']')
}

fn discover_local(report: &mut DiscoveryReport) {
    if let Some(host) = std::env::var_os("PGHOST").and_then(|value| value.into_string().ok())
        && !is_local_pg_host(&host)
    {
        report.sources.push(SourceStatus {
            source: "local".to_owned(),
            status: "skipped".to_owned(),
            message: Some(
                "PGHOST selects a remote endpoint; use its explicit URI or --source ssh instead"
                    .to_owned(),
            ),
        });
        return;
    }

    if let Some(clusters) = local_postgres_clusters()
        && clusters.iter().any(|cluster| cluster.online)
    {
        let mut enumerated = 0usize;
        for cluster in clusters.iter().filter(|cluster| cluster.online) {
            let endpoint =
                local_endpoint(std::env::var("PGHOST").unwrap_or_default(), cluster.port);
            match psql_databases_local(Some(cluster.port)) {
                Ok(databases) => {
                    enumerated += 1;
                    report.servers.push(DiscoveredServer {
                        id: format!("local:{}:{}", cluster.name, cluster.port),
                        name: cluster.name.clone(),
                        source: "local".to_owned(),
                        image: None,
                        status: "authenticated".to_owned(),
                        databases,
                        endpoints: vec![endpoint],
                        message: None,
                    });
                }
                Err(_) => {
                    let reachable = local_server_ready(Some(cluster.port));
                    report.servers.push(DiscoveredServer {
                        id: format!("local:{}:{}", cluster.name, cluster.port),
                        name: cluster.name.clone(),
                        source: "local".to_owned(),
                        image: None,
                        status: if reachable {
                            "reachable".to_owned()
                        } else {
                            "detected".to_owned()
                        },
                        databases: Vec::new(),
                        endpoints: vec![endpoint],
                        message: Some(if reachable {
                            "PostgreSQL is accepting connections, but database enumeration requires valid local credentials".to_owned()
                        } else {
                            "The local cluster manager reports this cluster online, but the current user could not verify connectivity".to_owned()
                        }),
                    });
                }
            }
        }
        let online = clusters.iter().filter(|cluster| cluster.online).count();
        report.sources.push(SourceStatus {
            source: "local".to_owned(),
            status: if enumerated == online {
                "complete"
            } else {
                "partial"
            }
            .to_owned(),
            message: Some(format!(
                "Enumerated databases for {enumerated} of {online} online local PostgreSQL cluster(s)"
            )),
        });
        return;
    }

    match psql_databases_local(None) {
        Ok(databases) => {
            let host = std::env::var("PGHOST").unwrap_or_default();
            let port = std::env::var("PGPORT")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(5432);
            let endpoint = if host.is_empty() || host.starts_with('/') {
                DiscoveredEndpoint {
                    host: if host.is_empty() {
                        "local-socket".to_owned()
                    } else {
                        host
                    },
                    port,
                    route: "local-socket".to_owned(),
                    context: None,
                }
            } else {
                DiscoveredEndpoint {
                    host: host.clone(),
                    port,
                    route: "tcp".to_owned(),
                    context: None,
                }
            };
            report.servers.push(DiscoveredServer {
                id: format!("local:{}:{port}", endpoint.host),
                name: "Local PostgreSQL".to_owned(),
                source: "local".to_owned(),
                image: None,
                status: "authenticated".to_owned(),
                databases,
                endpoints: vec![endpoint],
                message: None,
            });
            report.sources.push(SourceStatus {
                source: "local".to_owned(),
                status: "complete".to_owned(),
                message: None,
            });
        }
        Err(message) => {
            if local_server_ready(None) {
                let host = std::env::var("PGHOST").unwrap_or_default();
                let port = std::env::var("PGPORT")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(5432);
                let endpoint = local_endpoint(host, port);
                report.servers.push(DiscoveredServer {
                    id: format!("local:{}:{port}", endpoint.host),
                    name: "Local PostgreSQL".to_owned(),
                    source: "local".to_owned(),
                    image: None,
                    status: "reachable".to_owned(),
                    databases: Vec::new(),
                    endpoints: vec![endpoint],
                    message: Some(
                        "PostgreSQL is accepting connections, but database enumeration requires valid local credentials"
                            .to_owned(),
                    ),
                });
                report.sources.push(SourceStatus {
                    source: "local".to_owned(),
                    status: "partial".to_owned(),
                    message: Some(message.to_string()),
                });
            } else {
                report.sources.push(SourceStatus {
                    source: "local".to_owned(),
                    status: "unavailable".to_owned(),
                    message: Some(message.to_string()),
                });
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalPgCluster {
    name: String,
    port: u16,
    online: bool,
}

fn local_postgres_clusters() -> Option<Vec<LocalPgCluster>> {
    let output = Command::new("pg_lsclusters")
        .arg("--no-header")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_pg_lsclusters(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn parse_pg_lsclusters(output: &str) -> Vec<LocalPgCluster> {
    output
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 4 {
                return None;
            }
            let port = fields[2].parse::<u16>().ok()?;
            Some(LocalPgCluster {
                name: format!("PostgreSQL cluster {}/{}", fields[0], fields[1]),
                port,
                online: fields[3] == "online",
            })
        })
        .collect()
}

fn local_server_ready(port: Option<u16>) -> bool {
    let mut command = Command::new("pg_isready");
    if let Some(port) = port {
        command.args(["-p", &port.to_string()]);
    }
    command
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn psql_databases_local(port: Option<u16>) -> Result<Vec<String>> {
    let mut command = Command::new("psql");
    if let Some(port) = port {
        command.env("PGPORT", port.to_string());
    }
    let output = command
        .args(["-X", "-A", "-t", "-d", "postgres", "-c", DATABASE_QUERY])
        .output()
        .context("PostgreSQL client psql is not installed")?;
    if !output.status.success() {
        anyhow::bail!("could not query PostgreSQL using the current user's configured access")
    }
    Ok(parse_lines(&output.stdout))
}

fn local_endpoint(host: String, port: u16) -> DiscoveredEndpoint {
    if host.is_empty() || host.starts_with('/') {
        DiscoveredEndpoint {
            host: if host.is_empty() {
                "local-socket".to_owned()
            } else {
                host
            },
            port,
            route: "local-socket".to_owned(),
            context: None,
        }
    } else {
        DiscoveredEndpoint {
            host,
            port,
            route: "tcp".to_owned(),
            context: None,
        }
    }
}

fn is_local_pg_host(host: &str) -> bool {
    host.is_empty() || host.starts_with('/') || matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn discover_docker(report: &mut DiscoveryReport, ssh_host: Option<&str>) {
    let list_output = match run_docker_ps(ssh_host) {
        Ok(output) if output.status.success() => output,
        Ok(_) => {
            report.sources.push(SourceStatus {
                source: source_label("docker", ssh_host),
                status: "unavailable".to_owned(),
                message: Some(
                    "Docker is unavailable or the current user cannot access its daemon".to_owned(),
                ),
            });
            return;
        }
        Err(_) => {
            report.sources.push(SourceStatus {
                source: source_label("docker", ssh_host),
                status: "unavailable".to_owned(),
                message: Some("Could not execute the Docker CLI".to_owned()),
            });
            return;
        }
    };

    let mut found = 0usize;
    for line in String::from_utf8_lossy(&list_output.stdout).lines() {
        let Ok(container) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let image = container["Image"].as_str().unwrap_or_default();
        if !is_postgres_image(image) {
            continue;
        }
        let id = container["ID"].as_str().unwrap_or_default();
        let name = container["Names"].as_str().unwrap_or(id);
        if id.is_empty() || !is_safe_container_id(id) {
            continue;
        }

        let inspection = run_docker_inspect(ssh_host, id).ok();
        let parsed_inspection = inspection
            .as_ref()
            .filter(|output| output.status.success())
            .and_then(|output| serde_json::from_slice::<Value>(&output.stdout).ok())
            .unwrap_or(Value::Null);
        let mut endpoints = docker_endpoints(&parsed_inspection);
        if let Some(remote_host) = ssh_host {
            for endpoint in &mut endpoints {
                endpoint.route = "ssh-tunnel".to_owned();
                endpoint.context = Some(format!("remote Docker host {remote_host}"));
            }
            if endpoints.is_empty() {
                for (network, ip) in docker_networks(&parsed_inspection) {
                    if let Some(ip) = ip {
                        endpoints.push(DiscoveredEndpoint {
                            host: ip,
                            port: 5432,
                            route: "ssh-tunnel".to_owned(),
                            context: Some(format!("{remote_host}/{network}")),
                        });
                    }
                }
            }
        } else if let Some(networks) = parsed_inspection["Networks"].as_object() {
            for (network, values) in networks {
                if let Some(ip) = values["IPAddress"]
                    .as_str()
                    .filter(|value| !value.is_empty())
                {
                    endpoints.push(DiscoveredEndpoint {
                        host: ip.to_owned(),
                        port: 5432,
                        route: "docker-network".to_owned(),
                        context: Some(network.clone()),
                    });
                }
            }
        }

        let databases = run_docker_databases(ssh_host, id)
            .ok()
            .filter(|output| output.status.success())
            .map(|output| parse_lines(&output.stdout))
            .unwrap_or_default();
        let query_failed = databases.is_empty();
        report.servers.push(DiscoveredServer {
            id: if let Some(remote_host) = ssh_host {
                format!("ssh:{remote_host}:docker:{id}")
            } else {
                format!("docker:{id}")
            },
            name: name.trim_start_matches('/').to_owned(),
            source: if ssh_host.is_some() { "ssh-docker" } else { "docker" }.to_owned(),
            image: Some(image.to_owned()),
            status: if query_failed {
                "detected".to_owned()
            } else {
                "authenticated".to_owned()
            },
            databases,
            endpoints,
            message: if query_failed {
                Some("Container found, but its database list could not be queried with default local authentication".to_owned())
            } else {
                None
            },
        });
        found += 1;
    }

    report.sources.push(SourceStatus {
        source: source_label("docker", ssh_host),
        status: "complete".to_owned(),
        message: Some(format!("Found {found} PostgreSQL-compatible container(s)")),
    });
}

fn discover_ssh(report: &mut DiscoveryReport, host: &str) {
    if host.trim().is_empty() || host.starts_with('-') || host.chars().any(char::is_whitespace) {
        report.sources.push(SourceStatus {
            source: format!("ssh:{host}"),
            status: "unavailable".to_owned(),
            message: Some(
                "SSH host must be a non-empty OpenSSH host or alias without whitespace".to_owned(),
            ),
        });
        return;
    }

    match psql_databases(Some(host), None) {
        Ok(databases) => {
            let endpoint = ssh_postgres_endpoint(host).unwrap_or(DiscoveredEndpoint {
                host: "local-socket".to_owned(),
                port: 5432,
                route: "ssh-local-socket".to_owned(),
                context: Some(
                    "PostgreSQL is reachable from the remote host over its local socket".to_owned(),
                ),
            });
            let message = if endpoint.route == "ssh-local-socket" {
                "PostgreSQL is reachable on the remote host's local socket; a TCP SSH tunnel is unavailable"
            } else {
                "Use the reported remote listener as the SSH tunnel destination"
            };
            report.servers.push(DiscoveredServer {
                id: format!("ssh:{host}:postgresql"),
                name: format!("PostgreSQL on {host}"),
                source: "ssh".to_owned(),
                image: None,
                status: "authenticated".to_owned(),
                databases,
                endpoints: vec![endpoint],
                message: Some(message.to_owned()),
            });
        }
        Err(message) => report.sources.push(SourceStatus {
            source: format!("ssh:{host}:postgresql"),
            status: "unavailable".to_owned(),
            message: Some(message.to_string()),
        }),
    }
    discover_docker(report, Some(host));
}

fn ssh_postgres_endpoint(host: &str) -> Result<DiscoveredEndpoint> {
    let query = "SELECT coalesce(inet_server_port()::text, current_setting($$port$$)) || '|' || current_setting($$listen_addresses$$)";
    let output = Command::new("ssh")
        .args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "--",
            host,
            "psql",
            "-X",
            "-A",
            "-t",
            "-d",
            "postgres",
            "-c",
        ])
        .arg(format!("'{query}'"))
        .output()
        .context("SSH client is unavailable")?;
    if !output.status.success() {
        anyhow::bail!("could not inspect the remote PostgreSQL TCP listener")
    }
    let line = parse_lines(&output.stdout)
        .into_iter()
        .next()
        .context("remote PostgreSQL returned no listener information")?;
    parse_remote_listener(&line)
}

fn parse_remote_listener(value: &str) -> Result<DiscoveredEndpoint> {
    let (port, listen_addresses) = value
        .split_once('|')
        .context("remote PostgreSQL listener information is malformed")?;
    let port = port
        .parse::<u16>()
        .context("remote PostgreSQL returned an invalid listener port")?;
    let addresses = listen_addresses
        .split(',')
        .map(str::trim)
        .filter(|address| !address.is_empty());
    let addresses: Vec<_> = addresses.collect();
    if addresses.is_empty() {
        return Ok(DiscoveredEndpoint {
            host: "local-socket".to_owned(),
            port,
            route: "ssh-local-socket".to_owned(),
            context: Some("PostgreSQL is not configured to listen on TCP".to_owned()),
        });
    }
    let address = if addresses
        .iter()
        .any(|address| matches!(*address, "*" | "localhost" | "127.0.0.1"))
    {
        "127.0.0.1"
    } else if addresses
        .iter()
        .any(|address| matches!(*address, "::1" | "::"))
    {
        "::1"
    } else {
        addresses[0]
    };
    Ok(DiscoveredEndpoint {
        host: address.to_owned(),
        port,
        route: "ssh-tunnel".to_owned(),
        context: Some("remote PostgreSQL listener".to_owned()),
    })
}

fn psql_databases(ssh_host: Option<&str>, container_id: Option<&str>) -> Result<Vec<String>> {
    let output = if let Some(id) = container_id {
        run_docker_databases(ssh_host, id).context("could not query Docker container")?
    } else if let Some(host) = ssh_host {
        Command::new("ssh")
            .args([
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "--",
                host,
            ])
            .args(["psql", "-X", "-A", "-t", "-d", "postgres", "-c"])
            .arg(format!("'{DATABASE_QUERY}'"))
            .output()
            .context("SSH client is unavailable")?
    } else {
        Command::new("psql")
            .args(["-X", "-A", "-t", "-d", "postgres", "-c", DATABASE_QUERY])
            .output()
            .context("PostgreSQL client psql is not installed")?
    };
    if !output.status.success() {
        anyhow::bail!("could not query PostgreSQL using the current user's configured access")
    }
    Ok(parse_lines(&output.stdout))
}

fn run_docker_ps(ssh_host: Option<&str>) -> Result<Output> {
    command_for_source(ssh_host, "docker")
        .args(["ps"])
        .arg(if ssh_host.is_some() {
            "--format='{{json .}}'"
        } else {
            "--format={{json .}}"
        })
        .output()
        .context("failed to execute Docker CLI")
}

fn run_docker_inspect(ssh_host: Option<&str>, container_id: &str) -> Result<Output> {
    command_for_source(ssh_host, "docker")
        .args(["inspect"])
        .arg(if ssh_host.is_some() {
            "--format='{{json .NetworkSettings}}'"
        } else {
            "--format={{json .NetworkSettings}}"
        })
        .arg(container_id)
        .output()
        .context("failed to inspect Docker container")
}

fn run_docker_databases(ssh_host: Option<&str>, container_id: &str) -> Result<Output> {
    let default_user = command_for_source(ssh_host, "docker")
        .args([
            "exec",
            container_id,
            "psql",
            "-X",
            "-A",
            "-t",
            "-d",
            "postgres",
            "-c",
        ])
        .arg(if ssh_host.is_some() {
            format!("'{DATABASE_QUERY}'")
        } else {
            DATABASE_QUERY.to_owned()
        })
        .output()
        .context("failed to query Docker container")?;
    if default_user.status.success() {
        return Ok(default_user);
    }

    command_for_source(ssh_host, "docker")
        .args([
            "exec",
            "--user",
            "postgres",
            container_id,
            "psql",
            "-X",
            "-A",
            "-t",
            "-U",
            "postgres",
            "-d",
            "postgres",
            "-c",
        ])
        .arg(if ssh_host.is_some() {
            format!("'{DATABASE_QUERY}'")
        } else {
            DATABASE_QUERY.to_owned()
        })
        .output()
        .context("failed to query Docker container")
}

fn command_for_source(ssh_host: Option<&str>, program: &str) -> Command {
    if let Some(host) = ssh_host {
        let mut command = Command::new("ssh");
        command.args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "--",
            host,
            program,
        ]);
        command
    } else {
        Command::new(program)
    }
}

fn source_label(source: &str, ssh_host: Option<&str>) -> String {
    ssh_host.map_or_else(|| source.to_owned(), |host| format!("ssh:{host}:{source}"))
}

fn is_postgres_image(image: &str) -> bool {
    let image = image.to_ascii_lowercase();
    image.contains("postgres") || image.contains("postgis") || image.contains("timescaledb")
}

fn is_safe_container_id(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn parse_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn docker_networks(settings: &Value) -> Vec<(String, Option<String>)> {
    settings["Networks"]
        .as_object()
        .map(|networks| {
            networks
                .iter()
                .map(|(name, details)| {
                    (
                        name.clone(),
                        details["IPAddress"]
                            .as_str()
                            .filter(|ip| !ip.is_empty())
                            .map(str::to_owned),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn docker_endpoints(settings: &Value) -> Vec<DiscoveredEndpoint> {
    let mut endpoints = Vec::new();
    let Some(ports) = settings["Ports"].as_object() else {
        return endpoints;
    };
    for (container_port, bindings) in ports {
        let Some(port) = container_port
            .strip_suffix("/tcp")
            .and_then(|value| value.parse::<u16>().ok())
        else {
            continue;
        };
        if port != 5432 {
            continue;
        }
        let Some(bindings) = bindings.as_array() else {
            continue;
        };
        for binding in bindings {
            let Some(host_port) = binding["HostPort"]
                .as_str()
                .and_then(|value| value.parse::<u16>().ok())
            else {
                continue;
            };
            let host_ip = binding["HostIp"].as_str().unwrap_or_default();
            let host = match host_ip {
                "" | "0.0.0.0" | "::" => "127.0.0.1".to_owned(),
                value if value.contains(':') => format!("[{value}]"),
                value => value.to_owned(),
            };
            endpoints.push(DiscoveredEndpoint {
                host,
                port: host_port,
                route: "tcp".to_owned(),
                context: Some(format!("container-port:{port}")),
            });
        }
    }
    let mut unique = Vec::<DiscoveredEndpoint>::new();
    for endpoint in endpoints {
        if !unique.iter().any(|current| {
            current.host == endpoint.host
                && current.port == endpoint.port
                && current.route == endpoint.route
        }) {
            unique.push(endpoint);
        }
    }
    unique
}

#[cfg(test)]
mod tests {
    use super::{
        DiscoveredEndpoint, DiscoveredServer, DiscoverySource, docker_endpoints, docker_networks,
        inventory_matches_connection, is_postgres_image, is_safe_container_id,
        load_server_databases, parse_pg_lsclusters, parse_remote_listener,
    };
    use serde_json::json;
    use std::io::Write;

    #[test]
    fn parses_only_implemented_generic_sources() {
        assert_eq!(
            DiscoverySource::parse("local").unwrap(),
            DiscoverySource::Local
        );
        assert_eq!(
            DiscoverySource::parse("docker").unwrap(),
            DiscoverySource::Docker
        );
        assert_eq!(DiscoverySource::parse("ssh").unwrap(), DiscoverySource::Ssh);
        assert!(DiscoverySource::parse("aws-rds").is_err());
    }

    #[test]
    fn parses_online_and_stopped_local_postgresql_clusters() {
        let clusters = parse_pg_lsclusters(
            "16 main 5432 online postgres /var/lib/postgresql/16/main /var/log/postgresql/postgresql-16-main.log\n\
             17 staging 5433 down postgres /var/lib/postgresql/17/staging /var/log/postgresql/postgresql-17-staging.log\n",
        );
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].name, "PostgreSQL cluster 16/main");
        assert_eq!(clusters[0].port, 5432);
        assert!(clusters[0].online);
        assert!(!clusters[1].online);
    }

    #[test]
    fn classifies_remote_socket_and_tcp_listener_paths() {
        let socket = parse_remote_listener("5432|").expect("socket endpoint");
        assert_eq!(socket.route, "ssh-local-socket");
        assert_eq!(socket.host, "local-socket");

        let tcp = parse_remote_listener("5544|localhost").expect("tcp endpoint");
        assert_eq!(tcp.route, "ssh-tunnel");
        assert_eq!(tcp.host, "127.0.0.1");
        assert_eq!(tcp.port, 5544);

        let private_listener = parse_remote_listener("6432|10.0.0.8").expect("private endpoint");
        assert_eq!(private_listener.host, "10.0.0.8");
        assert_eq!(private_listener.port, 6432);
    }

    #[test]
    fn inventory_selection_matches_direct_tunnel_and_socket_routes() {
        let server = DiscoveredServer {
            id: "ssh:db-host:docker:abc123".into(),
            name: "postgres".into(),
            source: "ssh-docker".into(),
            image: Some("postgres:18".into()),
            status: "authenticated".into(),
            databases: vec!["app".into()],
            endpoints: vec![DiscoveredEndpoint {
                host: "172.20.0.2".into(),
                port: 5432,
                route: "ssh-tunnel".into(),
                context: Some("db-host/private-net".into()),
            }],
            message: None,
        };

        assert!(inventory_matches_connection(
            &server,
            "127.0.0.1",
            15432,
            Some("172.20.0.2"),
            5432
        ));
        assert!(!inventory_matches_connection(
            &server,
            "127.0.0.1",
            15432,
            Some("172.20.0.3"),
            5432
        ));

        let socket_server = DiscoveredServer {
            endpoints: vec![DiscoveredEndpoint {
                host: "local-socket".into(),
                port: 5432,
                route: "local-socket".into(),
                context: None,
            }],
            ..server
        };
        assert!(inventory_matches_connection(
            &socket_server,
            "localhost",
            5432,
            None,
            5432
        ));
    }

    #[test]
    fn recognizes_common_postgres_compatible_images() {
        assert!(is_postgres_image("postgres:17-alpine"));
        assert!(is_postgres_image("imresamu/postgis:18-3.6-alpine"));
        assert!(is_postgres_image("timescale/timescaledb-ha:pg18"));
        assert!(!is_postgres_image("redis:7"));
    }

    #[test]
    fn container_ids_are_safe_for_remote_cli_arguments() {
        assert!(is_safe_container_id("b6e8f4a19c01"));
        assert!(!is_safe_container_id("id; rm -rf /"));
        assert!(!is_safe_container_id(""));
    }

    #[test]
    fn extracts_published_and_network_only_endpoints() {
        let settings = json!({
            "Ports": {
                "5432/tcp": [{"HostIp":"127.0.0.1", "HostPort":"5434"}],
                "6432/tcp": null
            },
            "Networks": {
                "private-net": {"IPAddress":"172.20.0.4"}
            }
        });
        let endpoints = docker_endpoints(&settings);
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].host, "127.0.0.1");
        assert_eq!(endpoints[0].port, 5434);
        assert_eq!(endpoints[0].route, "tcp");

        let networks = docker_networks(&settings);
        assert_eq!(
            networks,
            vec![("private-net".into(), Some("172.20.0.4".into()))]
        );
    }

    #[test]
    fn docker_unpublished_port_is_not_reported_as_a_public_endpoint() {
        let settings = json!({
            "Ports": {"5432/tcp": null},
            "Networks": {}
        });
        assert!(docker_endpoints(&settings).is_empty());
    }

    #[test]
    fn loads_database_selection_from_a_versioned_inventory() {
        let mut file = tempfile::NamedTempFile::new().expect("inventory file");
        write!(
            file,
            "{}",
            json!({
                "schema_version": 1,
                "sources": [],
                "servers": [{
                    "id": "docker:container-id",
                    "name": "database",
                    "source": "docker",
                    "status": "authenticated",
                    "databases": ["app", "postgres"],
                    "endpoints": [],
                    "message": null
                }]
            })
        )
        .expect("write inventory");

        let databases = load_server_databases(file.path(), "docker:container-id")
            .expect("load server databases");
        assert_eq!(databases, ["app", "postgres"]);
    }

    #[test]
    fn rejects_unsupported_inventory_version() {
        let mut file = tempfile::NamedTempFile::new().expect("inventory file");
        write!(
            file,
            "{}",
            json!({"schema_version": 2, "sources": [], "servers": []})
        )
        .expect("write inventory");
        assert!(load_server_databases(file.path(), "missing").is_err());
    }

    #[test]
    fn rejects_a_server_that_was_only_detected() {
        let mut file = tempfile::NamedTempFile::new().expect("inventory file");
        write!(
            file,
            "{}",
            json!({
                "schema_version": 1,
                "sources": [],
                "servers": [{
                    "id": "docker:container-id",
                    "name": "database",
                    "source": "docker",
                    "status": "detected",
                    "databases": [],
                    "endpoints": [],
                    "message": null
                }]
            })
        )
        .expect("write inventory");
        assert!(load_server_databases(file.path(), "docker:container-id").is_err());
    }
}
