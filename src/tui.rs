use std::{
    io::{self, Stdout},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
    Frame, Terminal,
};

use rbxl_obfuscate::{
    default_output_path,
    extract::{ExtractOptions, ExtractSummary},
    LongScriptAction, LongScriptContext, LongScriptPhase, ObfuscationLevel, ObfuscationSummary,
    Options, ProgressEvent,
};

const APP_TITLE: &str = "rbx-obfuscator v1.0";
const STAGE_WEIGHTS_OBFUSCATE: [f64; 3] = [0.15, 0.70, 0.15];
const STAGE_WEIGHTS_EXTRACT: [f64; 3] = [0.25, 0.55, 0.20];
const LEVELS: [ObfuscationLevel; 4] = [
    ObfuscationLevel::Minimal,
    ObfuscationLevel::Low,
    ObfuscationLevel::Medium,
    ObfuscationLevel::High,
];

struct Theme;

impl Theme {
    const BACKGROUND: Color = Color::Rgb(0x1e, 0x22, 0x27);
    const PANEL_BG: Color = Color::Rgb(0x22, 0x27, 0x2e);
    const PANEL_BG_ACTIVE: Color = Color::Rgb(0x26, 0x32, 0x41);
    const BORDER: Color = Color::Rgb(0x4b, 0x55, 0x63);
    const BORDER_DIM: Color = Color::Rgb(0x37, 0x41, 0x51);
    const BORDER_ACTIVE: Color = Color::Rgb(0x58, 0xa6, 0xff);
    const TEXT: Color = Color::Rgb(0xdc, 0xe2, 0xeb);
    const TEXT_MUTED: Color = Color::Rgb(0x9c, 0xa3, 0xaf);
    const BLUE: Color = Color::Rgb(0x58, 0xa6, 0xff);
    const GREEN: Color = Color::Rgb(0x5e, 0xe7, 0x87);
    const YELLOW: Color = Color::Rgb(0xf2, 0xcc, 0x60);
    const RED: Color = Color::Rgb(0xff, 0x6b, 0x6b);
    const PURPLE: Color = Color::Rgb(0xd2, 0xa8, 0xff);
    const PROGRESS_TRACK: Color = Color::Rgb(0x3b, 0x46, 0x54);
    const PROGRESS_FILL: Color = Color::Rgb(0x58, 0xa6, 0xff);
}

