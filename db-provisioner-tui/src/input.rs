use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

use crate::app::{
    App, BackupPhase, HomeItem, InputTarget, MigratePhase, RestorePhase, Screen, SettingsItem,
};
use crate::models::SavedConnectionRecord;

pub fn handle_key_event(app: &mut App, key: KeyEvent) -> Result<()> {
    match app.screen {
        Screen::FirstRun => handle_first_run(app, key),
        Screen::Unlock => handle_unlock(app, key),
        Screen::Home => handle_home(app, key),
        Screen::ProvisionFull => handle_provision_full(app, key),
        Screen::MigrateDatabase => handle_migrate_database(app, key),
        Screen::BackupDatabase => handle_backup_database(app, key),
        Screen::RestoreDatabase => handle_restore_database(app, key),
        Screen::About => handle_about(app, key),
        Screen::ManageInstances => handle_manage_instances(app, key),
        Screen::AddInstance => handle_add_instance(app, key),
        Screen::ViewSavedConnections => handle_view_saved_connections(app, key),
        Screen::Settings => handle_settings(app, key),
        Screen::ChangePassword => handle_change_password(app, key),
        Screen::ConnectionWizard => handle_connection_wizard(app, key),
        Screen::ActiveQueries => handle_active_queries(app, key),
        Screen::ManageBackups => handle_manage_backups(app, key),
        Screen::EditConnectionAppName => handle_edit_connection_app_name(app, key),
    }
}

fn handle_first_run(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Tab | KeyCode::Down | KeyCode::Up => {
            app.focused_input = match app.focused_input {
                InputTarget::FirstRunPassword => InputTarget::FirstRunConfirm,
                _ => InputTarget::FirstRunPassword,
            };
        }
        KeyCode::Enter => app.start_first_run()?,
        _ => edit_text_field(app, key),
    }
    Ok(())
}

fn handle_unlock(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Enter => app.unlock()?,
        _ => edit_text_field(app, key),
    }
    Ok(())
}

fn handle_home(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Down => move_selection(&mut app.home_list_state, HomeItem::ALL.len(), 1),
        KeyCode::Up => move_selection(&mut app.home_list_state, HomeItem::ALL.len(), -1),
        KeyCode::PageDown => move_selection(&mut app.home_list_state, HomeItem::ALL.len(), 5),
        KeyCode::PageUp => move_selection(&mut app.home_list_state, HomeItem::ALL.len(), -5),
        KeyCode::End => set_selection(
            &mut app.home_list_state,
            HomeItem::ALL.len(),
            Some(HomeItem::ALL.len().saturating_sub(1)),
        ),
        KeyCode::Home => set_selection(&mut app.home_list_state, HomeItem::ALL.len(), Some(0)),
        KeyCode::Enter => match app.selected_home_item() {
            HomeItem::ProvisionFull => {
                app.screen = Screen::ProvisionFull;
                app.focused_input = InputTarget::ProvisionFullBaseUrl;
            }
            HomeItem::MigrateDatabase => {
                app.screen = Screen::MigrateDatabase;
                app.migrate_phase = MigratePhase::SelectSource;
                app.migrate_source_record = None;
                app.migrate_dest_instance = None;
                app.migrate_dest_db_name.clear();
                app.sync_migrate_source_selection()?;
            }
            HomeItem::BackupDatabase => {
                app.screen = Screen::BackupDatabase;
                app.backup_phase = BackupPhase::SelectSource;
                app.backup_source_record = None;
                app.sync_backup_source_selection()?;
            }
            HomeItem::RestoreDatabase => {
                app.screen = Screen::RestoreDatabase;
                app.restore_phase = RestorePhase::EnterFilePath;
                app.restore_file_path.clear();
                app.restore_dest_instance = None;
                app.restore_dest_db_name.clear();
            }
            HomeItem::ManageBackups => {
                app.screen = Screen::ManageBackups;
                app.refresh_backups_list();
                app.sync_backup_selection();
                app.confirming_delete_backup = false;
            }
            HomeItem::Instances => {
                app.screen = Screen::ManageInstances;
                app.confirming_delete_instance = false;
                app.trigger_health_checks();
            }
            HomeItem::ConnectionWizard => {
                app.screen = Screen::ConnectionWizard;
                app.focused_input = InputTarget::WizardInstanceName;
                app.clear_wizard();
                app.wizard_port.value = "5432".to_owned();
            }
            HomeItem::About => {
                app.screen = Screen::About;
            }
            HomeItem::ViewSavedConnections => {
                app.screen = Screen::ViewSavedConnections;
                app.sync_saved_connections_selection()?;
                app.reveal_selected_connection_string = false;
                app.trigger_health_checks();
            }
            HomeItem::Settings => {
                app.screen = Screen::Settings;
            }
            HomeItem::Quit => app.should_quit = true,
        },
        _ => {}
    }
    Ok(())
}

