//! Frame-driven column scrolling. Direct manipulation always takes priority.
use gpui_kit::{ScrollHandle, px};
use std::time::{Duration, Instant};

pub(crate) const COLUMN_WIDTH: f32 = 240.;
const DURATION: Duration = Duration::from_millis(220);

struct Transition {
    from: f32,
    to: f32,
    started: Instant,
    applied: f32,
}

#[derive(Default)]
pub(crate) struct ColumnScroll {
    pub handle: ScrollHandle,
    reveal: Option<usize>,
    transition: Option<Transition>,
}

impl ColumnScroll {
    pub fn reveal(&mut self, index: usize) {
        self.reveal = Some(index);
    }

    pub fn cancel(&mut self) {
        self.reveal = None;
        self.transition = None;
    }

    pub fn finish(&mut self) {
        if let Some(transition) = self.transition.take() {
            let mut offset = self.handle.offset();
            offset.x = px(transition.to);
            self.handle.set_offset(offset);
        }
    }

    fn animate_to(&mut self, target: f32, now: Instant) {
        let from: f32 = self.handle.offset().x.into();
        if (target - from).abs() <= 0.5 {
            self.transition = None;
            let mut offset = self.handle.offset();
            offset.x = px(target);
            self.handle.set_offset(offset);
            return;
        }
        self.transition = Some(Transition {
            from,
            to: target,
            started: now,
            applied: from,
        });
    }

    fn max_for_width(columns: usize, width: f32) -> f32 {
        (columns as f32 * COLUMN_WIDTH - width).max(0.)
    }

    pub fn max_offset(&self, columns: usize) -> f32 {
        Self::max_for_width(columns, self.handle.bounds().size.width.into())
    }

    /// Let GPUI lay out the current offset even when the real strip has just
    /// shrunk. The spacer recedes with the viewport and has no scroll target.
    pub fn trailing_space(&self, columns: usize) -> f32 {
        if self.transition.is_none() {
            return 0.;
        }
        let width: f32 = self.handle.bounds().size.width.into();
        let current: f32 = self.handle.offset().x.into();
        if current >= -Self::max_for_width(columns, width) {
            return 0.;
        }
        (width - current - columns as f32 * COLUMN_WIDTH).ceil()
    }

    pub fn step(&mut self, direction: i32, columns: usize) {
        let current: f32 = self.handle.offset().x.into();
        let target = self.transition.as_ref().map_or(current, |t| t.to);
        let max = self.max_offset(columns);
        self.reveal = None;
        self.animate_to(
            (target - direction as f32 * COLUMN_WIDTH).clamp(-max, 0.),
            Instant::now(),
        );
    }

    /// Called from render, once layout has supplied the viewport width.
    pub fn advance(&mut self, columns: usize, now: Instant) -> bool {
        let width: f32 = self.handle.bounds().size.width.into();
        self.advance_with_width(columns, width, now)
    }

