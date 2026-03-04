use super::{App, Mode, truncate_from_left};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::prelude::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

pub(super) fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = match app.mode {
        Mode::Zoxide => centered_rect(84, 76, frame.area()),
        _ => centered_rect(78, 72, frame.area()),
    };
    frame.render_widget(Clear, area);

    let outer = Block::default()
        .title(" fish-session ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(outer, area);

    let content = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };

    match app.mode {
        Mode::Zoxide => render_zoxide(frame, app, content),
        _ => render_sessions(frame, app, content),
    }
}

fn render_zoxide(frame: &mut Frame<'_>, app: &App, content: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(content);

    let query = if app.input.is_empty() {
        "Type to search directories..."
    } else {
        app.input.as_str()
    };
    let query_style = if app.input.is_empty() {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::White)
    };
    let search = Paragraph::new(Line::from(vec![Span::styled(query, query_style)]))
        .block(Block::default().borders(Borders::ALL).title(" Search "));
    frame.render_widget(search, chunks[0]);

    let line_width = chunks[1].width.saturating_sub(6) as usize;
    let items: Vec<ListItem<'_>> = if app.zoxide_matches.is_empty() {
        vec![ListItem::new(Line::from(
            "No matching directories. Type to refine search.",
        ))]
    } else {
        app.zoxide_matches
            .iter()
            .map(|item| {
                if let Some(session) = app.session_for_path(&item.path) {
                    let is_active = app.active_session.as_deref() == Some(session.name.as_str());
                    let marker = if is_active { "●" } else { "○" };
                    let marker_style = if is_active {
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    let session_style = if is_active {
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    };
                    let path_style = Style::default().fg(Color::DarkGray);
                    let prefix = format!("{marker} {} (", session.name);
                    let reserved = prefix.chars().count() + 1;
                    let available_for_path = line_width.saturating_sub(reserved);
                    let display_path =
                        truncate_from_left(&item.path.display().to_string(), available_for_path);

                    ListItem::new(Line::from(vec![
                        Span::styled(marker, marker_style),
                        Span::raw(" "),
                        Span::styled(session.name.as_str(), session_style),
                        Span::styled(" (", path_style),
                        Span::styled(display_path, path_style),
                        Span::styled(")", path_style),
                    ]))
                } else {
                    let display_path =
                        truncate_from_left(&item.path.display().to_string(), line_width);
                    ListItem::new(Line::from(vec![Span::styled(
                        display_path,
                        Style::default().fg(Color::Gray),
                    )]))
                }
            })
            .collect()
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Directories "),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(72, 74, 95))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    let mut state = ListState::default();
    if !app.zoxide_matches.is_empty() {
        state.select(Some(app.zoxide_selected));
    }
    frame.render_stateful_widget(list, chunks[1], &mut state);

    let hints = Paragraph::new(Line::from(vec![Span::raw(
        "Enter attach/create | Ctrl-R refresh | Esc close",
    )]))
    .alignment(Alignment::Center)
    .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(hints, chunks[2]);
}

fn render_sessions(frame: &mut Frame<'_>, app: &App, content: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(content);

    let (header_title, header_text, header_style) = match app.mode {
        Mode::Normal => (
            " Search ",
            if app.input.is_empty() {
                "Type to search sessions..."
            } else {
                app.input.as_str()
            },
            if app.input.is_empty() {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::White)
            },
        ),
        Mode::Create => (
            " New Session ",
            if app.input.is_empty() {
                "Type new session name..."
            } else {
                app.input.as_str()
            },
            if app.input.is_empty() {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::White)
            },
        ),
        Mode::Rename { .. } => (
            " Rename Session ",
            if app.input.is_empty() {
                "Type new name..."
            } else {
                app.input.as_str()
            },
            if app.input.is_empty() {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::White)
            },
        ),
        Mode::Zoxide => unreachable!("zoxide mode is rendered by render_zoxide"),
    };

    let header = Paragraph::new(Line::from(vec![Span::styled(header_text, header_style)]))
        .block(Block::default().borders(Borders::ALL).title(header_title));
    frame.render_widget(header, chunks[0]);

    let line_width = chunks[1].width.saturating_sub(6) as usize;
    let visible_indices = app.filtered_session_indices();
    let items: Vec<ListItem<'_>> = if app.sessions.is_empty() {
        vec![ListItem::new(Line::from(
            "No sessions yet. Press Ctrl-N to create one.",
        ))]
    } else if visible_indices.is_empty() {
        vec![ListItem::new(Line::from(
            "No sessions match search. Press Esc to clear search.",
        ))]
    } else {
        visible_indices
            .iter()
            .map(|index| {
                let session = &app.sessions[*index];
                let is_active = app.active_session.as_deref() == Some(session.name.as_str());
                let marker = if is_active { "●" } else { "○" };
                let marker_style = if is_active {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                let name_style = if is_active {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                };
                let path_style = Style::default().fg(Color::DarkGray);

                let prefix = format!("{marker} {} (", session.name);
                let reserved = prefix.chars().count() + 1;
                let display_path = truncate_from_left(
                    &session.cwd.display().to_string(),
                    line_width.saturating_sub(reserved),
                );

                ListItem::new(Line::from(vec![
                    Span::styled(marker, marker_style),
                    Span::raw(" "),
                    Span::styled(session.name.as_str(), name_style),
                    Span::styled(" (", path_style),
                    Span::styled(display_path, path_style),
                    Span::styled(")", path_style),
                ]))
            })
            .collect()
    };

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Sessions "))
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(72, 74, 95))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    let mut state = ListState::default();
    if !visible_indices.is_empty() {
        let selected_pos = visible_indices
            .iter()
            .position(|index| *index == app.selected)
            .unwrap_or(0);
        state.select(Some(selected_pos));
    }
    frame.render_stateful_widget(list, chunks[1], &mut state);

    let hint_line = match app.mode {
        Mode::Create | Mode::Rename { .. } => "Enter save | Esc cancel",
        _ => {
            "Type search | Enter attach | Ctrl-N new | Ctrl-D delete | Ctrl-R rename | Ctrl-O zoxide | Esc close"
        }
    };
    let hints = Paragraph::new(Line::from(vec![Span::raw(hint_line)]))
        .alignment(Alignment::Center)
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(hints, chunks[2]);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    if percent_x == 0 || percent_y == 0 {
        return r;
    }

    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
