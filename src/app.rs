use anyhow::{Context, Result};
use ratatui::widgets::ListState;
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use zeroize::Zeroizing;

use crate::crypto;
use crate::models::{
    ActiveQuery, BackupConfig, BackupFile, BackupMetadata, BackupOutcome, ConflictPolicy,
    DiscoveredDatabase, ExtraUserProvisionOutcome, PgToolBackend, ProvisionFullOutcome,
    ProvisionFullRequest, ProvisionOutcome, SavedConnectionRecord,
};
use crate::postgres::{
    InstanceBackupContext, backup_database_with_progress, backup_instance_with_progress,
    check_connection_health, check_pg_tools, check_version_warning, fetch_active_queries,
    is_instance_backup, kill_query, mask_connection_string, migrate_database_with_progress,
    parse_database_url, provision_full_with_progress, resolve_docker_image,
    restore_database_with_progress, restore_instance_with_progress,
};
use crate::storage::{Storage, backup_directory, display_database_path};
use crate::validation::{normalize_application_name, validate_database_name};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    FirstRun,
    Unlock,
    Home,
    ProvisionFull,
    MigrateDatabase,
    BackupDatabase,
    RestoreDatabase,
    ViewSavedConnections,
    About,
    Settings,
    ChangePassword,
    ManageInstances,
    AddInstance,
    EditConnectionAppName,
    ManageBackups,
    ActiveQueries,
    ConnectionWizard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeItem {
    ProvisionFull,
    MigrateDatabase,
    BackupDatabase,
    RestoreDatabase,
    ManageBackups,
    Instances,
    ConnectionWizard,
    About,
    ViewSavedConnections,
    Settings,
    Quit,
}

impl HomeItem {
    pub const ALL: [HomeItem; 11] = [
        HomeItem::ProvisionFull,
        HomeItem::MigrateDatabase,
        HomeItem::BackupDatabase,
        HomeItem::RestoreDatabase,
        HomeItem::ManageBackups,
        HomeItem::Instances,
        HomeItem::ConnectionWizard,
        HomeItem::About,
        HomeItem::ViewSavedConnections,
        HomeItem::Settings,
        HomeItem::Quit,
    ];

