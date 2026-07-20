use anyhow::Result;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

use crate::app::{
    App, BackupPhase, HealthStatus, HomeItem, InputTarget, MigratePhase, RestorePhase, Screen,
    SettingsItem, TextField,
};
use crate::models::{PgToolBackend, SavedConnectionRecord};

pub fn draw(frame: &mut Frame<'_>, app: &mut App) -> Result<()> {
    match app.screen {
        Screen::FirstRun => draw_first_run(frame, app),
        Screen::Unlock => draw_unlock(frame, app),
        Screen::Home => draw_home(frame, app),
        Screen::ProvisionFull => draw_provision_full(frame, app),
        Screen::MigrateDatabase => draw_migrate_database(frame, app)?,
        Screen::BackupDatabase => draw_backup_database(frame, app)?,
        Screen::RestoreDatabase => draw_restore_database(frame, app)?,
        Screen::About => draw_about(frame, app),
        Screen::ManageInstances => draw_manage_instances(frame, app)?,
        Screen::AddInstance => draw_add_instance(frame, app),
        Screen::ViewSavedConnections => draw_view_saved_connections(frame, app)?,
        Screen::Settings => draw_settings(frame, app),
        Screen::ChangePassword => draw_change_password(frame, app),
        Screen::ConnectionWizard => draw_connection_wizard(frame, app),
        Screen::ActiveQueries => draw_active_queries(frame, app)?,
        Screen::ManageBackups => draw_manage_backups(frame, app)?,
        Screen::EditConnectionAppName => draw_edit_connection_app_name(frame, app),
    }
    Ok(())
}

fn draw_first_run(frame: &mut Frame<'_>, app: &App) {
    let area = centered_rect(frame.area(), 70, 45);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title("First Run Setup")
        .borders(Borders::ALL);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(3),
        ])
        .split(area);

    frame.render_widget(
        Paragraph::new("Set the master password used to encrypt all stored secrets."),
        chunks[0],
    );
    render_field(
        frame,
        chunks[1],
        &app.first_run_password,
        app.focused_input == InputTarget::FirstRunPassword,
    );
    render_field(
        frame,
        chunks[2],
        &app.first_run_confirm,
        app.focused_input == InputTarget::FirstRunConfirm,
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("Enter saves the password."),
            Line::from(app.status.clone()),
        ])
        .wrap(Wrap { trim: true }),
        chunks[3],
    );

    let (area, field) = match app.focused_input {
        InputTarget::FirstRunPassword => (chunks[1], &app.first_run_password),
        _ => (chunks[2], &app.first_run_confirm),
    };
    set_cursor(frame, area, field, true);
}

fn draw_unlock(frame: &mut Frame<'_>, app: &App) {
    let area = centered_rect(frame.area(), 60, 35);
    frame.render_widget(Clear, area);
    let block = Block::default().title("Unlock").borders(Borders::ALL);
    frame.render_widget(block, area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(2),
        ])
        .split(area);
    render_field(frame, chunks[0], &app.unlock_password, true);
    frame.render_widget(
        Paragraph::new("Enter unlocks the app. Secrets remain encrypted at rest."),
        chunks[1],
    );
    frame.render_widget(Paragraph::new(app.status.clone()), chunks[2]);
    set_cursor(frame, chunks[0], &app.unlock_password, true);
}

fn draw_home(frame: &mut Frame<'_>, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(10), Constraint::Length(6)])
        .split(frame.area());

    let items: Vec<ListItem<'_>> = HomeItem::ALL
        .iter()
        .map(|item| ListItem::new(item.label()))
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .title("db-provisioner-tui")
                .borders(Borders::ALL),
        )
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
        .highlight_symbol(">> ");

    frame.render_stateful_widget(list, chunks[0], &mut app.home_list_state);
    draw_footer(frame, chunks[1], app, "Use arrows and Enter. q quits.");
}