pub fn run_wizard() -> Result<()> {
    let mut terminal = TerminalSession::enter()?;
    let mut state = WizardState::default();

    loop {
        terminal.draw(|frame| render_home(frame, &state))?;
        if event::poll(Duration::from_millis(100))? {
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match handle_home_key(&mut state, key.code)? {
                WizardAction::Continue => {}
                WizardAction::Quit => return Ok(()),
                WizardAction::Run => break,
            }
        }
    }

    run_operation(&mut terminal, state)
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable terminal raw mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend).context("failed to initialize terminal UI")?;
        terminal.clear().context("failed to clear terminal UI")?;
        Ok(Self { terminal })
    }

    fn draw<F>(&mut self, draw: F) -> Result<()>
    where
        F: FnOnce(&mut Frame<'_>),
    {
        self.terminal.draw(draw)?;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WizardMode {
    Obfuscate,
    Extract,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WizardStep {
    Mode,
    Input,
    Output,
    Level,
    Run,
}

#[derive(Debug)]
enum WizardAction {
    Continue,
    Run,
    Quit,
}

#[derive(Debug)]
struct WizardState {
    mode: WizardMode,
    step: WizardStep,
    input: String,
    output: String,
    level_index: usize,
    message: String,
}

impl Default for WizardState {
    fn default() -> Self {
        Self {
            mode: WizardMode::Obfuscate,
            step: WizardStep::Mode,
            input: String::new(),
            output: String::new(),
            level_index: 3,
            message: "Tip: drag or paste a file path into the terminal.".to_owned(),
        }
    }
}

impl WizardState {
    fn selected_level(&self) -> ObfuscationLevel {
        LEVELS[self.level_index]
    }

    fn is_input_valid(&self) -> bool {
        let path = PathBuf::from(clean_path_input(&self.input));
        path.is_file() && rbxl_obfuscate::validate_input_format(&path).is_ok()
    }

    fn is_output_valid(&self) -> bool {
        let input = PathBuf::from(clean_path_input(&self.input));
        let output = PathBuf::from(clean_path_input(&self.output));
        if output.as_os_str().is_empty() {
            return false;
        }
        match self.mode {
            WizardMode::Obfuscate => {
                rbxl_obfuscate::validate_input_format(&output).is_ok()
                    && rbxl_obfuscate::validate_output_path(&input, &output).is_ok()
            }
            WizardMode::Extract => {
                rbxl_obfuscate::extract::validate_extract_output(&input, &output).is_ok()
            }
        }
    }
}

fn handle_home_key(state: &mut WizardState, code: KeyCode) -> Result<WizardAction> {
    match code {
        KeyCode::Char('q') => return Ok(WizardAction::Quit),
        KeyCode::Esc => {
            state.step = match state.step {
                WizardStep::Mode => return Ok(WizardAction::Quit),
                WizardStep::Input => WizardStep::Mode,
                WizardStep::Output => WizardStep::Input,
                WizardStep::Level => WizardStep::Output,
                WizardStep::Run => {
                    if state.mode == WizardMode::Obfuscate {
                        WizardStep::Level
                    } else {
                        WizardStep::Output
                    }
                }
            };
        }
        KeyCode::Up => match state.step {
            WizardStep::Mode => state.mode = WizardMode::Obfuscate,
            WizardStep::Level => state.level_index = state.level_index.saturating_sub(1),
            _ => {}
        },
        KeyCode::Down => match state.step {
            WizardStep::Mode => state.mode = WizardMode::Extract,
            WizardStep::Level => state.level_index = (state.level_index + 1).min(LEVELS.len() - 1),
            _ => {}
        },
        KeyCode::Left => match state.step {
            WizardStep::Mode => state.mode = WizardMode::Obfuscate,
            WizardStep::Level => state.level_index = state.level_index.saturating_sub(1),
            _ => {}
        },
        KeyCode::Right => match state.step {
            WizardStep::Mode => state.mode = WizardMode::Extract,
            WizardStep::Level => state.level_index = (state.level_index + 1).min(LEVELS.len() - 1),
            _ => {}
        },
        KeyCode::Enter => match state.step {
            WizardStep::Mode => state.step = WizardStep::Input,
            WizardStep::Input => {
                if state.is_input_valid() {
                    if state.output.trim().is_empty() && state.mode == WizardMode::Obfuscate {
                        let input = PathBuf::from(clean_path_input(&state.input));
                        if let Ok(default_output) =
                            default_output_path(&input, state.selected_level())
                        {
                            state.output = default_output.display().to_string();
                        }
                    }
                    state.step = WizardStep::Output;
                    state.message = "Input file looks good.".to_owned();
                } else {
                    state.message =
                        "Input must be an existing .rbxl, .rbxm, .rbxlx, or .rbxmx file."
                            .to_owned();
                }
            }
            WizardStep::Output => {
                if state.is_output_valid() {
                    state.step = if state.mode == WizardMode::Obfuscate {
                        WizardStep::Level
                    } else {
                        WizardStep::Run
                    };
                    state.message = "Output path looks good.".to_owned();
                } else {
                    state.message =
                        "Output path is invalid or points at the same file as the input."
                            .to_owned();
                }
            }
            WizardStep::Level => state.step = WizardStep::Run,
            WizardStep::Run => return Ok(WizardAction::Run),
        },
        KeyCode::Backspace => match state.step {
            WizardStep::Input => {
                state.input.pop();
            }
            WizardStep::Output => {
                state.output.pop();
            }
            _ => {}
        },
        KeyCode::Char(ch) => match state.step {
            WizardStep::Input => state.input.push(ch),
            WizardStep::Output => state.output.push(ch),
            _ => {}
        },
        _ => {}
    }

    Ok(WizardAction::Continue)
}

fn run_operation(terminal: &mut TerminalSession, state: WizardState) -> Result<()> {
    let input = PathBuf::from(clean_path_input(&state.input));
    let output = PathBuf::from(clean_path_input(&state.output));
    let started_at = Instant::now();
    let mut progress_state = ProgressUiState::new(state.mode);
    let (sender, receiver) = mpsc::channel();
    let (script_action_sender, script_action_receiver) = mpsc::channel();
    let cancel_requested = Arc::new(AtomicBool::new(false));

    match state.mode {
        WizardMode::Obfuscate => {
            let level = state.selected_level();
            let worker_cancel = Arc::clone(&cancel_requested);
            thread::spawn(move || {
                let mut long_script_control = WorkerLongScriptControl::default();
                let result = rbxl_obfuscate::run_with_progress_controlled_and_script_actions(
                    Options {
                        input,
                        output: Some(output),
                        obfuscation_level: level,
                        dry_run: false,
                        strip_types: false,
                        verbose: false,
                        backup_dir: None,
                        skip_paths: Vec::new(),
                        manifest: None,
                    },
                    || worker_cancel.load(Ordering::SeqCst),
                    |context| long_script_control.next_action(context, &script_action_receiver),
                    |event| {
                        let _ = sender.send(WorkerMessage::Progress(event));
                    },
                )
                .map(WizardResult::Obfuscation)
                .map_err(|error| format!("{error:#}"));
                let _ = sender.send(WorkerMessage::Finished(result));
            });
        }
        WizardMode::Extract => {
            let worker_cancel = Arc::clone(&cancel_requested);
            thread::spawn(move || {
                let result = rbxl_obfuscate::extract::run_with_progress_controlled(
                    ExtractOptions {
                        input,
                        output_folder: output,
                        verbose: false,
                    },
                    || worker_cancel.load(Ordering::SeqCst),
                    |event| {
                        let _ = sender.send(WorkerMessage::Progress(event));
                    },
                )
                .map(WizardResult::Extraction)
                .map_err(|error| format!("{error:#}"));
                let _ = sender.send(WorkerMessage::Finished(result));
            });
        }
    }

    let result = 'progress: loop {
        while let Ok(message) = receiver.try_recv() {
            match message {
                WorkerMessage::Progress(event) => progress_state.apply(event),
                WorkerMessage::Finished(result) => break 'progress result,
            }
        }
        terminal.draw(|frame| render_progress(frame, &progress_state))?;
        if event::poll(Duration::from_millis(100))? {
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if progress_state.long_script_prompt_active {
                match key.code {
                    KeyCode::Enter => {
                        let _ = script_action_sender.send(LongScriptAction::Skip);
                        progress_state.notice = "Skip requested for the current long-running script.".to_owned();
                    }
                    KeyCode::Char('m') | KeyCode::Char('M') => {
                        let _ = script_action_sender.send(LongScriptAction::SwitchToMinify);
                        progress_state.notice = "Minify requested for the current long-running script.".to_owned();
                    }
                    KeyCode::Char('k') | KeyCode::Char('K') | KeyCode::Char('s') | KeyCode::Char('S') => {
                        let _ = script_action_sender.send(LongScriptAction::KeepWaiting);
                        progress_state.notice = "Stay-current requested for this long-running script.".to_owned();
                    }
                    _ => {}
                }
                continue;
            }
            if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                cancel_requested.store(true, Ordering::SeqCst);
                progress_state.notice =
                    "Cancel requested; waiting for the current operation to stop cleanly."
                        .to_owned();
            }
        }
    };

    let completion = CompletionState::from_result(result, started_at.elapsed())?;
    loop {
        terminal.draw(|frame| render_complete(frame, &completion))?;
        if event::poll(Duration::from_millis(100))? {
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Enter | KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') => {}
                _ => {}
            }
        }
    }
}

