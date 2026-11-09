//! The ratatui front end (PLAN.md "UI and output").
//!
//! Layout follows AIDA's window: results grid on top, system panel below, one
//! line of keys at the bottom. The measurement runs on its own thread; the UI
//! thread blocks on a single channel that carries both progress events and key
//! presses, so it is asleep — not polling, not redrawing — while a kernel is
//! being timed (PLAN.md rule 9).

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result};
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Axis, Block, Chart, Dataset, Gauge, GraphType, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use membench_core::result::{
    CellResult, Column, Level, Op, RunResults, Settings, Sweep, SystemInfo,
};
use membench_core::runner::{Event, Runner};
use membench_core::sysinfo;

use crate::render;
use crate::store;

/// Everything the two producer threads send to the UI thread.
enum Msg {
    Key(crossterm::event::KeyEvent),
    /// Boxed for the same reason as the runner's variant: an `Event` is much
    /// larger than a key press, and this is the queue both travel down.
    Progress(Box<Event>),
    /// The worker finished or died; the UI thread may exit.
    Idle,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Grid,
    Details,
    Sweep,
    Help,
}

pub struct App {
    settings: Settings,
    system: SystemInfo,
    cells: Vec<CellResult>,
    sweep: Option<Sweep>,
    warnings: Vec<String>,
    clock_mhz: Option<f64>,
    running: bool,
    cancel: Arc<AtomicBool>,
    view: View,
    row: usize,
    col: usize,
    status: String,
    started: Option<Instant>,
    cell_started: Option<Instant>,
    cell_label: String,
    trial: (usize, usize),
    last_results: Option<RunResults>,
    dirty: bool,
}

impl App {
    fn new(settings: Settings) -> Result<App> {
        let system = sysinfo::gather_for(&settings);
        Ok(App {
            settings,
            system,
            cells: Vec::new(),
            sweep: None,
            warnings: Vec::new(),
            clock_mhz: None,
            running: false,
            cancel: Arc::new(AtomicBool::new(false)),
            view: View::Grid,
            row: 0,
            col: 0,
            status: String::from("s start · ? keys · q quit"),
            started: None,
            cell_started: None,
            cell_label: String::new(),
            trial: (0, 0),
            last_results: None,
            dirty: true,
        })
    }

    /// Row order matches the printed table (`RunResults::grid_row_levels`):
    /// Memory on top, then the caches, so the two views read the same way.
    fn rows(&self) -> Vec<Level> {
        let order = [Level::Memory, Level::L1, Level::L2, Level::L3];
        let mut rows: Vec<Level> = order
            .into_iter()
            .filter(|l| self.settings.levels.contains(l))
            .collect();
        if rows.is_empty() {
            rows = order.to_vec();
        }
        rows
    }

    /// Start measuring, unless something is already running.
    fn start(&mut self, tx: Sender<Msg>) {
        if self.running {
            self.status = "already running — Ctrl-C or Esc stops it".to_string();
            return;
        }
        self.running = true;
        self.cells.clear();
        self.sweep = None;
        self.warnings.clear();
        self.started = Some(Instant::now());
        self.trial = (0, 0);
        self.status = "measuring…".to_string();

        // The worker only knows about `Event`; a forwarder turns each one into a
        // `Msg` so the UI thread can wait on a single channel and stay asleep
        // until something actually happens.
        let (etx, erx) = mpsc::channel::<Event>();
        let forward = tx.clone();
        thread::spawn(move || {
            while let Ok(event) = erx.recv() {
                if forward.send(Msg::Progress(Box::new(event))).is_err() {
                    break;
                }
            }
        });

        let done = tx.clone();
        let settings = self.settings.clone();
        let cancel = self.cancel.clone();
        thread::spawn(move || {
            let worker = match Runner::prepare(settings) {
                Ok(runner) => runner,
                Err(e) => {
                    let _ = tx.send(Msg::Progress(Box::new(Event::Failed(e.to_string()))));
                    let _ = tx.send(Msg::Idle);
                    return;
                }
            };
            // `Event::Finished` already travels on the progress channel, so the
            // worker only has to announce that it is finished here.
            if let Err(e) = worker.run(&cancel, Some(etx)) {
                let _ = done.send(Msg::Progress(Box::new(Event::Failed(e.to_string()))));
            }
            let _ = done.send(Msg::Idle);
        });
    }