fn draw_provision_full(frame: &mut Frame<'_>, app: &App) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(5),
        ])
        .split(frame.area());

    let instance_label = match &app.selected_instance {
        Some(name) => format!("Instance: {}  F5: load saved URI", name),
        None => {
            "No instance selected. Type a URL or select one from Instances.  F5: load saved URI"
                .to_owned()
        }
    };

    frame.render_widget(
        Paragraph::new(instance_label).block(
            Block::default()
                .title("Full Provisioning")
                .borders(Borders::ALL),
        ),
        layout[0],
    );

    render_field(
        frame,
        layout[1],
        &app.provision_full_base_url,
        app.focused_input == InputTarget::ProvisionFullBaseUrl,
    );
    render_field(
        frame,
        layout[2],
        &app.provision_full_database_name,
        app.focused_input == InputTarget::ProvisionFullDatabaseName,
    );
    render_field(
        frame,
        layout[3],
        &app.provision_full_application_name,
        app.focused_input == InputTarget::ProvisionFullApplicationName,
    );
    render_field(
        frame,
        layout[4],
        &app.provision_full_extra_username,
        app.focused_input == InputTarget::ProvisionFullExtraUsername,
    );
    render_field(
        frame,
        layout[5],
        &app.provision_full_extra_application_name,
        app.focused_input == InputTarget::ProvisionFullExtraApplicationName,
    );

    render_log_panel(frame, layout[6], app, "Provisioning Log");

    draw_footer(
        frame,
        layout[7],
        app,
        "Enter provisions. Tab/Up/Down switches field. F5 loads saved URI. Esc back.",
    );

    let (cursor_area, field) = match app.focused_input {
        InputTarget::ProvisionFullBaseUrl => (layout[1], &app.provision_full_base_url),
        InputTarget::ProvisionFullDatabaseName => (layout[2], &app.provision_full_database_name),
        InputTarget::ProvisionFullApplicationName => {
            (layout[3], &app.provision_full_application_name)
        }
        InputTarget::ProvisionFullExtraUsername => (layout[4], &app.provision_full_extra_username),
        InputTarget::ProvisionFullExtraApplicationName => {
            (layout[5], &app.provision_full_extra_application_name)
        }
        _ => return,
    };
    set_cursor(frame, cursor_area, field, true);
}

fn draw_migrate_database(frame: &mut Frame<'_>, app: &mut App) -> Result<()> {
    match app.migrate_phase {
        MigratePhase::SelectSource => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(8), Constraint::Length(5)])
                .split(frame.area());

            let records = app.migrate_sources()?;
            let items: Vec<ListItem<'_>> = if records.is_empty() {
                vec![ListItem::new("No saved connections to migrate")]
            } else {
                records
                    .iter()
                    .map(|record| {
                        let label = match record {
                            SavedConnectionRecord::Database(r) => format!(
                                "db | {} | {} | {}",
                                r.instance_name, r.database_name, r.created_at
                            ),
                            SavedConnectionRecord::ExtraUser(r) => format!(
                                "user | {} | {} | {}",
                                r.instance_name, r.database_name, r.created_at
                            ),
                            SavedConnectionRecord::Instance { name, .. } => {
                                format!("instance | {} | (base URL)", name)
                            }
                        };
                        ListItem::new(label)
                    })
                    .collect()
            };

            let list = List::new(items)
                .block(
                    Block::default()
                        .title("Select source connection")
                        .borders(Borders::ALL),
                )
                .highlight_style(Style::default().add_modifier(Modifier::BOLD))
                .highlight_symbol(">> ");

            frame.render_stateful_widget(list, layout[0], &mut app.migrate_source_list_state);

            draw_footer(
                frame,
                layout[1],
                app,
                "Up/Down: navigate  Enter: select source  Esc: back",
            );
        }
        MigratePhase::SelectDestInstance => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(8), Constraint::Length(5)])
                .split(frame.area());

            let items: Vec<ListItem<'_>> = if app.instances.is_empty() {
                vec![ListItem::new("No instances defined")]
            } else {
                app.instances
                    .iter()
                    .enumerate()
                    .map(|(i, name)| {
                        let marker = if i == app.migrate_dest_idx {
                            ">> "
                        } else {
                            "   "
                        };
                        ListItem::new(format!("{}{}", marker, name))
                    })
                    .collect()
            };

            let list = List::new(items)
                .block(
                    Block::default()
                        .title("Select destination instance")
                        .borders(Borders::ALL),
                )
                .highlight_style(Style::default().add_modifier(Modifier::BOLD))
                .highlight_symbol("");

            frame.render_widget(list, layout[0]);

            draw_footer(
                frame,
                layout[1],
                app,
                "Up/Down: navigate  Enter: select destination  Esc: back",
            );
        }
        MigratePhase::EnterDestDbName => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Min(3),
                ])
                .split(frame.area());

            let src_label = match &app.migrate_source_record {
                Some(SavedConnectionRecord::Database(r)) => {
                    format!("Source: {} / {}", r.instance_name, r.database_name)
                }
                Some(SavedConnectionRecord::ExtraUser(r)) => {
                    format!(
                        "Source: {} / {} (user: {})",
                        r.instance_name, r.database_name, r.username
                    )
                }
                Some(SavedConnectionRecord::Instance { name, .. }) => {
                    format!("Source: {} (base URL)", name)
                }
                None => "Source: none".to_owned(),
            };
            let dest_label = match &app.migrate_dest_instance {
                Some(name) => format!("Destination instance: {}", name),
                None => "Destination: none".to_owned(),
            };

            frame.render_widget(
                Paragraph::new(vec![Line::from(src_label), Line::from(dest_label)]).block(
                    Block::default()
                        .title("Migration Details")
                        .borders(Borders::ALL),
                ),
                layout[0],
            );

            render_field(
                frame,
                layout[1],
                &app.migrate_dest_db_name,
                app.focused_input == InputTarget::MigrateDestDbName,
            );

            set_cursor(frame, layout[1], &app.migrate_dest_db_name, true);

            render_log_panel(frame, layout[2], app, "Migration Log");

            draw_footer(
                frame,
                layout[3],
                app,
                "Enter database name and press Enter to start migration. Esc back.",
            );
        }
        MigratePhase::Running => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(5)])
                .split(frame.area());

            render_log_panel(frame, layout[0], app, "Migration Progress");

            draw_footer(
                frame,
                layout[1],
                app,
                "Migration in progress. You can keep navigating the TUI.",
            );
        }
    }
    Ok(())
}