    pub fn label(self) -> &'static str {
        match self {
            HomeItem::ProvisionFull => "Provision full (database + extra user)",
            HomeItem::MigrateDatabase => "Migrate database",
            HomeItem::BackupDatabase => "Backup database",
            HomeItem::RestoreDatabase => "Restore database",
            HomeItem::ManageBackups => "Manage backups",
            HomeItem::Instances => "Manage instances",
            HomeItem::ConnectionWizard => "Connection wizard (URL builder)",
            HomeItem::About => "About",
            HomeItem::ViewSavedConnections => "View saved connections",
            HomeItem::Settings => "Settings",
            HomeItem::Quit => "Quit",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsItem {
    ChangeMasterPassword,
    Back,
}

impl SettingsItem {
    pub const ALL: [SettingsItem; 2] = [SettingsItem::ChangeMasterPassword, SettingsItem::Back];

    pub fn label(self) -> &'static str {
        match self {
            SettingsItem::ChangeMasterPassword => "Change master password",
            SettingsItem::Back => "Back",
        }
    }
}

#[derive(Debug, Clone)]
pub enum HealthStatus {
    Unknown,
    Checking,
    Ok { latency_ms: u64, version: String },
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputTarget {
    FirstRunPassword,
    FirstRunConfirm,
    UnlockPassword,
    ProvisionDatabaseName,
    ProvisionApplicationName,
    ProvisionFullBaseUrl,
    ProvisionFullDatabaseName,
    ProvisionFullApplicationName,
    ProvisionFullExtraUsername,
    ProvisionFullExtraApplicationName,
    ExtraUserDatabaseName,
    ExtraUserUsername,
    ExtraUserApplicationName,
    ChangeCurrentPassword,
    ChangeNewPassword,
    ChangeConfirmPassword,
    InstanceName,
    InstanceUrl,
    MigrateDestDbName,
    RestoreFilePath,
    RestoreDestDbName,
    WizardInstanceName,
    WizardHost,
    WizardPort,
    WizardUsername,
    WizardPassword,
    WizardDatabase,
    WizardSslMode,
}

#[derive(Debug, Clone)]
pub struct TextField {
    pub label: &'static str,
    pub value: String,
    pub secret: bool,
    pub cursor_pos: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupPhase {
    SelectSource,
    SelectDatabases,
    ConfigureBackup,
    Running,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestorePhase {
    EnterFilePath,
    SelectDestInstance,
    EnterDestDbName,
    PreviewBundle,
    Running,
}

impl TextField {
    fn new(label: &'static str, secret: bool) -> Self {
        Self {
            label,
            value: String::new(),
            secret,
            cursor_pos: 0,
        }
    }

    pub fn display_value(&self) -> String {
        if self.secret {
            "*".repeat(self.value.chars().count())
        } else {
            self.value.clone()
        }
    }

    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor_pos = 0;
    }

    pub fn insert_char(&mut self, ch: char) {
        let pos = self.byte_idx();
        self.value.insert(pos, ch);
        self.cursor_pos += 1;
    }

    pub fn delete_before_cursor(&mut self) {
        if self.cursor_pos == 0 {
            return;
        }
        let pos = self.byte_idx();
        let prev = self.value[..pos].char_indices().next_back();
        if let Some((i, _)) = prev {
            self.value.drain(i..pos);
            self.cursor_pos -= 1;
        }
    }

    pub fn delete_after_cursor(&mut self) {
        let pos = self.byte_idx();
        if pos >= self.value.len() {
            return;
        }
        if let Some((_, ch)) = self.value[pos..].char_indices().next() {
            self.value.drain(pos..pos + ch.len_utf8());
        }
    }

    fn byte_idx(&self) -> usize {
        self.value
            .char_indices()
            .nth(self.cursor_pos)
            .map(|(i, _)| i)
            .unwrap_or(self.value.len())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigratePhase {
    SelectSource,
    SelectDestInstance,
    EnterDestDbName,
    Running,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditTarget {
    Database(i64),
    ExtraUser(i64),
}

pub struct App {
    pub storage: Storage,
    pub key: Option<Zeroizing<Vec<u8>>>,
    pub screen: Screen,
    pub should_quit: bool,
    pub status: String,
    pub logs: Vec<String>,
    pub pending_operation: bool,
    pub pg_tool_backend: PgToolBackend,
    pub focused_input: InputTarget,
    pub instances: Vec<String>,
    pub selected_instance_idx: usize,
    pub selected_instance: Option<String>,
    pub confirming_delete_instance: bool,
    pub instance_name_field: TextField,
    pub instance_url_field: TextField,
    pub migrate_phase: MigratePhase,
    pub migrate_source_list_state: ListState,
    pub migrate_dest_idx: usize,
    pub migrate_source_record: Option<SavedConnectionRecord>,
    pub migrate_dest_instance: Option<String>,
    pub migrate_dest_db_name: TextField,
    pub backup_phase: BackupPhase,
    pub backup_source_list_state: ListState,
    pub backup_source_record: Option<SavedConnectionRecord>,
    pub backup_database_list_state: ListState,
    pub backup_databases: Vec<DiscoveredDatabase>,
    pub backup_selected_databases: Vec<bool>,
    pub backup_config: BackupConfig,
    pub backup_source_version: String,
    pub restore_phase: RestorePhase,
    pub restore_file_path: TextField,
    pub restore_dest_idx: usize,
    pub restore_dest_instance: Option<String>,
    pub restore_dest_db_name: TextField,
    pub restore_conflict_policy: ConflictPolicy,
    pub restore_preview_databases: Vec<String>,
    pub restore_preview_globals: bool,
    pub restore_preview_source_version: String,
    pub restore_preview_source_instance: String,
    pub home_list_state: ListState,
    pub settings_list_state: ListState,
    pub saved_connections_list_state: ListState,
    pub reveal_selected_connection_string: bool,
    pub confirming_delete: bool,
    pub editing_app_name_field: TextField,
    pub editing_target: Option<EditTarget>,
    pub first_run_password: TextField,
    pub first_run_confirm: TextField,
    pub unlock_password: TextField,
    pub provision_database_name: TextField,
    pub provision_application_name: TextField,
    pub provision_full_base_url: TextField,
    pub provision_full_database_name: TextField,
    pub provision_full_application_name: TextField,
    pub provision_full_extra_username: TextField,
    pub provision_full_extra_application_name: TextField,
    pub extra_user_database_name: TextField,
    pub extra_user_username: TextField,
    pub extra_user_application_name: TextField,
    pub change_current_password: TextField,
    pub change_new_password: TextField,
    pub change_confirm_password: TextField,
    pub backups_list: Vec<crate::models::BackupFile>,
    pub backups_list_state: ListState,
    pub confirming_delete_backup: bool,
    pub health_status: Arc<Mutex<HashMap<String, HealthStatus>>>,
    pub active_queries: Vec<ActiveQuery>,
    pub queries_list_state: ListState,
    pub queries_instance: String,
    pub queries_url: Option<String>,
    pub confirming_kill_query: bool,
    pub query_kill_result: String,
    pub wizard_instance_name: TextField,
    pub wizard_host: TextField,
    pub wizard_port: TextField,
    pub wizard_username: TextField,
    pub wizard_password: TextField,
    pub wizard_database: TextField,
    pub wizard_ssl_mode: TextField,
    pub search_query: String,
    pub search_active: bool,
    pub machine_id: String,
    pub hostname: String,
    operation_receiver: Option<Receiver<WorkerEvent>>,
}

enum WorkerEvent {
    Log(String),
    Finished(String, OperationResult),
}

enum OperationResult {
    Full(Result<ProvisionFullOutcome, String>),
    Backup(Result<BackupOutcome, String>),
    Batch(Result<Vec<ProvisionFullOutcome>, String>),
}

impl App {
    pub fn new() -> Result<Self> {
        let storage = Storage::open()?;

        let (machine_id, hostname) = storage.ensure_machine_identity()?;

        let screen = if storage.is_initialized()? {
            Screen::Unlock
        } else {
            Screen::FirstRun
        };
        let status = format!("SQLite path: {}", display_database_path()?);

        let instances = storage.list_instances()?;
        let pg_tool_backend = check_pg_tools();
        let backups_list = Self::list_backup_files();

        let mut home_list_state = ListState::default();
        home_list_state.select(Some(0));
        let mut settings_list_state = ListState::default();
        settings_list_state.select(Some(0));
        let mut saved_connections_list_state = ListState::default();
        saved_connections_list_state.select(None);

        Ok(Self {
            storage,
            key: None,
            machine_id,
            hostname,
            screen,
            should_quit: false,
            status,
            logs: Vec::new(),
            pending_operation: false,
            pg_tool_backend,
            focused_input: match screen {
                Screen::FirstRun => InputTarget::FirstRunPassword,
                _ => InputTarget::UnlockPassword,
            },
            instances,
            selected_instance_idx: 0,
            selected_instance: None,
            confirming_delete_instance: false,
            instance_name_field: TextField::new("Instance name", false),
            instance_url_field: TextField::new("Base DATABASE_URL", false),
            migrate_phase: MigratePhase::SelectSource,
            migrate_source_list_state: ListState::default(),
            migrate_dest_idx: 0,
            migrate_source_record: None,
            migrate_dest_instance: None,
            migrate_dest_db_name: TextField::new("Destination database name", false),
            backup_phase: BackupPhase::SelectSource,
            backup_source_list_state: ListState::default(),
            backup_source_record: None,
            backup_database_list_state: ListState::default(),
            backup_databases: Vec::new(),
            backup_selected_databases: Vec::new(),
            backup_config: BackupConfig::default(),
            backup_source_version: String::new(),
            restore_phase: RestorePhase::EnterFilePath,
            restore_file_path: TextField::new("Backup file path", false),
            restore_dest_idx: 0,
            restore_dest_instance: None,
            restore_dest_db_name: TextField::new("Destination database name", false),
            restore_conflict_policy: ConflictPolicy::Skip,
            restore_preview_databases: Vec::new(),
            restore_preview_globals: false,
            restore_preview_source_version: String::new(),
            restore_preview_source_instance: String::new(),
            home_list_state,
            settings_list_state,
            saved_connections_list_state,
            reveal_selected_connection_string: false,
            confirming_delete: false,
            editing_app_name_field: TextField::new("Application name", false),
            editing_target: None,
            first_run_password: TextField::new("Master password", true),
            first_run_confirm: TextField::new("Confirm password", true),
            unlock_password: TextField::new("Master password", true),
            provision_full_base_url: TextField::new("Base DATABASE_URL", false),
            provision_full_database_name: TextField::new("Database name", false),
            provision_full_application_name: TextField::new("Application name", false),
            provision_full_extra_username: TextField::new("Extra username (optional)", false),
            provision_full_extra_application_name: TextField::new(
                "Extra app name (optional)",
                false,
            ),
            provision_database_name: TextField::new("Database name", false),
            provision_application_name: TextField::new("Application name", false),
            extra_user_database_name: TextField::new("Existing database", false),
            extra_user_username: TextField::new("New username", false),
            extra_user_application_name: TextField::new("Application name", false),
            change_current_password: TextField::new("Current password", true),
            change_new_password: TextField::new("New password", true),
            change_confirm_password: TextField::new("Confirm new password", true),
            backups_list,
            backups_list_state: ListState::default(),
            confirming_delete_backup: false,
            health_status: Arc::new(Mutex::new(HashMap::new())),
            active_queries: Vec::new(),
            queries_list_state: ListState::default(),
            queries_instance: String::new(),
            queries_url: None,
            confirming_kill_query: false,
            query_kill_result: String::new(),
            wizard_instance_name: TextField::new("Instance name", false),
            wizard_host: TextField::new("Host", false),
            wizard_port: TextField::new("Port", false),
            wizard_username: TextField::new("Username", false),
            wizard_password: TextField::new("Password", true),
            wizard_database: TextField::new("Database", false),
            wizard_ssl_mode: TextField::new("SSL mode (disable|prefer|require)", false),
            search_query: String::new(),
            search_active: false,
            operation_receiver: None,
        })
    }

    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
    }

    pub fn push_log(&mut self, entry: impl Into<String>) {
        self.logs.push(entry.into());
        if self.logs.len() > 20 {
            let overflow = self.logs.len() - 20;
            self.logs.drain(0..overflow);
        }
    }

    pub fn input_mut(&mut self, target: InputTarget) -> &mut TextField {
        match target {
            InputTarget::FirstRunPassword => &mut self.first_run_password,
            InputTarget::FirstRunConfirm => &mut self.first_run_confirm,
            InputTarget::UnlockPassword => &mut self.unlock_password,
            InputTarget::ProvisionFullBaseUrl => &mut self.provision_full_base_url,
            InputTarget::ProvisionFullDatabaseName => &mut self.provision_full_database_name,
            InputTarget::ProvisionFullApplicationName => &mut self.provision_full_application_name,
            InputTarget::ProvisionFullExtraUsername => &mut self.provision_full_extra_username,
            InputTarget::ProvisionFullExtraApplicationName => {
                &mut self.provision_full_extra_application_name
            }
            InputTarget::ProvisionDatabaseName => &mut self.provision_database_name,
            InputTarget::ProvisionApplicationName => &mut self.provision_application_name,
            InputTarget::ExtraUserDatabaseName => &mut self.extra_user_database_name,
            InputTarget::ExtraUserUsername => &mut self.extra_user_username,
            InputTarget::ExtraUserApplicationName => &mut self.extra_user_application_name,
            InputTarget::ChangeCurrentPassword => &mut self.change_current_password,
            InputTarget::ChangeNewPassword => &mut self.change_new_password,
            InputTarget::ChangeConfirmPassword => &mut self.change_confirm_password,
            InputTarget::InstanceName => &mut self.instance_name_field,
            InputTarget::InstanceUrl => &mut self.instance_url_field,
            InputTarget::MigrateDestDbName => &mut self.migrate_dest_db_name,
            InputTarget::RestoreFilePath => &mut self.restore_file_path,
            InputTarget::RestoreDestDbName => &mut self.restore_dest_db_name,
            InputTarget::WizardInstanceName => &mut self.wizard_instance_name,
            InputTarget::WizardHost => &mut self.wizard_host,
            InputTarget::WizardPort => &mut self.wizard_port,
            InputTarget::WizardUsername => &mut self.wizard_username,
            InputTarget::WizardPassword => &mut self.wizard_password,
            InputTarget::WizardDatabase => &mut self.wizard_database,
            InputTarget::WizardSslMode => &mut self.wizard_ssl_mode,
        }
    }

    pub fn selected_home_item(&self) -> HomeItem {
        HomeItem::ALL[self.home_list_state.selected().unwrap_or(0)]
    }

    pub fn selected_settings_item(&self) -> SettingsItem {
        SettingsItem::ALL[self.settings_list_state.selected().unwrap_or(0)]
    }

    pub fn saved_connections(&self) -> Result<Vec<SavedConnectionRecord>> {
        self.storage.list_saved_connections()
    }

    pub fn selected_saved_connection(&self) -> Result<Option<SavedConnectionRecord>> {
        let records = self.saved_connections()?;
        Ok(self
            .saved_connections_list_state
            .selected()
            .and_then(|idx| records.get(idx).cloned()))
    }

    pub fn sync_saved_connections_selection(&mut self) -> Result<()> {
        let len = self.saved_connections()?.len();
        match len {
            0 => self.saved_connections_list_state.select(None),
            _ => {
                let current = self.saved_connections_list_state.selected().unwrap_or(0);
                self.saved_connections_list_state
                    .select(Some(current.min(len - 1)));
            }
        }
        Ok(())
    }

    pub fn start_first_run(&mut self) -> Result<()> {
        if self.first_run_password.value.is_empty() {
            anyhow::bail!("master password cannot be empty")
        }
        if self.first_run_password.value != self.first_run_confirm.value {
            anyhow::bail!("password confirmation does not match")
        }

        let config = self
            .storage
            .initialize_master_password(&self.first_run_password.value)
            .context("failed to initialize master password")?;
        let key = crypto::derive_key(&self.first_run_password.value, &config)?;
        self.key = Some(key);
        self.first_run_password.clear();
        self.first_run_confirm.clear();
        self.screen = Screen::Home;
        self.focused_input = InputTarget::ProvisionDatabaseName;
        self.set_status("Master password initialized.");
        Ok(())
    }

    pub fn unlock(&mut self) -> Result<()> {
        let config = self.storage.load_kdf_config()?;
        let key = crypto::derive_key(&self.unlock_password.value, &config)?;
        self.storage.verify_master_password(key.as_slice())?;
        self.key = Some(key);
        self.unlock_password.clear();
        self.screen = Screen::Home;
        self.focused_input = InputTarget::ProvisionDatabaseName;
        self.set_status("Unlocked successfully.");
        Ok(())
    }

    pub fn load_full_base_uri(&mut self) -> Result<()> {
        let Some(instance_name) = &self.selected_instance else {
            self.set_status("No instance selected. Choose one from Manage instances first.");
            return Ok(());
        };
        let Some(secret) = self.storage.load_instance_secret(instance_name)? else {
            self.set_status(format!("No saved base URI for '{}'", instance_name));
            return Ok(());
        };
        let key = self.session_key()?;
        let plaintext = crypto::decrypt(key, &secret.encrypted)?;
        self.provision_full_base_url.value =
            String::from_utf8(plaintext.to_vec()).context("saved base URI is not valid UTF-8")?;
        self.set_status(format!("Loaded base URI for '{}'", instance_name));
        Ok(())
    }

    pub fn start_provision_full(&mut self) -> Result<()> {
        if self.pending_operation {
            anyhow::bail!("an operation is already running")
        }

        let raw_base = self.provision_full_base_url.value.trim().to_owned();
        if raw_base.is_empty() {
            anyhow::bail!("enter or load a base DATABASE_URL first");
        }
        validate_database_name(self.provision_full_database_name.value.trim())?;
        let extra_username_raw = self.provision_full_extra_username.value.trim().to_owned();
        if !extra_username_raw.is_empty() {
            validate_database_name(&extra_username_raw)?;
        }

        let base_url = raw_base;

        let extra_username = self.provision_full_extra_username.value.trim().to_owned();
        let extra_username = if extra_username.is_empty() {
            None
        } else {
            Some(extra_username)
        };
        let extra_app_name = self
            .provision_full_extra_application_name
            .value
            .trim()
            .to_owned();
        let extra_app_name = if extra_app_name.is_empty() && extra_username.is_some() {
            Some(self.provision_full_database_name.value.trim().to_owned())
        } else if extra_username.is_some() {
            Some(extra_app_name)
        } else {
            None
        };

        let request = ProvisionFullRequest {
            database_name: self.provision_full_database_name.value.trim().to_owned(),
            application_name: normalize_application_name(
                self.provision_full_database_name.value.trim(),
                &self.provision_full_application_name.value,
            ),
            extra_username,
            extra_application_name: extra_app_name,
            dedicated_owner: true,
        };

        let instance_name = self.selected_instance.clone().unwrap_or_default();
        let instance_name_for_worker = instance_name.clone();
        let start_msg = if request.extra_username.is_some() {
            format!(
                "Starting full provisioning for database '{}' with extra user",
                request.database_name,
            )
        } else {
            format!(
                "Starting full provisioning for database '{}'",
                request.database_name,
            )
        };

        self.start_worker(instance_name, start_msg, move |tx| {
            let result = provision_full_with_progress(&base_url, &request, |step| {
                let _ = tx.send(WorkerEvent::Log(step));
            })
            .map_err(|error| error.to_string());
            let _ = tx.send(WorkerEvent::Finished(
                instance_name_for_worker,
                OperationResult::Full(result),
            ));
        })
    }

    fn resolved_backend(&self, source_cs: Option<&str>) -> PgToolBackend {
        match &self.pg_tool_backend {
            PgToolBackend::Docker { .. } => {
                let image = resolve_docker_image(&self.pg_tool_backend, source_cs);
                PgToolBackend::Docker { image }
            }
            other => other.clone(),
        }
    }

    pub fn start_migrate_database(&mut self) -> Result<()> {
        if self.pending_operation {
            anyhow::bail!("an operation is already running")
        }

        let key = self.session_key()?;

        let source_record = self
            .migrate_source_record
            .as_ref()
            .context("no source selected")?;
        let encrypted = match source_record {
            SavedConnectionRecord::Database(r) => &r.encrypted,
            SavedConnectionRecord::ExtraUser(r) => &r.encrypted,
            SavedConnectionRecord::Instance { encrypted, .. } => encrypted,
        };
        let plaintext = crypto::decrypt(key, encrypted)?;
        let source_cs = String::from_utf8(plaintext.to_vec())
            .context("source connection string is not valid UTF-8")?;

        let dest_instance = self
            .migrate_dest_instance
            .as_ref()
            .context("no destination instance selected")?;
        let Some(secret) = self.storage.load_instance_secret(dest_instance)? else {
            anyhow::bail!("no saved base URI for instance '{dest_instance}'");
        };
        let plaintext = crypto::decrypt(key, &secret.encrypted)?;
        let dest_base_url = String::from_utf8(plaintext.to_vec())
            .context("destination base URI is not valid UTF-8")?;

        let dest_db_name = self.migrate_dest_db_name.value.trim().to_owned();
        if dest_db_name.is_empty() {
            anyhow::bail!("destination database name cannot be empty");
        }
        validate_database_name(&dest_db_name)?;

        self.migrate_phase = MigratePhase::Running;
        let instance_name = dest_instance.clone();
        let start_msg = format!("Starting migration to '{dest_db_name}' in {instance_name}");

        let src_cs = source_cs.clone();
        let backend = self.resolved_backend(Some(&src_cs));
        self.start_worker(instance_name.clone(), start_msg, move |tx| {
            if let Some(warning) = check_version_warning(&src_cs, &backend) {
                let _ = tx.send(WorkerEvent::Log(warning));
            }
            let result = migrate_database_with_progress(
                &src_cs,
                &dest_base_url,
                &dest_db_name,
                &backend,
                false,
                &mut |step| {
                    let _ = tx.send(WorkerEvent::Log(step.to_owned()));
                },
            )
            .map_err(|error| error.to_string());
            let _ = tx.send(WorkerEvent::Finished(
                instance_name,
                OperationResult::Full(result),
            ));
        })
    }

    fn start_worker<F>(
        &mut self,
        instance_name: String,
        start_message: String,
        work: F,
    ) -> Result<()>
    where
        F: FnOnce(mpsc::Sender<WorkerEvent>) + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        self.pending_operation = true;
        self.operation_receiver = Some(rx);
        self.logs.clear();
        self.push_log(start_message);
        if instance_name.is_empty() {
            self.set_status("Running operation in background. You can keep navigating the TUI.");
        } else {
            self.set_status(format!(
                "Running operation in background for {}. You can keep navigating the TUI.",
                instance_name
            ));
        }

        thread::spawn(move || work(tx));
        Ok(())
    }

    pub fn poll_background_tasks(&mut self) -> Result<()> {
        let Some(receiver) = self.operation_receiver.take() else {
            return Ok(());
        };

        loop {
            match receiver.try_recv() {
                Ok(WorkerEvent::Log(step)) => {
                    self.push_log(step);
                }
                Ok(WorkerEvent::Finished(instance_name, result)) => {
                    self.pending_operation = false;
                    match result {
                        OperationResult::Full(result) => {
                            self.finish_full_operation(instance_name, result)?;
                        }
                        OperationResult::Batch(result) => {
                            self.finish_batch_operation(instance_name, result)?;
                        }
                        OperationResult::Backup(result) => match result {
                            Ok(outcome) => {
                                self.push_log(format!("Backup saved to {}", outcome.file_path));
                                self.set_status(format!(
                                    "Backup of '{}' completed successfully.",
                                    outcome.database_name
                                ));
                            }
                            Err(error) => {
                                self.set_status(format!("Backup failed: {error}"));
                            }
                        },
                    }
                    match self.screen {
                        Screen::MigrateDatabase => {
                            self.migrate_phase = MigratePhase::SelectSource;
                            self.migrate_source_record = None;
                            self.migrate_dest_instance = None;
                            self.migrate_dest_db_name.clear();
                        }
                        Screen::BackupDatabase => {
                            self.backup_phase = BackupPhase::SelectSource;
                            self.backup_source_record = None;
                            self.backup_databases.clear();
                            self.backup_selected_databases.clear();
                            self.backup_config = BackupConfig::default();
                        }
                        Screen::RestoreDatabase => {
                            self.restore_phase = RestorePhase::EnterFilePath;
                            self.restore_file_path.clear();
                            self.restore_dest_instance = None;
                            self.restore_dest_db_name.clear();
                            self.restore_preview_databases.clear();
                            self.restore_conflict_policy = ConflictPolicy::Skip;
                        }
                        _ => {}
                    }
                    break;
                }
                Err(TryRecvError::Empty) => {
                    self.operation_receiver = Some(receiver);
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    self.pending_operation = false;
                    self.set_status("Background worker disconnected unexpectedly.");
                    break;
                }
            }
        }

        Ok(())
    }

    fn finish_full_operation(
        &mut self,
        instance_name: String,
        result: Result<ProvisionFullOutcome, String>,
    ) -> Result<()> {
        match result {
            Ok(outcome) => {
                let key = self.session_key()?.to_vec();
                let db_encrypted =
                    crypto::encrypt(&key, outcome.database_connection_string.as_bytes())?;
                let db_record = ProvisionOutcome {
                    database_name: outcome.database_name.clone(),
                    application_name: outcome.application_name.clone(),
                    role_name: outcome.role_name.clone(),
                    connection_string: outcome.database_connection_string,
                    database_created: outcome.database_created,
                    role_created: outcome.role_created,
                };
                self.storage.save_provisioned_database(
                    &instance_name,
                    &db_record,
                    &db_encrypted,
                )?;
                self.push_log(format!(
                    "Saved database connection string {}",
                    mask_connection_string(&db_record.connection_string)
                ));

                if let (Some(extra_username), Some(extra_cs)) = (
                    outcome.extra_username.as_ref(),
                    outcome.extra_connection_string.as_ref(),
                ) {
                    let extra_encrypted = crypto::encrypt(&key, extra_cs.as_bytes())?;
                    let extra_record = ExtraUserProvisionOutcome {
                        database_name: outcome.database_name.clone(),
                        username: extra_username.clone(),
                        application_name: outcome.application_name.clone(),
                        connection_string: extra_cs.clone(),
                        role_created: outcome.extra_role_created.unwrap_or(false),
                        grants_applied: outcome.extra_grants_applied.unwrap_or(false),
                    };
                    self.storage.save_provisioned_extra_user(
                        &instance_name,
                        &extra_record,
                        &extra_encrypted,
                    )?;
                    self.push_log(format!(
                        "Saved extra user connection string {}",
                        mask_connection_string(extra_cs)
                    ));
                    self.set_status(format!(
                        "Provisioned database '{}' with extra user '{}' in {}",
                        outcome.database_name, extra_username, instance_name
                    ));
                } else {
                    self.set_status(format!(
                        "Provisioned database '{}' in {}",
                        outcome.database_name, instance_name
                    ));
                }
            }
            Err(error) => {
                self.push_log(format!("Provisioning failed: {error}"));
                self.set_status(format!("Provisioning failed: {error}"));
            }
        }
        Ok(())
    }

    fn finish_batch_operation(
        &mut self,
        instance_name: String,
        result: Result<Vec<ProvisionFullOutcome>, String>,
    ) -> Result<()> {
        match result {
            Ok(outcomes) => {
                let restored_count = outcomes.iter().filter(|o| o.database_created).count();
                let skipped_count = outcomes.len() - restored_count;
                for outcome in outcomes {
                    if outcome.database_created {
                        self.finish_full_operation(instance_name.clone(), Ok(outcome))?;
                    } else {
                        self.push_log(format!(
                            "  [skip] '{}' — already existed, left untouched",
                            outcome.database_name
                        ));
                    }
                }
                self.set_status(format!(
                    "Restored {} database(s) in {instance_name}{}",
                    restored_count,
                    if skipped_count > 0 {
                        format!(" ({} skipped)", skipped_count)
                    } else {
                        String::new()
                    },
                ));
            }
            Err(error) => {
                self.push_log(format!("Restore failed: {error}"));
                self.set_status(format!("Restore failed: {error}"));
            }
        }
        Ok(())
    }

    pub fn delete_selected_connection(&mut self) -> Result<()> {
        let Some(record) = self.selected_saved_connection()? else {
            anyhow::bail!("no saved connection selected")
        };
        let id = match &record {
            SavedConnectionRecord::Database(r) => {
                self.storage.delete_provisioned_database(r.id)?;
                r.id
            }
            SavedConnectionRecord::ExtraUser(r) => {
                self.storage.delete_provisioned_extra_user(r.id)?;
                r.id
            }
            SavedConnectionRecord::Instance { name, .. } => {
                anyhow::bail!("use Manage Instances to delete instance '{name}'");
            }
        };
        self.sync_saved_connections_selection()?;
        self.reveal_selected_connection_string = false;
        self.set_status(format!("Deleted connection id={id}"));
        Ok(())
    }

    pub fn start_edit_application_name(&mut self) -> Result<()> {
        let Some(record) = self.selected_saved_connection()? else {
            anyhow::bail!("no saved connection selected")
        };
        let (target, current_name) = match &record {
            SavedConnectionRecord::Database(r) => {
                (EditTarget::Database(r.id), r.application_name.clone())
            }
            SavedConnectionRecord::ExtraUser(r) => {
                (EditTarget::ExtraUser(r.id), r.application_name.clone())
            }
            SavedConnectionRecord::Instance { name, .. } => {
                anyhow::bail!("cannot edit application name for instance '{name}'");
            }
        };
        self.editing_target = Some(target);
        self.editing_app_name_field.value = current_name;
        self.screen = Screen::EditConnectionAppName;
        Ok(())
    }

    pub fn save_application_name_edit(&mut self) -> Result<()> {
        let name = self.editing_app_name_field.value.trim().to_owned();
        if name.is_empty() {
            anyhow::bail!("application name cannot be empty")
        }
        let target = self
            .editing_target
            .take()
            .context("no active edit target")?;
        match target {
            EditTarget::Database(id) => {
                self.storage.update_database_application_name(id, &name)?;
            }
            EditTarget::ExtraUser(id) => {
                self.storage.update_extra_user_application_name(id, &name)?;
            }
        }
        self.editing_app_name_field.clear();
        self.screen = Screen::ViewSavedConnections;
        self.sync_saved_connections_selection()?;
        self.set_status(format!("Updated application name to \"{name}\""));
        Ok(())
    }

    pub fn reveal_selected_record(&mut self) -> Result<()> {
        let Some(record) = self.selected_saved_connection()? else {
            anyhow::bail!("no saved connection selected")
        };
        let encrypted = match record {
            SavedConnectionRecord::Database(record) => record.encrypted,
            SavedConnectionRecord::ExtraUser(record) => record.encrypted,
            SavedConnectionRecord::Instance { encrypted, .. } => encrypted,
        };

        let key = self.session_key()?;
        let plaintext = crypto::decrypt(key, &encrypted)?;
        let connection_string = String::from_utf8(plaintext.to_vec())
            .context("stored connection string is not valid UTF-8")?;
        self.reveal_selected_connection_string = true;
        self.set_status(connection_string);
        Ok(())
    }

    pub fn change_master_password(&mut self) -> Result<()> {
        if self.change_new_password.value.is_empty() {
            anyhow::bail!("new password cannot be empty")
        }
        if self.change_new_password.value != self.change_confirm_password.value {
            anyhow::bail!("new password confirmation does not match")
        }

        let config = self.storage.load_kdf_config()?;
        let current_key = crypto::derive_key(&self.change_current_password.value, &config)?;
        self.storage
            .verify_master_password(current_key.as_slice())?;
        let new_config = self
            .storage
            .change_master_password(current_key.as_slice(), &self.change_new_password.value)?;
        let new_key = crypto::derive_key(&self.change_new_password.value, &new_config)?;
        self.key = Some(new_key);

        self.change_current_password.clear();
        self.change_new_password.clear();
        self.change_confirm_password.clear();
        self.screen = Screen::Settings;
        self.focused_input = InputTarget::ChangeCurrentPassword;
        self.set_status("Master password changed and all secrets re-encrypted.");
        Ok(())
    }

    pub fn load_instances(&mut self) -> Result<()> {
        self.instances = self.storage.list_instances()?;
        self.selected_instance_idx = self
            .selected_instance_idx
            .min(self.instances.len().saturating_sub(1));
        Ok(())
    }

    pub fn add_instance(&mut self) -> Result<()> {
        let name = self.instance_name_field.value.trim().to_owned();
        if name.is_empty() {
            anyhow::bail!("instance name cannot be empty");
        }
        let raw_url = self.instance_url_field.value.trim().to_owned();
        if raw_url.is_empty() {
            anyhow::bail!("base DATABASE_URL cannot be empty");
        }
        let key = self.session_key()?;
        let parsed = parse_database_url(&raw_url)?;
        let encrypted = crypto::encrypt(key, raw_url.as_bytes())?;
        self.storage.save_instance_secret(&name, &encrypted)?;
        self.instance_name_field.clear();
        self.instance_url_field.clear();
        self.load_instances()?;
        self.screen = Screen::ManageInstances;
        self.set_status(format!(
            "Added instance '{name}' ({}:{} / {})",
            parsed.host, parsed.port, parsed.database
        ));
        Ok(())
    }

    pub fn selected_instance_in_mgmt(&self) -> String {
        self.instances[self.selected_instance_idx].clone()
    }

    pub fn delete_instance(&mut self) -> Result<()> {
        let name = self.selected_instance_in_mgmt();
        self.storage.delete_instance(&name)?;
        self.load_instances()?;
        self.confirming_delete_instance = false;
        if self.selected_instance.as_deref() == Some(&name) {
            self.selected_instance = None;
        }
        self.set_status(format!("Deleted instance '{name}'"));
        Ok(())
    }

    pub fn migrate_sources(&self) -> Result<Vec<SavedConnectionRecord>> {
        let mut records = self.storage.list_saved_connections()?;
        for (name, encrypted) in self.storage.list_instance_records()? {
            records.push(SavedConnectionRecord::Instance { name, encrypted });
        }
        Ok(records)
    }

    pub fn migrate_selected_source(&self) -> Result<Option<SavedConnectionRecord>> {
        let records = self.migrate_sources()?;
        Ok(self
            .migrate_source_list_state
            .selected()
            .and_then(|idx| records.get(idx).cloned()))
    }

    pub fn sync_migrate_source_selection(&mut self) -> Result<()> {
        let len = self.migrate_sources()?.len();
        match len {
            0 => self.migrate_source_list_state.select(None),
            _ => {
                let current = self.migrate_source_list_state.selected().unwrap_or(0);
                self.migrate_source_list_state
                    .select(Some(current.min(len - 1)));
            }
        }
        Ok(())
    }

    pub fn backup_sources(&self) -> Result<Vec<SavedConnectionRecord>> {
        let mut records = self.storage.list_saved_connections()?;
        for (name, encrypted) in self.storage.list_instance_records()? {
            records.push(SavedConnectionRecord::Instance { name, encrypted });
        }
        Ok(records)
    }

    pub fn sync_backup_source_selection(&mut self) -> Result<()> {
        let len = self.backup_sources()?.len();
        match len {
            0 => self.backup_source_list_state.select(None),
            _ => {
                let current = self.backup_source_list_state.selected().unwrap_or(0);
                self.backup_source_list_state
                    .select(Some(current.min(len - 1)));
            }
        }
        Ok(())
    }

    pub fn prepare_backup_database_selection(&mut self) -> Result<()> {
        let key = self.session_key()?;
        let Some(SavedConnectionRecord::Instance { encrypted, .. }) =
            self.backup_source_record.as_ref()
        else {
            anyhow::bail!("database selection requires an instance source")
        };
        let plaintext = crypto::decrypt(key, encrypted)?;
        let source_cs = String::from_utf8(plaintext.to_vec())
            .context("source connection string is not valid UTF-8")?;
        self.backup_databases = crate::postgres::discover_databases(&source_cs)?;
        if self.backup_databases.is_empty() {
            anyhow::bail!("instance contains no connectable non-template databases")
        }
        self.backup_selected_databases = vec![true; self.backup_databases.len()];
        self.backup_database_list_state.select(Some(0));
        self.backup_config = BackupConfig::default();
        self.backup_source_version =
            crate::postgres::detect_source_version(&source_cs).unwrap_or_else(|_| String::new());
        self.backup_phase = BackupPhase::SelectDatabases;
        Ok(())
    }

    pub fn start_backup_database(&mut self) -> Result<()> {
        if self.pending_operation {
            anyhow::bail!("an operation is already running")
        }

        let key = self.session_key()?;

        let source_record = self
            .backup_source_record
            .as_ref()
            .context("no source selected")?;
        let encrypted = match source_record {
            SavedConnectionRecord::Database(r) => &r.encrypted,
            SavedConnectionRecord::ExtraUser(r) => &r.encrypted,
            SavedConnectionRecord::Instance { encrypted, .. } => encrypted,
        };
        let plaintext = crypto::decrypt(key, encrypted)?;
        let source_cs = String::from_utf8(plaintext.to_vec())
            .context("source connection string is not valid UTF-8")?;

        let (instance_name, db_name, app_name) = match source_record {
            SavedConnectionRecord::Database(r) => (
                r.instance_name.clone(),
                r.database_name.clone(),
                r.application_name.clone(),
            ),
            SavedConnectionRecord::ExtraUser(r) => (
                r.instance_name.clone(),
                r.database_name.clone(),
                r.application_name.clone(),
            ),
            SavedConnectionRecord::Instance { name, .. } => {
                let parsed = parse_database_url(&source_cs)?;
                (name.clone(), parsed.database, String::new())
            }
        };
        let instance_scope = matches!(source_record, SavedConnectionRecord::Instance { .. });
        let selected_database_names: Vec<String> = self
            .backup_databases
            .iter()
            .zip(&self.backup_selected_databases)
            .filter(|(_, selected)| **selected)
            .map(|(database, _)| database.name.clone())
            .collect();

        let metadata = BackupMetadata {
            machine_id: self.machine_id.clone(),
            hostname: self.hostname.clone(),
            instance_name: instance_name.clone(),
            database_name: db_name.clone(),
            application_name: app_name,
            engine: "postgresql".to_owned(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        };

        let encrypt_key = key.to_vec();
        let src_cs = source_cs.clone();
        let backend = self.resolved_backend(Some(&src_cs));
        let backup_config = self.backup_config.clone();

        self.backup_phase = BackupPhase::Running;
        let start_msg = format!("Starting backup of '{db_name}'");

        self.start_worker(String::new(), start_msg, move |tx| {
            if let Some(warning) = check_version_warning(&src_cs, &backend) {
                let _ = tx.send(WorkerEvent::Log(warning));
            }
            let output_dir = match backup_directory() {
                Ok(path) => path,
                Err(error) => {
                    let _ = tx.send(WorkerEvent::Finished(
                        db_name,
                        OperationResult::Backup(Err(error.to_string())),
                    ));
                    return;
                }
            };
            let result = if instance_scope {
                backup_instance_with_progress(
                    &source_cs,
                    &encrypt_key,
                    &output_dir,
                    &backend,
                    InstanceBackupContext {
                        instance_name: &instance_name,
                        machine_id: &metadata.machine_id,
                        hostname: &metadata.hostname,
                    },
                    &selected_database_names,
                    &backup_config,
                    &mut |step| {
                        let _ = tx.send(WorkerEvent::Log(step.to_owned()));
                    },
                )
            } else {
                backup_database_with_progress(
                    &source_cs,
                    &encrypt_key,
                    &output_dir,
                    &backend,
                    Some(&metadata),
                    &mut |step| {
                        let _ = tx.send(WorkerEvent::Log(step.to_owned()));
                    },
                )
            }
            .map_err(|error| error.to_string());
            let _ = tx.send(WorkerEvent::Finished(
                db_name,
                OperationResult::Backup(result),
            ));
        })
    }

    pub fn prepare_restore_preview(&mut self) -> Result<()> {
        let key = self.session_key()?;
        let input_path = std::path::PathBuf::from(self.restore_file_path.value.trim());
        if !input_path.is_file() {
            anyhow::bail!("backup file does not exist or is not a regular file");
        }
        if crate::postgres::is_instance_backup(&input_path, key)? {
            let contents = crate::postgres::read_instance_backup(&input_path, key)?;
            self.restore_preview_databases = contents.databases;
            self.restore_preview_globals = contents.includes_globals;
            self.restore_preview_source_instance = contents.source_instance;
            self.restore_preview_source_version = contents.source_version;
            self.restore_conflict_policy = ConflictPolicy::Skip;
            self.restore_phase = RestorePhase::PreviewBundle;
        } else {
            self.restore_conflict_policy = ConflictPolicy::Fail;
            self.restore_phase = RestorePhase::EnterDestDbName;
            self.focused_input = InputTarget::RestoreDestDbName;
        }
        Ok(())
    }

    pub fn start_restore_database(&mut self) -> Result<()> {
        if self.pending_operation {
            anyhow::bail!("an operation is already running")
        }

        let key = self.session_key()?;

        let dest_instance = self
            .restore_dest_instance
            .as_ref()
            .context("no destination instance selected")?;
        let Some(secret) = self.storage.load_instance_secret(dest_instance)? else {
            anyhow::bail!("no saved base URI for instance '{dest_instance}'");
        };
        let plaintext = crypto::decrypt(key, &secret.encrypted)?;
        let dest_base_url = String::from_utf8(plaintext.to_vec())
            .context("destination base URI is not valid UTF-8")?;

        let dest_db_name = self.restore_dest_db_name.value.trim().to_owned();
        let input_path = std::path::PathBuf::from(self.restore_file_path.value.trim());
        if !input_path.is_file() {
            anyhow::bail!(
                "backup file does not exist or is not a regular file: {}",
                input_path.display()
            );
        }
        let instance_bundle = is_instance_backup(&input_path, key)?;
        if !instance_bundle {
            if dest_db_name.is_empty() {
                anyhow::bail!("destination database name cannot be empty");
            }
            validate_database_name(&dest_db_name)?;
        }

        let backend = self.resolved_backend(Some(&dest_base_url));
        let decrypt_key = key.to_vec();
        let instance_name = dest_instance.clone();

        self.restore_phase = RestorePhase::Running;
        let start_msg = format!(
            "Starting restore of '{}' to '{dest_db_name}' in {instance_name}",
            input_path.display()
        );

        self.start_worker(instance_name.clone(), start_msg, move |tx| {
            if instance_bundle {
                let result = restore_instance_with_progress(
                    &input_path,
                    &decrypt_key,
                    &dest_base_url,
                    &backend,
                    ConflictPolicy::Skip,
                    &mut |step| {
                        let _ = tx.send(WorkerEvent::Log(step.to_owned()));
                    },
                )
                .map_err(|error| error.to_string());
                let _ = tx.send(WorkerEvent::Finished(
                    instance_name,
                    OperationResult::Batch(result),
                ));
            } else {
                let result = restore_database_with_progress(
                    &input_path,
                    &decrypt_key,
                    &dest_base_url,
                    &dest_db_name,
                    &backend,
                    ConflictPolicy::Fail,
                    false,
                    &mut |step| {
                        let _ = tx.send(WorkerEvent::Log(step.to_owned()));
                    },
                )
                .map_err(|error| error.to_string());
                let _ = tx.send(WorkerEvent::Finished(
                    instance_name,
                    OperationResult::Full(result),
                ));
            }
        })
    }

    pub fn session_key(&self) -> Result<&[u8]> {
        self.key
            .as_deref()
            .map(Vec::as_slice)
            .context("app is locked")
    }

    pub fn build_url_from_wizard(&self) -> String {
        let host = self.wizard_host.value.trim();
        let port = self.wizard_port.value.trim();
        let user = self.wizard_username.value.trim();
        let pass = Self::percent_encode_password(self.wizard_password.value.trim());
        let db = self.wizard_database.value.trim();
        let ssl = self.wizard_ssl_mode.value.trim().to_lowercase();

        let port_str = if port.is_empty() || port == "5432" {
            String::new()
        } else {
            format!(":{}", port)
        };
        let auth = if user.is_empty() && pass.is_empty() {
            String::new()
        } else if pass.is_empty() {
            format!("{}@", user)
        } else {
            format!("{}:{}@", user, pass)
        };
        let ssl_str = if ssl.is_empty() || ssl == "disable" {
            String::new()
        } else {
            format!("?sslmode={}", ssl)
        };

        format!(
            "postgresql://{}{}{}/{}{}",
            auth, host, port_str, db, ssl_str
        )
    }

    pub fn clear_wizard(&mut self) {
        self.wizard_instance_name.clear();
        self.wizard_host.clear();
        self.wizard_port.clear();
        self.wizard_username.clear();
        self.wizard_password.clear();
        self.wizard_database.clear();
        self.wizard_ssl_mode.clear();
    }

    pub fn add_instance_from_wizard(&mut self) -> Result<()> {
        let name = self.wizard_instance_name.value.trim().to_owned();
        if name.is_empty() {
            anyhow::bail!("instance name cannot be empty");
        }
        let raw_url = self.build_url_from_wizard();
        if raw_url == "postgresql:///" {
            anyhow::bail!("at least host and database are required");
        }
        let key = self.session_key()?;
        let parsed = parse_database_url(&raw_url)?;
        let encrypted = crypto::encrypt(key, raw_url.as_bytes())?;
        self.storage.save_instance_secret(&name, &encrypted)?;
        self.clear_wizard();
        self.load_instances()?;
        self.screen = Screen::ManageInstances;
        self.set_status(format!(
            "Added instance '{name}' ({}:{} / {})",
            parsed.host, parsed.port, parsed.database
        ));
        Ok(())
    }

    fn percent_encode_password(pass: &str) -> String {
        url::form_urlencoded::byte_serialize(pass.as_bytes()).collect()
    }

    pub fn trigger_health_checks(&self) {
        let key = match self.session_key() {
            Ok(k) => k.to_vec(),
            Err(_) => return,
        };

        if self.screen != Screen::ManageInstances && self.screen != Screen::ViewSavedConnections {
            return;
        }

        let mut targets: Vec<(String, String)> = Vec::new();

        // Collect instance connection strings
        for name in &self.instances {
            if let Some(secret) = self.storage.load_instance_secret(name).ok().flatten()
                && let Ok(pt) = crypto::decrypt(&key, &secret.encrypted)
                && let Ok(cs) = String::from_utf8(pt.to_vec())
            {
                targets.push((format!("instance:{}", name), cs));
            }
        }

        // Collect saved connection strings
        if let Ok(records) = self.saved_connections() {
            for record in &records {
                let encrypted = match record {
                    SavedConnectionRecord::Database(r) => &r.encrypted,
                    SavedConnectionRecord::ExtraUser(r) => &r.encrypted,
                    SavedConnectionRecord::Instance { encrypted, .. } => encrypted,
                };
                let id = match record {
                    SavedConnectionRecord::Database(r) => format!("conn:db:{}", r.id),
                    SavedConnectionRecord::ExtraUser(r) => format!("conn:user:{}", r.id),
                    SavedConnectionRecord::Instance { name, .. } => {
                        format!("conn:instance:{}", name)
                    }
                };
                if let Ok(pt) = crypto::decrypt(&key, encrypted)
                    && let Ok(cs) = String::from_utf8(pt.to_vec())
                {
                    targets.push((id, cs));
                }
            }
        }

        let status_map = self.health_status.clone();
        {
            let mut map = status_map.lock().unwrap();
            for (id, _) in &targets {
                map.insert(id.clone(), HealthStatus::Checking);
            }
        }

        for (id, cs) in targets {
            let map = status_map.clone();
            thread::spawn(move || {
                let result = check_connection_health(&cs);
                let mut map = map.lock().unwrap();
                match result {
                    Ok((latency_ms, version)) => {
                        map.insert(
                            id,
                            HealthStatus::Ok {
                                latency_ms,
                                version,
                            },
                        );
                    }
                    Err(e) => {
                        map.insert(id, HealthStatus::Error(e.to_string()));
                    }
                }
            });
        }
    }

    pub fn start_monitor_instance(&mut self, instance_url: String, instance_name: String) {
        self.queries_url = Some(instance_url.clone());
        self.queries_instance = instance_name;
        self.active_queries.clear();
        self.queries_list_state.select(None);
        self.query_kill_result.clear();
        self.screen = Screen::ActiveQueries;
        self.refresh_active_queries();
    }

    pub fn refresh_active_queries(&mut self) {
        let Some(ref url) = self.queries_url.clone() else {
            return;
        };
        match fetch_active_queries(url) {
            Ok(queries) => {
                self.active_queries = queries;
                let len = self.active_queries.len();
                match len {
                    0 => self.queries_list_state.select(None),
                    _ => {
                        let current = self.queries_list_state.selected().unwrap_or(0);
                        self.queries_list_state.select(Some(current.min(len - 1)));
                    }
                }
            }
            Err(e) => {
                self.set_status(format!("Failed to fetch queries: {e}"));
            }
        }
    }

    pub fn kill_selected_query(&mut self) -> Result<()> {
        let Some(ref url) = self.queries_url.clone() else {
            anyhow::bail!("no monitored url");
        };
        let Some(selected) = self.queries_list_state.selected() else {
            anyhow::bail!("no query selected");
        };
        let Some(query) = self.active_queries.get(selected) else {
            anyhow::bail!("query not found");
        };
        let pid = query.pid;
        match kill_query(url, pid) {
            Ok(msg) => {
                self.query_kill_result = msg.clone();
                self.set_status(msg);
                self.refresh_active_queries();
            }
            Err(e) => {
                self.set_status(format!("Kill failed: {e}"));
            }
        }
        self.confirming_kill_query = false;
        Ok(())
    }

    pub fn refresh_backups_list(&mut self) {
        self.backups_list = Self::list_backup_files();
    }

    pub fn selected_backup_file(&self) -> Option<BackupFile> {
        self.backups_list_state
            .selected()
            .and_then(|idx| self.backups_list.get(idx).cloned())
    }

    pub fn sync_backup_selection(&mut self) {
        let len = self.backups_list.len();
        match len {
            0 => self.backups_list_state.select(None),
            _ => {
                let current = self.backups_list_state.selected().unwrap_or(0);
                self.backups_list_state.select(Some(current.min(len - 1)));
            }
        }
    }

    fn list_backup_files() -> Vec<BackupFile> {
        let Ok(dir) = backup_directory() else {
            return Vec::new();
        };
        if !dir.exists() {
            return Vec::new();
        }
        let mut files: Vec<BackupFile> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "enc") {
                    let filename = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_owned();
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    let modified = std::fs::metadata(&path)
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .map(|t| {
                            let duration =
                                t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                            let secs = duration.as_secs();
                            chrono::DateTime::from_timestamp(secs as i64, 0)
                                .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                                .unwrap_or_else(|| "unknown".to_owned())
                        })
                        .unwrap_or_else(|| "unknown".to_owned());
                    files.push(BackupFile {
                        path,
                        filename,
                        size,
                        modified,
                    });
                }
            }
        }
        files.sort_by(|a, b| b.modified.cmp(&a.modified));
        files
    }
}

pub fn handle_key_event(app: &mut App, key: crossterm::event::KeyEvent) -> Result<()> {
    crate::input::handle_key_event(app, key)
}

#[cfg(test)]
mod tests {
    use super::TextField;

