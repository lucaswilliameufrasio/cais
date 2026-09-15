use std::io::{self, IsTerminal, Write, stdout};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use cais::app::App;
use cais::ui;
use crossterm::{
    event::{self, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8080;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.first().is_some_and(|arg| arg == "serve") {
        return serve_from_args(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "reset") {
        return reset_from_args(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "backup") {
        return backup_from_args(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "restore") {
        return restore_from_args(&args[1..]);
    }
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_help();
        return Ok(());
    }
    if !args.is_empty() {
        print_help();
        return Ok(());
    }

    run_tui()
}

fn reset_from_args(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        eprintln!(
            "cais reset [--yes]\n\
             \n\
             Move the vault database and the encrypted backups aside so the next\n\
             launch starts a fresh first-run setup. The moved files cannot be\n\
             opened again without the old master password."
        );
        return Ok(());
    }
    let mut assume_yes = false;
    for arg in args {
        match arg.as_str() {
            "--yes" | "-y" => assume_yes = true,
            other => anyhow::bail!("unknown reset argument '{other}'"),
        }
    }

    if !assume_yes {
        if !io::stdin().is_terminal() {
            anyhow::bail!("refusing to reset without confirmation; pass --yes to skip the prompt");
        }
        println!("Close any running cais instance (TUI or serve) before continuing.");
        println!("This moves the vault (instances, saved connections) and the encrypted");
        println!("backups aside. They cannot be opened again without the old master password.\n");
        print!("Type RESET to continue: ");
        stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if answer.trim() != "RESET" {
            eprintln!("Aborted.");
            return Ok(());
        }
    }

    let reset = cais::storage::reset_vault()?;
    if reset.moved.is_empty() {
        println!("Nothing to reset — the vault is already empty.");
        return Ok(());
    }
    println!("Vault reset. Moved aside:");
    for (from, to) in &reset.moved {
        println!("  {} -> {}", from.display(), to.display());
    }
    println!("Next launch starts a fresh first-run setup.");
    Ok(())
}

fn serve_from_args(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_help();
        return Ok(());
    }

    let mut host = DEFAULT_HOST.to_owned();
    let mut port = DEFAULT_PORT;
    let mut open_browser = true;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--host" => {
                host = iter.next().context("--host requires a value")?.to_owned();
            }
            "--port" => {
                port = iter
                    .next()
                    .context("--port requires a value")?
                    .parse()
                    .context("--port must be a number")?;
            }
            "--no-browser" => open_browser = false,
            other => anyhow::bail!("unknown serve argument '{other}'"),
        }
    }

    cais::web::serve(&host, port, open_browser)
}

fn backup_from_args(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_backup_help();
        return Ok(());
    }
    let mut database_uri = None;
    let mut database_uri_env = None;
    let mut name = None;
    let mut output = None;
    let mut key_env = "CAIS_BACKUP_ENCRYPTION_KEY".to_owned();
    let mut databases = Vec::new();
    let mut config = cais::models::BackupConfig::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--database-uri" => database_uri = Some(required_value(&mut iter, arg)?),
            "--database-uri-env" => database_uri_env = Some(required_value(&mut iter, arg)?),
            "--name" => name = Some(required_value(&mut iter, arg)?),
            "--output" => output = Some(PathBuf::from(required_value(&mut iter, arg)?)),
            "--encryption-key-env" => key_env = required_value(&mut iter, arg)?,
            "--database" => databases.push(required_value(&mut iter, arg)?),
            "--no-globals" => config.include_globals = false,
            "--include-role-passwords" => config.include_role_passwords = true,
            other => anyhow::bail!("unknown backup argument '{other}'"),
        }
    }
    let source = resolve_database_uri(database_uri, database_uri_env)?;
    let name = name.context("backup requires --name")?;
    let output = output.context("backup requires --output")?;
    let key = encryption_key(&key_env)?;
    let backend = cais::postgres::check_pg_tools();
    let staging = if is_s3_uri(&output) {
        let dir = tempfile::tempdir().context("failed to create S3 staging directory")?;
        Some(dir)
    } else {
        None
    };
    let local_dir = staging.as_ref().map_or(output.as_path(), |dir| dir.path());
    let identity = (uuid::Uuid::now_v7().to_string(), hostname());
    let outcome = cais::postgres::backup_instance_with_progress(
        &source,
        &key,
        local_dir,
        &backend,
        cais::postgres::InstanceBackupContext {
            instance_name: &name,
            machine_id: &identity.0,
            hostname: &identity.1,
        },
        &if databases.is_empty() {
            cais::postgres::discover_databases(&source)?
                .into_iter()
                .map(|db| db.name)
                .collect()
        } else {
            databases
        },
        &config,
        &mut |message| eprintln!("{message}"),
    )?;
    if staging.is_some() {
        let destination = s3_object_destination(&output, Path::new(&outcome.file_path))?;
        s3_copy(Path::new(&outcome.file_path), &destination)?;
        println!("Backup uploaded to {destination}");
    } else {
        println!("Backup written to {}", outcome.file_path);
    }
    Ok(())
}