fn draw_view_saved_connections(frame: &mut Frame<'_>, app: &mut App) -> Result<()> {
    let mut constraints = vec![Constraint::Length(3)];
    constraints.push(Constraint::Min(10));
    constraints.push(Constraint::Length(10));
    constraints.push(Constraint::Length(5));

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());

    let all_records = app.saved_connections()?;
    let status_map = app.health_status.lock().unwrap();

    // Determine which records to show based on search
    let filtered_records: Vec<&SavedConnectionRecord> =
        if app.search_active && !app.search_query.is_empty() {
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

    // Search bar
    if app.search_active {
        let search_prefix = format!("/ {}", app.search_query);
        let search_para = Paragraph::new(search_prefix.as_str())
            .block(Block::default().title("Search").borders(Borders::ALL));
        frame.render_widget(search_para, layout[0]);
    }

    let items: Vec<ListItem<'_>> = if filtered_records.is_empty() {
        vec![ListItem::new("No saved connections yet")]
    } else {
        filtered_records
            .iter()
            .map(|record| {
                let (id_key, label) = match record {
                    SavedConnectionRecord::Database(record) => (
                        format!("conn:db:{}", record.id),
                        format!(
                            "db | {} | {} | {} | {}",
                            record.instance_name,
                            record.database_name,
                            record.role_name,
                            record.created_at
                        ),
                    ),
                    SavedConnectionRecord::ExtraUser(record) => (
                        format!("conn:user:{}", record.id),
                        format!(
                            "user | {} | {} | {} | {}",
                            record.instance_name,
                            record.database_name,
                            record.username,
                            record.created_at
                        ),
                    ),
                    SavedConnectionRecord::Instance { name, .. } => (
                        format!("conn:instance:{name}"),
                        format!("instance | {name} | (base URL)"),
                    ),
                };
                let badge = match status_map.get(&id_key) {
                    Some(HealthStatus::Ok { latency_ms, .. }) => {
                        format!(" [OK {}ms]", latency_ms)
                    }
                    Some(HealthStatus::Error(_)) => " [ERR]".to_owned(),
                    Some(HealthStatus::Checking) => " [...]".to_owned(),
                    None | Some(HealthStatus::Unknown) => String::new(),
                };
                ListItem::new(format!("{}{}", label, badge))
            })
            .collect()
    };
    drop(status_map);

    let list_start = if app.search_active { 1 } else { 0 };
    let list = List::new(items)
        .block(
            Block::default()
                .title("Saved Connections")
                .borders(Borders::ALL),
        )
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
        .highlight_symbol(">> ");

    frame.render_stateful_widget(
        list,
        layout[list_start],
        &mut app.saved_connections_list_state,
    );

    let selected = app
        .saved_connections_list_state
        .selected()
        .and_then(|idx| filtered_records.get(idx).copied());

    let detail = match selected {
        Some(SavedConnectionRecord::Database(record)) => {
            let mut lines = vec![
                Line::from("Type: Database owner connection"),
                Line::from(format!("Instance: {}", record.instance_name)),
                Line::from(format!("Database: {}", record.database_name)),
                Line::from(format!("Application: {}", record.application_name)),
                Line::from(format!("Role: {}", record.role_name)),
                Line::from(format!("Database created: {}", record.database_created)),
                Line::from(format!("Role created: {}", record.role_created)),
            ];
            append_reveal_line(&mut lines, app);
            lines
        }
        Some(SavedConnectionRecord::ExtraUser(record)) => {
            let mut lines = vec![
                Line::from("Type: Extra user connection"),
                Line::from(format!("Instance: {}", record.instance_name)),
                Line::from(format!("Database: {}", record.database_name)),
                Line::from(format!("Username: {}", record.username)),
                Line::from(format!("Application: {}", record.application_name)),
                Line::from(format!("Role created: {}", record.role_created)),
                Line::from(format!("Grants applied: {}", record.grants_applied)),
            ];
            append_reveal_line(&mut lines, app);
            lines
        }
        Some(SavedConnectionRecord::Instance { name, .. }) => {
            vec![
                Line::from("Type: Instance connection"),
                Line::from(format!("Name: {name}")),
                Line::from("Encrypted base URL available for backup/migration"),
            ]
        }
        None => vec![Line::from("No connection selected")],
    };

    let detail_start = if app.search_active { 2 } else { 1 };
    frame.render_widget(
        Paragraph::new(detail)
            .block(Block::default().title("Details").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        layout[detail_start],
    );

    let footer_start = if app.search_active { 3 } else { 2 };
    let footer_text = if app.search_active {
        "Type to filter. Enter stops filtering. Esc goes back."
    } else {
        "/: search  Enter reveals password. e: edit name. Delete: remove. r: health check. Esc back."
    };
    draw_footer(frame, layout[footer_start], app, footer_text);

    Ok(())
}