    #[test]
    fn new_field_is_empty_and_cursor_at_start() {
        let f = TextField::new("label", false);
        assert!(f.value.is_empty());
        assert_eq!(f.cursor_pos, 0);
    }

    #[test]
    fn insert_char_at_cursor_position() {
        let mut f = TextField::new("x", false);
        for ch in "ab".chars() {
            f.insert_char(ch);
        }
        assert_eq!(f.value, "ab");
        assert_eq!(f.cursor_pos, 2);

        f.cursor_pos = 0;
        f.insert_char('X');
        assert_eq!(f.value, "Xab");
        assert_eq!(f.cursor_pos, 1);

        f.cursor_pos = 2;
        f.insert_char('Y');
        assert_eq!(f.value, "XaYb");
        assert_eq!(f.cursor_pos, 3);
    }

    #[test]
    fn backspace_removes_char_before_cursor() {
        let mut f = TextField::new("x", false);
        f.value = "hello".to_owned();
        f.cursor_pos = 5;

        f.delete_before_cursor();
        assert_eq!(f.value, "hell");
        assert_eq!(f.cursor_pos, 4);

        // cursor in the middle
        f.cursor_pos = 2;
        f.delete_before_cursor();
        assert_eq!(f.value, "hll");
        assert_eq!(f.cursor_pos, 1);
    }