fn handle_provision_full(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => app.screen = Screen::Home,
        KeyCode::F(5) => app.load_full_base_uri()?,
        KeyCode::Tab | KeyCode::Down => {
            app.focused_input = match app.focused_input {
                InputTarget::ProvisionFullBaseUrl => InputTarget::ProvisionFullDatabaseName,
                InputTarget::ProvisionFullDatabaseName => InputTarget::ProvisionFullApplicationName,
                InputTarget::ProvisionFullApplicationName => {
                    InputTarget::ProvisionFullExtraUsername
                }
                InputTarget::ProvisionFullExtraUsername => {
                    InputTarget::ProvisionFullExtraApplicationName
                }
                _ => InputTarget::ProvisionFullBaseUrl,
            };
        }
        KeyCode::Up => {
            app.focused_input = match app.focused_input {
                InputTarget::ProvisionFullExtraApplicationName => {
                    InputTarget::ProvisionFullExtraUsername
                }
                InputTarget::ProvisionFullExtraUsername => {
                    InputTarget::ProvisionFullApplicationName
                }
                InputTarget::ProvisionFullApplicationName => InputTarget::ProvisionFullDatabaseName,
                InputTarget::ProvisionFullDatabaseName => InputTarget::ProvisionFullBaseUrl,
                _ => InputTarget::ProvisionFullExtraApplicationName,
            };
        }
        KeyCode::Enter => app.start_provision_full()?,
        _ => edit_text_field(app, key),
    }
    Ok(())
}

fn handle_migrate_database(app: &mut App, key: KeyEvent) -> Result<()> {
    match app.migrate_phase {
        MigratePhase::SelectSource => {
            app.sync_migrate_source_selection()?;
            let records = app.migrate_sources()?;

            match key.code {
                KeyCode::Esc => {
                    app.screen = Screen::Home;
                    app.migrate_phase = MigratePhase::SelectSource;
                    app.migrate_source_record = None;
                }
                KeyCode::Down => {
                    move_selection(&mut app.migrate_source_list_state, records.len(), 1);
                }
                KeyCode::Up => {
                    move_selection(&mut app.migrate_source_list_state, records.len(), -1);
                }
                KeyCode::Enter => {
                    let selected = app.migrate_selected_source()?;
                    if let Some(record) = selected {
                        app.migrate_source_record = Some(record);
                        app.migrate_phase = MigratePhase::SelectDestInstance;
                    } else {
                        app.set_status("No source connection selected.");
                    }
                }
                _ => {}
            }
        }
        MigratePhase::SelectDestInstance => {
            let len = app.instances.len();

            match key.code {
                KeyCode::Esc => {
                    app.migrate_phase = MigratePhase::SelectSource;
                    app.migrate_source_record = None;
                }
                KeyCode::Down => {
                    move_selection_if_nonempty(&mut app.migrate_dest_idx, len, 1);
                }
                KeyCode::Up => {
                    move_selection_if_nonempty(&mut app.migrate_dest_idx, len, -1);
                }
                KeyCode::Enter if len > 0 => {
                    app.migrate_dest_instance = Some(app.instances[app.migrate_dest_idx].clone());
                    app.migrate_phase = MigratePhase::EnterDestDbName;
                    app.focused_input = InputTarget::MigrateDestDbName;
                }
                _ => {}
            }
        }
        MigratePhase::EnterDestDbName => match key.code {
            KeyCode::Esc => {
                app.migrate_phase = MigratePhase::SelectDestInstance;
                app.migrate_dest_instance = None;
                app.migrate_dest_db_name.clear();
            }
            KeyCode::Enter => {
                app.start_migrate_database()?;
            }
            _ => edit_text_field(app, key),
        },
        MigratePhase::Running => {
            if let KeyCode::Esc = key.code {
                app.screen = Screen::Home;
            }
        }
    }
    Ok(())
}

