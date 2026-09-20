//! Dotted awakening home: a 24-second cortex/sphere orbit in ordinary cells.
//! Geometry is cached once; only projection and bounded dot rasterization run
//! each frame. Text labels are composited AFTER Braille, never sampled into it.
use std::f64::consts::{PI, TAU};
use std::sync::OnceLock;

use ratatui::{buffer::Buffer, layout::Rect, style::Color};

use crate::palette;

const PERIOD_MS: u64 = 24_000;
const GLYPH_P: [&str; 9] = [
    "00000", "00000", "11110", "10001", "10001", "10001", "11110", "10000", "10000",
];
const GLYPH_ONE: [&str; 9] = [
    "00100", "01100", "00100", "00100", "00100", "00100", "01110", "00000", "00000",
];

#[derive(Clone, Copy)]
struct Point {
    x: f64,
    y: f64,
    z: f64,
}
struct Neuron {
    brain: Point,
    sphere: Point,
    phase: f64,
    fold: f64,
}
struct Network {
    nodes: Vec<Neuron>,
    edges: Vec<(usize, usize)>,
}

fn network() -> &'static Network {
    static NETWORK: OnceLock<Network> = OnceLock::new();
    NETWORK.get_or_init(|| {
        let mut nodes = Vec::with_capacity(2400);
        for i in 0..2400 {
            let y = 1.0 - 2.0 * (i as f64 + 0.5) / 2400.0;
            let angle = i as f64 * 2.399_963_229_728_653;
            let r = (1.0 - y * y).sqrt();
            let (x, z) = (angle.cos() * r, angle.sin() * r);
            let fold = (x.abs() * 15.0 + (y * 7.0).sin() * 2.4 + (z * 9.0).sin() * 1.3).sin();
            let ridge = 1.0 - 0.045 * (fold + 1.0);
            nodes.push(Neuron {
                brain: Point {
                    x: x * 183.0 * ridge,
                    y: y * 220.0 * ridge,
                    z: z * 151.0 * ridge - z.max(0.0) * 29.0 * (-(x / 0.085).powi(2)).exp(),
                },
                sphere: Point {
                    x: x * 192.0,
                    y: y * 192.0,
                    z: z * 192.0,
                },
                phase: (i as f64 * 0.618_033_988_75).fract() * TAU,
                fold,
            });
        }
        let mut bins = std::collections::HashMap::<(i32, i32, i32), Vec<usize>>::new();
        let key = |p: Point| {
            (
                (p.x / 32.0).floor() as i32,
                (p.y / 32.0).floor() as i32,
                (p.z / 32.0).floor() as i32,
            )
        };
        for (i, n) in nodes.iter().enumerate() {
            bins.entry(key(n.brain)).or_default().push(i);
        }
        let mut edges = Vec::new();
        for (i, n) in nodes.iter().enumerate() {
            let (x, y, z) = key(n.brain);
            let mut near = Vec::new();
            for dz in -1..=1 {
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        if let Some(indices) = bins.get(&(x + dx, y + dy, z + dz)) {
                            for &j in indices {
                                if j > i {
                                    let m = nodes[j].brain;
                                    let d = (n.brain.x - m.x).powi(2)
                                        + (n.brain.y - m.y).powi(2)
                                        + (n.brain.z - m.z).powi(2);
                                    if d < 42.0_f64.powi(2) {
                                        near.push((j, d));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            near.sort_by(|a, b| a.1.total_cmp(&b.1));
            edges.extend(near.iter().take(3).map(|&(j, _)| (i, j)));
        }
        Network { nodes, edges }
    })
}

fn project(p: Point) -> Point {
    let scale = 900.0 / (900.0 - p.z);
    Point {
        x: 500.0 + p.x * scale,
        y: 302.0 + p.y * scale,
        z: p.z,
    }
}
fn rotate(p: Point, angle: f64) -> Point {
    let x = p.x * angle.cos() + p.z * angle.sin();
    let z = p.z * angle.cos() - p.x * angle.sin();
    project(Point {
        x,
        y: p.y * 0.18_f64.cos() - z * 0.18_f64.sin(),
        z: p.y * 0.18_f64.sin() + z * 0.18_f64.cos(),
    })
}

struct Dots {
    area: Rect,
    scale: f64,
    left: f64,
    top: f64,
    bits: Vec<u8>,
    light: Vec<u8>,
}
impl Dots {
    fn new(area: Rect) -> Self {
        let scale = (f64::from(area.width) * 2.0 / 900.0).min(f64::from(area.height) * 4.0 / 620.0);
        Self {
            area,
            scale,
            left: f64::from(area.width) - 500.0 * scale,
            top: f64::from(area.height) * 2.0 - 310.0 * scale,
            bits: vec![0; usize::from(area.width) * usize::from(area.height)],
            light: vec![0; usize::from(area.width) * usize::from(area.height)],
        }
    }
    fn pixel(&mut self, x: f64, y: f64, light: u8) {
        let x = (x * self.scale + self.left).round() as i32;
        let y = (y * self.scale + self.top).round() as i32;
        if x < 0
            || y < 0
            || x >= i32::from(self.area.width) * 2
            || y >= i32::from(self.area.height) * 4
        {
            return;
        }
        let i = y as usize / 4 * usize::from(self.area.width) + x as usize / 2;
        const MASK: [[u8; 4]; 2] = [[1, 2, 4, 64], [8, 16, 32, 128]];
        // A terminal cell has one foreground colour. Preserve the logo's
        // own dots instead of whitening every background neuron in its cell.
        if light == 232 && self.light[i] != 232 {
            self.bits[i] = 0;
        } else if light != 232 && self.light[i] == 232 {
            return;
        }
        self.bits[i] |= MASK[x as usize % 2][y as usize % 4];
        self.light[i] = self.light[i].max(light);
    }
    fn line(&mut self, a: Point, b: Point, light: u8) {
        let steps =
            (((a.x - b.x).abs().max((a.y - b.y).abs()) * self.scale).ceil() as usize).clamp(1, 512);
        for i in 0..=steps {
            let u = i as f64 / steps as f64;
            self.pixel(a.x + (b.x - a.x) * u, a.y + (b.y - a.y) * u, light);
        }
    }
    fn paint(&self, buf: &mut Buffer) {
        for y in 0..self.area.height {
            for x in 0..self.area.width {
                let i = usize::from(y) * usize::from(self.area.width) + usize::from(x);
                if self.bits[i] == 0 {
                    continue;
                }
                let color = match self.light[i] {
                    0..=69 => palette::RULE,
                    70..=139 => palette::FAINT,
                    140..=199 => palette::DIM,
                    _ => palette::INK,
                };
                buf[(self.area.x + x, self.area.y + y)]
                    .set_char(char::from_u32(0x2800 + u32::from(self.bits[i])).unwrap())
                    .set_fg(color)
                    .set_bg(palette::GROUND);
            }
        }
    }
    fn label(&self, buf: &mut Buffer, p: Point, text: &str) {
        let width = text.len() as u16;
        if self.area.width < width || self.area.height == 0 {
            return;
        }
        let x = ((p.x * self.scale + self.left) / 2.0 - f64::from(width) / 2.0)
            .round()
            .max(0.0) as u16;
        let y = ((p.y * self.scale + self.top) / 4.0).round().max(0.0) as u16;
        buf.set_string(
            self.area.x + x.min(self.area.width - width),
            self.area.y + y.min(self.area.height - 1),
            text,
            ratatui::style::Style::new()
                .fg(palette::DIM)
                .bg(palette::GROUND),
        );
    }
}

fn centered(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: Color) {
    let width = text.chars().count() as u16;
    if width <= area.width && y < area.bottom() {
        buf.set_string(
            area.x + (area.width - width) / 2,
            y,
            text,
            ratatui::style::Style::new().fg(color).bg(palette::GROUND),
        );
    }
}

/// Draw only into unused welcome space; the caller owns composer/overlay priority.
pub(super) fn draw(area: Rect, buf: &mut Buffer, now_ms: u64, reduced_motion: bool) {
    if area.height < 5 || area.width < 16 {
        return;
    }
    if area.height < 12 || area.width < 30 {
        centered(
            buf,
            area,
            area.y + area.height / 2 - 1,
            "phaseone",
            palette::INK,
        );
        centered(
            buf,
            area,
            area.y + area.height / 2 + 1,
            "we love Pi",
            palette::DIM,
        );
        return;
    }
    let time = if reduced_motion {
        0
    } else {
        now_ms % PERIOD_MS
    };
    let phase = time as f64 / PERIOD_MS as f64 * TAU;
    let morph = (1.0 - phase.cos()) / 2.0;
    let art = Rect {
        height: area.height.saturating_sub(3),
        ..area
    };
    let mut dots = Dots::new(art);
    let net = network();
    let points: Vec<_> = net
        .nodes
        .iter()
        .map(|n| {
            rotate(
                Point {
                    x: n.brain.x * (1.0 - morph) + n.sphere.x * morph,
                    y: n.brain.y * (1.0 - morph) + n.sphere.y * morph,
                    z: n.brain.z * (1.0 - morph) + n.sphere.z * morph,
                },
                phase + 0.15,
            )
        })
        .collect();
    let brightness: Vec<_> = net
        .nodes
        .iter()
        .zip(&points)
        .map(|(n, p)| {
            let firing = (phase * 3.0 + n.phase).cos().max(0.0).powi(30);
            let depth = ((p.z + 215.0) / 430.0).clamp(0.0, 1.0);
            let reserve =
                0.4 + 0.6 * (((p.x - 500.0).hypot(p.y - 302.0) - 65.0) / 75.0).clamp(0.0, 1.0);
            ((34.0 + (n.fold + 1.0) * 9.0 + firing * 150.0) * (0.4 + depth * 0.6) * reserve) as u8
        })
        .collect();
    for &(i, j) in &net.edges {
        dots.line(points[i], points[j], brightness[i].min(brightness[j]));
    }
    for (p, b) in points.iter().zip(&brightness) {
        dots.pixel(p.x, p.y, *b);
    }
    let mut labels = Vec::new();
    for i in 0..3 {
        let a = phase + i as f64 * TAU / 3.0;
        let radius = 315.0 + 18.0 * (a * 2.0).sin();
        let center = project(Point {
            x: a.cos() * radius,
            y: (a + 0.25).sin() * 145.0 + (i as f64 - 1.0) * 22.0,
            z: a.sin() * 240.0,
        });
        let size = (20.0 + i as f64 * 6.0) * 900.0 / (900.0 - center.z);
        let agent: Vec<_> = (0..64)
            .map(|j| {
                let y = 1.0 - 2.0 * (j as f64 + 0.5) / 64.0;
                let angle = j as f64 * 2.399_963_23 + a;
                Point {
                    x: center.x + angle.cos() * (1.0 - y * y).sqrt() * size,
                    y: center.y + y * size,
                    z: 0.0,
                }
            })
            .collect();
        for (j, p) in agent.iter().enumerate() {
            dots.pixel(p.x, p.y, 100);
            if j + 8 < agent.len() {
                dots.line(*p, agent[j + 8], 42);
            }
        }
        let anchor = Point {
            x: 500.0 + (phase + i as f64 * 2.1).cos() * 110.0,
            y: 302.0 + (phase + i as f64 * 2.1).sin() * 125.0,
            z: 0.0,
        };
        let curve = |u: f64| Point {
            x: center.x + (anchor.x - center.x) * u + (u * PI).sin() * 30.0,
            y: center.y + (anchor.y - center.y) * u - (u * PI).sin() * 45.0,
            z: 0.0,
        };
        for j in 0..48 {
            dots.line(curve(j as f64 / 48.0), curve((j + 1) as f64 / 48.0), 42);
        }
        for j in 0..3 {
            let u =
                (time as f64 / PERIOD_MS as f64 * 2.0 + i as f64 / 3.0 + j as f64 / 3.0).fract();
            let p = curve(u);
            dots.pixel(p.x, p.y, (u * PI).sin().powi(2).mul_add(180.0, 42.0) as u8);
        }
        if (i == 0 || i == 2) && a.sin().max(0.0).powi(4) > 0.5 {
            labels.push((
                Point {
                    y: center.y + size + 18.0,
                    ..center
                },
                if i == 0 { "10841" } else { "[big]" },
            ));
        }
    }
    // The chosen small-p/tall-1 mark is steady while the cortex rotates around it.
    for (letter, rows) in [GLYPH_P, GLYPH_ONE].iter().enumerate() {
        for (y, row) in rows.iter().enumerate() {
            for (x, byte) in row.bytes().enumerate() {
                if byte == b'1' {
                    dots.pixel(
                        428.0 + (x + letter * 6) as f64 * 14.4,
                        244.0 + y as f64 * 14.4,
                        232,
                    );
                }
            }
        }
    }
    dots.paint(buf);
    for (p, text) in labels {
        dots.label(buf, p, text);
    }
    centered(buf, area, art.bottom(), "phaseone", palette::INK);
    centered(buf, area, art.bottom() + 2, "we love Pi", palette::DIM);
}