fn draw_about(frame: &mut Frame<'_>, app: &App) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(3),
        ])
        .split(frame.area());

    let info = vec![
        Line::from(format!("Version: {}", env!("CARGO_PKG_VERSION"))),
        Line::from(format!("Description: {}", env!("CARGO_PKG_DESCRIPTION"))),
        Line::from(Span::raw("")),
        Line::from("Stores provisioned connection strings encrypted in SQLite."),
        Line::from("Encryption: AES-256-GCM with Argon2id key derivation."),
        Line::from("No external secrets storage (no AWS SSM)."),
        Line::from("Background worker keeps the UI responsive during provisioning."),
    ];

    frame.render_widget(
        Paragraph::new(info)
            .block(Block::default().title("About").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        layout[0],
    );

    let authors = format!("Authors: {}", env!("CARGO_PKG_AUTHORS"));
    let repository = format!("Repository: {}", env!("CARGO_PKG_REPOSITORY"));
    let license = format!("License: {}", env!("CARGO_PKG_LICENSE"));

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(authors),
            Line::from(repository),
            Line::from(license),
        ])
        .block(Block::default().title("Metadata").borders(Borders::ALL))
        .wrap(Wrap { trim: true }),
        layout[1],
    );

    frame.render_widget(
        Paragraph::new(vec![
            Line::from("Storage location:"),
            Line::from(crate::storage::display_database_path().unwrap_or_default()),
        ])
        .block(Block::default().title("Storage").borders(Borders::ALL))
        .wrap(Wrap { trim: true }),
        layout[2],
    );

    let tool_info = match &app.pg_tool_backend {
        PgToolBackend::Native {
            dump_ver,
            restore_ver,
        } => vec![
            Line::from(format!("pg_dump:    native  ({})", dump_ver.trim())),
            Line::from(format!("pg_restore: native  ({})", restore_ver.trim())),
        ],
        PgToolBackend::Docker { image } => {
            vec![Line::from(format!("pg_dump/pg_restore: Docker ({image})"))]
        }
        PgToolBackend::NotFound => vec![
            Line::from("pg_dump/pg_restore: NOT FOUND"),
            Line::from("Install postgresql client tools or Docker"),
        ],
    };

    frame.render_widget(
        Paragraph::new(tool_info)
            .block(Block::default().title("Tools").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        layout[3],
    );

    draw_footer(frame, layout[4], app, "Esc goes back to Home.");
}

fn draw_manage_instances(frame: &mut Frame<'_>, app: &mut App) -> Result<()> {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(7)])
        .split(frame.area());

    let items: Vec<ListItem<'_>> = if app.instances.is_empty() {
        vec![ListItem::new("No instances defined")]
    } else {
        let status_map = app.health_status.lock().unwrap();
        app.instances
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let marker = if i == app.selected_instance_idx {
                    ">> "
                } else {
                    "   "
                };
                let suffix = if app.selected_instance.as_deref() == Some(name) {
                    " [selected]"
                } else {
                    ""
                };
                let badge = match status_map.get(&format!("instance:{name}")) {
                    Some(HealthStatus::Ok { latency_ms, .. }) => {
                        format!(" [OK {}ms]", latency_ms)
                    }
                    Some(HealthStatus::Error(_)) => " [ERR]".to_owned(),
                    Some(HealthStatus::Checking) => " [...]".to_owned(),
                    None | Some(HealthStatus::Unknown) => String::new(),
                };
                ListItem::new(format!("{}{}{}{}", marker, name, suffix, badge))
            })
            .collect()
    };

    let list = List::new(items)
        .block(
            Block::default()
                .title("Manage Instances")
                .borders(Borders::ALL),
        )
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
        .highlight_symbol("");

    frame.render_widget(list, layout[0]);

    let help = if app.confirming_delete_instance {
        "Delete this instance? Press y to confirm, any other key to cancel."
    } else {
        "a: add  Enter: select  Delete: remove  m: monitor queries  r: health check  Esc: back"
    };
    draw_footer(frame, layout[1], app, help);
    Ok(())
}

