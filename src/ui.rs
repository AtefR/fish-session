mod events;
mod render;

use crate::client;
use crate::config::AppConfig;
use crate::protocol::SessionInfo;
use anyhow::{Context, Result, bail};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use names::Generator;
use ratatui::Terminal;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::fs;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn run_ui() -> Result<()> {
    if let Some(selection) = pick_session_with_active(None)? {
        client::attach_session_with_replay(&selection.name, selection.replay)?;
    }

    Ok(())
}

pub fn pick_session() -> Result<Option<PickerSelection>> {
    pick_session_with_active(None)
}

pub fn pick_session_with_active(active_session: Option<&str>) -> Result<Option<PickerSelection>> {
    client::ensure_daemon()?;

    let mut terminal = setup_terminal()?;
    let result = (|| -> Result<Option<PickerSelection>> {
        let config = AppConfig::load().unwrap_or_default();
        let mut app = App::new(config, active_session.map(str::to_string));
        app.refresh()?;

        if app.config.zoxide.enabled
            && app.config.zoxide.auto_open
            && app.enter_zoxide_mode().is_err()
        {
            app.mode = Mode::Normal;
            app.input.clear();
        }

        let action = events::run_event_loop(&mut terminal, &mut app)?;
        Ok(action.attach)
    })();

    restore_terminal(&mut terminal)?;
    result
}

#[derive(Clone)]
struct ZoxideEntry {
    path: PathBuf,
    zoxide_score: f64,
}

#[derive(Clone)]
struct ZoxideMatch {
    path: PathBuf,
    zoxide_score: f64,
    total_score: f64,
}

struct App {
    config: AppConfig,
    active_session: Option<String>,
    sessions: Vec<SessionInfo>,
    selected: usize,
    zoxide_index: Vec<ZoxideEntry>,
    zoxide_matches: Vec<ZoxideMatch>,
    zoxide_selected: usize,
    mode: Mode,
    input: String,
    create_suggestion: String,
}

#[derive(Clone)]
enum Mode {
    Normal,
    Create,
    Rename { old_name: String },
    Zoxide,
}

struct UiAction {
    attach: Option<PickerSelection>,
}

impl UiAction {
    fn quit() -> Self {
        Self { attach: None }
    }

    fn attach(name: String, replay: bool) -> Self {
        Self {
            attach: Some(PickerSelection { name, replay }),
        }
    }
}

#[derive(Clone)]
pub struct PickerSelection {
    pub name: String,
    pub replay: bool,
}

impl App {
    fn new(config: AppConfig, active_session: Option<String>) -> Self {
        Self {
            config,
            active_session,
            sessions: Vec::new(),
            selected: 0,
            zoxide_index: Vec::new(),
            zoxide_matches: Vec::new(),
            zoxide_selected: 0,
            mode: Mode::Normal,
            input: String::new(),
            create_suggestion: String::new(),
        }
    }

    fn enter_create_mode(&mut self) {
        self.input.clear();
        self.mode = Mode::Create;
        self.create_suggestion = suggest_session_name(&self.sessions);
    }

    fn create_name(&self) -> Option<String> {
        let typed = self.input.trim();
        if !typed.is_empty() {
            return Some(typed.to_string());
        }
        if self.create_suggestion.is_empty() {
            return None;
        }
        Some(self.create_suggestion.clone())
    }

    fn refresh(&mut self) -> Result<()> {
        let previously_selected = self.selected_session_name();
        let sessions = client::list_sessions()?;
        self.sessions = sessions;

        if self.sessions.is_empty() {
            self.selected = 0;
            return Ok(());
        }

        let preferred_selected = previously_selected.or_else(|| self.active_session.clone());
        self.selected = preferred_selected
            .and_then(|name| self.sessions.iter().position(|s| s.name == name))
            .unwrap_or_else(|| self.selected.min(self.sessions.len().saturating_sub(1)));
        self.sync_selected_with_search();

        Ok(())
    }

