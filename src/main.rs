//! Terminal/control thread. All stream lifecycle work stays outside audio callbacks.

mod audio;
mod meters;
mod safety;
mod state;

use anyhow::{Context, Result};
use audio::{DeviceEntry, Engine};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use std::{
    io::{self, IsTerminal},
    sync::atomic::Ordering::Relaxed,
    time::Duration,
};

struct App {
    inputs: Vec<DeviceEntry>,
    outputs: Vec<DeviceEntry>,
    input: Option<usize>,
    output: Option<usize>,
    saved: state::State,
    focus_output: bool,
    buffer: u32,
    // Owning an engine is the running state. Dropping it explicitly pauses audio.
    engine: Option<Engine>,
    confirming: bool,
    status: String,
    meters: meters::Panel,
}
impl App {
    fn new(saved: state::State) -> Result<Self> {
        let (inputs, outputs) = audio::devices()?;
        let mut app = Self {
            inputs,
            outputs,
            input: None,
            output: None,
            saved,
            focus_output: false,
            buffer: 128,
            engine: None,
            confirming: false,
            status: "Stopped. Select devices, then press Enter.".into(),
            meters: meters::Panel::default(),
        };
        app.restore_selection();
        Ok(app)
    }
    fn restore_selection(&mut self) {
        self.input = state::selection(
            self.saved.input.as_deref(),
            self.inputs.iter().map(|entry| entry.name.as_str()),
        );
        self.output = state::selection(
            self.saved.output.as_deref(),
            self.outputs.iter().map(|entry| entry.name.as_str()),
        );
        self.buffer = self.saved.buffer;
        let missing: Vec<_> = [
            ("input", &self.saved.input, self.input),
            ("output", &self.saved.output, self.output),
        ]
        .into_iter()
        .filter_map(|(kind, name, selected)| {
            name.as_ref()
                .filter(|_| selected.is_none())
                .map(|name| format!("{kind} '{name}'"))
        })
        .collect();
        if !missing.is_empty() {
            self.status = format!(
                "Saved {} unavailable or ambiguous. Select devices or press r to refresh.",
                missing.join(" and ")
            );
        }
    }
    fn current_state(&self) -> state::State {
        state::State {
            // Keep unavailable selections so reconnecting later can restore them.
            input: self
                .input
                .and_then(|i| self.inputs.get(i))
                .map(|entry| entry.name.clone())
                .or_else(|| self.saved.input.clone()),
            output: self
                .output
                .and_then(|i| self.outputs.get(i))
                .map(|entry| entry.name.clone())
                .or_else(|| self.saved.output.clone()),
            buffer: self.buffer,
        }
    }
    fn risk(&self) -> bool {
        self.input
            .and_then(|i| self.inputs.get(i))
            .zip(self.output.and_then(|i| self.outputs.get(i)))
            .is_some_and(|(i, o)| safety::feedback_risk(&i.name, &o.name))
    }
    fn start(&mut self) {
        self.confirming = false;
        let Some((input, output)) = self
            .input
            .and_then(|i| self.inputs.get(i))
            .zip(self.output.and_then(|i| self.outputs.get(i)))
        else {
            self.status = "An input and output device are required. Press r to refresh.".into();
            return;
        };
        match Engine::start(&input.device, &output.device, self.buffer) {
            Ok(engine) => {
                self.meters = meters::Panel::default();
                self.engine = Some(engine);
                self.status = "Routing. Space: stop / m: mute".into();
            }
            Err(error) => {
                self.status = format!(
                    "Cannot start: {error:#}. Check device settings and microphone permission."
                );
            }
        }
    }
    fn draw(&mut self, frame: &mut Frame) {
        let areas = Layout::vertical([
            Constraint::Length(3),
            Constraint::Percentage(40),
            Constraint::Min(5),
            Constraint::Length(7),
            Constraint::Length(5),
        ])
        .split(frame.area());
        frame.render_widget(
            Paragraph::new(format!(
                "Audio Monitor | buffer request: {} frames | {}",
                self.buffer,
                if self.engine.is_some() {
                    "RUNNING"
                } else {
                    "STOPPED"
                }
            ))
            .block(Block::default().borders(Borders::ALL)),
            areas[0],
        );
        let columns = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(areas[1]);
        for (index, (entries, selected, title)) in [
            (&self.inputs, self.input, "Input"),
            (&self.outputs, self.output, "Output"),
        ]
        .into_iter()
        .enumerate()
        {
            let lines = if entries.is_empty() {
                "No devices".into()
            } else {
                entries
                    .iter()
                    .enumerate()
                    .map(|(i, entry)| {
                        format!(
                            "{} {}",
                            if Some(i) == selected { ">" } else { " " },
                            entry.name
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let scroll = selected
                .unwrap_or(0)
                .saturating_sub(columns[index].height.saturating_sub(3) as usize);
            frame.render_widget(
                Paragraph::new(lines)
                    .scroll((scroll.min(u16::MAX as usize) as u16, 0))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(title)
                            .border_style(Style::default().fg(
                                if self.focus_output == (index == 1) {
                                    Color::Cyan
                                } else {
                                    Color::White
                                },
                            )),
                    ),
                columns[index],
            );
        }
        self.meters.draw(
            frame,
            areas[2],
            self.engine.as_ref().map(|engine| engine.stats.as_ref()),
        );
        let details = if let Some(engine) = &self.engine {
            let s = &engine.stats;
            format!("{}\nObserved callbacks: input {} / output {} frames\nQueue: {:.2} ms (not total latency) | underruns: {} | dropped input frames: {}\nMuted: {} | mono input duplicates to outputs; otherwise channels map by index.",
                engine.description, s.input_callback.load(Relaxed), s.output_callback.load(Relaxed),
                s.queued.load(Relaxed) as f64 * 1000.0 / engine.input_rate as f64,
                s.underruns.load(Relaxed), s.overflows.load(Relaxed), s.muted.load(Relaxed))
        } else {
            "48 kHz preferred; supported formats/rates selected automatically.\nChannel mapping: mono → all outputs; otherwise channel N → channel N.\nUnmatched output channels are silent; excess input channels are discarded.".into()
        };
        frame.render_widget(
            Paragraph::new(details).block(Block::default().borders(Borders::ALL).title("Audio")),
            areas[3],
        );
        let warning = if self.confirming {
            "WARNING: microphone → speakers may cause acoustic feedback. Press y to connect, n/Esc to cancel."
        } else if self.risk() {
            "WARNING: microphone → speakers may cause acoustic feedback (inferred from device names)."
        } else {
            "Device-name detection cannot identify every feedback-prone route."
        };
        frame.render_widget(Paragraph::new(format!("{warning}\n{}\nTab: input/output  ↑↓: select  Enter: start  Space: stop  m: mute  b: buffer  r: refresh  q: quit", self.status))
            .style(Style::default().fg(if self.risk() { Color::Yellow } else { Color::White }))
            .block(Block::default().borders(Borders::ALL)), areas[4]);
    }
    fn key(&mut self, code: KeyCode) -> Result<bool> {
        if code == KeyCode::Char('q') {
            return Ok(false);
        }
        if self.confirming {
            // Keep the displayed device pair fixed while approval is pending.
            // Only confirmation/cancellation and quitting are accepted here.
            match code {
                KeyCode::Char('y') => self.start(),
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.confirming = false;
                    self.status = "Connection cancelled.".into();
                }
                _ => {}
            }
            return Ok(true);
        }
        match code {
            KeyCode::Char(' ') | KeyCode::Esc => {
                self.engine = None;
                self.status = "Stopped.".into();
            }
            KeyCode::Char('m') => {
                if let Some(e) = &self.engine {
                    e.stats.muted.fetch_xor(true, Relaxed);
                }
            }
            KeyCode::Enter if self.engine.is_none() => {
                if self.risk() {
                    self.confirming = true;
                } else {
                    self.start();
                }
            }
            KeyCode::Tab => self.focus_output = !self.focus_output,
            KeyCode::Up | KeyCode::Down if self.engine.is_none() => {
                let (selected, len) = if self.focus_output {
                    (&mut self.output, self.outputs.len())
                } else {
                    (&mut self.input, self.inputs.len())
                };
                if len > 0 {
                    *selected = Some(match *selected {
                        Some(index) if code == KeyCode::Up => (index + len - 1) % len,
                        Some(index) => (index + 1) % len,
                        None if code == KeyCode::Up => len - 1,
                        None => 0,
                    });
                }
            }
            KeyCode::Char('b') if self.engine.is_none() => {
                self.buffer = match self.buffer {
                    64 => 128,
                    128 => 256,
                    256 => 512,
                    _ => 64,
                };
            }
            KeyCode::Char('r') if self.engine.is_none() => match audio::devices() {
                Ok((inputs, outputs)) => {
                    self.saved = self.current_state();
                    self.inputs = inputs;
                    self.outputs = outputs;
                    self.status = "Devices refreshed.".into();
                    self.restore_selection();
                }
                Err(e) => self.status = format!("Device refresh failed: {e:#}"),
            },
            _ => {}
        }
        Ok(true)
    }
}

fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!("audio-monitor\nInteractive audio routing. No audio starts until Enter is pressed.\nRun in a terminal. See README.md for controls and macOS permissions.");
        return Ok(());
    }
    anyhow::ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "Run in an interactive terminal"
    );
    let (state_path, saved, warning) = match state::path() {
        Ok(path) => {
            let (saved, warning) = match state::load(&path) {
                Ok(saved) => (saved, None),
                Err(error) => (
                    state::State::default(),
                    Some(format!(
                        "Cannot restore settings: {error:#}. Using defaults."
                    )),
                ),
            };
            (Some(path), saved, warning)
        }
        Err(error) => (
            None,
            state::State::default(),
            Some(format!("Settings persistence disabled: {error:#}")),
        ),
    };
    let mut app = App::new(saved).context("Cannot enumerate audio devices")?;
    if let Some(warning) = warning {
        app.status = format!("{warning} {}", app.status);
    }
    let result = run(&mut app);
    if let Some(path) = state_path {
        if let Err(error) = state::save(&path, &app.current_state()) {
            eprintln!("Cannot save settings to {}: {error:#}", path.display());
        }
    }
    result
}

