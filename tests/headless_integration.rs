use std::net::{SocketAddr, TcpStream};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use postgres::{Client, NoTls};
use testcontainers::{GenericImage, ImageExt, core::ContainerPort, runners::SyncRunner};

struct DockerPostgres {
    port: u16,
    _container: testcontainers::Container<GenericImage>,
}

impl DockerPostgres {
    fn start() -> Self {
        let container = GenericImage::new("postgres", "17")
            .with_env_var("POSTGRES_PASSWORD", "postgres")
            .with_env_var("POSTGRES_USER", "postgres")
            .with_env_var("POSTGRES_DB", "postgres")
            .start()
            .expect("start PostgreSQL container");
        let port = container
            .get_host_port_ipv4(5432)
            .expect("mapped PostgreSQL port");
        let postgres = Self {
            port,
            _container: container,
        };
        postgres.wait_ready();
        postgres
    }

    fn url(&self) -> String {
        format!(
            "postgresql://postgres:postgres@127.0.0.1:{}/postgres",
            self.port
        )
    }

    fn db_url(&self, database: &str) -> String {
        format!(
            "postgresql://postgres:postgres@127.0.0.1:{}/{}",
            self.port, database
        )
    }

    fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if Client::connect(&self.url(), NoTls).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        panic!("PostgreSQL container did not become ready");
    }
}

struct Floci {
    port: u16,
    _container: testcontainers::Container<GenericImage>,
}

impl Floci {
    fn start() -> Self {
        let container = GenericImage::new(
            "floci/floci",
            "2.0.1@sha256:4e451c39c7bb88e3cd4f87e8fc0c25d5b47695a51185d521e2241fa00486e8eb",
        )
        .with_exposed_port(ContainerPort::Tcp(4566))
        .start()
        .expect("start Floci container");
        let port = container
            .get_host_port_ipv4(4566)
            .expect("mapped Floci port");
        let floci = Self {
            port,
            _container: container,
        };
        floci.wait_ready();
        floci
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn wait_ready(&self) {
        let address: SocketAddr = format!("127.0.0.1:{}", self.port)
            .parse()
            .expect("Floci socket address");
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if TcpStream::connect_timeout(&address, Duration::from_millis(250)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("Floci container did not become ready");
    }
}

fn run_cais(args: &[&str], env: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cais"))
        .args(args)
        .envs(env.iter().copied())
        .output()
        .expect("run cais")
}

fn run_aws(endpoint: &str, args: &[&str]) -> Output {
    Command::new("aws")
        .args(args)
        .env("AWS_ACCESS_KEY_ID", "test")
        .env("AWS_SECRET_ACCESS_KEY", "test")
        .env("AWS_DEFAULT_REGION", "us-east-1")
        .arg("--endpoint-url")
        .arg(endpoint)
        .output()
        .expect("run aws CLI")
}

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn headless_backup_upload_and_restore_round_trip_through_floci() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    let pg = DockerPostgres::start();
    let floci = Floci::start();
    let bucket = "cais-headless-e2e";
    let key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    let mut admin = Client::connect(&pg.url(), NoTls).expect("connect to PostgreSQL");
    admin
        .batch_execute("CREATE DATABASE source_a")
        .expect("create source_a");
    admin
        .batch_execute("CREATE DATABASE source_b")
        .expect("create source_b");
    drop(admin);

    let mut source_a = Client::connect(&pg.db_url("source_a"), NoTls).expect("connect source_a");
    source_a
        .batch_execute(
            "CREATE TABLE records (id integer PRIMARY KEY, value text NOT NULL); INSERT INTO records VALUES (1, 'a');",
        )
        .expect("seed source_a");
    drop(source_a);

    let mut source_b = Client::connect(&pg.db_url("source_b"), NoTls).expect("connect source_b");
    source_b
        .batch_execute(
            "CREATE TABLE records (id integer PRIMARY KEY, value text NOT NULL); INSERT INTO records VALUES (1, 'b');",
        )
        .expect("seed source_b");
    drop(source_b);

    assert_success(
        &run_aws(&floci.endpoint(), &["s3", "mb", &format!("s3://{bucket}")]),
        "create Floci bucket",
    );

    let backup = run_cais(
        &[
            "backup",
            "--database-uri",
            &pg.url(),
            "--name",
            "headless-e2e",
            "--output",
            &format!("s3://{bucket}/backups/"),
            "--database",
            "source_a",
            "--database",
            "source_b",
            "--no-globals",
        ],
        &[
            ("CAIS_BACKUP_ENCRYPTION_KEY", key),
            ("AWS_ACCESS_KEY_ID", "test"),
            ("AWS_SECRET_ACCESS_KEY", "test"),
            ("AWS_DEFAULT_REGION", "us-east-1"),
            ("CAIS_S3_ENDPOINT_URL", &floci.endpoint()),
        ],
    );
    assert_success(&backup, "headless backup upload");

    let listing = run_aws(
        &floci.endpoint(),
        &[
            "s3",
            "ls",
            &format!("s3://{bucket}/backups/"),
            "--recursive",
        ],
    );
    assert_success(&listing, "list uploaded backup");
    let object_key = String::from_utf8_lossy(&listing.stdout)
        .lines()
        .find_map(|line| line.split_whitespace().last())
        .expect("uploaded backup object")
        .to_owned();

    let restore = run_cais(
        &[
            "restore",
            "--backup",
            &format!("s3://{bucket}/{object_key}"),
            "--target-uri",
            &pg.url(),
            "--map",
            "source_a=restored_a",
            "--map",
            "source_b=restored_b",
            "--conflict",
            "fail",
            "--yes",
        ],
        &[
            ("CAIS_BACKUP_ENCRYPTION_KEY", key),
            ("AWS_ACCESS_KEY_ID", "test"),
            ("AWS_SECRET_ACCESS_KEY", "test"),
            ("AWS_DEFAULT_REGION", "us-east-1"),
            ("CAIS_S3_ENDPOINT_URL", &floci.endpoint()),
        ],
    );
    assert_success(&restore, "headless restore download and restore");

    let mut verify = Client::connect(&pg.url(), NoTls).expect("connect for verification");
    for database in ["restored_a", "restored_b"] {
        let exists = verify
            .query_opt("SELECT 1 FROM pg_database WHERE datname = $1", &[&database])
            .expect("check restored database")
            .is_some();
        assert!(exists, "restored database {database} is missing");
    }
    drop(verify);

    let mut restored_a =
        Client::connect(&pg.db_url("restored_a"), NoTls).expect("connect restored_a");
    let value: String = restored_a
        .query_one("SELECT value FROM records WHERE id = 1", &[])
        .expect("read restored_a")
        .get(0);
    assert_eq!(value, "a");
    drop(restored_a);

    let mut restored_b =
        Client::connect(&pg.db_url("restored_b"), NoTls).expect("connect restored_b");
    let value: String = restored_b
        .query_one("SELECT value FROM records WHERE id = 1", &[])
        .expect("read restored_b")
        .get(0);
    assert_eq!(value, "b");
}