fn handle_backup_database(app: &mut App, key: KeyEvent) -> Result<()> {
    match app.backup_phase {
        BackupPhase::SelectSource => {
            app.sync_backup_source_selection()?;
            let records = app.backup_sources()?;

            match key.code {
                KeyCode::Esc => {
                    app.screen = Screen::Home;
                    app.backup_phase = BackupPhase::SelectSource;
                    app.backup_source_record = None;
                }
                KeyCode::Down => {
                    move_selection(&mut app.backup_source_list_state, records.len(), 1);
                }
                KeyCode::Up => {
                    move_selection(&mut app.backup_source_list_state, records.len(), -1);
                }
                KeyCode::Enter => {
                    let selected = app
                        .backup_source_list_state
                        .selected()
                        .and_then(|idx| records.get(idx).cloned());
                    if let Some(record) = selected {
                        app.backup_source_record = Some(record);
                        app.start_backup_database()?;
                    } else {
                        app.set_status("No source connection selected.");
                    }
                }
                _ => {}
            }
        }
        BackupPhase::Running => {
            if let KeyCode::Esc = key.code {
                app.screen = Screen::Home;
            }
        }
    }
    Ok(())
}

fn handle_restore_database(app: &mut App, key: KeyEvent) -> Result<()> {
    match app.restore_phase {
        RestorePhase::EnterFilePath => match key.code {
            KeyCode::Esc => {
                app.screen = Screen::Home;
                app.restore_file_path.clear();
            }
            KeyCode::Enter => {
                if app.restore_file_path.value.trim().is_empty() {
                    app.set_status("Please enter a backup file path.");
                } else {
                    app.restore_phase = RestorePhase::SelectDestInstance;
                }
            }
            _ => edit_text_field(app, key),
        },
        RestorePhase::SelectDestInstance => {
            let len = app.instances.len();

            match key.code {
                KeyCode::Esc => {
                    app.restore_phase = RestorePhase::EnterFilePath;
                    app.restore_dest_instance = None;
                }
                KeyCode::Down => {
                    move_selection_if_nonempty(&mut app.restore_dest_idx, len, 1);
                }
                KeyCode::Up => {
                    move_selection_if_nonempty(&mut app.restore_dest_idx, len, -1);
                }
                KeyCode::Enter if len > 0 => {
                    app.restore_dest_instance = Some(app.instances[app.restore_dest_idx].clone());
                    app.restore_phase = RestorePhase::EnterDestDbName;
                    app.focused_input = InputTarget::RestoreDestDbName;
                }
                _ => {}
            }
        }
        RestorePhase::EnterDestDbName => match key.code {
            KeyCode::Esc => {
                app.restore_phase = RestorePhase::SelectDestInstance;
                app.restore_dest_instance = None;
                app.restore_dest_db_name.clear();
            }
            KeyCode::Enter => {
                app.start_restore_database()?;
            }
            _ => edit_text_field(app, key),
        },
        RestorePhase::Running => {
            if let KeyCode::Esc = key.code {
                app.screen = Screen::Home;
            }
        }
    }
    Ok(())
}