fn run(app: &mut App) -> Result<()> {
    let mut terminal = ratatui::try_init()?;
    // Ratatui's panic hook restores the terminal; this guard handles normal errors.
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            ratatui::restore();
        }
    }
    let _restore = Restore;
    let result = (|| -> Result<()> {
        loop {
            if app
                .engine
                .as_ref()
                .is_some_and(|e| e.stats.failed.load(Relaxed))
            {
                // Error callbacks only set a flag; stopping and reporting belong on
                // this thread. Never silently reroute to a new default device.
                app.engine = None;
                app.status = "Audio stream failed. Check connections/permissions, refresh and select devices again.".into();
            }
            terminal.draw(|frame| app.draw(frame))?;
            if event::poll(Duration::from_millis(40))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        break;
                    }
                    if !app.key(key.code)? {
                        break;
                    }
                }
            }
        }
        Ok(())
    })();
    // Stop callbacks before restoring the terminal.
    app.engine = None;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_devices_remain_saved_and_cannot_start_audio() -> Result<()> {
        let saved = state::State {
            input: Some("Disconnected microphone".into()),
            output: Some("Disconnected headphones".into()),
            buffer: 256,
        };
        let mut app = App {
            inputs: Vec::new(),
            outputs: Vec::new(),
            input: None,
            output: None,
            saved: saved.clone(),
            focus_output: false,
            buffer: 128,
            engine: None,
            confirming: false,
            status: String::new(),
            meters: meters::Panel::default(),
        };
        app.restore_selection();
        assert_eq!(app.current_state(), saved);
        assert!(app.status.contains("unavailable or ambiguous"));
        assert!(app.engine.is_none());
        app.key(KeyCode::Enter)?;
        assert!(app.engine.is_none());
        assert!(app.status.contains("required"));
        app.key(KeyCode::Down)?;
        app.key(KeyCode::Char('b'))?;
        let mut expected = saved;
        expected.buffer = 512;
        assert_eq!(app.current_state(), expected);
        assert!(!app.key(KeyCode::Char('q'))?);
        Ok(())
    }
}