    fn stop(&mut self) {
        if self.running {
            self.cancel.store(true, Ordering::SeqCst);
            self.status = "stopping after the current trial…".to_string();
        }
    }

    fn handle_progress(&mut self, event: Event) {
        match event {
            Event::CellStart { level, op, .. } => {
                self.cell_started = Some(Instant::now());
                self.cell_label = format!("{} {}", level.short(), op.label());
                self.trial = (0, self.settings.trials);
            }
            Event::Trial { trial, trials, .. } => {
                self.trial = (trial, trials);
            }
            Event::CellDone(cell) => {
                // Replace a previous measurement of the same cell so a rerun
                // does not double the rows.
                if let Some(existing) = self
                    .cells
                    .iter_mut()
                    .find(|c| c.level == cell.level && c.op == cell.op)
                {
                    *existing = cell;
                } else {
                    self.cells.push(cell);
                }
                self.cell_started = None;
            }
            Event::SweepStart { .. } => {
                self.sweep = Some(Sweep {
                    points: Vec::new(),
                    notes: Vec::new(),
                    duration_ms: self.settings.sweep_duration_ms.unwrap_or(60),
                    trials: self.settings.sweep_trials.unwrap_or(3),
                });
            }
            Event::SweepPoint(point) => {
                if let Some(sweep) = &mut self.sweep {
                    sweep.points.push(point);
                }
            }
            Event::Finished(results) => {
                let results = *results;
                self.running = false;
                self.clock_mhz = results.clock.as_ref().map(|c| c.mhz);
                self.warnings = results
                    .warnings
                    .iter()
                    .map(|w| format!("[{}] {}", w.code, w.message))
                    .collect();
                for cell in &results.cells {
                    if !self
                        .cells
                        .iter()
                        .any(|c| c.level == cell.level && c.op == cell.op)
                    {
                        self.cells.push(cell.clone());
                    }
                }
                if let Some(sweep) = &results.sweep {
                    let have = self.sweep.as_ref().map_or(0, |s| s.points.len());
                    if self.sweep.is_none() || sweep.points.len() >= have {
                        self.sweep = Some(sweep.clone());
                    }
                }
                let secs = self
                    .started
                    .map(|s| s.elapsed().as_secs_f64())
                    .unwrap_or(0.0);
                self.status = format!(
                    "{} cells in {secs:.1} s · d details · l sweep · e export · r again · q quit",
                    results.cells.len()
                );
                self.last_results = Some(results);
            }
            Event::Failed(message) => {
                self.running = false;
                self.cell_started = None;
                self.status = format!("failed: {message}");
            }
        }
        self.dirty = true;
    }

    fn results_view(&self) -> RunResults {
        match &self.last_results {
            Some(r) => r.clone(),
            None => {
                let mut r = RunResults::new(self.system.clone(), self.settings.clone());
                r.cells = self.cells.clone();
                r.sweep = self.sweep.clone();
                r.clock = self
                    .clock_mhz
                    .map(|mhz| membench_core::result::ClockMeasurement {
                        hz: mhz * 1e6,
                        mhz,
                        cpu: 0,
                        method: "measured".to_string(),
                    });
                r
            }
        }
    }

    fn export(&mut self) {
        let results = self.results_view();
        if results.cells.is_empty() && results.sweep.is_none() {
            self.status = "nothing to export yet".to_string();
            return;
        }
        match store::save(&results, None) {
            Ok(path) => self.status = format!("wrote {}", path.display()),
            Err(e) => self.status = format!("export failed: {e:#}"),
        }
        self.dirty = true;
    }

    fn selected_cell(&self) -> Option<CellResult> {
        let rows = self.rows();
        let level = *rows.get(self.row)?;
        let columns = [Column::Read, Column::Write, Column::Copy, Column::Latency];
        let column = *columns.get(self.col)?;
        let view = RunResults {
            cells: self.cells.clone(),
            ..self.results_view()
        };
        view.grid_cell(level, column).cloned()
    }
}