fn handle_view_saved_connections(app: &mut App, key: KeyEvent) -> Result<()> {
    app.sync_saved_connections_selection()?;
    let all_records = app.saved_connections()?;
    let records: Vec<&SavedConnectionRecord> = if app.search_active && !app.search_query.is_empty()
    {
        let q = app.search_query.to_lowercase();
        all_records
            .iter()
            .filter(|r| {
                let text = match r {
                    SavedConnectionRecord::Database(d) => format!(
                        "{} {} {} {}",
                        d.instance_name, d.database_name, d.role_name, d.application_name
                    ),
                    SavedConnectionRecord::ExtraUser(e) => format!(
                        "{} {} {} {}",
                        e.instance_name, e.database_name, e.username, e.application_name
                    ),
                    SavedConnectionRecord::Instance { name, .. } => name.clone(),
                };
                text.to_lowercase().contains(&q)
            })
            .collect()
    } else {
        all_records.iter().collect()
    };

    if app.confirming_delete {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.confirming_delete = false;
                app.delete_selected_connection()?;
            }
            KeyCode::Esc => {
                app.confirming_delete = false;
                app.reveal_selected_connection_string = false;
                app.screen = Screen::Home;
            }
            _ => {
                app.confirming_delete = false;
                app.set_status("Deletion cancelled.");
            }
        }
        return Ok(());
    }

    if app.search_active {
        match key.code {
            KeyCode::Esc => {
                app.search_active = false;
                app.search_query.clear();
                app.reveal_selected_connection_string = false;
                app.screen = Screen::Home;
            }
            KeyCode::Enter => {
                app.search_active = false;
            }
            KeyCode::Backspace => {
                app.search_query.pop();
            }
            KeyCode::Char(ch) => {
                app.search_query.push(ch);
            }
            _ => {}
        }
        return Ok(());
    }

    match key.code {
        KeyCode::Esc => {
            app.reveal_selected_connection_string = false;
            if app.search_active {
                app.search_active = false;
                app.search_query.clear();
            } else {
                app.screen = Screen::Home;
            }
        }
        KeyCode::Down => move_selection(&mut app.saved_connections_list_state, records.len(), 1),
        KeyCode::Up => move_selection(&mut app.saved_connections_list_state, records.len(), -1),
        KeyCode::PageDown => {
            move_selection(&mut app.saved_connections_list_state, records.len(), 5)
        }
        KeyCode::PageUp => move_selection(&mut app.saved_connections_list_state, records.len(), -5),
        KeyCode::Home => set_selection(
            &mut app.saved_connections_list_state,
            records.len(),
            Some(0),
        ),
        KeyCode::End => set_selection(
            &mut app.saved_connections_list_state,
            records.len(),
            Some(records.len().saturating_sub(1)),
        ),
        KeyCode::Enter => app.reveal_selected_record()?,
        KeyCode::Char('/') => {
            app.search_active = true;
            app.search_query.clear();
            app.set_status("Type to filter connections. Enter to stop filtering, Esc to go back.");
        }
        KeyCode::Char('r') => {
            app.trigger_health_checks();
            app.set_status("Health checks triggered.");
        }
        KeyCode::Char('e') => {
            app.start_edit_application_name()?;
        }
        KeyCode::Delete if !all_records.is_empty() => {
            app.confirming_delete = true;
            app.set_status("Delete this connection? Press y to confirm, any other key to cancel.");
        }
        _ => {}
    }
    Ok(())
}