fn restore_from_args(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_restore_help();
        return Ok(());
    }
    let mut source = None;
    let mut database_uri = None;
    let mut database_uri_env = None;
    let mut key_env = "CAIS_BACKUP_ENCRYPTION_KEY".to_owned();
    let mut selected = Vec::new();
    let mut mappings = Vec::new();
    let mut database_name = None;
    let mut policy = cais::models::ConflictPolicy::Skip;
    let mut yes = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--source" | "--backup" => source = Some(required_value(&mut iter, arg)?),
            "--database-uri" | "--target-uri" => {
                database_uri = Some(required_value(&mut iter, arg)?)
            }
            "--database-uri-env" | "--target-uri-env" => {
                database_uri_env = Some(required_value(&mut iter, arg)?)
            }
            "--encryption-key-env" => key_env = required_value(&mut iter, arg)?,
            "--database" => selected.push(required_value(&mut iter, arg)?),
            "--database-name" => database_name = Some(required_value(&mut iter, arg)?),
            "--map" | "--rename" => mappings.push(parse_mapping(&required_value(&mut iter, arg)?)?),
            "--conflict" => policy = parse_conflict_policy(&required_value(&mut iter, arg)?)?,
            "--yes" | "-y" => yes = true,
            other => anyhow::bail!("unknown restore argument '{other}'"),
        }
    }
    if !yes {
        anyhow::bail!("restore is destructive or mutating; pass --yes to execute")
    }
    let source = source.context("restore requires --source")?;
    let target = resolve_database_uri(database_uri, database_uri_env)?;
    let key = encryption_key(&key_env)?;
    let backend = cais::postgres::check_pg_tools();
    let staging = tempfile::tempdir().context("failed to create restore staging directory")?;
    let local_source = if is_s3_uri(Path::new(&source)) {
        let path = staging.path().join("restore.bundle.enc");
        s3_copy(&source, &path)?;
        path
    } else {
        PathBuf::from(source)
    };
    let outcomes = if cais::postgres::is_instance_backup(&local_source, &key)? {
        if database_name.is_some() {
            anyhow::bail!("--database-name is only valid for a single-database backup")
        }
        cais::postgres::restore_instance_selected_with_progress(
            &local_source,
            &key,
            &target,
            &backend,
            policy,
            &selected,
            &mappings,
            &mut |message| eprintln!("{message}"),
        )?
    } else {
        if !selected.is_empty() || mappings.len() > 1 {
            anyhow::bail!(
                "single-database backups accept only one --map SOURCE=DESTINATION mapping"
            )
        }
        let (metadata, _) = cais::postgres::read_encrypted_dump(&local_source, &key)?;
        let source_name = metadata
            .as_ref()
            .map(|value| value.database_name.as_str())
            .context("single-database backup has no database metadata")?;
        if let Some((mapped_source, _)) = mappings.first()
            && mapped_source != source_name
        {
            anyhow::bail!("single-database backup contains '{source_name}', not '{mapped_source}'")
        }
        if database_name.is_some() && !mappings.is_empty() {
            anyhow::bail!("use either --database-name or --map for a single-database backup")
        }
        let destination = database_name
            .or_else(|| mappings.first().map(|(_, value)| value.clone()))
            .unwrap_or_else(|| source_name.to_owned());
        let outcome = cais::postgres::restore_database_with_progress(
            &local_source,
            &key,
            &target,
            &destination,
            &backend,
            policy,
            false,
            &mut |message| eprintln!("{message}"),
        )?;
        vec![outcome]
    };
    println!("Restored {} database(s)", outcomes.len());
    Ok(())
}

