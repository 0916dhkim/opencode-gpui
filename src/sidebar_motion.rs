//! Bounded, reversible sidebar reveal. Width and content opacity share one phase.
use std::time::{Duration, Instant};

const DURATION: Duration = Duration::from_millis(180);

#[derive(Clone, Debug)]
pub struct SidebarMotion {
    position: f32,
    from: f32,
    visible: bool,
    started: Option<Instant>,
    duration: Duration,
}

impl SidebarMotion {
    pub fn new(visible: bool) -> Self {
        let position = if visible { 1. } else { 0. };
        Self {
            position,
            from: position,
            visible,
            started: None,
            duration: DURATION,
        }
    }

    pub fn position(&self) -> f32 {
        self.position
    }

    pub fn is_animating(&self) -> bool {
        self.started.is_some()
    }

    /// Startup, automatic narrow-window collapse, and reduced motion are instant.
    pub fn snap(&mut self, visible: bool) {
        *self = Self::new(visible);
    }

    pub fn retarget(&mut self, visible: bool, now: Instant, reduced_motion: bool) {
        self.advance(now);
        if reduced_motion {
            self.snap(visible);
        } else if self.visible != visible {
            self.from = self.position;
            self.visible = visible;
            let target = if visible { 1. } else { 0. };
            // A reversal starts at the current phase, not either endpoint, and
            // a short remaining trip must not take a full new 180ms.
            self.duration = DURATION.mul_f32((target - self.from).abs());
            self.started = (!self.duration.is_zero()).then_some(now);
        }
    }

    pub fn advance(&mut self, now: Instant) {
        let Some(started) = self.started else {
            return;
        };
        let elapsed = now.saturating_duration_since(started);
        if elapsed >= self.duration {
            self.snap(self.visible);
            return;
        }
        let t = elapsed.as_secs_f32() / self.duration.as_secs_f32();
        let eased = 1. - (1. - t).powi(3);
        let target = if self.visible { 1. } else { 0. };
        self.position = (self.from + (target - self.from) * eased).clamp(0., 1.);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_exact_and_animation_stops() {
        let start = Instant::now();
        let mut motion = SidebarMotion::new(false);
        assert!(!motion.is_animating());
        for visible in [true, false] {
            motion.retarget(visible, start, false);
            assert!(motion.is_animating());
            let mut previous = motion.position();
            for ms in 1..=180 {
                motion.advance(start + Duration::from_millis(ms));
                let phase = motion.position();
                assert!((0. ..=1.).contains(&phase));
                assert!(if visible {
                    phase >= previous
                } else {
                    phase <= previous
                });
                previous = phase;
            }
            assert_eq!(motion.position(), if visible { 1. } else { 0. });
            assert!(!motion.is_animating());
            motion.advance(start + Duration::from_secs(60));
            assert!(!motion.is_animating());
        }
    }

    #[test]
    fn reversal_preserves_position_and_shortens_the_remaining_trip() {
        let start = Instant::now();
        let mut motion = SidebarMotion::new(false);
        motion.retarget(true, start, false);
        let reverse = start + Duration::from_millis(30);
        motion.advance(reverse);
        let position = motion.position();
        assert!(position > 0. && position < 1.);
        motion.retarget(false, reverse, false);
        assert_eq!(motion.position(), position);
        assert!(motion.duration < DURATION);
        motion.advance(reverse + Duration::from_millis(180));
        assert_eq!(motion.position(), 0.);
        assert!(!motion.is_animating());
    }

    #[test]
    fn repeated_target_does_not_restart_the_animation() {
        let start = Instant::now();
        let mut motion = SidebarMotion::new(true);
        motion.retarget(true, start, false);
        assert!(!motion.is_animating());
        motion.retarget(false, start, false);
        motion.retarget(false, start + Duration::from_millis(90), false);
        motion.advance(start + DURATION);
        assert_eq!(motion.position(), 0.);
        assert!(!motion.is_animating());
    }

    #[test]
    fn reduced_motion_and_automatic_collapse_cancel_pending_motion() {
        let start = Instant::now();
        let mut motion = SidebarMotion::new(false);
        motion.retarget(true, start, false);
        motion.retarget(true, start + Duration::from_millis(50), true);
        assert_eq!(motion.position(), 1.);
        assert!(!motion.is_animating());
        motion.retarget(false, start + Duration::from_millis(60), false);
        motion.snap(false);
        assert_eq!(motion.position(), 0.);
        assert!(!motion.is_animating());
    }

    #[test]
    fn rapid_reversals_remain_bounded_and_eventually_settle() {
        let start = Instant::now();
        let mut motion = SidebarMotion::new(true);
        for i in 0..100 {
            motion.retarget(i % 2 == 1, start + Duration::from_millis(i * 7), false);
            assert!((0. ..=1.).contains(&motion.position()));
        }
        motion.advance(start + Duration::from_secs(2));
        assert_eq!(motion.position(), 1.);
        assert!(!motion.is_animating());
    }
}