#[derive(Debug)]
enum WorkerMessage {
    Progress(ProgressEvent),
    Finished(std::result::Result<WizardResult, String>),
}

#[derive(Default)]
struct WorkerLongScriptControl {
    active_script: Option<String>,
    stay_on_current_preset: bool,
    stay_on_minify: bool,
}

impl WorkerLongScriptControl {
    fn next_action(
        &mut self,
        context: LongScriptContext,
        receiver: &mpsc::Receiver<LongScriptAction>,
    ) -> Option<LongScriptAction> {
        if self.active_script.as_deref() != Some(context.script_path.as_str()) {
            self.active_script = Some(context.script_path.clone());
            self.stay_on_current_preset = false;
            self.stay_on_minify = false;
            while receiver.try_recv().is_ok() {}
        }

        let mut requested = None;
        while let Ok(action) = receiver.try_recv() {
            requested = Some(action);
        }

        match requested {
            Some(LongScriptAction::KeepWaiting) => {
                match context.phase {
                    LongScriptPhase::Prompt | LongScriptPhase::AutoSwitchToMinify => {
                        self.stay_on_current_preset = true;
                    }
                    LongScriptPhase::MinifyPrompt | LongScriptPhase::AutoSkip => {
                        self.stay_on_minify = true;
                    }
                }
                Some(LongScriptAction::KeepWaiting)
            }
            Some(action) => Some(action),
            None => match context.phase {
                LongScriptPhase::AutoSwitchToMinify if self.stay_on_current_preset => {
                    Some(LongScriptAction::KeepWaiting)
                }
                LongScriptPhase::AutoSkip if self.stay_on_minify => Some(LongScriptAction::KeepWaiting),
                _ => None,
            },
        }
    }
}

#[derive(Debug)]
enum WizardResult {
    Obfuscation(ObfuscationSummary),
    Extraction(ExtractSummary),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StageStatus {
    Pending,
    InProgress,
    Completed,
}

impl StageStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Pending => "○ Pending",
            Self::InProgress => "◌ In progress",
            Self::Completed => "✓ Completed",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Pending => Theme::TEXT_MUTED,
            Self::InProgress => Theme::YELLOW,
            Self::Completed => Theme::GREEN,
        }
    }
}

#[derive(Debug)]
struct ProgressUiState {
    mode: WizardMode,
    subtitle: &'static str,
    stages: Vec<(String, StageStatus)>,
    stage_completion: [f64; 3],
    current_stage: usize,
    current_name: String,
    current_thing: String,
    compatibility: String,
    scripts_completed: usize,
    scripts_total: usize,
    eta: Option<u64>,
    notice: String,
    long_script_prompt_active: bool,
    long_script_can_minify: bool,
}

impl ProgressUiState {
    fn new(mode: WizardMode) -> Self {
        let (subtitle, stages) = match mode {
            WizardMode::Obfuscate => (
                "Full obfuscation in progress",
                vec![
                    (
                        "Extract scripts from RBXL/RBXM".to_owned(),
                        StageStatus::Pending,
                    ),
                    ("Obfuscating scripts".to_owned(), StageStatus::Pending),
                    ("Compile output file".to_owned(), StageStatus::Pending),
                ],
            ),
            WizardMode::Extract => (
                "Extract Components",
                vec![
                    ("Parse RBXL/RBXM".to_owned(), StageStatus::Pending),
                    ("Export components".to_owned(), StageStatus::Pending),
                    ("Write manifest".to_owned(), StageStatus::Pending),
                ],
            ),
        };

        Self {
            mode,
            subtitle,
            stages,
            stage_completion: [0.0, 0.0, 0.0],
            current_stage: 1,
            current_name: String::new(),
            current_thing: "Preparing".to_owned(),
            compatibility: "None".to_owned(),
            scripts_completed: 0,
            scripts_total: 0,
            eta: None,
            notice: String::new(),
            long_script_prompt_active: false,
            long_script_can_minify: false,
        }
    }