fn required_value<'a>(iter: &mut std::slice::Iter<'a, String>, option: &str) -> Result<String> {
    iter.next()
        .cloned()
        .with_context(|| format!("{option} requires a value"))
}

fn resolve_database_uri(uri: Option<String>, env_name: Option<String>) -> Result<String> {
    match (uri, env_name) {
        (Some(_), Some(_)) => {
            anyhow::bail!("use only one of --database-uri and --database-uri-env")
        }
        (Some(uri), None) => Ok(uri),
        (None, Some(name)) => std::env::var(&name).with_context(|| format!("{name} is not set")),
        (None, None) => anyhow::bail!("provide --database-uri or --database-uri-env"),
    }
}

fn encryption_key(env_name: &str) -> Result<Vec<u8>> {
    let encoded = std::env::var(env_name).with_context(|| format!("{env_name} is not set"))?;
    let key = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded.trim())
        .context("backup key must be standard base64")?;
    if key.len() != 32 {
        anyhow::bail!("backup key must decode to exactly 32 bytes")
    }
    Ok(key)
}

fn hostname() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_owned())
}

fn parse_mapping(value: &str) -> Result<(String, String)> {
    let (source, destination) = value
        .split_once('=')
        .context("mapping must use SOURCE=DESTINATION")?;
    if source.is_empty() || destination.is_empty() || destination.contains('=') {
        anyhow::bail!("mapping must use non-empty SOURCE=DESTINATION")
    }
    Ok((source.to_owned(), destination.to_owned()))
}

fn parse_conflict_policy(value: &str) -> Result<cais::models::ConflictPolicy> {
    match value {
        "fail" => Ok(cais::models::ConflictPolicy::Fail),
        "skip" => Ok(cais::models::ConflictPolicy::Skip),
        "replace" => Ok(cais::models::ConflictPolicy::Replace),
        _ => anyhow::bail!("conflict policy must be fail, skip, or replace"),
    }
}

fn is_s3_uri(path: &Path) -> bool {
    path.to_string_lossy().starts_with("s3://")
}

fn s3_object_destination(prefix: &Path, local: &Path) -> Result<String> {
    let prefix = prefix.to_string_lossy();
    let filename = local
        .file_name()
        .and_then(|name| name.to_str())
        .context("backup output has no filename")?;
    if prefix.ends_with('/') {
        Ok(format!("{prefix}{filename}"))
    } else {
        Ok(prefix.into_owned())
    }
}

