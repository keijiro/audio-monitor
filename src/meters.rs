//! Display ballistics live on the UI thread, independent of audio callbacks.

use crate::audio::Stats;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph, Widget},
    Frame,
};
use std::{sync::atomic::Ordering::Relaxed, time::Instant};

const FLOOR: f32 = -120.0;
const HOLD_SECONDS: f32 = 1.0;

#[derive(Default)]
pub struct Panel {
    channels: Vec<Channel>,
    updated: Option<Instant>,
}

struct Channel {
    level: f32,
    rms: f32,
    peak: f32,
    hold: f32,
    clip: f32,
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            level: FLOOR,
            rms: FLOOR,
            peak: FLOOR,
            hold: 0.0,
            clip: 0.0,
        }
    }
}

fn db(amplitude: f32) -> f32 {
    20.0 * amplitude.max(0.000001).log10()
}

impl Channel {
    fn update(&mut self, peak: f32, rms: f32, elapsed: f32) {
        let peak_db = db(peak);
        self.level = peak_db.max(self.level - 36.0 * elapsed);
        self.rms = db(rms);
        // Only the part of this interval after the hold expires contributes
        // to the falloff, including after a delayed terminal refresh.
        let decay = (elapsed - self.hold).max(0.0);
        self.hold = (self.hold - elapsed).max(0.0);
        self.peak = (self.peak - 18.0 * decay).max(FLOOR);
        if peak_db >= self.peak {
            self.peak = peak_db;
            self.hold = HOLD_SECONDS;
        }
        self.clip = if peak >= 1.0 {
            HOLD_SECONDS
        } else {
            (self.clip - elapsed).max(0.0)
        };
    }
}

impl Panel {
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, stats: Option<&Stats>) {
        let now = Instant::now();
        let elapsed = self
            .updated
            .replace(now)
            .map_or(0.0, |last| now.duration_since(last).as_secs_f32());
        if let Some(stats) = stats {
            self.channels
                .resize_with(stats.meters.len(), Channel::default);
            for (channel, meter) in self.channels.iter_mut().zip(&stats.meters) {
                // Consume every channel even when the panel cannot fit all rows.
                // Concurrent input updates appear in this frame or the next.
                channel.update(
                    f32::from_bits(meter.peak.swap(0, Relaxed)),
                    f32::from_bits(meter.rms.load(Relaxed)),
                    elapsed,
                );
            }
        } else {
            self.channels.clear();
        }
        frame.render_widget(&*self, area);
    }
}

impl Widget for &Panel {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray))
            .title_top(Line::from(vec![
                Span::styled(" Input levels ", Style::default().fg(Color::Cyan)),
                Span::styled(" dBFS ", Style::default().fg(Color::DarkGray)),
            ]));
        let block = if area.width >= 56 && !self.channels.is_empty() {
            block.title_bottom(Line::from(vec![
                Span::styled(" █ RMS ", Style::default().fg(Color::Green)),
                Span::styled("░ peak ", Style::default().fg(Color::DarkGray)),
                Span::styled("│ hold 1s ", Style::default().fg(Color::White)),
            ]))
        } else {
            block
        };
        let mut inner = block.inner(area);
        block.render(area, buf);
        if inner.width > 2 {
            inner.x += 1;
            inner.width -= 2;
        }
        if inner.is_empty() {
            return;
        }
        if self.channels.is_empty() {
            Paragraph::new("Select devices and start routing to see input levels.")
                .style(Style::default().fg(Color::DarkGray))
                .render(inner, buf);
            return;
        }

        let label_width = 6.min(inner.width);
        let info_width = if inner.width >= 60 {
            22
        } else if inner.width >= 24 {
            6
        } else {
            0
        };
        let bar_width = inner.width.saturating_sub(label_width + info_width);
        let bar_x = inner.x + label_width;
        // Leave a row for the scale only when channel data still fits.
        let scale_height = u16::from(inner.height > 1);
        if scale_height > 0 && bar_width >= 10 {
            let ticks: &[i32] = if bar_width >= 42 {
                &[-60, -48, -36, -24, -12, 0]
            } else if bar_width >= 24 {
                &[-60, -30, -12, 0]
            } else {
                &[-60, 0]
            };
            for tick in ticks {
                let label = tick.to_string();
                let position = position(*tick as f32, bar_width);
                let offset = position
                    .saturating_sub(label.len() as u16 / 2)
                    .min(bar_width.saturating_sub(label.len() as u16));
                buf.set_stringn(
                    bar_x + offset,
                    inner.y,
                    label,
                    (bar_width - offset) as usize,
                    Style::default().fg(Color::DarkGray),
                );
            }
            if info_width == 22 {
                buf.set_stringn(
                    bar_x + bar_width,
                    inner.y,
                    "    PEAK     RMS",
                    info_width as usize,
                    Style::default().fg(Color::DarkGray),
                );
            }
        }

        let rows = inner.height.saturating_sub(scale_height) as usize;
        let hidden = self.channels.len().saturating_sub(rows);
        let visible = if hidden > 0 && rows > 1 {
            rows - 1
        } else {
            rows
        };
        for (index, channel) in self.channels.iter().take(visible).enumerate() {
            let y = inner.y + scale_height + index as u16;
            buf.set_stringn(
                inner.x,
                y,
                format!("CH {:02} ", index + 1),
                label_width as usize,
                Style::default().fg(Color::Cyan),
            );
            for offset in 0..bar_width {
                // Each LED occupies one cell followed by an unlit spacer.
                if offset % 2 != 0 {
                    continue;
                }
                let threshold = -60.0 + 60.0 * (offset + 1) as f32 / bar_width as f32;
                let color = if threshold > -6.0 {
                    Color::Red
                } else if threshold > -18.0 {
                    Color::Yellow
                } else {
                    Color::Green
                };
                let (symbol, color) = if channel.rms >= threshold {
                    ("█", color)
                } else if channel.level >= threshold {
                    ("░", color)
                } else {
                    ("▁", Color::DarkGray)
                };
                buf[(bar_x + offset, y)].set_symbol(symbol).set_fg(color);
            }
            if bar_width > 0 && channel.peak > -60.0 {
                let offset = position(channel.peak, bar_width) / 2 * 2;
                buf[(bar_x + offset, y)]
                    .set_symbol("│")
                    .set_fg(if channel.clip > 0.0 {
                        Color::Red
                    } else {
                        Color::White
                    });
            }
            let info_x = bar_x + bar_width;
            if info_width == 22 {
                buf.set_stringn(
                    info_x,
                    y,
                    format!(
                        "  {:>6}  {:>6}",
                        reading(channel.peak),
                        reading(channel.rms)
                    ),
                    info_width as usize,
                    Style::default().fg(Color::White),
                );
            }
            if info_width > 0 && channel.clip > 0.0 {
                buf.set_stringn(
                    info_x + info_width - 4,
                    y,
                    "CLIP",
                    4,
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                );
            }
        }
        if hidden > 0 && rows > 1 {
            buf.set_stringn(
                inner.x,
                inner.y + inner.height - 1,
                format!("+ {} more channels", self.channels.len() - visible),
                inner.width as usize,
                Style::default().fg(Color::DarkGray),
            );
        }
    }
}