fn handle_settings(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => app.screen = Screen::Home,
        KeyCode::Down => move_selection(&mut app.settings_list_state, SettingsItem::ALL.len(), 1),
        KeyCode::Up => move_selection(&mut app.settings_list_state, SettingsItem::ALL.len(), -1),
        KeyCode::PageDown => {
            move_selection(&mut app.settings_list_state, SettingsItem::ALL.len(), 5)
        }
        KeyCode::PageUp => {
            move_selection(&mut app.settings_list_state, SettingsItem::ALL.len(), -5)
        }
        KeyCode::Home => set_selection(
            &mut app.settings_list_state,
            SettingsItem::ALL.len(),
            Some(0),
        ),
        KeyCode::End => set_selection(
            &mut app.settings_list_state,
            SettingsItem::ALL.len(),
            Some(SettingsItem::ALL.len().saturating_sub(1)),
        ),
        KeyCode::Enter => match app.selected_settings_item() {
            SettingsItem::ChangeMasterPassword => {
                app.screen = Screen::ChangePassword;
                app.focused_input = InputTarget::ChangeCurrentPassword;
            }
            SettingsItem::Back => app.screen = Screen::Home,
        },
        _ => {}
    }
    Ok(())
}

fn handle_change_password(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => app.screen = Screen::Settings,
        KeyCode::Tab | KeyCode::Down => {
            app.focused_input = match app.focused_input {
                InputTarget::ChangeCurrentPassword => InputTarget::ChangeNewPassword,
                InputTarget::ChangeNewPassword => InputTarget::ChangeConfirmPassword,
                _ => InputTarget::ChangeCurrentPassword,
            };
        }
        KeyCode::Up => {
            app.focused_input = match app.focused_input {
                InputTarget::ChangeConfirmPassword => InputTarget::ChangeNewPassword,
                InputTarget::ChangeNewPassword => InputTarget::ChangeCurrentPassword,
                _ => InputTarget::ChangeConfirmPassword,
            };
        }
        KeyCode::Enter => app.change_master_password()?,
        _ => edit_text_field(app, key),
    }
    Ok(())
}

fn handle_about(app: &mut App, key: KeyEvent) -> Result<()> {
    if key.code == KeyCode::Esc {
        app.screen = Screen::Home;
    }
    Ok(())
}

fn handle_manage_instances(app: &mut App, key: KeyEvent) -> Result<()> {
    let len = app.instances.len();
    if app.confirming_delete_instance {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.delete_instance()?;
                app.set_status("Instance deleted.");
            }
            _ => {
                app.confirming_delete_instance = false;
                app.set_status("Deletion cancelled.");
            }
        }
        return Ok(());
    }

    match key.code {
        KeyCode::Esc => app.screen = Screen::Home,
        KeyCode::Down => move_selection_if_nonempty(&mut app.selected_instance_idx, len, 1),
        KeyCode::Up => move_selection_if_nonempty(&mut app.selected_instance_idx, len, -1),
        KeyCode::Char('a') => {
            app.instance_name_field.clear();
            app.instance_url_field.clear();
            app.screen = Screen::AddInstance;
            app.focused_input = InputTarget::InstanceName;
        }
        KeyCode::Enter => {
            if len > 0 {
                let name = app.selected_instance_in_mgmt();
                if app.selected_instance.as_deref() == Some(&name) {
                    app.selected_instance = None;
                    app.set_status("Deselected instance.");
                } else {
                    app.selected_instance = Some(name);
                    app.set_status(format!(
                        "Selected instance '{}'",
                        app.selected_instance.as_ref().unwrap()
                    ));
                }
                app.screen = Screen::Home;
            }
        }
        KeyCode::Delete if len > 0 => {
            app.confirming_delete_instance = true;
            app.set_status("Delete this instance? Press y to confirm, any other key to cancel.");
        }
        KeyCode::Char('r') => {
            app.trigger_health_checks();
            app.set_status("Health checks triggered.");
        }
        KeyCode::Char('m') if len > 0 => {
            let name = app.selected_instance_in_mgmt();
            let key = match app.session_key() {
                Ok(k) => k.to_vec(),
                Err(_) => return Ok(()),
            };
            if let Some(secret) = app.storage.load_instance_secret(&name).ok().flatten()
                && let Ok(pt) = crate::crypto::decrypt(&key, &secret.encrypted)
                && let Ok(cs) = String::from_utf8(pt.to_vec())
            {
                app.start_monitor_instance(cs, name);
            }
        }
        _ => {}
    }
    Ok(())
}

