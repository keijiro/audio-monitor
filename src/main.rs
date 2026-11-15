//! Terminal/control thread. All stream lifecycle work stays outside audio callbacks.

mod audio;
mod safety;

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
    input: usize,
    output: usize,
    focus_output: bool,
    buffer: u32,
    // Owning an engine is the running state. Dropping it explicitly pauses audio.
    engine: Option<Engine>,
    confirming: bool,
    status: String,
}
impl App {
    fn new() -> Result<Self> {
        let (inputs, outputs) = audio::devices()?;
        Ok(Self {
            inputs,
            outputs,
            input: 0,
            output: 0,
            focus_output: false,
            buffer: 128,
            engine: None,
            confirming: false,
            status: "Stopped. Select devices, then press Enter.".into(),
        })
    }
    fn risk(&self) -> bool {
        self.inputs
            .get(self.input)
            .zip(self.outputs.get(self.output))
            .is_some_and(|(i, o)| safety::feedback_risk(&i.name, &o.name))
    }
    fn start(&mut self) {
        self.confirming = false;
        let Some((input, output)) = self
            .inputs
            .get(self.input)
            .zip(self.outputs.get(self.output))
        else {
            self.status = "An input and output device are required. Press r to refresh.".into();
            return;
        };
        match Engine::start(&input.device, &output.device, self.buffer) {
            Ok(engine) => {
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
    fn draw(&self, frame: &mut Frame) {
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
                        format!("{} {}", if i == selected { ">" } else { " " }, entry.name)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let scroll = selected.saturating_sub(columns[index].height.saturating_sub(3) as usize);
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
        let mut meter_lines = Vec::new();
        if let Some(engine) = &self.engine {
            for (ch, meter) in engine.stats.meters.iter().enumerate() {
                // Rendering consumes the peak hold. A concurrent input update
                // appears either in this frame or the next, without blocking it.
                let peak = f32::from_bits(meter.peak.swap(0, Relaxed));
                let rms = f32::from_bits(meter.rms.load(Relaxed));
                let db = |x: f32| 20.0 * x.max(0.000001).log10();
                let bars = (((db(peak) + 60.0) / 60.0).clamp(0.0, 1.0) * 24.0) as usize;
                meter_lines.push(format!(
                    "CH {:2} [{:<24}] peak {:6.1} dBFS / RMS {:6.1} {}",
                    ch + 1,
                    "#".repeat(bars),
                    db(peak),
                    db(rms),
                    if peak >= 1.0 { "CLIP" } else { "" }
                ));
            }
        } else {
            meter_lines.push("Input meters appear while routing.".into());
        }
        frame.render_widget(
            Paragraph::new(meter_lines.join("\n"))
                .block(Block::default().borders(Borders::ALL).title("Input levels")),
            areas[2],
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
                    *selected = if code == KeyCode::Up {
                        (*selected + len - 1) % len
                    } else {
                        (*selected + 1) % len
                    };
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
                    // Enumeration order is not a stable identifier. Reset both
                    // selections rather than reuse indices for different devices.
                    self.inputs = inputs;
                    self.outputs = outputs;
                    self.input = 0;
                    self.output = 0;
                    self.status = "Devices refreshed.".into();
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
    let mut app = App::new().context("Cannot enumerate audio devices")?;
    let mut terminal = ratatui::try_init()?;
    // Ratatui's panic hook restores the terminal; this guard handles normal errors.
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            ratatui::restore();
        }
    }
    let _restore = Restore;
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
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    break;
                }
                if !app.key(key.code)? {
                    break;
                }
            }
        }
    }
    // Stop callbacks before restoring the terminal.
    drop(app);
    Ok(())
}