fn draw_add_instance(frame: &mut Frame<'_>, app: &App) {
    let area = centered_rect(frame.area(), 70, 35);
    frame.render_widget(Clear, area);
    let block = Block::default().title("Add Instance").borders(Borders::ALL);
    frame.render_widget(block, area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(2),
        ])
        .split(area);
    render_field(
        frame,
        chunks[0],
        &app.instance_name_field,
        app.focused_input == InputTarget::InstanceName,
    );
    render_field(
        frame,
        chunks[1],
        &app.instance_url_field,
        app.focused_input == InputTarget::InstanceUrl,
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("Enter saves. Esc cancels. Tab switches fields."),
            Line::from(app.status.clone()),
        ])
        .wrap(Wrap { trim: true }),
        chunks[2],
    );

    let (cursor_area, field) = match app.focused_input {
        InputTarget::InstanceName => (chunks[0], &app.instance_name_field),
        _ => (chunks[1], &app.instance_url_field),
    };
    set_cursor(frame, cursor_area, field, true);
}

fn draw_settings(frame: &mut Frame<'_>, app: &mut App) {
    let items: Vec<ListItem<'_>> = SettingsItem::ALL
        .iter()
        .map(|item| ListItem::new(item.label()))
        .collect();
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(5)])
        .split(frame.area());
    let list = List::new(items)
        .block(Block::default().title("Settings").borders(Borders::ALL))
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
        .highlight_symbol(">> ");
    frame.render_stateful_widget(list, layout[0], &mut app.settings_list_state);
    draw_footer(frame, layout[1], app, "Enter opens. Esc goes back.");
}

fn draw_change_password(frame: &mut Frame<'_>, app: &App) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(5),
        ])
        .split(frame.area());

    render_field(
        frame,
        layout[0],
        &app.change_current_password,
        app.focused_input == InputTarget::ChangeCurrentPassword,
    );
    render_field(
        frame,
        layout[1],
        &app.change_new_password,
        app.focused_input == InputTarget::ChangeNewPassword,
    );
    render_field(
        frame,
        layout[2],
        &app.change_confirm_password,
        app.focused_input == InputTarget::ChangeConfirmPassword,
    );

    draw_footer(
        frame,
        layout[3],
        app,
        "Enter re-encrypts all secrets with the new password. Esc goes back.",
    );

    let (cursor_area, field) = match app.focused_input {
        InputTarget::ChangeCurrentPassword => (layout[0], &app.change_current_password),
        InputTarget::ChangeNewPassword => (layout[1], &app.change_new_password),
        _ => (layout[2], &app.change_confirm_password),
    };
    set_cursor(frame, cursor_area, field, true);
}

