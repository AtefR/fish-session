use super::{App, Mode, UiAction, ensure_session_for_directory, render};
use crate::client;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use std::env;
use std::io::Stdout;
use std::path::PathBuf;
use std::time::Duration;

pub(super) fn run_event_loop(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    app: &mut App,
) -> Result<UiAction> {
    loop {
        terminal.draw(|frame| render::render(frame, app))?;

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        let mode = app.mode.clone();
        match mode {
            Mode::Normal => {
                if handle_text_input(key.code, key.modifiers, &mut app.input) {
                    app.sync_selected_with_search();
                    continue;
                }

                if key.code == KeyCode::Esc {
                    if !app.input.is_empty() {
                        app.input.clear();
                        app.sync_selected_with_search();
                        continue;
                    }
                    return Ok(UiAction::quit());
                }

                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => app.move_session_up(),
                    KeyCode::Down | KeyCode::Char('j') => app.move_session_down(),
                    KeyCode::Enter => {
                        if let Some(name) = app.selected_visible_session_name() {
                            return Ok(UiAction::attach(name, true));
                        }
                    }
                    KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if !app.config.zoxide.enabled {
                            continue;
                        }

                        let _ = app.enter_zoxide_mode();
                    }
                    KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if let Some(current_name) = app.selected_visible_session_name() {
                            app.input = current_name.clone();
                            app.mode = Mode::Rename {
                                old_name: current_name,
                            };
                        }
                    }
                    KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.enter_create_mode();
                    }
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if let Some(name) = app.selected_visible_session_name()
                            && client::delete_session(&name).is_ok()
                        {
                            let _ = app.refresh();
                            app.sync_selected_with_search();
                        }
                    }
                    _ => {}
                }
            }
            Mode::Create => {
                if handle_text_input(key.code, key.modifiers, &mut app.input) {
                    continue;
                }

                match key.code {
                    KeyCode::Esc => {
                        app.mode = Mode::Normal;
                        app.input.clear();
                        app.create_suggestion.clear();
                    }
                    KeyCode::Enter => {
                        let Some(name) = app.create_name() else {
                            continue;
                        };

                        let cwd = env::var("PWD").ok().map(PathBuf::from);
                        if client::create_session(&name, cwd).is_ok() {
                            return Ok(UiAction::attach(name, false));
                        } else if app.input.trim().is_empty() {
                            let _ = app.refresh();
                            app.create_suggestion = super::suggest_session_name(&app.sessions);
                        }
                    }
                    _ => {}
                }
            }
            Mode::Rename { old_name } => {
                if handle_text_input(key.code, key.modifiers, &mut app.input) {
                    continue;
                }

                match key.code {
                    KeyCode::Esc => {
                        app.mode = Mode::Normal;
                        app.input.clear();
                    }
                    KeyCode::Enter => {
                        let new_name = app.input.trim().to_string();
                        if new_name.is_empty() {
                            continue;
                        }

                        let from = old_name.clone();
                        if client::rename_session(&from, &new_name).is_ok() {
                            app.mode = Mode::Normal;
                            app.input.clear();
                            let _ = app.refresh();
                        }
                    }
                    _ => {}
                }
            }
            Mode::Zoxide => {
                if handle_text_input(key.code, key.modifiers, &mut app.input) {
                    app.refresh_zoxide_matches();
                    continue;
                }

                match key.code {
                    KeyCode::Esc => {
                        return Ok(UiAction::quit());
                    }
                    KeyCode::Up | KeyCode::Char('k') => app.move_zoxide_up(),
                    KeyCode::Down | KeyCode::Char('j') => app.move_zoxide_down(),
                    KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if app.refresh_zoxide_index().is_ok() {
                            app.refresh_zoxide_matches();
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(path) = app.selected_zoxide_path()
                            && let Ok((session_name, replay)) = ensure_session_for_directory(&path)
                        {
                            return Ok(UiAction::attach(session_name, replay));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn handle_text_input(code: KeyCode, modifiers: KeyModifiers, input: &mut String) -> bool {
    if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) {
        return false;
    }

    match code {
        KeyCode::Char(c) => {
            input.push(c);
            true
        }
        KeyCode::Backspace => {
            input.pop();
            true
        }
        _ => false,
    }
}
