//! The statusline (handoff §10): one BLOCK+ row on every screen, at every width.
//!
//! Fields that do not fit are dropped in a fixed order, one step at a time, until the row fits.
//! Each step builds the real segments and measures them in cells, so the drop decision can
//! never disagree with what is drawn.

use ratatui::text::Line;

use crate::band::{Band, Seg};
use crate::palette;
use crate::wrap::cell_width;

/// Everything the statusline shows. `None` is unknown and renders as `—` (or is omitted where
/// §10 says so); it is never shown as a zero.
#[derive(Debug, Clone, Default)]
pub struct StatusBar {
    /// The model reference `env/profile`.
    pub model: Option<String>,
    /// `None` is the adapter default, which is known: it shows as `default`.
    pub effort: Option<String>,
    pub repo: Option<String>,
    /// Omitted outside git.
    pub branch: Option<String>,
    /// Running workers; omitted at 0.
    pub workers: usize,
    /// Context use as shown (`10%`).
    pub ctx: Option<String>,
    /// At the summarize threshold the percent gets an amber `! `.
    pub ctx_warn: bool,
    pub spend: Option<String>,
    pub clock: Option<String>,
    /// Lines added / removed; not tracked yet, so `diff —` until a diff seam exists.
    pub diff: Option<(u64, u64)>,
}

/// The §10 drop order: each step removes one more thing than the step before.
const STEPS: usize = 7;

impl StatusBar {
    pub fn line(&self, width: usize) -> Line<'static> {
        let usable = width.saturating_sub(2);
        let mut chosen = self.segments(STEPS - 1);
        for step in 0..STEPS {
            let (left, right) = self.segments(step);
            if cells(&left) + cells(&right) + 2 <= usable {
                chosen = (left, right);
                break;
            }
        }
        // Past the last step the chip truncates by the Band rule; the right never does.
        let (left, right) = chosen;
        Band {
            bg: palette::BLOCK_PLUS,
            left,
            right,
            width,
            pad: 1,
        }
        .render()
    }

    /// The row after drop step `step`: 1 fold effort into the chip, 2 clock, 3 branch,
    /// 4 repo, 5 diff, 6 the `spend` label (its value stays).
    fn segments(&self, step: usize) -> (Vec<Seg>, Vec<Seg>) {
        let model = self.model.as_deref().unwrap_or("—");
        let effort = self.effort.as_deref().unwrap_or("default");
        let folded = step >= 1;
        let chip = if folded {
            format!(" {model}:{effort} ")
        } else {
            format!(" {model} ")
        };
        let mut left = vec![Seg {
            fg: palette::GROUND,
            bg: Some(palette::INK),
            text: chip,
        }];
        if step < 4
            && let Some(repo) = &self.repo
        {
            left.push(Seg::new(palette::INK, format!("   {repo}")));
            if step < 3
                && let Some(branch) = &self.branch
            {
                left.push(Seg::new(palette::DIM, format!(" {branch}")));
            }
        }
        if !folded {
            left.push(Seg::new(palette::DIM, "   effort "));
            left.push(Seg::new(palette::INK, effort));
        }

        // Right-hand groups, separated by three cells.
        let mut groups: Vec<Vec<Seg>> = Vec::new();
        if self.workers > 0 {
            groups.push(vec![
                Seg::new(palette::LIVE, "▪"),
                Seg::new(palette::INK, format!(" {}", self.workers)),
                Seg::new(palette::DIM, " workers"),
            ]);
        }
        let mut ctx = vec![Seg::new(palette::DIM, "ctx ")];
        if self.ctx_warn {
            ctx.push(Seg::new(palette::ATTN, "! "));
        }
        ctx.push(Seg::new(palette::INK, self.ctx.as_deref().unwrap_or("—")));
        groups.push(ctx);
        let spend = Seg::new(palette::INK, self.spend.as_deref().unwrap_or("—"));
        groups.push(if step >= 6 {
            vec![spend]
        } else {
            vec![Seg::new(palette::DIM, "spend "), spend]
        });
        if step < 2
            && let Some(clock) = &self.clock
        {
            groups.push(vec![Seg::new(palette::INK, clock.clone())]);
        }
        if step < 5 {
            groups.push(match self.diff {
                Some((added, removed)) => vec![
                    Seg::new(palette::OK, format!("+{added}")),
                    Seg::new(palette::FAIL, format!(" −{removed}")),
                ],
                None => vec![Seg::new(palette::DIM, "diff "), Seg::new(palette::INK, "—")],
            });
        }
        let mut right = Vec::new();
        for (n, group) in groups.into_iter().enumerate() {
            if n > 0 {
                right.push(Seg::new(palette::DIM, "   "));
            }
            right.extend(group);
        }
        (left, right)
    }
}

fn cells(segs: &[Seg]) -> usize {
    segs.iter().map(|s| cell_width(&s.text)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar() -> StatusBar {
        StatusBar {
            model: Some("claude/opus-5.5".into()),
            effort: Some("high".into()),
            repo: Some("phaseone".into()),
            branch: Some("main".into()),
            ctx: Some("62%".into()),
            clock: Some("0h14".into()),
            ..StatusBar::default()
        }
    }

    #[test]
    fn the_warn_mark_is_amber_and_the_label_stays_dim() {
        let line = StatusBar {
            ctx_warn: true,
            ..bar()
        }
        .line(116);
        let ctx = line.spans.iter().position(|s| s.content == "ctx ").unwrap();
        assert_eq!(line.spans[ctx].style.fg, Some(palette::DIM));
        assert_eq!(line.spans[ctx + 1].content, "! ");
        assert_eq!(line.spans[ctx + 1].style.fg, Some(palette::ATTN));
    }

    #[test]
    fn known_line_counts_replace_the_unknown_diff() {
        let text = StatusBar {
            diff: Some((12, 3)),
            ..bar()
        }
        .line(116)
        .to_string();
        assert!(text.trim_end().ends_with("+12 −3"), "{text}");
        assert!(!text.contains("diff"));
    }

    #[test]
    fn every_width_fits_exactly_and_the_right_side_survives() {
        for width in [20, 40, 52, 76, 96, 116, 236] {
            let line = bar().line(width);
            assert_eq!(cell_width(&line.to_string()), width, "width {width}");
            if width >= 40 {
                assert!(line.to_string().contains("ctx 62%"), "width {width}");
            }
        }
    }
}