    fn apply(&mut self, event: ProgressEvent) {
        match event {
            ProgressEvent::StageStarted {
                stage_index, name, ..
            } => {
                self.current_stage = stage_index;
                self.current_name = name.clone();
                if let Some((_, status)) = self.stages.get_mut(stage_index.saturating_sub(1)) {
                    *status = StageStatus::InProgress;
                }
            }
            ProgressEvent::StageCompleted { stage_index, .. } => {
                if let Some((_, status)) = self.stages.get_mut(stage_index.saturating_sub(1)) {
                    *status = StageStatus::Completed;
                }
                if let Some(completion) =
                    self.stage_completion.get_mut(stage_index.saturating_sub(1))
                {
                    *completion = 1.0;
                }
            }
            ProgressEvent::CurrentItem { value, .. } => {
                if self.current_thing != value {
                    self.long_script_prompt_active = false;
                    self.long_script_can_minify = false;
                }
                self.current_thing = value;
            }
            ProgressEvent::ScriptProgress {
                completed,
                total,
                current_path,
            } => {
                self.scripts_completed = completed;
                self.scripts_total = total;
                if total > 0 && self.current_stage > 0 {
                    if let Some(completion) = self.stage_completion.get_mut(self.current_stage - 1)
                    {
                        *completion = (completed as f64 / total as f64).clamp(0.0, 1.0);
                    }
                }
                if let Some(path) = current_path {
                    self.current_thing = path;
                }
            }
            ProgressEvent::CompatibilityNote { message } => self.compatibility = message,
            ProgressEvent::EtaUpdated { seconds_remaining } => self.eta = seconds_remaining,
            ProgressEvent::LongScriptPrompt {
                message,
                can_minify,
                ..
            } => {
                self.long_script_prompt_active = true;
                self.long_script_can_minify = can_minify;
                self.notice = message;
            }
            ProgressEvent::LongScriptDecision { message, action, .. } => {
                self.notice = message;
                if matches!(
                    action,
                    LongScriptAction::Skip | LongScriptAction::SwitchToMinify
                ) {
                    self.long_script_prompt_active = false;
                    self.long_script_can_minify = false;
                }
            }
            ProgressEvent::Warning { message } => {
                self.long_script_prompt_active = false;
                self.long_script_can_minify = false;
                self.notice = message;
            }
            ProgressEvent::Finished => {}
        }
    }

    fn stage_progress(&self) -> f64 {
        self.stage_completion
            .get(self.current_stage.saturating_sub(1))
            .copied()
            .unwrap_or(0.0)
    }

    fn overall_progress(&self) -> f64 {
        let weights = match self.mode {
            WizardMode::Obfuscate => STAGE_WEIGHTS_OBFUSCATE,
            WizardMode::Extract => STAGE_WEIGHTS_EXTRACT,
        };
        self.stage_completion
            .iter()
            .zip(weights)
            .map(|(completion, weight)| completion * weight)
            .sum::<f64>()
            .clamp(0.0, 1.0)
    }
}

#[derive(Debug)]
struct CompletionState {
    mode: String,
    input: String,
    output_label: String,
    output: String,
    level: Option<String>,
    scripts_label: String,
    scripts_line: Option<String>,
    guis_line: Option<String>,
    content_refs_line: Option<String>,
    duration: Duration,
    backup: Option<String>,
    command: String,
}

impl CompletionState {
    fn from_result(
        result: std::result::Result<WizardResult, String>,
        duration: Duration,
    ) -> Result<Self> {
        match result {
            Ok(WizardResult::Obfuscation(summary)) => Ok(Self {
                mode: "Full Obfuscation".to_owned(),
                input: display_path(&summary.input),
                output_label: "Output".to_owned(),
                output: display_path(&summary.output),
                level: Some(summary.prometheus_preset.to_owned()),
                scripts_label: "Scripts obfuscated".to_owned(),
                scripts_line: Some(format!(
                    "{} / {}",
                    summary.scripts_processed, summary.scripts_found
                )),
                guis_line: None,
                content_refs_line: None,
                duration,
                backup: Some(if summary.backup_created {
                    "Created".to_owned()
                } else {
                    "Not requested".to_owned()
                }),
                command: format!(
                    "rbx-obfuscator \\\n  \"{}\" \\\n  \"{}\" \\\n  --level {}",
                    display_path(&summary.input),
                    display_path(&summary.output),
                    summary.obfuscation_level.as_str()
                ),
            }),
            Ok(WizardResult::Extraction(summary)) => Ok(Self {
                mode: "Extract Components".to_owned(),
                input: display_path(&summary.input),
                output_label: "Output folder".to_owned(),
                output: display_path(&summary.output_folder),
                level: None,
                scripts_label: "Scripts exported".to_owned(),
                scripts_line: Some(format!(
                    "{} / {}",
                    summary.scripts_exported, summary.scripts_found
                )),
                guis_line: Some(summary.guis_exported.to_string()),
                content_refs_line: Some(summary.content_refs_found.to_string()),
                duration,
                backup: None,
                command: format!(
                    "rbx-obfuscator extract \\\n  \"{}\" \\\n  \"{}\"",
                    display_path(&summary.input),
                    display_path(&summary.output_folder)
                ),
            }),
            Err(error) => bail!(error),
        }
    }
}