    #[test]
    fn backspace_at_start_does_nothing() {
        let mut f = TextField::new("x", false);
        f.value = "abc".to_owned();
        f.cursor_pos = 0;

        f.delete_before_cursor();
        assert_eq!(f.value, "abc");
        assert_eq!(f.cursor_pos, 0);
    }

    #[test]
    fn delete_removes_char_at_cursor() {
        let mut f = TextField::new("x", false);
        f.value = "hello".to_owned();
        f.cursor_pos = 0;

        f.delete_after_cursor();
        assert_eq!(f.value, "ello");
        assert_eq!(f.cursor_pos, 0);

        f.cursor_pos = 2;
        f.delete_after_cursor();
        assert_eq!(f.value, "elo");
        assert_eq!(f.cursor_pos, 2);
    }

    #[test]
    fn delete_at_end_does_nothing() {
        let mut f = TextField::new("x", false);
        f.value = "abc".to_owned();
        f.cursor_pos = 3;

        f.delete_after_cursor();
        assert_eq!(f.value, "abc");
        assert_eq!(f.cursor_pos, 3);
    }

    #[test]
    fn clear_resets_all() {
        let mut f = TextField::new("x", false);
        f.value = "some text".to_owned();
        f.cursor_pos = 4;

        f.clear();
        assert!(f.value.is_empty());
        assert_eq!(f.cursor_pos, 0);
    }