fn render_log_panel(frame: &mut Frame<'_>, area: Rect, app: &App, title: &str) {
    let log_lines: Vec<Line<'_>> = if app.logs.is_empty() {
        if app.pending_operation {
            vec![Line::from("Operation in progress...")]
        } else {
            vec![Line::from("No operation run yet.")]
        }
    } else {
        app.logs.iter().cloned().map(Line::from).collect()
    };

    frame.render_widget(
        Paragraph::new(log_lines)
            .block(Block::default().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn append_reveal_line(lines: &mut Vec<Line<'_>>, app: &App) {
    if app.reveal_selected_connection_string {
        lines.push(Line::from(Span::styled(
            app.status.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
    } else {
        lines.push(Line::from(
            "Press Enter to reveal the decrypted connection string.",
        ));
    }
}

fn set_cursor(frame: &mut Frame<'_>, area: Rect, field: &TextField, focused: bool) {
    if focused {
        let byte_pos = field
            .value
            .char_indices()
            .nth(field.cursor_pos)
            .map(|(i, _)| i)
            .unwrap_or(field.value.len());
        frame.set_cursor_position((area.x + 1 + byte_pos as u16, area.y + 1));
    }
}

fn render_field(frame: &mut Frame<'_>, area: Rect, field: &TextField, focused: bool) {
    let title = if focused {
        format!("> {}", field.label)
    } else {
        field.label.to_owned()
    };
    frame.render_widget(
        Paragraph::new(field.display_value())
            .block(Block::default().title(title).borders(Borders::ALL)),
        area,
    );
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &App, help: &str) {
    frame.render_widget(
        Paragraph::new(vec![Line::from(help), Line::from(app.status.clone())])
            .block(Block::default().title("Status").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_connection_wizard(frame: &mut Frame<'_>, app: &App) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(3),
        ])
        .split(frame.area());

    frame.render_widget(
        Paragraph::new("Connection Wizard — fill in the fields to build a DATABASE_URL").block(
            Block::default()
                .title("Connection Wizard")
                .borders(Borders::ALL),
        ),
        layout[0],
    );

    render_field(
        frame,
        layout[1],
        &app.wizard_instance_name,
        app.focused_input == InputTarget::WizardInstanceName,
    );
    render_field(
        frame,
        layout[2],
        &app.wizard_host,
        app.focused_input == InputTarget::WizardHost,
    );
    render_field(
        frame,
        layout[3],
        &app.wizard_port,
        app.focused_input == InputTarget::WizardPort,
    );
    render_field(
        frame,
        layout[4],
        &app.wizard_username,
        app.focused_input == InputTarget::WizardUsername,
    );
    render_field(
        frame,
        layout[5],
        &app.wizard_password,
        app.focused_input == InputTarget::WizardPassword,
    );
    render_field(
        frame,
        layout[6],
        &app.wizard_database,
        app.focused_input == InputTarget::WizardDatabase,
    );
    render_field(
        frame,
        layout[7],
        &app.wizard_ssl_mode,
        app.focused_input == InputTarget::WizardSslMode,
    );

    let preview = format!("URL: {}", app.build_url_from_wizard());
    let preview_para = Paragraph::new(vec![
        Line::from(preview),
        Line::from(String::new()),
        Line::from(app.status.clone()),
    ])
    .block(Block::default().title("Preview").borders(Borders::ALL))
    .wrap(Wrap { trim: true });
    frame.render_widget(preview_para, layout[8]);

    let cursor = match app.focused_input {
        InputTarget::WizardInstanceName => (layout[1], &app.wizard_instance_name),
        InputTarget::WizardHost => (layout[2], &app.wizard_host),
        InputTarget::WizardPort => (layout[3], &app.wizard_port),
        InputTarget::WizardUsername => (layout[4], &app.wizard_username),
        InputTarget::WizardPassword => (layout[5], &app.wizard_password),
        InputTarget::WizardDatabase => (layout[6], &app.wizard_database),
        InputTarget::WizardSslMode => (layout[7], &app.wizard_ssl_mode),
        _ => return,
    };
    set_cursor(frame, cursor.0, cursor.1, true);
}

fn draw_active_queries(frame: &mut Frame<'_>, app: &mut App) -> Result<()> {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(8),
            Constraint::Length(5),
        ])
        .split(frame.area());

    let header = format!(
        "Instance: {}  Queries: {}",
        app.queries_instance,
        app.active_queries.len()
    );
    frame.render_widget(
        Paragraph::new(header).block(
            Block::default()
                .title("Active Queries")
                .borders(Borders::ALL),
        ),
        layout[0],
    );

    let items: Vec<ListItem<'_>> = if app.active_queries.is_empty() {
        vec![ListItem::new("No active queries")]
    } else {
        app.active_queries
            .iter()
            .map(|q| {
                let duration = if q.duration_secs < 60 {
                    format!("{}s", q.duration_secs)
                } else {
                    format!("{}m {}s", q.duration_secs / 60, q.duration_secs % 60)
                };
                ListItem::new(format!(
                    "PID:{:<6} {}  {:<15}  {:<8}  {}",
                    q.pid, q.user, duration, q.state, q.database
                ))
            })
            .collect()
    };

    let list = List::new(items)
        .block(
            Block::default()
                .title("PID  | User     | Duration     | State     | Database")
                .borders(Borders::ALL),
        )
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
        .highlight_symbol(">> ");

    frame.render_stateful_widget(list, layout[1], &mut app.queries_list_state);

    let detail = app
        .queries_list_state
        .selected()
        .and_then(|idx| app.active_queries.get(idx))
        .map(|q| {
            let q_short: String = q.query.chars().take(200).collect();
            vec![
                Line::from(format!(
                    "PID: {}  User: {}  DB: {}  Client: {}",
                    q.pid, q.user, q.database, q.client_addr
                )),
                Line::from(format!(
                    "State: {}  Duration: {}s",
                    q.state, q.duration_secs
                )),
                Line::from(""),
                Line::from(format!("Query: {}", q_short)),
            ]
        })
        .unwrap_or_else(|| vec![Line::from("Select a query to see details")]);

    frame.render_widget(
        Paragraph::new(detail)
            .block(Block::default().title("Query Detail").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        layout[2],
    );

    let help = if app.confirming_kill_query {
        "Terminate this query? Press y to confirm, any other key to cancel."
    } else if app.active_queries.is_empty() {
        "Esc: back"
    } else {
        "Up/Down: navigate  r: refresh  k: kill query  Esc: back"
    };
    draw_footer(frame, layout[3], app, help);

    Ok(())
}

fn draw_manage_backups(frame: &mut Frame<'_>, app: &mut App) -> Result<()> {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(7)])
        .split(frame.area());

    let items: Vec<ListItem<'_>> = if app.backups_list.is_empty() {
        vec![ListItem::new("No backups found")]
    } else {
        app.backups_list
            .iter()
            .map(|f| {
                let size_str = if f.size < 1024 {
                    format!("{} B", f.size)
                } else if f.size < 1024 * 1024 {
                    format!("{:.1} KB", f.size as f64 / 1024.0)
                } else {
                    format!("{:.1} MB", f.size as f64 / (1024.0 * 1024.0))
                };
                ListItem::new(format!("{}  {}  {}", f.modified, size_str, f.filename))
            })
            .collect()
    };

    let list = List::new(items)
        .block(
            Block::default()
                .title("Manage Backups")
                .borders(Borders::ALL),
        )
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
        .highlight_symbol(">> ");

    frame.render_stateful_widget(list, layout[0], &mut app.backups_list_state);

    let help = if app.confirming_delete_backup {
        "Delete this backup? Press y to confirm, any other key to cancel."
    } else {
        "Up/Down: navigate  Enter: restore  Delete: remove  r: refresh  Esc: back"
    };
    draw_footer(frame, layout[1], app, help);

    Ok(())
}

fn draw_edit_connection_app_name(frame: &mut Frame<'_>, app: &mut App) {
    let area = centered_rect(frame.area(), 60, 25);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title("Edit Application Name")
        .borders(Borders::ALL);
    frame.render_widget(block, area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([Constraint::Length(3), Constraint::Min(2)])
        .split(area);
    render_field(frame, chunks[0], &app.editing_app_name_field, true);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("Enter saves. Esc cancels."),
            Line::from(app.status.clone()),
        ])
        .wrap(Wrap { trim: true }),
        chunks[1],
    );
    set_cursor(frame, chunks[0], &app.editing_app_name_field, true);
}

fn draw_backup_database(frame: &mut Frame<'_>, app: &mut App) -> Result<()> {
    match app.backup_phase {
        BackupPhase::SelectSource => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(8), Constraint::Length(5)])
                .split(frame.area());

            app.sync_backup_source_selection()?;
            let records = app.backup_sources()?;
            let items: Vec<ListItem<'_>> = if records.is_empty() {
                vec![ListItem::new("No saved connections to backup")]
            } else {
                records
                    .iter()
                    .map(|record| {
                        let label = match record {
                            SavedConnectionRecord::Database(r) => format!(
                                "db | {} | {} | {}",
                                r.instance_name, r.database_name, r.created_at
                            ),
                            SavedConnectionRecord::ExtraUser(r) => format!(
                                "user | {} | {} | {}",
                                r.instance_name, r.database_name, r.created_at
                            ),
                            SavedConnectionRecord::Instance { name, .. } => {
                                format!("instance | {} | (base URL)", name)
                            }
                        };
                        ListItem::new(label)
                    })
                    .collect()
            };

            let list = List::new(items)
                .block(
                    Block::default()
                        .title("Select a connection to backup")
                        .borders(Borders::ALL),
                )
                .highlight_style(Style::default().add_modifier(Modifier::BOLD))
                .highlight_symbol(">> ");

            frame.render_stateful_widget(list, layout[0], &mut app.backup_source_list_state);

            draw_footer(
                frame,
                layout[1],
                app,
                "Up/Down: navigate  Enter: backup  Esc: back",
            );
        }
        BackupPhase::Running => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(5)])
                .split(frame.area());

            render_log_panel(frame, layout[0], app, "Backup Progress");

            draw_footer(
                frame,
                layout[1],
                app,
                "Backup in progress. You can keep navigating the TUI.",
            );
        }
        BackupPhase::SelectDatabases => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(8), Constraint::Length(5)])
                .split(frame.area());
            let items: Vec<ListItem<'_>> = app
                .backup_databases
                .iter()
                .enumerate()
                .map(|(index, database)| {
                    let marker = if app.backup_selected_databases[index] {
                        "[x]"
                    } else {
                        "[ ]"
                    };
                    ListItem::new(format!(
                        "{marker} {} | owner: {} | encoding: {} | tablespace: {}",
                        database.name, database.owner, database.encoding, database.tablespace
                    ))
                })
                .collect();
            let list = List::new(items)
                .block(
                    Block::default()
                        .title("Select databases for instance backup")
                        .borders(Borders::ALL),
                )
                .highlight_style(Style::default().add_modifier(Modifier::BOLD))
                .highlight_symbol(">> ");
            frame.render_stateful_widget(list, layout[0], &mut app.backup_database_list_state);
            draw_footer(
                frame,
                layout[1],
                app,
                "Up/Down: navigate  Space: toggle  A: all  Enter: continue  Esc: back",
            );
        }
        BackupPhase::ConfigureBackup => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(8), Constraint::Length(5)])
                .split(frame.area());
            let selected_db_count = app.backup_selected_databases.iter().filter(|s| **s).count();
            let globals_label = if app.backup_config.include_globals {
                "[x]"
            } else {
                "[ ]"
            };
            let passwords_label = if app.backup_config.include_role_passwords {
                "[x]"
            } else {
                "[ ]"
            };
            let tspace_label = match app.backup_config.tablespace_mode {
                crate::models::TablespaceMode::Flatten => "Flatten",
                crate::models::TablespaceMode::Preserve => "Preserve",
            };
            let items = vec![
                ListItem::new(format!(
                    "Databases: {selected_db_count} selected (Enter to start backup)"
                )),
                ListItem::new(format!(
                    "{globals_label} G: Include cluster roles and memberships"
                )),
                ListItem::new(format!("{passwords_label} P: Include role password hashes")),
                ListItem::new(format!("     T: Tablespace mode: {tspace_label}")),
                ListItem::new(format!(
                    "     Source version: {}",
                    app.backup_source_version
                )),
            ];
            let list = List::new(items)
                .block(
                    Block::default()
                        .title("Instance backup summary")
                        .borders(Borders::ALL),
                )
                .highlight_style(Style::default().add_modifier(Modifier::BOLD));
            frame.render_widget(list, layout[0]);
            draw_footer(
                frame,
                layout[1],
                app,
                "G: toggle globals  P: toggle passwords  T: toggle tablespaces  Enter: start  Esc: back",
            );
        }
    }
    Ok(())
}