    fn advance_with_width(&mut self, columns: usize, width: f32, now: Instant) -> bool {
        if width <= 0. {
            return self.reveal.is_some();
        }
        let max = Self::max_for_width(columns, width);
        let current: f32 = self.handle.offset().x.into();
        // Scrollbar dragging/native scrolling must not fight an animation.
        if self
            .transition
            .as_ref()
            .is_some_and(|t| (current - t.applied).abs() > 0.5)
        {
            self.transition = None;
        }
        let base = self.transition.as_ref().map_or(current, |t| t.to);
        let target = if let Some(index) = self.reveal.take() {
            let index = index.min(columns.saturating_sub(1));
            let left = index as f32 * COLUMN_WIDTH;
            let right = left + COLUMN_WIDTH.min(width);
            if left + base < 0. {
                -left
            } else if right + base > width {
                width - right
            } else {
                base
            }
        } else {
            base
        }
        .clamp(-max, 0.);
        // A directory refresh can shorten the strip without a reveal request.
        // Keep easing toward an unchanged target when input repeats.
        if target != base {
            self.animate_to(target, now);
        }
        let Some(transition) = self.transition.as_mut() else {
            return false;
        };
        let progress =
            (now.duration_since(transition.started).as_secs_f32() / DURATION.as_secs_f32()).min(1.);
        let eased = 1. - (1. - progress).powi(3);
        let x = transition.from + (transition.to - transition.from) * eased;
        let mut offset = self.handle.offset();
        offset.x = px(x);
        self.handle.set_offset(offset);
        transition.applied = x;
        if progress >= 1. {
            self.transition = None;
        }
        self.transition.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::point;

    fn x(scroll: &ColumnScroll) -> f32 {
        scroll.handle.offset().x.into()
    }

    #[test]
    fn reveal_eases_and_finishes_at_the_minimum_visible_offset() {
        let mut scroll = ColumnScroll::default();
        let now = Instant::now();
        scroll.reveal(2);
        assert!(scroll.advance_with_width(3, 480., now));
        assert_eq!(x(&scroll), 0.);
        assert!(scroll.advance_with_width(3, 480., now + DURATION / 2));
        assert_eq!(x(&scroll), -210.); // ease-out cubic: 87.5% at half time
        assert!(!scroll.advance_with_width(3, 480., now + DURATION));
        assert_eq!(x(&scroll), -240.);
        scroll.reveal(1); // already visible; no movement
        assert!(!scroll.advance_with_width(3, 480., now + DURATION));
        assert_eq!(x(&scroll), -240.);
    }

    #[test]
    fn navigation_retargets_without_jumping_or_restarting_identical_requests() {
        let mut scroll = ColumnScroll::default();
        let now = Instant::now();
        scroll.reveal(2);
        scroll.advance_with_width(4, 480., now);
        scroll.advance_with_width(4, 480., now + DURATION / 2);
        scroll.reveal(2);
        scroll.advance_with_width(4, 480., now + DURATION);
        assert_eq!(x(&scroll), -240.);
        scroll.reveal(3);
        scroll.advance_with_width(4, 480., now + DURATION);
        assert_eq!(x(&scroll), -240.);
        let halfway = now + DURATION + DURATION / 2;
        scroll.advance_with_width(4, 480., halfway);
        let before = x(&scroll);
        scroll.reveal(0);
        scroll.advance_with_width(4, 480., halfway);
        assert_eq!(x(&scroll), before);
        scroll.advance_with_width(4, 480., halfway + DURATION);
        assert_eq!(x(&scroll), 0.);
    }

    #[test]
    fn a_shorter_branch_eases_back_from_the_existing_offset() {
        let mut scroll = ColumnScroll::default();
        let now = Instant::now();
        scroll.handle.set_offset(point(px(-960.), px(0.)));
        // Replacing the last five columns with a leaf leaves three columns.
        scroll.reveal(2);
        assert!(scroll.advance_with_width(3, 480., now));
        assert_eq!(x(&scroll), -960.);
        assert!(scroll.advance_with_width(3, 480., now + DURATION / 2));
        assert_eq!(x(&scroll), -330.);
        assert!(!scroll.advance_with_width(3, 480., now + DURATION));
        assert_eq!(x(&scroll), -240.);
    }

    #[test]
    fn a_refresh_can_shorten_the_trail_without_a_reveal_request() {
        let mut scroll = ColumnScroll::default();
        let now = Instant::now();
        scroll.handle.set_offset(point(px(-480.), px(0.)));
        assert!(scroll.advance_with_width(1, 480., now));
        assert_eq!(x(&scroll), -480.);
        scroll.finish(); // Reduce Motion settles in the first frame.
        assert_eq!(x(&scroll), 0.);
        assert!(!scroll.advance_with_width(1, 480., now));
    }

    #[test]
    fn manual_scrolling_interrupts_and_unlaid_out_columns_wait() {
        let mut scroll = ColumnScroll::default();
        let now = Instant::now();
        scroll.reveal(3);
        assert!(scroll.advance_with_width(4, 0., now));
        assert_eq!(x(&scroll), 0.);
        scroll.advance_with_width(4, 480., now);
        scroll.handle.set_offset(point(px(-55.), px(0.)));
        assert!(!scroll.advance_with_width(4, 480., now + DURATION / 2));
        assert_eq!(x(&scroll), -55.);
        scroll.reveal(2);
        scroll.cancel();
        assert!(!scroll.advance_with_width(4, 480., now + DURATION));
        assert_eq!(x(&scroll), -55.);
    }
}