fn handle_add_instance(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.instance_name_field.clear();
            app.instance_url_field.clear();
            app.screen = Screen::ManageInstances;
        }
        KeyCode::Tab | KeyCode::Down | KeyCode::Up => {
            app.focused_input = match app.focused_input {
                InputTarget::InstanceName => InputTarget::InstanceUrl,
                _ => InputTarget::InstanceName,
            };
        }
        KeyCode::Enter => app.add_instance()?,
        _ => edit_text_field(app, key),
    }
    Ok(())
}

fn handle_connection_wizard(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.clear_wizard();
            app.screen = Screen::Home;
        }
        KeyCode::F(5) => {
            let url = app.build_url_from_wizard();
            match crate::postgres::check_connection_health(&url) {
                Ok((latency, version)) => {
                    app.set_status(format!(
                        "Connection OK ({}ms) — {}",
                        latency,
                        version.trim()
                    ));
                }
                Err(e) => {
                    app.set_status(format!("Connection failed: {e}"));
                }
            }
        }
        KeyCode::Tab | KeyCode::Down => {
            app.focused_input = match app.focused_input {
                InputTarget::WizardInstanceName => InputTarget::WizardHost,
                InputTarget::WizardHost => InputTarget::WizardPort,
                InputTarget::WizardPort => InputTarget::WizardUsername,
                InputTarget::WizardUsername => InputTarget::WizardPassword,
                InputTarget::WizardPassword => InputTarget::WizardDatabase,
                InputTarget::WizardDatabase => InputTarget::WizardSslMode,
                _ => InputTarget::WizardInstanceName,
            };
        }
        KeyCode::Up => {
            app.focused_input = match app.focused_input {
                InputTarget::WizardSslMode => InputTarget::WizardDatabase,
                InputTarget::WizardDatabase => InputTarget::WizardPassword,
                InputTarget::WizardPassword => InputTarget::WizardUsername,
                InputTarget::WizardUsername => InputTarget::WizardPort,
                InputTarget::WizardPort => InputTarget::WizardHost,
                InputTarget::WizardHost => InputTarget::WizardInstanceName,
                _ => InputTarget::WizardSslMode,
            };
        }
        KeyCode::Enter => app.add_instance_from_wizard()?,
        _ => edit_text_field(app, key),
    }
    Ok(())
}

fn handle_active_queries(app: &mut App, key: KeyEvent) -> Result<()> {
    let len = app.active_queries.len();
    if app.confirming_kill_query {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.kill_selected_query()?;
            }
            _ => {
                app.confirming_kill_query = false;
                app.set_status("Kill cancelled.");
            }
        }
        return Ok(());
    }

    match key.code {
        KeyCode::Esc => app.screen = Screen::ManageInstances,
        KeyCode::Down => move_selection(&mut app.queries_list_state, len, 1),
        KeyCode::Up => move_selection(&mut app.queries_list_state, len, -1),
        KeyCode::PageDown => move_selection(&mut app.queries_list_state, len, 5),
        KeyCode::PageUp => move_selection(&mut app.queries_list_state, len, -5),
        KeyCode::Home => set_selection(&mut app.queries_list_state, len, Some(0)),
        KeyCode::End => set_selection(
            &mut app.queries_list_state,
            len,
            Some(len.saturating_sub(1)),
        ),
        KeyCode::Char('r') => {
            app.refresh_active_queries();
            app.set_status("Queries refreshed.");
        }
        KeyCode::Char('k') if len > 0 => {
            app.confirming_kill_query = true;
            app.set_status("Terminate this query? Press y to confirm, any other key to cancel.");
        }
        _ => {}
    }
    Ok(())
}