    fn enter_zoxide_mode(&mut self) -> Result<()> {
        self.mode = Mode::Zoxide;
        self.input.clear();
        self.zoxide_selected = 0;
        self.refresh_zoxide_index()?;
        self.refresh_zoxide_matches();
        self.select_active_zoxide_match();
        Ok(())
    }

    fn refresh_zoxide_index(&mut self) -> Result<()> {
        self.zoxide_index = query_zoxide_index()?;
        Ok(())
    }

    fn refresh_zoxide_matches(&mut self) {
        self.zoxide_matches =
            fuzzy_filter(&self.input, &self.zoxide_index, self.config.zoxide.limit);
        self.sort_zoxide_matches_for_display();
        if self.zoxide_matches.is_empty() {
            self.zoxide_selected = 0;
        } else {
            self.zoxide_selected = self
                .zoxide_selected
                .min(self.zoxide_matches.len().saturating_sub(1));
        }
    }

    fn selected_session(&self) -> Option<&SessionInfo> {
        self.sessions.get(self.selected)
    }

    fn selected_session_name(&self) -> Option<String> {
        self.selected_session().map(|session| session.name.clone())
    }

    fn selected_visible_session_name(&self) -> Option<String> {
        if matches!(self.mode, Mode::Normal) {
            let visible = self.filtered_session_indices();
            if visible.is_empty() || !visible.contains(&self.selected) {
                return None;
            }
        }

        self.selected_session_name()
    }

    fn selected_zoxide_path(&self) -> Option<PathBuf> {
        self.zoxide_matches
            .get(self.zoxide_selected)
            .map(|item| item.path.clone())
    }

    fn active_session_cwd(&self) -> Option<PathBuf> {
        let active = self.active_session.as_deref()?;
        self.sessions
            .iter()
            .find(|session| session.name == active)
            .map(|session| session.cwd.clone())
    }

    fn filtered_session_indices(&self) -> Vec<usize> {
        if !matches!(self.mode, Mode::Normal) {
            return (0..self.sessions.len()).collect();
        }

        let query = self.input.trim().to_ascii_lowercase();
        if query.is_empty() {
            return (0..self.sessions.len()).collect();
        }

        let terms: Vec<&str> = query
            .split_whitespace()
            .filter(|term| !term.is_empty())
            .collect();
        self.sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| {
                let haystack = format!(
                    "{} {}",
                    session.name.to_ascii_lowercase(),
                    session.cwd.display().to_string().to_ascii_lowercase()
                );
                if terms.iter().all(|term| haystack.contains(term)) {
                    Some(index)
                } else {
                    None
                }
            })
            .collect()
    }

    fn sync_selected_with_search(&mut self) {
        let visible = self.filtered_session_indices();
        if visible.is_empty() {
            self.selected = 0;
            return;
        }

        if !visible.contains(&self.selected) {
            self.selected = visible[0];
        }
    }

    fn move_session_up(&mut self) {
        let visible = self.filtered_session_indices();
        if visible.is_empty() {
            return;
        }

        let current_pos = visible
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        let next_pos = if current_pos == 0 {
            visible.len() - 1
        } else {
            current_pos - 1
        };
        self.selected = visible[next_pos];
    }

    fn move_session_down(&mut self) {
        let visible = self.filtered_session_indices();
        if visible.is_empty() {
            return;
        }

        let current_pos = visible
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        let next_pos = (current_pos + 1) % visible.len();
        self.selected = visible[next_pos];
    }

    fn move_zoxide_up(&mut self) {
        if self.zoxide_matches.is_empty() {
            return;
        }
        if self.zoxide_selected == 0 {
            self.zoxide_selected = self.zoxide_matches.len() - 1;
        } else {
            self.zoxide_selected -= 1;
        }
    }

    fn move_zoxide_down(&mut self) {
        if self.zoxide_matches.is_empty() {
            return;
        }
        self.zoxide_selected = (self.zoxide_selected + 1) % self.zoxide_matches.len();
    }

    fn session_for_path(&self, path: &Path) -> Option<&SessionInfo> {
        self.sessions
            .iter()
            .find(|session| paths_equal(&session.cwd, path))
    }

    fn sort_zoxide_matches_for_display(&mut self) {
        let sessions = self.sessions.clone();
        let active = self.active_session.clone();

        self.zoxide_matches.sort_by(|left, right| {
            let left_rank = zoxide_rank_for_path(&left.path, &sessions, active.as_deref());
            let right_rank = zoxide_rank_for_path(&right.path, &sessions, active.as_deref());
            match left_rank.cmp(&right_rank) {
                Ordering::Equal => Ordering::Equal,
                other => other,
            }
        });
    }

    fn select_active_zoxide_match(&mut self) {
        let Some(active_cwd) = self.active_session_cwd() else {
            return;
        };
        if let Some(index) = self
            .zoxide_matches
            .iter()
            .position(|item| paths_equal(&item.path, &active_cwd))
        {
            self.zoxide_selected = index;
        }
    }
}