fn render_home(frame: &mut Frame<'_>, state: &WizardState) {
    let area = frame.area();
    render_background(frame, area);
    let area = app_area(area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(6),
            Constraint::Length(15),
            Constraint::Min(4),
        ])
        .split(area);
    render_header(frame, chunks[0], "Interactive setup wizard");

    let body_lines = vec![
        Line::from(Span::styled("Choose what you want to do:", text_style())),
        Line::from(""),
        option_line(
            state.mode == WizardMode::Obfuscate,
            "Full Obfuscation (recommended)",
        ),
        option_line(
            state.mode == WizardMode::Extract,
            "Extract RBXL/RBXM Components Only",
        ),
    ];
    frame.render_widget(
        Paragraph::new(body_lines)
            .style(text_style())
            .block(card_block(None, false, false)),
        chunks[1],
    );

    let steps = step_lines(state);
    frame.render_widget(
        Paragraph::new(steps)
            .block(card_block(Some(" Setup "), true, false))
            .style(text_style())
            .wrap(Wrap { trim: false }),
        chunks[2],
    );

    render_footer(
        frame,
        chunks[3],
        &[
            state.message.as_str(),
            "Enter = continue | ↑/↓ = move | ←/→ = change option | q = quit",
            "Direct argument mode still works.",
        ],
    );
}

fn render_progress(frame: &mut Frame<'_>, state: &ProgressUiState) {
    let area = frame.area();
    render_background(frame, area);
    let area = app_area(area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(6),
            Constraint::Length(7),
            Constraint::Length(5),
            Constraint::Min(3),
        ])
        .split(area);
    render_header(frame, chunks[0], state.subtitle);

    render_stage_cards(frame, chunks[1], state);

    let eta = state
        .eta
        .map(format_seconds)
        .unwrap_or_else(|| "calculating...".to_owned());
    let script_line = script_status_line(state);
    let status_columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
        .split(chunks[2]);
    let left_lines = vec![
        Line::from(vec![
            Span::styled("Current thing: ", neutral_style()),
            Span::styled(state.current_thing.clone(), text_bold_style()),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Compatibility: ", purple_style()),
            Span::styled(state.compatibility.clone(), text_style()),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(left_lines)
            .block(card_block(None, false, false))
            .wrap(Wrap { trim: false }),
        status_columns[0],
    );

    let right_lines = vec![
        script_line,
        Line::from(""),
        Line::from(vec![Span::styled(
            "Estimated time remaining:",
            neutral_style(),
        )]),
        Line::from(vec![Span::styled(eta, blue_bold())]),
    ];
    frame.render_widget(
        Paragraph::new(right_lines)
            .block(card_block(None, false, false))
            .wrap(Wrap { trim: false }),
        status_columns[1],
    );

    let progress_lines = vec![
        percent_bar_line("Overall progress", state.overall_progress()),
        percent_bar_line(
            format!(
                "Stage progress ({}/{})",
                state.current_stage,
                state.stages.len()
            ),
            state.stage_progress(),
        ),
    ];
    frame.render_widget(
        Paragraph::new(progress_lines)
            .block(card_block(None, false, false))
            .wrap(Wrap { trim: false }),
        chunks[3],
    );

    if state.long_script_prompt_active {
        if state.long_script_can_minify {
            render_footer(
                frame,
                chunks[4],
                &[
                    state.notice.as_str(),
                    "Enter = skip script | m = use Minify | k = force stay | q = cancel",
                ],
            );
        } else {
            render_footer(
                frame,
                chunks[4],
                &[
                    state.notice.as_str(),
                    "Enter = skip script | k = force stay | q = cancel",
                ],
            );
        }
    } else {
        let notice = if state.notice.is_empty() {
            "Press q to cancel | Logs: hidden | Mode: interactive"
        } else {
            state.notice.as_str()
        };
        render_footer(frame, chunks[4], &[notice]);
    }
}

fn render_complete(frame: &mut Frame<'_>, state: &CompletionState) {
    let area = frame.area();
    render_background(frame, area);
    let area = app_area(area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Min(14),
            Constraint::Length(3),
        ])
        .split(area);
    render_header(frame, chunks[0], "Operation complete");

    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled("[✓] Done", green_bold())])),
        chunks[1],
    );

    let mut summary = vec![
        summary_line("Mode", &state.mode, false),
        summary_line("Input", &state.input, true),
        summary_line(&state.output_label, &state.output, true),
    ];
    if let Some(level) = &state.level {
        summary.push(summary_line("Prometheus level", level, false));
    }
    if let Some(scripts) = &state.scripts_line {
        summary.push(summary_line(&state.scripts_label, scripts, false));
    }
    if let Some(guis) = &state.guis_line {
        summary.push(summary_line("GUIs exported", guis, false));
    }
    if let Some(content_refs) = &state.content_refs_line {
        summary.push(summary_line("Content refs found", content_refs, false));
    }
    summary.push(summary_line(
        "Duration",
        &format_seconds(state.duration.as_secs()),
        false,
    ));
    if let Some(backup) = &state.backup {
        summary.push(summary_line("Backup", backup, false));
    }

    let command_lines = state
        .command
        .lines()
        .map(|line| Line::from(Span::styled(line.to_owned(), green_style())))
        .collect::<Vec<_>>();
    let body = if chunks[2].width >= 90 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
            .split(chunks[2])
            .to_vec()
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
            .split(chunks[2])
            .to_vec()
    };
    frame.render_widget(
        Paragraph::new(summary)
            .block(card_block(Some(" Summary "), false, false))
            .wrap(Wrap { trim: false }),
        body[0],
    );

    let mut command_card_lines = command_lines;
    command_card_lines.push(Line::from(""));
    command_card_lines.push(Line::from(Span::styled(
        "Run the above command next time to skip the wizard.",
        neutral_style(),
    )));
    frame.render_widget(
        Paragraph::new(command_card_lines)
            .block(card_block(
                Some(" Equivalent direct command "),
                false,
                false,
            ))
            .wrap(Wrap { trim: false }),
        body[1],
    );

    render_footer(frame, chunks[3], &["Enter = exit | q = quit"]);
}