fn s3_copy(source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<()> {
    let mut command = std::process::Command::new("aws");
    command
        .args(["s3", "cp"])
        .arg(source.as_ref())
        .arg(destination.as_ref())
        .arg("--only-show-errors");
    if let Ok(endpoint) = std::env::var("CAIS_S3_ENDPOINT_URL")
        && !endpoint.trim().is_empty()
    {
        command.args(["--endpoint-url", &endpoint]);
    }
    let output = command
        .output()
        .context("failed to execute aws; install AWS CLI for S3 storage")?;
    if !output.status.success() {
        anyhow::bail!(
            "aws s3 cp failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    Ok(())
}

fn print_backup_help() {
    eprintln!(
        "cais backup --database-uri <URI>|--database-uri-env <VAR> --name <NAME> --output <DIR|s3://...> [OPTIONS]\n\
         \n\
         Creates an encrypted DBP2 bundle. The key is read from CAIS_BACKUP_ENCRYPTION_KEY\n\
         (standard base64, exactly 32 decoded bytes), or --encryption-key-env <VAR>.\n\
         \n\
         --database <NAME>             Select a database (repeatable; default: all)\n\
         --no-globals                  Exclude cluster roles and memberships\n\
         --include-role-passwords     Include role passwords in globals\n\
         S3 requires the aws CLI; set CAIS_S3_ENDPOINT_URL for S3-compatible providers."
    );
}

fn print_restore_help() {
    eprintln!(
        "cais restore --source <PATH|s3://...> --database-uri <URI>|--database-uri-env <VAR> --yes [OPTIONS]\n\
         \n\
         Restores an encrypted backup. The key is read from CAIS_BACKUP_ENCRYPTION_KEY\n\
         (standard base64, exactly 32 decoded bytes), or --encryption-key-env <VAR>.\n\
         \n\
         --database <NAME>             Select a bundled database (repeatable; default: all)\n\
         --database-name <NAME>        Destination name for a single-database backup\n\
         --map SOURCE=DESTINATION     Rename a bundled database (repeatable)\n\
         --rename SOURCE=DESTINATION  Alias for --map\n\
         --backup                      Alias for --source; --target-uri* alias destination options\n\
         --conflict <fail|skip|replace> Conflict policy (default: skip)\n\
         S3 requires the aws CLI; set CAIS_S3_ENDPOINT_URL for S3-compatible providers."
    );
}

fn print_help() {
    eprintln!(
        "cais — PostgreSQL database management\n\
         \n\
         USAGE:\n\
         \x20 cais                 Run the TUI\n\
         \x20 cais serve [OPTIONS]  Run the local web interface\n\
         \x20 cais backup [OPTIONS] Run an encrypted headless DBP2 backup\n\
         \x20 cais restore [OPTIONS] Restore an encrypted headless DBP2 backup\n\
         \x20 cais reset [--yes]    Move the vault and encrypted backups aside (fresh start)\n\
         \n\
         serve OPTIONS:\n\
         \x20 --host <HOST>       Bind address (default: 127.0.0.1)\n\
         \x20 --port <PORT>       Bind port (default: 8080)\n\
         \x20 --no-browser        Do not auto-open the browser\n\
         \n\
         reset OPTIONS:\n\
         \x20 --yes               Skip interactive confirmation\n\
         Use 'cais backup --help' or 'cais restore --help' for headless options."
    );
}

fn run_tui() -> Result<()> {
    let mut terminal = setup_terminal()?;
    let result = run(&mut terminal);
    restore_terminal(&mut terminal)?;
    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let mut app = App::new()?;
    loop {
        if let Err(error) = app.poll_background_tasks() {
            app.set_status(format!("Error: {error:#}"));
        }

        terminal.draw(|frame| {
            let _ = ui::draw(frame, &mut app);
        })?;

        if app.should_quit {
            break;
        }

        if event::poll(Duration::from_millis(200))?
            && let Event::Key(key) = event::read()?
            && let Err(error) = cais::app::handle_key_event(&mut app, key)
        {
            app.set_status(format!("Error: {error:#}"));
        }
    }
    Ok(())
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_conflict_policy, parse_mapping};
    use cais::models::ConflictPolicy;

    #[test]
    fn parses_database_rename_mapping() {
        assert_eq!(
            parse_mapping("orders=orders_restored").expect("mapping"),
            ("orders".to_owned(), "orders_restored".to_owned())
        );
    }

    #[test]
    fn rejects_malformed_database_rename_mapping() {
        assert!(parse_mapping("orders").is_err());
        assert!(parse_mapping("=orders").is_err());
        assert!(parse_mapping("orders=").is_err());
        assert!(parse_mapping("orders=restored=extra").is_err());
    }

    #[test]
    fn parses_all_conflict_policies() {
        assert_eq!(
            parse_conflict_policy("fail").expect("fail"),
            ConflictPolicy::Fail
        );
        assert_eq!(
            parse_conflict_policy("skip").expect("skip"),
            ConflictPolicy::Skip
        );
        assert_eq!(
            parse_conflict_policy("replace").expect("replace"),
            ConflictPolicy::Replace
        );
        assert!(parse_conflict_policy("unknown").is_err());
    }
}