fn setup_terminal() -> Result<Terminal<ratatui::backend::CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
) -> Result<()> {
    disable_raw_mode().context("failed to disable raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
        .context("failed to leave alternate screen")?;
    terminal.show_cursor()?;
    Ok(())
}

fn query_zoxide_index() -> Result<Vec<ZoxideEntry>> {
    let output = Command::new("zoxide")
        .arg("query")
        .arg("-l")
        .arg("-s")
        .output()
        .context("failed to run zoxide")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            bail!("zoxide query failed");
        }
        bail!("zoxide query failed: {stderr}");
    }

    let mut entries = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some((zoxide_score, path)) = parse_scored_zoxide_line(line) {
            entries.push(ZoxideEntry { path, zoxide_score });
        }
    }

    Ok(entries)
}

fn parse_scored_zoxide_line(line: &str) -> Option<(f64, PathBuf)> {
    let trimmed = line.trim_start();
    let split = trimmed.find(char::is_whitespace)?;
    let (score_str, rest) = trimmed.split_at(split);
    let path_str = rest.trim();

    if path_str.is_empty() {
        return None;
    }

    let zoxide_score = score_str.parse::<f64>().ok()?;
    Some((zoxide_score, PathBuf::from(path_str)))
}

fn fuzzy_filter(query: &str, entries: &[ZoxideEntry], limit: usize) -> Vec<ZoxideMatch> {
    if entries.is_empty() || limit == 0 {
        return Vec::new();
    }

    let query = query.trim().to_ascii_lowercase();
    let mut matches = Vec::new();

    if query.is_empty() {
        for entry in entries {
            matches.push(ZoxideMatch {
                path: entry.path.clone(),
                zoxide_score: entry.zoxide_score,
                total_score: entry.zoxide_score,
            });
        }
    } else {
        let terms: Vec<&str> = query
            .split_whitespace()
            .filter(|term| !term.is_empty())
            .collect();

        for entry in entries {
            let candidate = entry.path.to_string_lossy().to_ascii_lowercase();
            let mut total_fuzzy = 0.0;
            let mut matched_all = true;

            for term in &terms {
                match fuzzy_term_score(term, &candidate) {
                    Some(score) => total_fuzzy += score,
                    None => {
                        matched_all = false;
                        break;
                    }
                }
            }

            if matched_all {
                let total_score = entry.zoxide_score + total_fuzzy * 100.0;
                matches.push(ZoxideMatch {
                    path: entry.path.clone(),
                    zoxide_score: entry.zoxide_score,
                    total_score,
                });
            }
        }

        matches.sort_by(|a, b| {
            b.total_score
                .total_cmp(&a.total_score)
                .then_with(|| b.zoxide_score.total_cmp(&a.zoxide_score))
        });
    }

    if query.is_empty() {
        matches.sort_by(|a, b| b.zoxide_score.total_cmp(&a.zoxide_score));
    }

    matches.truncate(limit);
    matches
}