fn render_stage_cards(frame: &mut Frame<'_>, area: Rect, state: &ProgressUiState) {
    let cards = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(33),
            Constraint::Percentage(34),
            Constraint::Percentage(33),
        ])
        .split(area);

    for (index, card_area) in cards.iter().enumerate() {
        let stage_number = index + 1;
        let (name, status) = &state.stages[index];
        let active = stage_number == state.current_stage;
        let dim = matches!(status, StageStatus::Pending);
        let title_style = if active {
            blue_bold()
        } else {
            text_bold_style()
        };
        let lines = vec![
            Line::from(Span::styled(
                format!("{stage_number}/{}", state.stages.len()),
                if matches!(status, StageStatus::Completed) {
                    green_bold()
                } else {
                    title_style
                },
            )),
            Line::from(Span::styled(
                name.clone(),
                if dim { neutral_style() } else { text_style() },
            )),
            Line::from(Span::styled(
                status.label(),
                Style::default().fg(status.color()),
            )),
        ];
        frame.render_widget(
            Paragraph::new(lines)
                .block(card_block(None, active, dim))
                .wrap(Wrap { trim: false }),
            *card_area,
        );
    }
}

fn script_status_line(state: &ProgressUiState) -> Line<'static> {
    if state.scripts_total == 0 {
        let label = match state.mode {
            WizardMode::Obfuscate => "Scripts discovered:",
            WizardMode::Extract => "Scripts exported:",
        };
        return Line::from(vec![
            Span::styled(label.to_owned(), neutral_style()),
            Span::raw(" "),
            Span::styled("calculating...", yellow_style()),
        ]);
    }

    let label = match state.mode {
        WizardMode::Obfuscate => "Scripts obfuscated:",
        WizardMode::Extract => "Scripts exported:",
    };
    Line::from(vec![
        Span::styled(label.to_owned(), neutral_style()),
        Span::raw(" "),
        Span::styled(state.scripts_completed.to_string(), yellow_style()),
        Span::styled(format!(" / {}", state.scripts_total), text_style()),
    ])
}

fn render_background(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(Theme::BACKGROUND)),
        area,
    );
}

fn render_header(frame: &mut Frame<'_>, area: Rect, subtitle: &str) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(APP_TITLE, blue_bold()))).alignment(Alignment::Left),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(subtitle, neutral_style())))
            .alignment(Alignment::Right),
        columns[1],
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, lines: &[&str]) {
    let lines = lines
        .iter()
        .map(|line| footer_line(line))
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(border_dim_style())
                .style(Style::default().bg(Theme::BACKGROUND)),
        ),
        area,
    );
}

fn step_lines(state: &WizardState) -> Vec<Line<'_>> {
    let input_valid = state.is_input_valid();
    let output_valid = state.is_output_valid();
    let output_label = if state.mode == WizardMode::Obfuscate {
        "Output file"
    } else {
        "Output folder"
    };
    vec![
        step_title(
            1,
            "Input file",
            input_valid,
            state.step == WizardStep::Input,
        ),
        step_value(&state.input),
        Line::from(Span::styled(
            "  ─────────────────────────────────────────",
            dim_style(),
        )),
        step_title(
            2,
            output_label,
            output_valid,
            state.step == WizardStep::Output,
        ),
        step_value(&state.output),
        Line::from(Span::styled(
            "  ─────────────────────────────────────────",
            dim_style(),
        )),
        step_title(
            3,
            "Prometheus level",
            state.mode == WizardMode::Extract,
            state.step == WizardStep::Level,
        ),
        level_line(state),
        Line::from(Span::styled(
            "  ─────────────────────────────────────────",
            dim_style(),
        )),
        step_title(4, "Run", false, state.step == WizardStep::Run),
        Line::from(Span::styled("  Press Enter to start", neutral_style())),
    ]
}

fn step_title(index: usize, title: &str, checked: bool, active: bool) -> Line<'_> {
    let marker = if checked { "  ✓" } else { "" };
    let style = if active {
        blue_bold()
    } else {
        text_bold_style()
    };
    let line = Line::from(vec![
        Span::styled(format!("{index}. "), style),
        Span::styled(format!("{title}{marker}"), style),
    ]);
    if active {
        line.style(Style::default().bg(Theme::PANEL_BG_ACTIVE))
    } else {
        line
    }
}

fn step_value(value: &str) -> Line<'_> {
    if value.trim().is_empty() {
        Line::from(Span::styled("  Waiting for path...", neutral_style()))
    } else {
        Line::from(Span::styled(format!("  {value}"), green_style()))
    }
}

