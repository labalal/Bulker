use anyhow::{Context, Result};
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
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginConfig {
    pub plugin: PluginMetadata,
    pub source: PluginSource,
    pub config: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginMetadata {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: Option<String>,
    pub repository: Option<String>,
    pub license: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PluginSource {
    #[serde(rename = "git")]
    Git { url: String, branch: Option<String>, tag: Option<String> },
    #[serde(rename = "local")]
    Local { path: String },
    #[serde(rename = "builtin")]
    Builtin { name: String },
}

#[derive(Debug, Clone)]
pub struct InstalledPlugin {
    pub metadata: PluginMetadata,
    pub enabled: bool,
    pub install_path: PathBuf,
    pub source_type: String,
}

pub struct PluginTui {
    plugins: Arc<Mutex<Vec<InstalledPlugin>>>,
    list_state: ListState,
    show_help: bool,
    show_detail: bool,
    selected_plugin: Option<usize>,
    plugin_dir: PathBuf,
    config_dir: PathBuf,
}

impl PluginTui {
    pub fn new(plugin_dir: PathBuf, config_dir: PathBuf) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        
        let plugins = Self::load_installed_plugins(&plugin_dir, &config_dir);
        
        Self {
            plugins: Arc::new(Mutex::new(plugins)),
            list_state,
            show_help: false,
            show_detail: false,
            selected_plugin: None,
            plugin_dir,
            config_dir,
        }
    }

    fn load_installed_plugins(plugin_dir: &Path, config_dir: &Path) -> Vec<InstalledPlugin> {
        let mut plugins = Vec::new();
        
        if let Ok(entries) = fs::read_dir(plugin_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let manifest_path = path.join("plugin.toml");
                    if manifest_path.exists() {
                        if let Ok(content) = fs::read_to_string(&manifest_path) {
                            if let Ok(config) = toml::from_str::<PluginConfig>(&content) {
                                let enabled = Self::is_plugin_enabled(&config.plugin.id, config_dir);
                                plugins.push(InstalledPlugin {
                                    metadata: config.plugin,
                                    enabled,
                                    install_path: path.clone(),
                                    source_type: config.source.to_string(),
                                });
                            }
                        }
                    }
                }
            }
        }
        
        plugins
    }

    fn is_plugin_enabled(plugin_id: &str, config_dir: &Path) -> bool {
        let enabled_file = config_dir.join("plugins_enabled.toml");
        if let Ok(content) = fs::read_to_string(&enabled_file) {
            if let Ok(val) = content.parse::<toml::Value>() {
                return val.get(plugin_id).and_then(|v| v.as_bool()).unwrap_or(true);
            }
        }
        true
    }

    fn save_plugin_enabled(&self, plugin_id: &str, enabled: bool) -> Result<()> {
        let enabled_file = self.config_dir.join("plugins_enabled.toml");
        let mut val = if enabled_file.exists() {
            fs::read_to_string(&enabled_file)?.parse::<toml::Value>()?
        } else {
            toml::Value::Table(toml::Table::new())
        };
        val[plugin_id] = toml::Value::Boolean(enabled);
        fs::write(&enabled_file, toml::to_string_pretty(&val)?)?;
        Ok(())
    }

    pub fn run(&mut self) -> Result<()> {
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

        res
    }

    fn run_app(&mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
        loop {
            terminal.draw(|f| self.ui(f))?;

            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            if self.show_help || self.show_detail {
                                self.show_help = false;
                                self.show_detail = false;
                            } else {
                                break;
                            }
                        }
                        KeyCode::Char('h') | KeyCode::F(1) => self.show_help = !self.show_help,
                        KeyCode::Char('d') => self.show_detail = !self.show_detail,
                        KeyCode::Up | KeyCode::Char('k') => self.previous(),
                        KeyCode::Down | KeyCode::Char('j') => self.next(),
                        KeyCode::Enter | KeyCode::Char(' ') => self.toggle_enabled(),
                        KeyCode::Char('i') => self.install_plugin()?,
                        KeyCode::Char('u') => self.uninstall_plugin()?,
                        KeyCode::Char('r') => self.refresh_plugins()?,
                        KeyCode::Char('c') => self.open_config()?,
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    fn ui(&mut self, f: &mut ratatui::Frame) {
        let size = f.area();

        if self.show_help {
            self.render_help(f, size);
            return;
        }

        if self.show_detail {
            if let Some(idx) = self.selected_plugin {
                self.render_detail(f, size, idx);
            }
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

        let plugins = self.plugins.lock().unwrap();
        
        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Bulker Plugin Manager", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled("[h/F1] Help  [q/Esc] Quit", Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(vec![
                Span::styled("Plugins: ", Style::default().fg(Color::Yellow)),
                Span::raw(format!("{} installed", plugins.len())),
                Span::raw("  "),
                Span::styled("[↑/↓] Navigate  [Space] Toggle  [i] Install  [u] Uninstall  [r] Refresh  [d] Detail  [c] Config", Style::default().fg(Color::DarkGray)),
            ]),
        ])
        .block(Block::default().borders(Borders::ALL).title(" Plugin Manager "))
        .alignment(Alignment::Left);
        f.render_widget(header, chunks[0]);

        let items: Vec<ListItem> = plugins
            .iter()
            .enumerate()
            .map(|(i, plugin)| {
                let style = if Some(i) == self.list_state.selected() {
                    Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };

                let enabled_icon = if plugin.enabled { "●" } else { "○" };
                let enabled_color = if plugin.enabled { Color::Green } else { Color::Red };
                
                let source_color = match plugin.source_type.as_str() {
                    "builtin" => Color::Cyan,
                    "git" => Color::Blue,
                    "local" => Color::Yellow,
                    _ => Color::White,
                };

                ListItem::new(Line::from(vec![
                    Span::styled(format!("{} ", enabled_icon), Style::default().fg(enabled_color)),
                    Span::styled(&plugin.metadata.name, style.fg(Color::Cyan)),
                    Span::styled(" v", style),
                    Span::styled(&plugin.metadata.version, style.fg(Color::Yellow)),
                    Span::styled("  ", style),
                    Span::styled(&plugin.metadata.description, style.fg(Color::DarkGray)),
                    Span::styled("  [", style),
                    Span::styled(&plugin.source_type, Style::default().fg(source_color).add_modifier(Modifier::BOLD)),
                    Span::styled("]", style),
                ]))
                .style(style)
            })
            .collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Installed Plugins "))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("► ");

        f.render_stateful_widget(list, chunks[1], &mut self.list_state);

        let enabled_count = plugins.iter().filter(|p| p.enabled).count();
        let footer = Paragraph::new(vec![
            Line::from(vec![
                Span::styled(format!("Enabled: {} / {}", enabled_count, plugins.len()), Style::default().fg(Color::Yellow)),
                Span::raw("  "),
                Span::styled("[Space] Enable/Disable  [i] Install from TOML  [u] Uninstall  [r] Refresh", Style::default().fg(Color::DarkGray)),
            ]),
        ])
        .block(Block::default().borders(Borders::ALL))
        .alignment(Alignment::Left);
        f.render_widget(footer, chunks[2]);
    }

    fn render_detail(&self, f: &mut ratatui::Frame, size: Rect, idx: usize) {
        let plugins = self.plugins.lock().unwrap();
        if let Some(plugin) = plugins.get(idx) {
            let detail_text = vec![
                Line::from(""),
                Line::from(vec![Span::styled(&plugin.metadata.name, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))]),
                Line::from(vec![Span::styled("Version: ", Style::default().fg(Color::Yellow)), Span::raw(&plugin.metadata.version)]),
                Line::from(vec![Span::styled("ID: ", Style::default().fg(Color::Yellow)), Span::raw(&plugin.metadata.id)]),
                Line::from(vec![Span::styled("Description: ", Style::default().fg(Color::Yellow)), Span::raw(&plugin.metadata.description)]),
                Line::from(""),
                Line::from(vec![Span::styled("Source: ", Style::default().fg(Color::Yellow)), Span::raw(&plugin.source_type)]),
                Line::from(vec![Span::styled("Install Path: ", Style::default().fg(Color::Yellow)), Span::raw(plugin.install_path.display().to_string())]),
                Line::from(vec![Span::styled("Status: ", Style::default().fg(Color::Yellow)), Span::raw(if plugin.enabled { "Enabled" } else { "Disabled" })]),
                Line::from(""),
            ];

            let mut lines = detail_text;
            if let Some(author) = &plugin.metadata.author {
                lines.push(Line::from(vec![Span::styled("Author: ", Style::default().fg(Color::Yellow)), Span::raw(author)]));
            }
            if let Some(repo) = &plugin.metadata.repository {
                lines.push(Line::from(vec![Span::styled("Repository: ", Style::default().fg(Color::Yellow)), Span::raw(repo)]));
            }
            if let Some(license) = &plugin.metadata.license {
                lines.push(Line::from(vec![Span::styled("License: ", Style::default().fg(Color::Yellow)), Span::raw(license)]));
            }

            lines.push(Line::from(""));
            lines.push(Line::from(vec![Span::styled("Press [d] or [Esc] to close", Style::default().fg(Color::DarkGray))]));

            let detail = Paragraph::new(lines)
                .block(Block::default().borders(Borders::ALL).title(" Plugin Details "))
                .wrap(Wrap { trim: true })
                .alignment(Alignment::Left);

            let area = centered_rect(70, 70, size);
            f.render_widget(detail, area);
        }
    }

    fn render_help(&self, f: &mut ratatui::Frame, size: Rect) {
        let help_text = vec![
            Line::from(""),
            Line::from(vec![Span::styled("BULKER PLUGIN MANAGER - KEYBINDINGS", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))]),
            Line::from(""),
            Line::from(vec![Span::styled("Navigation:", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))]),
            Line::from("  ↑/k, ↓/j     Move selection up/down"),
            Line::from("  PgUp/PgDn    Page up/down"),
            Line::from("  Home/End     Jump to first/last"),
            Line::from(""),
            Line::from(vec![Span::styled("Plugin Management:", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))]),
            Line::from("  Space/Enter  Toggle plugin enabled/disabled"),
            Line::from("  i            Install new plugin from TOML file"),
            Line::from("  u            Uninstall selected plugin"),
            Line::from("  r            Refresh plugin list"),
            Line::from("  d            Show plugin details"),
            Line::from("  c            Open plugin config directory"),
            Line::from(""),
            Line::from(vec![Span::styled("General:", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))]),
            Line::from("  h/F1         Toggle this help"),
            Line::from("  q/Esc        Quit (or close help/detail)"),
            Line::from(""),
            Line::from(vec![Span::styled("Plugin TOML Format:", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))]),
            Line::from("  [plugin]"),
            Line::from("  id = \"my_plugin\""),
            Line::from("  name = \"My Plugin\""),
            Line::from("  version = \"1.0.0\""),
            Line::from("  description = \"Plugin description\""),
            Line::from("  author = \"Author Name\""),
            Line::from("  repository = \"https://github.com/...\" (optional)"),
            Line::from("  license = \"MIT\" (optional)"),
            Line::from("  "),
            Line::from("  [source]"),
            Line::from("  type = \"git\"  # or \"local\" or \"builtin\""),
            Line::from("  url = \"https://github.com/user/repo.git\" (for git)"),
            Line::from("  branch = \"main\" (optional)"),
            Line::from("  tag = \"v1.0.0\" (optional)"),
            Line::from("  path = \"/path/to/plugin\" (for local)"),
            Line::from("  name = \"builtin_name\" (for builtin)"),
            Line::from(""),
            Line::from(vec![Span::styled("Press any key to close help...", Style::default().fg(Color::DarkGray))]),
        ];

        let help = Paragraph::new(help_text)
            .block(Block::default().borders(Borders::ALL).title(" Help "))
            .wrap(Wrap { trim: true })
            .alignment(Alignment::Left);

        let area = centered_rect(80, 90, size);
        f.render_widget(help, area);
    }

    fn previous(&mut self) {
        let plugins = self.plugins.lock().unwrap();
        if plugins.is_empty() { return; }
        
        let i = match self.list_state.selected() {
            Some(i) => if i == 0 { plugins.len() - 1 } else { i - 1 },
            None => 0,
        };
        self.list_state.select(Some(i));
        self.selected_plugin = Some(i);
    }

    fn next(&mut self) {
        let plugins = self.plugins.lock().unwrap();
        if plugins.is_empty() { return; }
        
        let i = match self.list_state.selected() {
            Some(i) => if i >= plugins.len() - 1 { 0 } else { i + 1 },
            None => 0,
        };
        self.list_state.select(Some(i));
        self.selected_plugin = Some(i);
    }

    fn toggle_enabled(&mut self) {
        let mut plugins = self.plugins.lock().unwrap();
        if let Some(idx) = self.list_state.selected() {
            if let Some(plugin) = plugins.get_mut(idx) {
                plugin.enabled = !plugin.enabled;
                let _ = self.save_plugin_enabled(&plugin.metadata.id, plugin.enabled);
            }
        }
    }

    fn install_plugin(&mut self) -> Result<()> {
        disable_raw_mode()?;
        execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
        
        println!("Enter path to plugin.toml file:");
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let toml_path = PathBuf::from(input.trim());
        
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;

        if !toml_path.exists() {
            eprintln!("File not found: {}", toml_path.display());
            return Ok(());
        }

        let content = fs::read_to_string(&toml_path)?;
        let config: PluginConfig = toml::from_str(&content)
            .context("Failed to parse plugin.toml")?;

        let plugin_dir_name = config.plugin.id.replace(['/', '\\'], "_");
        let install_path = self.plugin_dir.join(&plugin_dir_name);
        
        if install_path.exists() {
            eprintln!("Plugin '{}' already installed at {}", config.plugin.id, install_path.display());
            return Ok(());
        }

        fs::create_dir_all(&install_path)?;
        
        match config.source {
            PluginSource::Git { ref url, ref branch, ref tag } => {
                let mut cmd = std::process::Command::new("git");
                cmd.args(["clone", &url, &install_path.to_string_lossy()]);
                if let Some(b) = branch {
                    cmd.args(["--branch", &b]);
                }
                if let Some(t) = tag {
                    cmd.args(["--tag", &t]);
                }
                let output = cmd.output()?;
                if !output.status.success() {
                    fs::remove_dir_all(&install_path)?;
                    anyhow::bail!("Git clone failed: {}", String::from_utf8_lossy(&output.stderr));
                }
            }
            PluginSource::Local { ref path } => {
                let src = PathBuf::from(path);
                if !src.exists() {
                    fs::remove_dir_all(&install_path)?;
                    anyhow::bail!("Local path does not exist: {}", src.display());
                }
                copy_dir_all(&src, &install_path)?;
            }
            PluginSource::Builtin { .. } => {
                anyhow::bail!("Builtin plugins are registered in code, not installed via TOML");
            }
        }

        fs::write(install_path.join("plugin.toml"), toml::to_string_pretty(&config)?)?;
        
        self.refresh_plugins()?;
        println!("Plugin '{}' installed successfully!", config.plugin.id);
        
        Ok(())
    }

    fn uninstall_plugin(&mut self) -> Result<()> {
        let (plugin_id, install_path) = {
            let plugins = self.plugins.lock().unwrap();
            if let Some(idx) = self.list_state.selected() {
                if let Some(plugin) = plugins.get(idx) {
                    (plugin.metadata.id.clone(), plugin.install_path.clone())
                } else {
                    return Ok(());
                }
            } else {
                return Ok(());
            }
        };
        
        disable_raw_mode()?;
        execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
        
        println!("Uninstall plugin '{}'? This will remove: {}", plugin_id, install_path.display());
        println!("Type 'yes' to confirm: ");
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;

        if input.trim().to_lowercase() == "yes" {
            fs::remove_dir_all(&install_path)?;
            self.save_plugin_enabled(&plugin_id, false)?;
            self.refresh_plugins()?;
            println!("Plugin '{}' uninstalled.", plugin_id);
        } else {
            println!("Uninstall cancelled.");
        }
        Ok(())
    }

    fn refresh_plugins(&mut self) -> Result<()> {
        let new_plugins = Self::load_installed_plugins(&self.plugin_dir, &self.config_dir);
        let is_empty = new_plugins.is_empty();
        let len = new_plugins.len();
        *self.plugins.lock().unwrap() = new_plugins;
        if let Some(idx) = self.list_state.selected() {
            if idx >= len && !is_empty {
                self.list_state.select(Some(len - 1));
            }
        } else if !is_empty {
            self.list_state.select(Some(0));
        }
        Ok(())
    }

    fn open_config(&self) -> Result<()> {
        let config_file = self.config_dir.join("plugins_enabled.toml");
        if !config_file.exists() {
            fs::write(&config_file, "# Plugin enabled states\n# plugin_id = true/false\n")?;
        }
        
        disable_raw_mode()?;
        execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
        
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nano".to_string());
        std::process::Command::new(&editor).arg(&config_file).status()?;
        
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
        
        Ok(())
    }
}

impl PluginSource {
    fn to_string(&self) -> String {
        match self {
            PluginSource::Git { .. } => "git".to_string(),
            PluginSource::Local { .. } => "local".to_string(),
            PluginSource::Builtin { .. } => "builtin".to_string(),
        }
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

fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        
        if src_path.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

pub fn scan_plugin_dir(plugin_dir: &PathBuf, config_dir: &PathBuf) -> Result<Vec<InstalledPlugin>> {
    Ok(PluginTui::load_installed_plugins(plugin_dir, config_dir))
}