fn fuzzy_term_score(term: &str, candidate: &str) -> Option<f64> {
    let mut score = 0.0;
    let mut cursor = 0usize;
    let mut prev_index: Option<usize> = None;

    for needle in term.chars() {
        let found = find_char_from(candidate, needle, cursor)?;

        score += 1.0;

        if is_boundary(candidate, found) {
            score += 0.8;
        }

        if let Some(prev) = prev_index {
            if found == prev + 1 {
                score += 1.5;
            } else {
                let gap = found.saturating_sub(prev + 1) as f64;
                score -= (gap * 0.02).min(0.8);
            }
        }

        score -= ((found as f64) * 0.005).min(0.7);

        prev_index = Some(found);
        cursor = found + candidate[found..].chars().next()?.len_utf8();
    }

    let coverage = term.chars().count() as f64 / candidate.chars().count().max(1) as f64;
    score += coverage * 2.0;

    Some(score.max(0.0))
}

fn find_char_from(candidate: &str, needle: char, start_byte: usize) -> Option<usize> {
    candidate[start_byte..]
        .char_indices()
        .find(|(_, ch)| *ch == needle)
        .map(|(offset, _)| start_byte + offset)
}

fn is_boundary(candidate: &str, byte_idx: usize) -> bool {
    if byte_idx == 0 {
        return true;
    }

    let previous = candidate[..byte_idx].chars().next_back();
    match previous {
        Some(ch) => !ch.is_ascii_alphanumeric(),
        None => true,
    }
}

fn ensure_session_for_directory(path: &Path) -> Result<(String, bool)> {
    let sessions = client::list_sessions()?;

    if let Some(existing) = sessions
        .iter()
        .find(|session| paths_equal(&session.cwd, path))
    {
        return Ok((existing.name.clone(), true));
    }

    let name = next_session_name(path, &sessions);
    client::create_session(&name, Some(path.to_path_buf()))?;
    Ok((name, false))
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (fs::canonicalize(left), fs::canonicalize(right)) {
        return a == b;
    }

    left == right
}

fn zoxide_rank_for_path(path: &Path, sessions: &[SessionInfo], active: Option<&str>) -> u8 {
    for session in sessions {
        if paths_equal(&session.cwd, path) {
            if active == Some(session.name.as_str()) {
                return 0;
            }
            return 1;
        }
    }

    2
}