fn position(db: f32, width: u16) -> u16 {
    (((db + 60.0) / 60.0).clamp(0.0, 1.0) * width.saturating_sub(1) as f32).round() as u16
}

fn reading(db: f32) -> String {
    if db <= FLOOR {
        "−∞".into()
    } else {
        format!("{db:.1}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::Meter;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn peak_and_clip_holds_expire_using_elapsed_time() {
        let mut channel = Channel::default();
        channel.update(1.0, 0.1, 0.0);
        channel.update(0.0, 0.0, 0.75);
        assert_eq!(channel.peak, 0.0);
        assert!(channel.clip > 0.0);
        assert_eq!(channel.rms, FLOOR);
        assert_eq!(channel.level, -27.0);
        channel.update(0.0, 0.0, 0.5);
        assert_eq!(channel.clip, 0.0);
        assert_eq!(channel.peak, -4.5);
        channel.update(0.8, 0.1, 0.04);
        assert_eq!(channel.peak, db(0.8));
        assert_eq!(channel.hold, HOLD_SECONDS);
    }

    #[test]
    fn rendering_fits_small_panels_and_consumes_hidden_peaks() -> std::io::Result<()> {
        let stats = Stats {
            meters: (0..8).map(|_| Meter::default()).collect(),
            ..Stats::default()
        };
        let mut panel = Panel::default();
        for width in [0, 1, 4, 12, 32, 64, 100] {
            for height in [0, 1, 2, 3, 5, 12] {
                for meter in &stats.meters {
                    meter.peak.store(0.5f32.to_bits(), Relaxed);
                }
                let mut terminal = Terminal::new(TestBackend::new(width, height))?;
                terminal.draw(|frame| panel.draw(frame, frame.area(), Some(&stats)))?;
                assert!(stats
                    .meters
                    .iter()
                    .all(|meter| meter.peak.load(Relaxed) == 0));
            }
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 5))?;
        terminal.draw(|frame| panel.draw(frame, frame.area(), None))?;
        assert!(panel.channels.is_empty());
        Ok(())
    }

    #[test]
    fn meter_shows_scale_readings_clip_and_level_colors() {
        let mut clipped = Channel::default();
        clipped.update(1.0, 0.5, 0.0);
        let panel = Panel {
            channels: vec![clipped, Channel::default()],
            ..Panel::default()
        };
        let area = Rect::new(0, 0, 100, 5);
        let mut buffer = Buffer::empty(area);
        (&panel).render(area, &mut buffer);
        let lines: Vec<String> = (0..area.height)
            .map(|y| (0..area.width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        assert!(lines[1].contains("-60"));
        assert!(lines[1].contains("PEAK"));
        assert!(lines[2].contains("CH 01"));
        assert!(lines[2].contains("CLIP"));
        assert!(lines[3].contains("−∞"));
        for color in [Color::Green, Color::Yellow, Color::Red] {
            assert!(buffer.content.iter().any(|cell| cell.fg == color));
        }
    }
}