fn draw_restore_database(frame: &mut Frame<'_>, app: &mut App) -> Result<()> {
    match app.restore_phase {
        RestorePhase::EnterFilePath => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Min(3)])
                .split(frame.area());

            render_field(
                frame,
                layout[0],
                &app.restore_file_path,
                app.focused_input == InputTarget::RestoreFilePath,
            );

            set_cursor(frame, layout[0], &app.restore_file_path, true);

            draw_footer(
                frame,
                layout[1],
                app,
                "Enter the path to a .pgdump.enc file. Enter to confirm. Esc back.",
            );
        }
        RestorePhase::SelectDestInstance => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(8), Constraint::Length(5)])
                .split(frame.area());

            let items: Vec<ListItem<'_>> = if app.instances.is_empty() {
                vec![ListItem::new("No instances defined")]
            } else {
                app.instances
                    .iter()
                    .enumerate()
                    .map(|(i, name)| {
                        let marker = if i == app.restore_dest_idx {
                            ">> "
                        } else {
                            "   "
                        };
                        ListItem::new(format!("{}{}", marker, name))
                    })
                    .collect()
            };

            let list = List::new(items)
                .block(
                    Block::default()
                        .title("Select destination instance")
                        .borders(Borders::ALL),
                )
                .highlight_style(Style::default().add_modifier(Modifier::BOLD))
                .highlight_symbol("");

            frame.render_widget(list, layout[0]);

            draw_footer(
                frame,
                layout[1],
                app,
                "Up/Down: navigate  Enter: select destination  Esc: back",
            );
        }
        RestorePhase::EnterDestDbName => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Min(3),
                ])
                .split(frame.area());

            let dest = app.restore_dest_instance.as_deref().unwrap_or("<none>");
            let info = Paragraph::new(vec![Line::from(format!("Destination instance: {dest}"))])
                .block(
                    Block::default()
                        .title("Restore Details")
                        .borders(Borders::ALL),
                );
            frame.render_widget(info, layout[0]);

            render_field(
                frame,
                layout[1],
                &app.restore_dest_db_name,
                app.focused_input == InputTarget::RestoreDestDbName,
            );

            set_cursor(frame, layout[1], &app.restore_dest_db_name, true);

            render_log_panel(frame, layout[2], app, "Restore Log");

            draw_footer(
                frame,
                centered_rect(frame.area(), 80, 100),
                app,
                "Enter DB name and press Enter to start restore. Esc back.",
            );
        }
        RestorePhase::Running => {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(5)])
                .split(frame.area());

            render_log_panel(frame, layout[0], app, "Restore Progress");

            draw_footer(
                frame,
                layout[1],
                app,
                "Restore in progress. You can keep navigating the TUI.",
            );
        }
    }
    Ok(())
}

fn centered_rect(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}
