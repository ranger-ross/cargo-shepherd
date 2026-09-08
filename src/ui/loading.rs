//! Shimmer sweep for loading text, ported from omp's `theme/shimmer.ts`.

use std::time::Duration;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

const SPEED_CELLS_PER_S: f32 = 30.0;
const PADDING: f32 = 10.0;
const BAND_HALF_WIDTH: f32 = 8.0;

const TIER_HIGH: f32 = 0.65;
const TIER_MID: f32 = 0.22;

/// 24-bit RGB stop.
pub type Rgb = [u8; 3];

/// Three-tier color stack a character cycles through as the band sweeps.
pub struct ShimmerPalette {
    pub low: Rgb,
    pub mid: Rgb,
    pub high: Rgb,
    pub bold_crest: bool,
}
/// omp dark theme base (dim `#5f6673`, muted `#777d88`) with a blue crest
/// `#58a6ff` instead of omp's amber accent.
pub const DEFAULT_PALETTE: ShimmerPalette = ShimmerPalette {
    low: rgb(0x5f6673),
    mid: rgb(0x777d88),
    high: rgb(0x58a6ff),
    bold_crest: true,
};
/// Same sweep with a red crest for destructive states.
pub const DELETE_PALETTE: ShimmerPalette = ShimmerPalette {
    low: rgb(0x5f6673),
    mid: rgb(0x777d88),
    high: rgb(0xf46b6b),
    bold_crest: true,
};

/// Shimmered `Line` for `text` at `elapsed` since the load started.
/// `None` selects [`DEFAULT_PALETTE`]. Same-tier runs coalesce into one
/// `Span`, so a frame emits a handful of spans, not one per char.
pub fn shimmer_line(
    text: &str,
    elapsed: Duration,
    palette: Option<&ShimmerPalette>,
) -> Line<'static> {
    let palette = palette.unwrap_or(&DEFAULT_PALETTE);
    let len = text.chars().count();
    if len == 0 {
        return Line::from("");
    }
    let (low, mid, high) = (
        style_for(Tier::Low, palette),
        style_for(Tier::Mid, palette),
        style_for(Tier::High, palette),
    );
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_tier = Tier::Low;
    let mut started = false;
    for (i, c) in text.chars().enumerate() {
        let tier = tier_for(intensity(elapsed, i, len));
        if started && tier != run_tier {
            let style = match run_tier {
                Tier::Low => low,
                Tier::Mid => mid,
                Tier::High => high,
            };
            spans.push(Span::styled(std::mem::take(&mut run), style));
        }
        run_tier = tier;
        started = true;
        run.push(c);
    }
    let style = match run_tier {
        Tier::Low => low,
        Tier::Mid => mid,
        Tier::High => high,
    };
    spans.push(Span::styled(run, style));
    Line::from(spans)
}

/// Renderable shimmer indicator. See the module docs for usage.
pub struct Loading<'a> {
    text: &'a str,
    elapsed: Duration,
    palette: Option<&'a ShimmerPalette>,
}

impl<'a> Loading<'a> {
    pub fn new(text: &'a str, elapsed: Duration) -> Self {
        Self {
            text,
            elapsed,
            palette: None,
        }
    }

    pub fn palette(mut self, palette: &'a ShimmerPalette) -> Self {
        self.palette = Some(palette);
        self
    }

    pub fn line(self) -> Line<'static> {
        shimmer_line(self.text, self.elapsed, self.palette)
    }
}

impl Widget for Loading<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        Paragraph::new(self.line()).render(area, buf);
    }
}

const fn rgb(hex: u32) -> Rgb {
    [(hex >> 16) as u8, (hex >> 8) as u8, hex as u8]
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tier {
    Low,
    Mid,
    High,
}

fn tier_for(intensity: f32) -> Tier {
    if intensity >= TIER_HIGH {
        Tier::High
    } else if intensity >= TIER_MID {
        Tier::Mid
    } else {
        Tier::Low
    }
}

/// Smooth cosine bump sweeping left to right with edge padding.
/// Returns 0 outside the band, 1 at the crest.
fn intensity(elapsed: Duration, index: usize, len: usize) -> f32 {
    let period = len as f32 + PADDING * 2.0;
    let pos = (elapsed.as_secs_f32() * SPEED_CELLS_PER_S) % period;
    let dist = (index as f32 + PADDING - pos).abs();
    if dist >= BAND_HALF_WIDTH {
        return 0.0;
    }
    0.5 * (1.0 + (std::f32::consts::PI * dist / BAND_HALF_WIDTH).cos())
}

fn style_for(tier: Tier, palette: &ShimmerPalette) -> Style {
    let rgb = match tier {
        Tier::Low => palette.low,
        Tier::Mid => palette.mid,
        Tier::High => palette.high,
    };
    let style = Style::default().fg(Color::Rgb(rgb[0], rgb[1], rgb[2]));
    if tier == Tier::High && palette.bold_crest {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crest_sits_on_band_position() {
        // len 4 -> period 24; pos 10 lands the crest on index 0.
        let t = Duration::from_millis(10_000 / 30);
        assert!(intensity(t, 0, 4) > 0.99);
    }

    #[test]
    fn intensity_falls_off_with_distance() {
        let t = Duration::from_millis(10_000 / 30);
        let mid = intensity(t, 5, 12);
        assert!(mid > TIER_MID && mid < TIER_HIGH);
        assert!(intensity(t, 0, 12) > mid);
    }

    #[test]
    fn off_band_is_zero_and_wraps() {
        let t = Duration::ZERO;
        assert_eq!(intensity(t, 5, 8), 0.0);
    }

    #[test]
    fn tier_thresholds() {
        assert_eq!(tier_for(0.65), Tier::High);
        assert_eq!(tier_for(0.649), Tier::Mid);
        assert_eq!(tier_for(0.22), Tier::Mid);
        assert_eq!(tier_for(0.219), Tier::Low);
    }

    #[test]
    fn line_preserves_text_and_coalesces_runs() {
        let line = shimmer_line("Working…", Duration::from_millis(500), None);
        let rebuilt: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rebuilt, "Working…");
        assert!(line.spans.len() <= 8);
    }

    #[test]
    fn crest_span_is_bold_blue() {
        let t = Duration::from_millis(10_000 / 30);
        let line = shimmer_line("abcd", t, None);
        let blue = Style::default()
            .fg(Color::Rgb(0x58, 0xa6, 0xff))
            .add_modifier(Modifier::BOLD);
        assert!(line.spans.iter().any(|s| s.style == blue));
    }

    #[test]
    fn empty_text_is_empty_line() {
        let line = shimmer_line("", Duration::ZERO, None);
        assert_eq!(line.width(), 0);
    }

    #[test]
    fn custom_palette_replaces_crest() {
        let palette = ShimmerPalette {
            low: [0, 0, 0],
            mid: [1, 1, 1],
            high: [2, 3, 4],
            bold_crest: false,
        };
        let t = Duration::from_millis(10_000 / 30);
        let line = shimmer_line("abcd", t, Some(&palette));
        let crest = Style::default().fg(Color::Rgb(2, 3, 4));
        assert!(line.spans.iter().any(|s| s.style == crest));
    }
}