fn handle_manage_backups(app: &mut App, key: KeyEvent) -> Result<()> {
    let len = app.backups_list.len();
    if app.confirming_delete_backup {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.confirming_delete_backup = false;
                if let Some(file) = app.selected_backup_file() {
                    let _ = std::fs::remove_file(&file.path);
                    app.refresh_backups_list();
                    app.sync_backup_selection();
                    app.set_status(format!("Deleted backup {}", file.filename));
                }
            }
            _ => {
                app.confirming_delete_backup = false;
                app.set_status("Deletion cancelled.");
            }
        }
        return Ok(());
    }

    match key.code {
        KeyCode::Esc => app.screen = Screen::Home,
        KeyCode::Down => move_selection(&mut app.backups_list_state, len, 1),
        KeyCode::Up => move_selection(&mut app.backups_list_state, len, -1),
        KeyCode::PageDown => move_selection(&mut app.backups_list_state, len, 5),
        KeyCode::PageUp => move_selection(&mut app.backups_list_state, len, -5),
        KeyCode::Home => set_selection(&mut app.backups_list_state, len, Some(0)),
        KeyCode::End => set_selection(
            &mut app.backups_list_state,
            len,
            Some(len.saturating_sub(1)),
        ),
        KeyCode::Enter => {
            if let Some(file) = app.selected_backup_file() {
                app.restore_file_path.value = file.path.to_string_lossy().to_string();
                app.restore_phase = RestorePhase::EnterFilePath;
                app.restore_dest_instance = None;
                app.restore_dest_db_name.clear();
                app.screen = Screen::RestoreDatabase;
            }
        }
        KeyCode::Delete if len > 0 => {
            app.confirming_delete_backup = true;
            app.set_status("Delete this backup? Press y to confirm, any other key to cancel.");
        }
        KeyCode::Char('r') => {
            app.refresh_backups_list();
            app.sync_backup_selection();
            app.set_status("Backup list refreshed.");
        }
        _ => {}
    }
    Ok(())
}

fn handle_edit_connection_app_name(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.editing_app_name_field.clear();
            app.editing_target = None;
            app.screen = Screen::ViewSavedConnections;
        }
        KeyCode::Enter => app.save_application_name_edit()?,
        _ => edit_text_field(app, key),
    }
    Ok(())
}

fn edit_text_field(app: &mut App, key: KeyEvent) {
    let field = app.input_mut(app.focused_input);
    match key.code {
        KeyCode::Left => {
            field.cursor_pos = field.cursor_pos.saturating_sub(1);
        }
        KeyCode::Right => {
            let max = field.value.chars().count();
            if field.cursor_pos < max {
                field.cursor_pos += 1;
            }
        }
        KeyCode::Home => {
            field.cursor_pos = 0;
        }
        KeyCode::End => {
            field.cursor_pos = field.value.chars().count();
        }
        KeyCode::Backspace => {
            field.delete_before_cursor();
        }
        KeyCode::Delete => {
            field.delete_after_cursor();
        }
        KeyCode::Char(ch) => {
            field.insert_char(ch);
        }
        _ => {}
    }
}

fn move_selection_if_nonempty(selected: &mut usize, len: usize, delta: isize) {
    if len == 0 {
        return;
    }
    let current = *selected as isize;
    let next = (current + delta).rem_euclid(len as isize) as usize;
    *selected = next;
}

fn move_selection(state: &mut ratatui::widgets::ListState, len: usize, delta: isize) {
    if len == 0 {
        state.select(None);
        return;
    }

    let current = state.selected().unwrap_or(0) as isize;
    let next = (current + delta).rem_euclid(len as isize) as usize;
    state.select(Some(next));
}

fn set_selection(state: &mut ratatui::widgets::ListState, len: usize, idx: Option<usize>) {
    match (len, idx) {
        (0, _) => state.select(None),
        (_, Some(idx)) => state.select(Some(idx.min(len - 1))),
        (_, None) => state.select(None),
    }
}
