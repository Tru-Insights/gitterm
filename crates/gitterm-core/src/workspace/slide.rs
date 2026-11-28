//! Workspace slide rules from Iced's SlideScrolled/SlideAnimationTick and MouseMoved.
use std::time::Instant;

pub const SLIDE_DURATION_MS: f32 = 400.0;
pub const SWIPE_DEBOUNCE_MS: u64 = 150;
pub const EDGE_PEEK_ZONE: f32 = 30.0;

pub fn nearest(offset: f32, width: f32, count: usize) -> usize {
    let position = ((offset + width * 0.5) / width) as usize;
    position.min(count.saturating_sub(1))
}

pub fn eased(elapsed_ms: f32) -> f32 {
    let t = (elapsed_ms / SLIDE_DURATION_MS).min(1.0);
    1.0 - (1.0 - t).powi(3)
}

pub fn edge_peek(x: f32, width: f32, position: usize, count: usize) -> (bool, bool) {
    (
        position > 0 && (0.0..EDGE_PEEK_ZONE).contains(&x),
        position + 1 < count && x > width - EDGE_PEEK_ZONE && x <= width,
    )
}

/// Presentation only. The host applies `scroll`'s nearest position using its
/// existing workspace-selection continuation, retaining the swipe's start order.
#[derive(Default)]
pub struct Slide {
    pub offset: f32,
    pub from: Option<u64>,
    pub peek: (bool, bool),
    selected: Option<u64>,
    width: f32,
    last_scroll: Option<Instant>,
    animation: Option<(Instant, f32, f32)>,
}
impl Slide {
    pub fn pinned_from(&self, next: u64) -> Option<u64> {
        self.from.or(self.selected.filter(|&id| id != next))
    }
    pub fn moving(&self) -> bool {
        self.last_scroll.is_some() || self.animation.is_some()
    }
    pub fn sync(&mut self, selected: u64, position: usize, width: f32, now: Instant) {
        let target = position as f32 * width;
        if self.selected != Some(selected) {
            if self.selected.is_some() && self.last_scroll.is_none() {
                self.from = self.selected;
                self.animation = Some((now, self.offset, target));
            } else if self.selected.is_none() {
                self.offset = target;
            }
            self.selected = Some(selected);
        }
        if self.width != width {
            self.offset = target;
            self.width = width;
            self.animation = None;
            self.last_scroll = None;
            self.from = None;
        }
        if !self.moving() {
            self.offset = target;
        }
    }
    pub fn scroll(&mut self, delta: f32, count: usize, now: Instant) -> Option<usize> {
        if self.animation.is_some() || self.width <= 0.0 || count == 0 {
            return None;
        }
        self.from = self.from.or(self.selected);
        self.offset = (self.offset + delta).clamp(0.0, (count - 1) as f32 * self.width);
        self.last_scroll = Some(now);
        Some(nearest(self.offset, self.width, count))
    }
    pub fn tick(&mut self, position: usize, now: Instant) {
        if let Some((at, start, target)) = self.animation {
            let elapsed = now.duration_since(at).as_millis() as f32;
            self.offset = start + (target - start) * eased(elapsed);
            if elapsed >= SLIDE_DURATION_MS {
                self.offset = target;
                self.animation = None;
                if self.last_scroll.is_none() {
                    self.from = None;
                }
            }
        } else if self
            .last_scroll
            .is_some_and(|at| now.duration_since(at).as_millis() >= SWIPE_DEBOUNCE_MS as u128)
        {
            self.last_scroll = None;
            let target = position as f32 * self.width;
            if (self.offset - target).abs() > 1.0 {
                self.animation = Some((now, self.offset, target));
            } else {
                self.offset = target;
                self.from = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn workspace_swipe_sequence_matches_iced_selection_snap_and_peek_thresholds() {
        let now = Instant::now();
        let mut slide = Slide::default();
        slide.sync(10, 0, 1000., now);
        assert_eq!(slide.scroll(499., 3, now), Some(0));
        assert_eq!(slide.scroll(1., 3, now), Some(1));
        slide.sync(20, 1, 1000., now);
        assert_eq!(slide.from, Some(10));
        assert_eq!(edge_peek(0., 1000., 1, 3), (true, false));
        assert_eq!(edge_peek(29.999, 1000., 1, 3), (true, false));
        assert_eq!(edge_peek(30., 1000., 1, 3), (false, false));
        assert_eq!(edge_peek(970., 1000., 1, 3), (false, false));
        assert_eq!(edge_peek(1000., 1000., 1, 3), (false, true));
        assert_eq!(edge_peek(1000.01, 1000., 1, 3), (false, false));
        assert_eq!(edge_peek(0., 1000., 0, 3), (false, false));
        assert_eq!(edge_peek(1000., 1000., 2, 3), (false, false));
        slide.tick(1, now + Duration::from_millis(149));
        assert_eq!(slide.offset, 500.);
        slide.tick(1, now + Duration::from_millis(150));
        assert!(slide.moving());
        assert_eq!(slide.scroll(999., 3, now), None);
        slide.tick(1, now + Duration::from_millis(350));
        assert_eq!(slide.offset, 937.5); // Iced's cubic ease-out at 200/400 ms.
        slide.tick(1, now + Duration::from_millis(550));
        assert_eq!(slide.offset, 1000.);
        assert!(!slide.moving());
        assert_eq!(slide.from, None);
        // Once a hidden starting panel leaves the order, use the settled order.
        slide.sync(20, 0, 1000., now + Duration::from_millis(551));
        assert_eq!(slide.offset, 0.);
        slide.sync(20, 1, 1000., now + Duration::from_millis(552));

        assert_eq!(
            slide.scroll(-501., 3, now + Duration::from_secs(1)),
            Some(0)
        );
    }
}