/// Open the TUI and return when the user quits.
pub fn run(settings: &Settings) -> Result<()> {
    let mut app = App::new(settings.clone())?;
    let mut terminal = terminal()?;
    let result = pump(&mut terminal, &mut app, false);
    // The terminal goes back to the user whatever happened above.
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
    let _ = disable_raw_mode();
    result
}

/// Open the TUI, start measuring immediately, and hand the finished document
/// back to the CLI so it can print the plain table and write `--json`.
pub fn run_and_measure(settings: &Settings) -> Result<Option<RunResults>> {
    let mut app = App::new(settings.clone())?;
    let mut terminal = terminal()?;
    let result = pump(&mut terminal, &mut app, true);
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
    let _ = disable_raw_mode();
    result.map(|_| app.last_results.clone())
}

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Raw mode on, alternate screen on. Every caller restores the terminal on the
/// way out, including the error paths.
fn terminal() -> Result<Term> {
    enable_raw_mode().context("could not put the terminal in raw mode")?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen).context("could not enter the alternate screen")?;
    Terminal::new(CrosstermBackend::new(out)).context("could not create the terminal")
}

/// The single loop: block on the channel, act, redraw only when something
/// changed. Nothing here runs on a timer while a measurement is in flight.
fn pump(terminal: &mut Term, app: &mut App, start_now: bool) -> Result<()> {
    let (tx, rx): (Sender<Msg>, Receiver<Msg>) = mpsc::channel();

    // One thread turns key presses into messages, which is what lets the UI
    // thread sleep on one channel instead of polling two sources.
    let keys = tx.clone();
    let key_thread = thread::spawn(move || loop {
        match event::read() {
            Ok(TermEvent::Key(key)) => {
                let quit =
                    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
                if keys.send(Msg::Key(key)).is_err() {
                    break;
                }
                if quit {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    });

    if start_now {
        app.start(tx.clone());
    }
    render_once(terminal, app)?;

    loop {
        let message = rx.recv().context("the event channel closed")?;
        match message {
            Msg::Progress(other) => app.handle_progress(*other),
            Msg::Idle => {
                app.running = false;
                app.dirty = true;
            }
            Msg::Key(key) if key.kind == KeyEventKind::Press => {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc if !app.running => break,
                    KeyCode::Char('q') => {
                        app.stop();
                    }
                    KeyCode::Esc if app.view != View::Grid => app.view = View::Grid,
                    KeyCode::Esc => app.stop(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.stop();
                    }
                    KeyCode::Char('s') | KeyCode::Char('r') if !app.running => {
                        app.view = View::Grid;
                        app.start(tx.clone());
                    }
                    KeyCode::Char('l') => {
                        if app.sweep.is_none() && !app.running {
                            let mut settings = app.settings.clone();
                            settings.levels = vec![];
                            settings.ops = vec![Op::Latency];
                            settings.with_sweep = true;
                            app.settings = settings;
                            app.start(tx.clone());
                        }
                        app.view = View::Sweep;
                    }
                    KeyCode::Char('d') => {
                        app.view = if app.view == View::Details {
                            View::Grid
                        } else {
                            View::Details
                        };
                    }
                    KeyCode::Char('e') => app.export(),
                    KeyCode::Char('?') => {
                        app.view = if app.view == View::Help {
                            View::Grid
                        } else {
                            View::Help
                        };
                    }
                    KeyCode::Up | KeyCode::Char('k') => app.row = app.row.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => {
                        app.row = (app.row + 1).min(app.rows().len().saturating_sub(1));
                    }
                    KeyCode::Left => app.col = app.col.saturating_sub(1),
                    KeyCode::Right => app.col = (app.col + 1).min(3),
                    KeyCode::Tab => {
                        app.col = (app.col + 1) % 4;
                        if app.col == 0 {
                            app.row = (app.row + 1) % app.rows().len().max(1);
                        }
                    }
                    KeyCode::Char(c @ '1'..='4') if app.view == View::Grid => {
                        app.col = c as usize - '1' as usize;
                    }
                    _ => {}
                }
                app.dirty = true;
            }
            Msg::Key(_) => {}
        }
        if app.dirty {
            render_once(terminal, app)?;
        }
    }
    // The key thread is blocked in `event::read()` and cannot be joined; it
    // exits with the process. Nothing is written to the terminal after this
    // point except by the caller, which restores the screen first.
    drop(key_thread);
    Ok(())
}

fn render_once(terminal: &mut Term, app: &mut App) -> Result<()> {
    terminal.draw(|frame| draw(frame, app))?;
    app.dirty = false;
    Ok(())
}

fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    // Two lines are always reserved at the bottom: the progress bar and the key
    // hints. Everything above them belongs to the current view.
    let [body, progress, keys] = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    match app.view {
        View::Grid => {
            let height = grid_height(app).min(body.height.saturating_sub(6).max(6));
            let [grid, system] =
                Layout::vertical([Constraint::Length(height), Constraint::Min(4)]).areas(body);
            draw_grid(frame, app, grid);
            draw_system(frame, app, system);
        }
        View::Details => draw_details(frame, app, body),
        View::Sweep => draw_sweep(frame, app, body),
        View::Help => draw_help(frame, app, body),
    }
    draw_progress(frame, app, progress);
    draw_keys(frame, app, keys);
}

fn grid_height(app: &App) -> u16 {
    // title + header + one row per level + border
    3 + app.rows().len() as u16
}

fn draw_grid(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::bordered()
        .title(" membench ")
        .title_style(Style::default().add_modifier(Modifier::BOLD));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 2 || inner.width < 20 {
        return;
    }

    let columns = [Column::Read, Column::Write, Column::Copy, Column::Latency];
    let rows = app.rows();
    let label_width = rows
        .iter()
        .map(|l| l.grid_label().len())
        .max()
        .unwrap_or(6)
        .max(6) as u16;
    // Three separators between four columns, or the rightmost figure gets
    // chopped by one cell at certain widths.
    let cell_width = inner
        .width
        .saturating_sub(label_width + 1 + columns.len() as u16)
        / columns.len() as u16;

    // Header row.
    let mut header = Line::from(Span::styled(
        format!("{:<w$}", "", w = (label_width + 1) as usize),
        Style::default(),
    ));
    for column in columns {
        // `push_span` takes `&mut self` in this version, hence the statement
        // form rather than a chain.
        header.push_span(Span::styled(
            format!("{:>w$} ", column.to_string(), w = cell_width as usize),
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Cyan),
        ));
    }
    frame.render_widget(
        Paragraph::new(header),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    for (row_index, level) in rows.iter().enumerate() {
        let y = inner.y + 1 + row_index as u16;
        if y >= inner.y + inner.height {
            break;
        }
        let selected = row_index == app.row;
        let mut line = Line::from(Span::styled(
            format!(
                "{:<w$} ",
                level.grid_label(),
                w = (label_width + 1) as usize
            ),
            if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            },
        ));
        for (col_index, column) in columns.iter().enumerate() {
            let measuring = app.running
                && app.cell_label.starts_with(&level.short().to_string())
                && app.cell_label.ends_with(column_op_label(*column).as_str());
            let text = if measuring {
                let (trial, trials) = app.trial;
                format!("{} {trial}/{trials}", spinner(app),)
            } else {
                match app
                    .cells
                    .iter()
                    .find(|c| c.level == *level && c.headline && c.op.column() == *column)
                    .or_else(|| {
                        app.cells
                            .iter()
                            .find(|c| c.level == *level && c.op.column() == *column)
                    }) {
                    Some(cell) => render::fmt_value(cell.value, &cell.unit),
                    None => "—".to_string(),
                }
            };
            let style = if measuring {
                Style::default().fg(Color::Yellow)
            } else if selected && col_index == app.col {
                Style::default().bg(Color::DarkGray)
            } else if text == "—" {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            let _ = col_index;
            line.push_span(Span::styled(
                format!("{:>w$} ", text, w = cell_width as usize),
                style,
            ));
        }
        frame.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
    }
}

fn column_op_label(column: Column) -> String {
    match column {
        Column::Read => "read".to_string(),
        Column::Write => "write".to_string(),
        Column::Copy => "copy".to_string(),
        Column::Latency => "latency".to_string(),
    }
}

fn spinner(app: &App) -> &'static str {
    const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    let elapsed = app
        .cell_started
        .map(|s| s.elapsed().as_millis())
        .unwrap_or(0);
    FRAMES[(elapsed / 80 % 8) as usize]
}

