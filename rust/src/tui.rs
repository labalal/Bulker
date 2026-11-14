use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Terminal,
};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct ArchiveItem {
    pub title: String,
    pub artist: String,
    pub path: PathBuf,
    pub size: u64,
    pub duration: Option<f64>,
    pub bitrate: Option<u32>,
    pub selected: bool,
}

pub struct ArchiveTui {
    items: Arc<Mutex<Vec<ArchiveItem>>>,
    list_state: ListState,
    filter: String,
    show_help: bool,
}

impl ArchiveTui {
    pub fn new(items: Vec<ArchiveItem>) -> Self {
        let mut list_state = ListState::default();
        if !items.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            items: Arc::new(Mutex::new(items)),
            list_state,
            filter: String::new(),
            show_help: false,
        }
    }

    pub fn run(&mut self) -> Result<Vec<ArchiveItem>> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let res = self.run_app(&mut terminal);

        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        terminal.show_cursor()?;

        let selected: Vec<ArchiveItem> = self.items.lock().unwrap()
            .iter()
            .filter(|i| i.selected)
            .cloned()
            .collect();

        res?;
        Ok(selected)
    }

    fn run_app(&mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
        loop {
            terminal.draw(|f| self.ui(f))?;

            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('h') | KeyCode::F(1) => self.show_help = !self.show_help,
                        KeyCode::Up | KeyCode::Char('k') => self.previous(),
                        KeyCode::Down | KeyCode::Char('j') => self.next(),
                        KeyCode::Enter | KeyCode::Char(' ') => self.toggle_selected(),
                        KeyCode::Char('a') => self.select_all(),
                        KeyCode::Char('n') => self.select_none(),
                        KeyCode::Char('/') => self.start_filter(),
                        KeyCode::Char('d') => self.delete_selected(),
                        KeyCode::Char('m') => self.move_selected(),
                        KeyCode::Char('e') => self.export_selected(),
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    fn ui(&mut self, f: &mut ratatui::Frame) {
        let size = f.size();
        
        if self.show_help {
            self.render_help(f, size);
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(3),
            ])
            .split(size);

        // Header
        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Bulker Archive Manager", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled("[h/F1] Help  [q/Esc] Quit", Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(vec![
                Span::styled("Filter: ", Style::default().fg(Color::Yellow)),
                Span::raw(&self.filter),
                Span::raw("  "),
                Span::styled("[/] Filter  [a] All  [n] None  [Space] Toggle  [d] Delete  [m] Move  [e] Export", Style::default().fg(Color::DarkGray)),
            ]),
        ])
        .block(Block::default().borders(Borders::ALL).title(" Archive Manager "))
        .alignment(Alignment::Left);
        f.render_widget(header, chunks[0]);

        // List
        let items_guard = self.items.lock().unwrap();
        let items: Vec<ListItem> = items_guard
            .iter()
            .filter(|item| self.filter.is_empty() || 
                item.title.to_lowercase().contains(&self.filter.to_lowercase()) ||
                item.artist.to_lowercase().contains(&self.filter.to_lowercase()))
            .map(|item| {
                let style = if item.selected {
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                
                let dur_str = item.duration.map(|d| format!(" {:.0}:{:02.0}", d / 60.0, d % 60.0)).unwrap_or_default();
                let br_str = item.bitrate.map(|b| format!(" {}kbps", b)).unwrap_or_default();
                let size_str = format!(" ({:.1}MB)", item.size as f64 / 1_048_576.0);
                
                ListItem::new(Line::from(vec![
                    Span::styled(if item.selected { "● " } else { "○ " }, Style::default().fg(Color::Green)),
                    Span::styled(&item.artist, style.fg(Color::Cyan)),
                    Span::styled(" - ", style),
                    Span::styled(&item.title, style),
                    Span::styled(dur_str, Style::default().fg(Color::DarkGray)),
                    Span::styled(br_str, Style::default().fg(Color::DarkGray)),
                    Span::styled(size_str, Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Songs "))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("► ");

        f.render_stateful_widget(list, chunks[1], &mut self.list_state);

        // Footer
        let selected_count = items_guard.iter().filter(|i| i.selected).count();
        let total_count = items_guard.len();
        let footer = Paragraph::new(vec![
            Line::from(vec![
                Span::styled(format!("Selected: {} / {}", selected_count, total_count), Style::default().fg(Color::Yellow)),
                Span::raw("  "),
                Span::styled("[Enter/Space] Toggle  [a] Select All  [n] Clear  [d] Delete  [m] Move to Folder  [e] Export List", Style::default().fg(Color::DarkGray)),
            ]),
        ])
        .block(Block::default().borders(Borders::ALL))
        .alignment(Alignment::Left);
        f.render_widget(footer, chunks[2]);
    }

    fn render_help(&self, f: &mut ratatui::Frame, size: Rect) {
        let help_text = vec![
            Line::from(""),
            Line::from(vec![Span::styled("BULKER ARCHIVE MANAGER - KEYBINDINGS", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))]),
            Line::from(""),
            Line::from(vec![Span::styled("Navigation:", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))]),
            Line::from("  ↑/k, ↓/j     Move selection up/down"),
            Line::from("  PgUp/PgDn    Page up/down"),
            Line::from("  Home/End     Jump to first/last"),
            Line::from(""),
            Line::from(vec![Span::styled("Selection:", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))]),
            Line::from("  Space/Enter  Toggle selection of current item"),
            Line::from("  a            Select all visible items"),
            Line::from("  n            Clear all selections"),
            Line::from(""),
            Line::from(vec![Span::styled("Filtering:", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))]),
            Line::from("  /            Start filter mode (type to filter, Esc to clear)"),
            Line::from(""),
            Line::from(vec![Span::styled("Actions:", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))]),
            Line::from("  d            Delete selected files (with confirmation)"),
            Line::from("  m            Move selected to organized folder structure"),
            Line::from("  e            Export selected list to file"),
            Line::from(""),
            Line::from(vec![Span::styled("General:", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))]),
            Line::from("  h/F1         Toggle this help"),
            Line::from("  q/Esc        Quit"),
            Line::from(""),
            Line::from(vec![Span::styled("Press any key to close help...", Style::default().fg(Color::DarkGray))]),
        ];

        let help = Paragraph::new(help_text)
            .block(Block::default().borders(Borders::ALL).title(" Help "))
            .wrap(Wrap { trim: true })
            .alignment(Alignment::Left);
        
        let area = centered_rect(60, 80, size);
        f.render_widget(help, area);
    }

    fn previous(&mut self) {
        let items = self.items.lock().unwrap();
        let filtered_count = items.iter().filter(|item| self.filter.is_empty() || 
            item.title.to_lowercase().contains(&self.filter.to_lowercase()) ||
            item.artist.to_lowercase().contains(&self.filter.to_lowercase())).count();
        
        if filtered_count == 0 { return; }
        
        let i = match self.list_state.selected() {
            Some(i) => {
                if i == 0 { filtered_count - 1 } else { i - 1 }
            }
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    fn next(&mut self) {
        let items = self.items.lock().unwrap();
        let filtered_count = items.iter().filter(|item| self.filter.is_empty() || 
            item.title.to_lowercase().contains(&self.filter.to_lowercase()) ||
            item.artist.to_lowercase().contains(&self.filter.to_lowercase())).count();
        
        if filtered_count == 0 { return; }
        
        let i = match self.list_state.selected() {
            Some(i) => {
                if i >= filtered_count - 1 { 0 } else { i + 1 }
            }
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    fn toggle_selected(&mut self) {
        let mut items = self.items.lock().unwrap();
        let filtered_indices: Vec<usize> = items.iter().enumerate()
            .filter(|(_, item)| self.filter.is_empty() || 
                item.title.to_lowercase().contains(&self.filter.to_lowercase()) ||
                item.artist.to_lowercase().contains(&self.filter.to_lowercase()))
            .map(|(i, _)| i)
            .collect();
        
        if let Some(selected) = self.list_state.selected() {
            if let Some(&real_idx) = filtered_indices.get(selected) {
                items[real_idx].selected = !items[real_idx].selected;
            }
        }
    }

    fn select_all(&mut self) {
        let mut items = self.items.lock().unwrap();
        for item in items.iter_mut() {
            if self.filter.is_empty() || 
                item.title.to_lowercase().contains(&self.filter.to_lowercase()) ||
                item.artist.to_lowercase().contains(&self.filter.to_lowercase()) {
                item.selected = true;
            }
        }
    }

    fn select_none(&mut self) {
        let mut items = self.items.lock().unwrap();
        for item in items.iter_mut() {
            item.selected = false;
        }
    }

    fn start_filter(&mut self) {
        self.filter.clear();
        loop {
            if let Ok(Event::Key(key)) = event::read() {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Esc => break,
                        KeyCode::Backspace => { self.filter.pop(); }
                        KeyCode::Char(c) => self.filter.push(c),
                        _ => {}
                    }
                }
            }
        }
    }

    fn delete_selected(&mut self) {
        // TODO: Implement with confirmation dialog
    }

    fn move_selected(&mut self) {
        // TODO: Implement move to organized structure
    }

    fn export_selected(&mut self) {
        // TODO: Implement export to file
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
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

pub fn scan_archive_dir(archive_dir: &PathBuf) -> Result<Vec<ArchiveItem>> {
    use std::fs;
    use lofty::probe::Probe;
    use lofty::file::TaggedFileExt;
    use lofty::tag::Accessor;
    
    let mut items = Vec::new();
    
    fn scan_dir(dir: &PathBuf, items: &mut Vec<ArchiveItem>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    scan_dir(&path, items);
                } else if path.extension().and_then(|s| s.to_str()) == Some("opus") {
                    if let Ok(tagged) = Probe::open(&path).and_then(|p| p.read()) {
                        let title = tagged.primary_tag()
                            .and_then(|t| t.title())
                            .map(|cow| cow.to_string())
                            .unwrap_or_else(|| path.file_stem().and_then(|s| s.to_str()).unwrap_or("Unknown").to_string());
                        
                        let artist = tagged.primary_tag()
                            .and_then(|t| t.artist())
                            .map(|cow| cow.to_string())
                            .unwrap_or_else(|| "Unknown Artist".to_string());
                        
                        let duration = None; // Duration not directly available from tags in lofty
                        
                        let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                        
                        items.push(ArchiveItem {
                            title,
                            artist,
                            path,
                            size,
                            duration,
                            bitrate: None,
                            selected: false,
                        });
                    }
                }
            }
        }
    }
    
    scan_dir(archive_dir, &mut items);
    Ok(items)
}