fn next_session_name(path: &Path, sessions: &[SessionInfo]) -> String {
    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(sanitize_session_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "session".to_string());

    let existing: HashSet<String> = sessions
        .iter()
        .map(|session| session.name.clone())
        .collect();
    if !existing.contains(&stem) {
        return stem;
    }

    let mut n = 2;
    loop {
        let candidate = format!("{stem}-{n}");
        if !existing.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

fn sanitize_session_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_dash = false;

    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if c == '-' || c == '_' {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }

    out.trim_matches('-').to_string()
}

fn suggest_session_name(sessions: &[SessionInfo]) -> String {
    let max_space = names::ADJECTIVES.len().saturating_mul(names::NOUNS.len());
    let max_attempts = max_space.saturating_mul(2).clamp(64, 50_000);
    suggest_session_name_from_candidates(sessions, Generator::default(), max_attempts)
}

fn suggest_session_name_from_candidates<I>(
    sessions: &[SessionInfo],
    candidates: I,
    max_attempts: usize,
) -> String
where
    I: IntoIterator<Item = String>,
{
    let existing: HashSet<String> = sessions
        .iter()
        .map(|session| session.name.clone())
        .collect();

    if let Some(candidate) = pick_unique_candidate(&existing, candidates.into_iter(), max_attempts)
    {
        return candidate;
    }

    fallback_session_name(&existing)
}

fn pick_unique_candidate<I>(
    existing: &HashSet<String>,
    candidates: I,
    max_attempts: usize,
) -> Option<String>
where
    I: Iterator<Item = String>,
{
    for candidate in candidates.take(max_attempts) {
        if !existing.contains(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn fallback_session_name(existing: &HashSet<String>) -> String {
    if !existing.contains("session") {
        return "session".to_string();
    }
    let mut n = 2usize;
    loop {
        let candidate = format!("session-{n}");
        if !existing.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

fn truncate_from_left(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }

    let count = input.chars().count();
    if count <= max_chars {
        return input.to_string();
    }

    if max_chars == 1 {
        return "…".to_string();
    }

    let tail_len = max_chars - 1;
    let tail: String = input
        .chars()
        .rev()
        .take(tail_len)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::{
        ZoxideEntry, fuzzy_filter, next_session_name, parse_scored_zoxide_line,
        sanitize_session_name, suggest_session_name, suggest_session_name_from_candidates,
    };
    use crate::protocol::SessionInfo;
    use std::path::PathBuf;

    #[test]
    fn sanitize_name_normalizes_symbols() {
        assert_eq!(sanitize_session_name("My Project!!"), "my-project");
        assert_eq!(sanitize_session_name("api_v2"), "api_v2");
    }

    #[test]
    fn next_name_appends_suffix_on_collision() {
        let sessions = vec![
            SessionInfo {
                name: "project".to_string(),
                cwd: PathBuf::from("/tmp/a"),
                pid: 1,
                attached: false,
            },
            SessionInfo {
                name: "project-2".to_string(),
                cwd: PathBuf::from("/tmp/b"),
                pid: 2,
                attached: false,
            },
        ];

        let next = next_session_name(&PathBuf::from("/home/vincent/Code/project"), &sessions);
        assert_eq!(next, "project-3");
    }

    #[test]
    fn parse_scored_line_extracts_score_and_path() {
        let parsed = parse_scored_zoxide_line("  32.2 /home/vincent/Code/delv3").unwrap();
        assert_eq!(parsed.0, 32.2);
        assert_eq!(parsed.1, PathBuf::from("/home/vincent/Code/delv3"));
    }

    #[test]
    fn fuzzy_filter_prioritizes_fuzzy_match() {
        let entries = vec![
            ZoxideEntry {
                path: PathBuf::from("/home/vincent/Code/alpha-service"),
                zoxide_score: 1000.0,
            },
            ZoxideEntry {
                path: PathBuf::from("/home/vincent/Code/codex"),
                zoxide_score: 50.0,
            },
        ];

        let results = fuzzy_filter("cdx", &entries, 10);
        assert_eq!(
            results.first().unwrap().path,
            PathBuf::from("/home/vincent/Code/codex")
        );
    }

    #[test]
    fn suggested_name_skips_existing_candidates() {
        let sessions = vec![SessionInfo {
            name: "calm-cloud".to_string(),
            cwd: PathBuf::from("/tmp"),
            pid: 1,
            attached: false,
        }];
        let candidates = vec!["calm-cloud".to_string(), "vivid-river".to_string()];

        let suggested = suggest_session_name_from_candidates(&sessions, candidates, 10);
        assert_eq!(suggested, "vivid-river");
    }

    #[test]
    fn suggested_name_falls_back_when_candidates_exhausted() {
        let sessions = vec![
            SessionInfo {
                name: "taken".to_string(),
                cwd: PathBuf::from("/tmp"),
                pid: 1,
                attached: false,
            },
            SessionInfo {
                name: "session".to_string(),
                cwd: PathBuf::from("/tmp"),
                pid: 1,
                attached: false,
            },
            SessionInfo {
                name: "session-2".to_string(),
                cwd: PathBuf::from("/tmp"),
                pid: 1,
                attached: false,
            },
        ];
        let candidates = vec!["taken".to_string(), "taken".to_string()];

        let suggested = suggest_session_name_from_candidates(&sessions, candidates, 2);
        assert_eq!(suggested, "session-3");
    }

    #[test]
    fn suggested_name_uses_names_crate_style_when_available() {
        let sessions = vec![SessionInfo {
            name: "alpha".to_string(),
            cwd: PathBuf::from("/tmp"),
            pid: 1,
            attached: false,
        }];

        let suggested = suggest_session_name(&sessions);
        assert!(suggested.contains('-'));
        assert!(!suggested.is_empty());
        assert_ne!(suggested, "alpha");
    }
}