fn draw_system(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered().title(" system ");
    let mut lines: Vec<Line> = render::system_panel(&app.system)
        .lines()
        .map(|l| Line::from(l.trim_end().to_string()))
        .collect();
    if let Some(mhz) = app.clock_mhz {
        lines.push(Line::styled(
            format!("  {:<11}{mhz:.0} MHz measured during this run", "Clock"),
            Style::default().fg(Color::Green),
        ));
    }
    for w in &app.warnings {
        lines.push(Line::styled(w.clone(), Style::default().fg(Color::Red)));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        block.inner(area),
    );
    frame.render_widget(block, area);
}

fn draw_progress(frame: &mut Frame, app: &App, area: Rect) {
    if !app.running {
        return;
    }
    let (trial, trials) = app.trial;
    let fraction = if trials > 0 {
        trial as f64 / trials as f64
    } else {
        0.0
    };
    let gauge = Gauge::default()
        .ratio(fraction.clamp(0.0, 1.0))
        .label(format!("{} {trial}/{trials}", app.cell_label))
        .style(Style::default().fg(Color::Yellow));
    frame.render_widget(gauge, area);
}

fn draw_keys(frame: &mut Frame, app: &App, area: Rect) {
    let keys = if app.running {
        "Ctrl-C/Esc stop · d details · e export · q quit once it finishes"
    } else {
        "s start · l sweep · d details · e export · ? keys · q quit"
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {keys}  {}", app.status),
            Style::default().fg(Color::DarkGray),
        ))),
        area,
    );
}