    #[test]
    fn insert_delete_with_middle_cursor() {
        let mut f = TextField::new("x", false);
        for ch in "hello".chars() {
            f.insert_char(ch);
        }
        assert_eq!(f.value, "hello");
        assert_eq!(f.cursor_pos, 5);

        f.cursor_pos = 2;
        f.delete_before_cursor();
        assert_eq!(f.value, "hllo");
        assert_eq!(f.cursor_pos, 1);

        f.insert_char('E');
        assert_eq!(f.value, "hEllo");
        assert_eq!(f.cursor_pos, 2);
    }

    #[test]
    fn insert_with_unicode() {
        let mut f = TextField::new("x", false);
        f.insert_char('ć');
        assert_eq!(f.value, "ć");
        assert_eq!(f.cursor_pos, 1);

        f.insert_char('a');
        assert_eq!(f.value, "ća");
        assert_eq!(f.cursor_pos, 2);

        f.cursor_pos = 1;
        f.insert_char('ż');
        assert_eq!(f.value, "ćża");
        assert_eq!(f.cursor_pos, 2);
    }

    #[test]
    fn backspace_with_unicode() {
        let mut f = TextField::new("x", false);
        f.value = "a💥b".to_owned();
        f.cursor_pos = 2; // between 💥 and b

        f.delete_before_cursor();
        assert_eq!(f.value, "ab");
        assert_eq!(f.cursor_pos, 1);
    }

    #[test]
    fn delete_after_cursor_with_unicode() {
        let mut f = TextField::new("x", false);
        f.value = "a🔥b".to_owned();
        f.cursor_pos = 1;

        f.delete_after_cursor();
        assert_eq!(f.value, "ab");
        assert_eq!(f.cursor_pos, 1);
    }

    #[test]
    fn display_value_shows_secret_as_asterisks() {
        let mut f = TextField::new("pwd", true);
        f.value = "secret".to_owned();
        assert_eq!(f.display_value(), "******");
    }

    #[test]
    fn display_value_shows_plain_text() {
        let mut f = TextField::new("url", false);
        f.value = "http://example.com".to_owned();
        assert_eq!(f.display_value(), "http://example.com");
    }

    #[test]
    fn home_and_end_cursor_movement() {
        let mut f = TextField::new("x", false);
        f.value = "hello".to_owned();
        f.cursor_pos = 3;

        // simulate Home
        f.cursor_pos = 0;
        assert_eq!(f.cursor_pos, 0);

        // simulate End
        f.cursor_pos = f.value.chars().count();
        assert_eq!(f.cursor_pos, 5);
    }
}