fn level_line(state: &WizardState) -> Line<'_> {
    if state.mode == WizardMode::Extract {
        return Line::from(Span::styled("  not needed for extraction", neutral_style()));
    }

    let labels = ["Weak", "Low", "Medium", "Strong"];
    let spans = labels
        .iter()
        .enumerate()
        .flat_map(|(index, label)| {
            let text = if index == state.level_index {
                format!("[{label}]")
            } else {
                (*label).to_owned()
            };
            [
                Span::styled(
                    text,
                    if index == state.level_index {
                        Style::default()
                            .fg(Theme::TEXT)
                            .bg(Theme::BLUE)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        neutral_style()
                    },
                ),
                Span::raw("   "),
            ]
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

fn option_line(selected: bool, text: &str) -> Line<'_> {
    let line = Line::from(vec![
        Span::styled(if selected { ">  " } else { "   " }, text_style()),
        Span::styled(
            text.to_owned(),
            if selected {
                yellow_style()
            } else {
                text_style()
            },
        ),
    ]);
    if selected {
        line.style(Style::default().bg(Theme::PANEL_BG_ACTIVE))
    } else {
        line
    }
}

fn percent_bar_line(label: impl Into<String>, progress: f64) -> Line<'static> {
    let width = 38usize;
    let progress = progress.clamp(0.0, 1.0);
    let filled = (progress * width as f64).round() as usize;
    let percent = (progress * 100.0).round() as usize;
    let label = label.into();
    Line::from(vec![
        Span::styled(format!("{label:<22} "), neutral_style()),
        Span::styled("━".repeat(filled.min(width)), progress_fill_style()),
        Span::styled(
            "━".repeat(width.saturating_sub(filled.min(width))),
            progress_track_style(),
        ),
        Span::styled(format!("  {percent:>3}%"), text_bold_style()),
    ])
}

fn summary_line(label: &str, value: &str, path_value: bool) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}: "), blue_bold()),
        Span::styled(
            value.to_owned(),
            if path_value {
                green_style()
            } else {
                text_style()
            },
        ),
    ])
}

fn card_block(title: Option<&'static str>, active: bool, dim: bool) -> Block<'static> {
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if active {
            active_border_style()
        } else if dim {
            border_dim_style()
        } else {
            border_style()
        })
        .padding(Padding::new(2, 2, 0, 0))
        .style(Style::default().fg(Theme::TEXT).bg(if active {
            Theme::PANEL_BG_ACTIVE
        } else {
            Theme::PANEL_BG
        }));
    if let Some(title) = title {
        block = block.title(Span::styled(title, blue_bold()));
    }
    block
}

fn app_area(area: Rect) -> Rect {
    let horizontal = if area.width >= 110 { 4 } else { 3 };
    Rect {
        x: area.x + horizontal,
        y: area.y + 1,
        width: area.width.saturating_sub(horizontal * 2),
        height: area.height.saturating_sub(2),
    }
}

fn blue_bold() -> Style {
    blue_style().add_modifier(Modifier::BOLD)
}

fn green_bold() -> Style {
    green_style().add_modifier(Modifier::BOLD)
}

fn text_bold_style() -> Style {
    text_style().add_modifier(Modifier::BOLD)
}

fn blue_style() -> Style {
    Style::default().fg(Theme::BLUE)
}

fn green_style() -> Style {
    Style::default().fg(Theme::GREEN)
}

fn yellow_style() -> Style {
    Style::default().fg(Theme::YELLOW)
}

fn purple_style() -> Style {
    Style::default().fg(Theme::PURPLE)
}

fn neutral_style() -> Style {
    Style::default().fg(Theme::TEXT_MUTED)
}

fn dim_style() -> Style {
    Style::default().fg(Theme::BORDER_DIM).bg(Theme::PANEL_BG)
}

fn text_style() -> Style {
    Style::default().fg(Theme::TEXT)
}

fn border_style() -> Style {
    Style::default().fg(Theme::BORDER).bg(Theme::PANEL_BG)
}

fn border_dim_style() -> Style {
    Style::default().fg(Theme::BORDER_DIM).bg(Theme::BACKGROUND)
}

fn active_border_style() -> Style {
    Style::default()
        .fg(Theme::BORDER_ACTIVE)
        .bg(Theme::PANEL_BG_ACTIVE)
}

fn progress_track_style() -> Style {
    Style::default()
        .fg(Theme::PROGRESS_TRACK)
        .bg(Theme::PANEL_BG)
}

fn progress_fill_style() -> Style {
    Style::default()
        .fg(Theme::PROGRESS_FILL)
        .bg(Theme::PANEL_BG)
}

fn danger_style() -> Style {
    Style::default().fg(Theme::RED)
}

fn footer_line(line: &str) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, part) in line.split('|').enumerate() {
        if index > 0 {
            spans.push(Span::styled(" | ", Style::default().fg(Theme::BORDER_DIM)));
        }
        let trimmed = part.trim();
        if let Some((key, rest)) = trimmed.split_once('=') {
            spans.push(Span::styled(key.trim().to_owned(), blue_bold()));
            spans.push(Span::styled(" = ", neutral_style()));
            spans.push(Span::styled(rest.trim().to_owned(), neutral_style()));
        } else if trimmed == "Press q to cancel" {
            spans.push(Span::styled("Press ", neutral_style()));
            spans.push(Span::styled("q", danger_style()));
            spans.push(Span::styled(" to cancel", neutral_style()));
        } else {
            spans.push(Span::styled(trimmed.to_owned(), neutral_style()));
        }
    }
    Line::from(spans)
}

fn clean_path_input(value: &str) -> String {
    let trimmed = value.trim().trim_matches('"').trim_matches('\'');
    trimmed.replace("\\ ", " ")
}