fn draw_details(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered().title(" cell details (d to go back) ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(cell) = app.selected_cell() else {
        frame.render_widget(
            Paragraph::new("no measurement in that cell yet — press s to run"),
            inner,
        );
        return;
    };
    let mut lines = vec![
        Line::from(Span::styled(
            format!("{} {}", cell.level.grid_label(), cell.op.label()),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::default(),
        Line::from(format!(
            "  value    {}",
            render::fmt_value(cell.value, &cell.unit)
        )),
        Line::from(format!(
            "  best     {}",
            render::fmt_value(cell.best, &cell.unit)
        )),
        Line::from(format!(
            "  median   {}   mean {}   stddev {:.3}",
            render::fmt_value(cell.median, &cell.unit),
            render::fmt_value(cell.mean, &cell.unit),
            cell.stddev
        )),
        Line::from(format!(
            "  spread   {:.2}% across {} retained trial(s), {} rejected",
            cell.spread_pct,
            cell.trials.len(),
            cell.rejected_trials
        )),
        Line::from(format!(
            "  pages    asked {}, verified {}{}",
            cell.pages_requested.label(),
            cell.pages_actual.label(),
            cell.huge_pages_bytes
                .map(|h| format!(", {} huge-backed", sysinfo::human_bytes(h)))
                .unwrap_or_default()
        )),
        Line::from(format!("  isa      {}", cell.isa.label())),
        Line::from(format!(
            "  buffer   {} per thread × {} thread(s){}",
            sysinfo::human_bytes(cell.buffer_bytes_per_thread as u64),
            cell.threads,
            if cell.cpus.is_empty() {
                String::new()
            } else {
                format!(" on cpus {}", compact_cpus(&cell.cpus))
            }
        )),
    ];
    // Bytes moved only means something once a pass has been timed.
    if cell.bytes_moved > 0 {
        lines.push(Line::from(format!(
            "  bytes    {} counted per pass ({})",
            cell.bytes_moved,
            if cell.op == Op::Copy || cell.op == Op::CopyNt {
                "source only"
            } else {
                "one pass"
            }
        )));
    }
    if let Some(cycles) = cell.cycles {
        lines.push(Line::from(format!(
            "  latency  {:.2} ns = {cycles:.1} cycles at {:.0} MHz",
            cell.value,
            cell.clock_mhz.unwrap_or(0.0)
        )));
    }
    for note in &cell.notes {
        lines.push(Line::styled(
            format!("  note     {note}"),
            Style::default().fg(Color::Yellow),
        ));
    }
    if !cell.trials.is_empty() {
        let trials = cell
            .trials
            .iter()
            .map(|v| format!("{v:.2}"))
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(Line::from(format!("  trials   {trials}")));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        inner,
    );
}

fn compact_cpus(cpus: &[usize]) -> String {
    if cpus.len() > 12 {
        format!("{} cpus", cpus.len())
    } else {
        cpus.iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn draw_sweep(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered().title(" latency vs block size (log scale) — l or d to go back ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(sweep) = &app.sweep else {
        frame.render_widget(Paragraph::new("no sweep yet — press l to run one"), inner);
        return;
    };
    if sweep.points.len() < 2 {
        frame.render_widget(
            Paragraph::new(format!("{} point(s) so far", sweep.points.len())),
            inner,
        );
        return;
    }
    // x is log2(bytes), so equal distances are equal factors — the plateaus
    // line up with the cache sizes and that is the whole point of the view.
    let points: Vec<(f64, f64)> = sweep
        .points
        .iter()
        .map(|p| ((p.size_bytes as f64).log2(), p.ns))
        .collect();
    let xs: Vec<f64> = points.iter().map(|p| p.0).collect();
    let ys: Vec<f64> = points.iter().map(|p| p.1).collect();
    let x_min = xs.iter().cloned().fold(f64::INFINITY, f64::min);
    let x_max = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let y_max = ys.iter().cloned().fold(0.0f64, f64::max) * 1.15;

    let datasets = vec![Dataset::default()
        .name("ns")
        .marker(symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Cyan))
        .data(&points)];

    let chart = Chart::new(datasets)
        .x_axis(Axis::default().bounds([x_min, x_max]).labels(vec![
            label_for_pow2(x_min),
            label_for_pow2((x_min + x_max) / 2.0),
            label_for_pow2(x_max),
        ]))
        .y_axis(Axis::default().bounds([0.0, y_max]).labels(vec![
            "0".to_string(),
            format!("{:.0}", y_max / 2.0),
            format!("{y_max:.0}"),
        ]));
    frame.render_widget(chart, inner);
}

fn label_for_pow2(x: f64) -> String {
    let bytes = 2f64.powf(x) as u64;
    sysinfo::human_bytes(bytes)
}

fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered().title(" keys ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let text = vec![
        Line::from("s        start the benchmark (r repeats it)"),
        Line::from("l        latency sweep, drawn above as ns vs block size"),
        Line::from("↑↓ / jk  choose a row      ←→ / 1-4 choose a column"),
        Line::from("d        the selected cell in detail"),
        Line::from("e        export JSON to ~/.local/share/membench/"),
        Line::from("Ctrl-C   stop after the current trial (buffers are unmapped)"),
        Line::from("q        quit"),
        Line::default(),
        Line::styled(
            "Every figure carries its own unit: GB/s is 10^9 B/s (SI), ns is nanoseconds",
            Style::default().fg(Color::DarkGray),
        ),
        Line::styled(
            "per dependent load. Copy counts source bytes unless --copy-counts-both.",
            Style::default().fg(Color::DarkGray),
        ),
        Line::styled(
            format!(
                "membench {} · nothing here is a theoretical peak",
                env!("CARGO_PKG_VERSION")
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    let _ = app;
    frame.render_widget(
        Paragraph::new(Text::from(text)).wrap(Wrap { trim: false }),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use membench_core::result::{CellResult, Isa, Level, Op, PageKind};
    use ratatui::backend::TestBackend;

    /// An `App` that does not read this machine, so the assertions below are
    /// about layout rather than about the host.
    fn fixture() -> App {
        let mut system = SystemInfo::default();
        system.cpu.model = Some("Test Processor 9000".to_string());
        system.topology.logical_cpus = 8;
        system.topology.physical_cores = 4;
        system.memory.total_bytes = Some(32 * 1024 * 1024 * 1024);
        system.os.membench_version = "test".to_string();
        let mut cells = Vec::new();
        for (level, op, value, unit) in [
            (Level::Memory, Op::Read, 51.0, "GB/s"),
            (Level::Memory, Op::WriteNt, 47.5, "GB/s"),
            (Level::Memory, Op::CopyNt, 24.0, "GB/s"),
            (Level::Memory, Op::Latency, 92.5, "ns"),
            (Level::L1, Op::Read, 1900.0, "GB/s"),
        ] {
            let mut cell = CellResult::new(level, op, value, unit);
            cell.headline = true;
            cell.isa = Isa::Avx2;
            cell.pages_requested = PageKind::TwoM;
            cell.pages_actual = PageKind::FourK;
            cell.trials = vec![value, value * 0.97];
            cell.threads = 4;
            cell.buffer_bytes_per_thread = 1 << 24;
            cells.push(cell);
        }
        App {
            settings: Settings::default(),
            system,
            cells,
            sweep: None,
            warnings: vec!["[huge-pages] 2M pages were requested but 4K pages are in use".into()],
            clock_mhz: Some(4350.0),
            running: false,
            cancel: Arc::new(AtomicBool::new(false)),
            view: View::Grid,
            row: 0,
            col: 0,
            status: String::from("idle"),
            started: None,
            cell_started: None,
            cell_label: String::new(),
            trial: (0, 0),
            last_results: None,
            dirty: true,
        }
    }

    /// Render into a `TestBackend` and flatten the buffer back into text, so
    /// the assertions read like what a person sees.
    fn render(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        let cells = terminal.backend().buffer().content();
        let w = width as usize;
        cells
            .chunks(w)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_grid_draws_units_and_the_system_panel_together() {
        let mut app = fixture();
        let screen = render(&mut app, 110, 30);
        assert!(screen.contains("membench"), "{screen}");
        assert!(screen.contains("Read"), "{screen}");
        assert!(screen.contains("51.0 GB/s"), "{screen}");
        assert!(screen.contains("47.5 GB/s"), "{screen}");
        assert!(screen.contains("92.5 ns"), "{screen}");
        // Above 1000 GB/s the label changes scale, and the label is the point.
        assert!(screen.contains("1.90 TB/s"), "{screen}");
        assert!(screen.contains("Test Processor 9000"), "{screen}");
        assert!(screen.contains("huge-pages"), "{screen}");
        assert!(screen.contains("4350 MHz"), "{screen}");
    }

    #[test]
    fn a_cell_that_never_ran_stays_a_dash() {
        let mut app = fixture();
        app.settings.levels = vec![Level::L3];
        let screen = render(&mut app, 110, 30);
        assert!(screen.contains("L3 Cache"), "{screen}");
        assert!(screen.contains('—'), "{screen}");
        assert!(!screen.contains("0.0 GB/s"), "{screen}");
    }

    #[test]
    fn the_sweep_view_survives_a_short_ladder() {
        let mut app = fixture();
        app.view = View::Sweep;
        let screen = render(&mut app, 100, 26);
        assert!(screen.contains("no sweep yet"), "{screen}");
        app.sweep = Some(Sweep {
            points: (0..12)
                .map(|i| membench_core::result::SweepPoint {
                    size_bytes: 4096usize << (i * 2),
                    nodes: 4096usize << ((i * 2) / 64),
                    ns: 4.0 + i as f64 * 3.0,
                    cycles: Some(17.0 + i as f64 * 13.0),
                    trials: 3,
                    best_ns: 4.0 + i as f64 * 3.0,
                    spread_pct: 1.0,
                    pages_actual: PageKind::TwoM,
                    huge_pages_bytes: None,
                })
                .collect(),
            notes: vec![],
            duration_ms: 60,
            trials: 3,
        });
        let screen = render(&mut app, 100, 26);
        // The chart is drawn with braille cells; what matters is that the axes
        // carry the sizes, so the plateaus can be read against the caches.
        assert!(screen.contains("latency vs block size"), "{screen}");
        assert!(screen.contains("4K") || screen.contains("16K"), "{screen}");
    }

    #[test]
    fn details_show_the_selected_cell_with_every_convention() {
        let mut app = fixture();
        app.view = View::Details;
        app.row = 0;
        app.col = 0;
        let screen = render(&mut app, 110, 30);
        assert!(screen.contains("Memory Read"), "{screen}");
        assert!(screen.contains("median"), "{screen}");
        assert!(screen.contains("asked 2M, verified 4K"), "{screen}");
        assert!(screen.contains("AVX2"), "{screen}");
    }

    #[test]
    fn a_narrow_terminal_does_not_panic() {
        // The TUI gets resized by the window manager constantly.
        let mut app = fixture();
        for (w, h) in [(20, 8), (40, 12), (60, 16), (120, 20)] {
            let _ = render(&mut app, w, h);
        }
    }
}
