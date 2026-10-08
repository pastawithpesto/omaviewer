//! ACK-driven bitrate and frame-rate adaptation for EGFX.

use std::time::Duration;

const BAD_ACKS_TO_REDUCE: u8 = 2;
const HEALTHY_ACKS_TO_INCREASE: u8 = 12;
const MIN_CONGESTION_DELAY: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AdaptiveTarget {
    pub(crate) bitrate: u32,
    pub(crate) fps: u32,
}

/// Steps media load down on repeated slow/backlogged ACKs and restores it only
/// after a run of fresh, healthy ACKs. Capture idleness never feeds this state.
#[derive(Debug)]
pub(crate) struct AdaptiveRateController {
    configured_bitrate: u32,
    configured_fps: u32,
    adapt_bitrate: bool,
    target: AdaptiveTarget,
    last_ack_count: u64,
    bad_acks: u8,
    healthy_acks: u8,
}

impl AdaptiveRateController {
    pub(crate) fn new(
        configured_bitrate: u32,
        configured_fps: u32,
        adapt_bitrate: bool,
        initial_ack_count: u64,
    ) -> Self {
        Self {
            configured_bitrate,
            configured_fps: configured_fps.max(1),
            adapt_bitrate,
            target: AdaptiveTarget {
                bitrate: configured_bitrate,
                fps: configured_fps.max(1),
            },
            last_ack_count: initial_ack_count,
            bad_acks: 0,
            healthy_acks: 0,
        }
    }

    pub(crate) fn target(&self) -> AdaptiveTarget {
        self.target
    }

    /// Consume a feedback snapshot. Returns true only when the target changed.
    pub(crate) fn observe_ack(
        &mut self,
        ack_count: u64,
        ack_delay: Option<Duration>,
        client_queue_depth: u32,
        frames_in_flight: u32,
        max_frames_in_flight: u32,
        frame_ack_suspended: bool,
    ) -> bool {
        if ack_count < self.last_ack_count {
            self.reset_for_new_session();
            return true;
        }
        if ack_count == self.last_ack_count || frame_ack_suspended {
            return false;
        }
        self.last_ack_count = ack_count;

        let max_frames_in_flight = max_frames_in_flight.max(1);
        let delay_limit = MIN_CONGESTION_DELAY.max(Duration::from_secs_f64(
            3.0 / f64::from(self.target.fps.max(1)),
        ));
        let congested = client_queue_depth >= 2
            || (max_frames_in_flight > 1
                && frames_in_flight.saturating_add(1) >= max_frames_in_flight)
            || ack_delay.is_some_and(|delay| delay > delay_limit);

        if congested {
            self.healthy_acks = 0;
            self.bad_acks = self.bad_acks.saturating_add(1);
            let severe_delay = ack_delay.is_some_and(|delay| delay > delay_limit.saturating_mul(3));
            if severe_delay || self.bad_acks >= BAD_ACKS_TO_REDUCE {
                let previous = self.target;
                self.reduce_load();
                self.bad_acks = 0;
                return self.target != previous;
            }
            return false;
        }

        self.bad_acks = 0;
        self.healthy_acks = self.healthy_acks.saturating_add(1);
        if self.healthy_acks >= HEALTHY_ACKS_TO_INCREASE {
            let previous = self.target;
            self.increase_load();
            self.healthy_acks = 0;
            return self.target != previous;
        }
        false
    }

    pub(crate) fn reset_for_new_session(&mut self) {
        self.target = AdaptiveTarget {
            bitrate: self.configured_bitrate,
            fps: self.configured_fps,
        };
        self.last_ack_count = 0;
        self.bad_acks = 0;
        self.healthy_acks = 0;
    }

    fn bitrate_floor(&self) -> u32 {
        self.configured_bitrate
            .min(500_000.max(self.configured_bitrate / 8))
    }

    fn fps_floor(&self) -> u32 {
        self.configured_fps.min(8.max(self.configured_fps / 3))
    }

    fn reduce_load(&mut self) {
        if self.adapt_bitrate {
            self.target.bitrate = self
                .target
                .bitrate
                .saturating_mul(3)
                .saturating_div(4)
                .max(self.bitrate_floor());
        }
        self.target.fps = self
            .target
            .fps
            .saturating_mul(3)
            .saturating_div(4)
            .max(self.fps_floor());
    }

    fn increase_load(&mut self) {
        if self.adapt_bitrate {
            self.target.bitrate = self
                .target
                .bitrate
                .saturating_add((self.configured_bitrate / 10).max(100_000))
                .min(self.configured_bitrate);
        }
        self.target.fps = self.target.fps.saturating_add(1).min(self.configured_fps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overload_steps_down_then_recovers_only_after_healthy_acks() {
        let mut controller = AdaptiveRateController::new(8_000_000, 30, true, 0);
        let initial = controller.target();

        assert!(!controller.observe_ack(1, Some(Duration::from_millis(250)), 0, 0, 3, false,));
        assert!(controller.observe_ack(2, Some(Duration::from_millis(250)), 0, 0, 3, false,));
        let reduced = controller.target();
        assert!(reduced.bitrate < initial.bitrate);
        assert!(reduced.fps < initial.fps);

        for ack in 3..3 + u64::from(HEALTHY_ACKS_TO_INCREASE) - 1 {
            assert!(!controller.observe_ack(ack, Some(Duration::from_millis(40)), 0, 0, 3, false));
        }
        assert!(controller.observe_ack(
            3 + u64::from(HEALTHY_ACKS_TO_INCREASE) - 1,
            Some(Duration::from_millis(40)),
            0,
            0,
            3,
            false,
        ));
        assert!(controller.target().bitrate > reduced.bitrate);
        assert!(controller.target().fps > reduced.fps);
        assert!(controller.target().bitrate <= initial.bitrate);
        assert!(controller.target().fps <= initial.fps);

        assert!(controller.observe_ack(0, None, 0, 0, 3, false));
        assert_eq!(controller.target(), initial);
    }

    #[test]
    fn idle_repeated_feedback_and_suspended_ack_do_not_change_targets() {
        let mut controller = AdaptiveRateController::new(2_000_000, 20, true, 0);
        let initial = controller.target();
        for _ in 0..40 {
            assert!(!controller.observe_ack(0, None, 99, 99, 1, false));
        }
        assert!(!controller.observe_ack(1, None, 99, 99, 1, true));
        assert_eq!(controller.target(), initial);
    }

    #[test]
    fn targets_are_clamped_to_configured_limits_and_floors() {
        let mut controller = AdaptiveRateController::new(1_000_000, 12, true, 0);
        for ack in 1..40 {
            controller.observe_ack(ack, Some(Duration::from_secs(2)), 0, 0, 2, false);
        }
        assert_eq!(controller.target().bitrate, 500_000);
        assert_eq!(controller.target().fps, 8);

        controller.reset_for_new_session();
        assert_eq!(
            controller.target(),
            AdaptiveTarget {
                bitrate: 1_000_000,
                fps: 12
            }
        );
    }

    #[test]
    fn sustained_ack_backlog_steps_down_without_a_delay_sample() {
        let mut controller = AdaptiveRateController::new(6_000_000, 30, true, 0);
        assert!(!controller.observe_ack(1, None, 0, 0, 3, false));
        assert!(!controller.observe_ack(2, None, 0, 2, 3, false));
        assert!(controller.observe_ack(3, None, 0, 2, 3, false));
        assert!(controller.target().bitrate < 6_000_000);
        assert!(controller.target().fps < 30);
    }
}