fn display_path(path: &std::path::Path) -> String {
    let path = path.display().to_string();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        if let Ok(stripped) = PathBuf::from(&path).strip_prefix(&home) {
            return format!("~/{}", stripped.display());
        }
    }
    path
}

fn format_seconds(seconds: u64) -> String {
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    format!("{minutes:02}:{seconds:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn home_screen_renders_without_panicking() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = WizardState::default();

        terminal.draw(|frame| render_home(frame, &state)).unwrap();
    }

    #[test]
    fn ui_uses_rbx_obfuscator_branding() {
        let backend = TestBackend::new(100, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = WizardState::default();

        terminal.draw(|frame| render_home(frame, &state)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("rbx-obfuscator v1.0"));
        assert!(!text.contains("Roblox-Obfuscator"));
    }

    #[test]
    fn progress_screen_renders_without_panicking() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = ProgressUiState::new(WizardMode::Obfuscate);
        state.apply(ProgressEvent::StageStarted {
            stage_index: 2,
            stage_total: 3,
            name: "Obfuscate with Prometheus".to_owned(),
        });
        state.apply(ProgressEvent::ScriptProgress {
            completed: 49,
            total: 300,
            current_path: Some("VehicleController.client.luau".to_owned()),
        });

        terminal
            .draw(|frame| render_progress(frame, &state))
            .unwrap();
    }

    #[test]
    fn weighted_overall_progress_differs_from_stage_progress() {
        let mut state = ProgressUiState::new(WizardMode::Obfuscate);
        state.apply(ProgressEvent::StageStarted {
            stage_index: 1,
            stage_total: 3,
            name: "Extract scripts from RBXL/RBXM".to_owned(),
        });
        state.apply(ProgressEvent::StageCompleted {
            stage_index: 1,
            stage_total: 3,
            name: "Extract scripts from RBXL/RBXM".to_owned(),
        });
        state.apply(ProgressEvent::StageStarted {
            stage_index: 2,
            stage_total: 3,
            name: "Obfuscate with Prometheus".to_owned(),
        });
        state.apply(ProgressEvent::ScriptProgress {
            completed: 49,
            total: 300,
            current_path: Some("VehicleController.client.luau".to_owned()),
        });

        assert_ne!(
            (state.overall_progress() * 100.0).round() as usize,
            (state.stage_progress() * 100.0).round() as usize
        );
    }

    #[test]
    fn stage_one_does_not_render_zero_over_zero_scripts() {
        let backend = TestBackend::new(100, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = ProgressUiState::new(WizardMode::Obfuscate);
        state.apply(ProgressEvent::StageStarted {
            stage_index: 1,
            stage_total: 3,
            name: "Extract scripts from RBXL/RBXM".to_owned(),
        });

        terminal
            .draw(|frame| render_progress(frame, &state))
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("Scripts discovered"));
        assert!(text.contains("calculating"));
        assert!(!text.contains("Scripts obfuscated: 0 / 0"));
    }

    #[test]
    fn complete_screen_renders_without_panicking() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = CompletionState {
            mode: "Full Obfuscation".to_owned(),
            input: "~/Documents/Ro-TransLink.rbxl".to_owned(),
            output_label: "Output".to_owned(),
            output: "~/Documents/Ro-TransLink-obfuscated.rbxl".to_owned(),
            level: Some("Strong".to_owned()),
            scripts_label: "Scripts obfuscated".to_owned(),
            scripts_line: Some("300 / 300".to_owned()),
            guis_line: None,
            content_refs_line: None,
            duration: Duration::from_secs(462),
            backup: Some("Created".to_owned()),
            command: "rbx-obfuscator \\\n  \"input.rbxl\" \\\n  \"output.rbxl\" \\\n  --level high"
                .to_owned(),
        };

        terminal
            .draw(|frame| render_complete(frame, &state))
            .unwrap();
    }

    #[test]
    fn completion_command_uses_rbx_obfuscator_and_no_copy_unavailable_footer() {
        let backend = TestBackend::new(120, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = CompletionState {
            mode: "Full Obfuscation".to_owned(),
            input: "~/Documents/train game.rbxl".to_owned(),
            output_label: "Output".to_owned(),
            output: "~/Documents/train game-obfuscated_High.rbxl".to_owned(),
            level: Some("Strong".to_owned()),
            scripts_label: "Scripts obfuscated".to_owned(),
            scripts_line: Some("18 / 19".to_owned()),
            guis_line: None,
            content_refs_line: None,
            duration: Duration::from_secs(8),
            backup: Some("Not requested".to_owned()),
            command: "rbx-obfuscator \\\n  \"~/Documents/train game.rbxl\" \\\n  \"~/Documents/train game-obfuscated_High.rbxl\" \\\n  --level high"
                .to_owned(),
        };

        terminal
            .draw(|frame| render_complete(frame, &state))
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("rbx-obfuscator"));
        assert!(!text.contains("copy command unavailable"));
    }

    #[test]
    fn eta_format_handles_zero_seconds() {
        assert_eq!(format_seconds(0), "00:00");
        assert_eq!(format_seconds(137), "02:17");
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        let area = *buffer.area();
        let mut text = String::new();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                if let Some(cell) = buffer.cell((x, y)) {
                    text.push_str(cell.symbol());
                }
            }
            text.push('\n');
        }
        text
    }
}
