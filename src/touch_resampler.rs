//! One touch position per frame, resampled at the frame's time.
//!
//! A touchscreen reports at its own rate (120–240 Hz) and the display draws
//! at another (60–120 Hz), and the samples reach the loop in bursts. GPUI
//! scrolls by every `Moved` as it is dispatched, so a frame that happens to
//! collect one sample scrolls by one step and the next, collecting three,
//! by three: content under a finger moving at a constant speed advances
//! unevenly from frame to frame, and the scroll looks jerky however fast
//! frames are drawn. GPUI also stamps each sample with the time it was
//! dispatched; a burst of samples stamped within a millisecond of each other
//! throws off the velocity a fling starts with.
//!
//! Android's own views avoid both by resampling (`InputConsumer`): moves are
//! held until the frame, and each frame gets one position per pointer,
//! interpolated between the two samples around the frame's time less a
//! small latency, or extrapolated a little past the newest one. This module
//! does the same for GPUI. Every position it hands out belongs to a time
//! that advances evenly with the frames, so steps follow the finger's speed
//! and the velocity GPUI fits is the finger's.
//!
//! `Down` passes at once (a tap must not wait for a frame); `Up` passes at
//! once at the position resampled for its own time, which stays on the same
//! trajectory as the frames before it; `Cancel` passes at once. Times are
//! nanoseconds of the monotonic clock: Android's event times and vsync times
//! are both on it.
//!
//! Main thread only, like everything in GPUI.

use std::collections::{HashMap, VecDeque};

/// How far behind the frame's time positions are resampled: enough for a
/// sample after that time to have usually arrived, so the position is
/// interpolated rather than guessed (Android's `RESAMPLE_LATENCY`).
pub(crate) const RESAMPLE_LATENCY_NS: i64 = 5_000_000;

/// Two samples closer than this say too little about speed to extrapolate
/// from (Android's `RESAMPLE_MIN_DELTA`).
const MIN_DELTA_NS: i64 = 2_000_000;

/// Two samples further apart than this say nothing about the speed now
/// (Android's `RESAMPLE_MAX_DELTA`).
const MAX_DELTA_NS: i64 = 20_000_000;

/// The furthest a position is extrapolated past the newest sample (Android's
/// `RESAMPLE_MAX_PREDICTION`).
const MAX_PREDICTION_NS: i64 = 8_000_000;

/// Samples kept per pointer; a frame never needs more than the two around
/// its time, and the rest only cover a stalled loop.
const MAX_SAMPLES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Phase {
    Down,
    Move,
    Up,
    Cancel,
}

/// A raw sample from the platform, or a resampled one for GPUI.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Sample {
    pub id: i32,
    pub phase: Phase,
    pub x: f32,
    pub y: f32,
    /// Monotonic nanoseconds.
    pub time: i64,
}

#[derive(Clone, Copy, Debug)]
struct Point {
    time: i64,
    x: f32,
    y: f32,
}

struct Track {
    /// Raw samples, oldest first.
    samples: VecDeque<Point>,
    /// The time the last position handed out stands for.
    shown_time: i64,
    shown: (f32, f32),
}

#[derive(Default)]
pub(crate) struct TouchResampler {
    tracks: HashMap<i32, Track>,
}

impl TouchResampler {
    /// Takes one raw sample and returns what is to be dispatched now: the
    /// sample itself for `Down` and `Cancel`, the resampled release for `Up`,
    /// nothing for `Move` (it waits for [`frame`](Self::frame)).
    pub(crate) fn push(&mut self, sample: Sample) -> Option<Sample> {
        let point = Point {
            time: sample.time,
            x: sample.x,
            y: sample.y,
        };
        match sample.phase {
            Phase::Down => {
                self.tracks.insert(
                    sample.id,
                    Track {
                        samples: VecDeque::from([point]),
                        shown_time: sample.time,
                        shown: (sample.x, sample.y),
                    },
                );
                Some(sample)
            }
            Phase::Move => {
                let Some(track) = self.tracks.get_mut(&sample.id) else {
                    // A move without its down (the loop started mid-gesture):
                    // nothing to resample against, pass it as it is.
                    return Some(sample);
                };
                // Samples out of order (equal times from a host without event
                // times) keep the order they came in.
                let time = track
                    .samples
                    .back()
                    .map_or(point.time, |last| point.time.max(last.time));
                track.samples.push_back(Point { time, ..point });
                while track.samples.len() > MAX_SAMPLES {
                    track.samples.pop_front();
                }
                None
            }
            Phase::Up => {
                let Some(mut track) = self.tracks.remove(&sample.id) else {
                    return Some(sample);
                };
                track.samples.push_back(point);
                let (x, y) = track.position_at(sample.time - RESAMPLE_LATENCY_NS);
                Some(Sample { x, y, ..sample })
            }
            Phase::Cancel => {
                self.tracks.remove(&sample.id);
                Some(sample)
            }
        }
    }

    /// The moves to dispatch before drawing a frame at `frame_time`: one per
    /// pointer whose resampled position changed.
    pub(crate) fn frame(&mut self, frame_time: i64) -> Vec<Sample> {
        let target = frame_time - RESAMPLE_LATENCY_NS;
        let mut moves = Vec::new();
        for (&id, track) in &mut self.tracks {
            let Some(newest) = track.samples.back().copied() else {
                continue;
            };
            // Nothing new since the last frame: keep the position shown. (An
            // extrapolated one included: going back to the newest sample
            // would step the content backwards while the finger rests.)
            if newest.time <= track.shown_time || target <= track.shown_time {
                continue;
            }
            let (x, y) = track.position_at(target);
            track.shown_time = if target - newest.time > MAX_PREDICTION_NS {
                newest.time
            } else {
                target.min(track.extrapolation_limit())
            };
            // Drop samples no later frame interpolates from.
            while track.samples.len() > 2 && track.samples[1].time <= target {
                track.samples.pop_front();
            }
            if (x, y) != track.shown {
                track.shown = (x, y);
                moves.push(Sample {
                    id,
                    phase: Phase::Move,
                    x,
                    y,
                    time: target,
                });
            }
        }
        moves.sort_by_key(|sample| sample.id);
        moves
    }

    /// Whether a later frame still has movement to show: a sample newer than
    /// the time last shown.
    pub(crate) fn pending(&self) -> bool {
        self.tracks.values().any(|track| {
            track
                .samples
                .back()
                .is_some_and(|newest| newest.time > track.shown_time)
        })
    }
}

impl Track {
    /// The latest time a position can stand for: the newest sample, or a
    /// little past it when the two newest show a speed to go on.
    fn extrapolation_limit(&self) -> i64 {
        let len = self.samples.len();
        let newest = self.samples[len - 1];
        if len < 2 {
            return newest.time;
        }
        let delta = newest.time - self.samples[len - 2].time;
        if !(MIN_DELTA_NS..=MAX_DELTA_NS).contains(&delta) {
            return newest.time;
        }
        newest.time + MAX_PREDICTION_NS.min(delta / 2)
    }

    fn position_at(&self, time: i64) -> (f32, f32) {
        let len = self.samples.len();
        let newest = self.samples[len - 1];
        if time >= newest.time {
            if len < 2 {
                return (newest.x, newest.y);
            }
            // Samples that old say the finger may have stopped since: no
            // guessing past them.
            if time - newest.time > MAX_PREDICTION_NS {
                return (newest.x, newest.y);
            }
            let before = self.samples[len - 2];
            let time = time.min(self.extrapolation_limit());
            if time == newest.time {
                return (newest.x, newest.y);
            }
            return lerp(before, newest, time);
        }
        let oldest = self.samples[0];
        if time <= oldest.time {
            return (oldest.x, oldest.y);
        }
        let after = self
            .samples
            .iter()
            .position(|point| point.time >= time)
            .unwrap_or(len - 1);
        lerp(self.samples[after - 1], self.samples[after], time)
    }
}

/// The position at `time` on the line through `a` and `b`.
fn lerp(a: Point, b: Point, time: i64) -> (f32, f32) {
    let span = b.time - a.time;
    if span <= 0 {
        return (b.x, b.y);
    }
    let alpha = (time - a.time) as f32 / span as f32;
    (a.x + (b.x - a.x) * alpha, a.y + (b.y - a.y) * alpha)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    fn sample(phase: Phase, y: f32, time: i64) -> Sample {
        Sample {
            id: 0,
            phase,
            x: 0.,
            y,
            time,
        }
    }

    /// A finger moving at 1 px/ms, sampled every `touch` ms with arrival in
    /// bursts, drawn every `frame` ms: the steps between frames.
    fn steps(touch: i64, frame: i64) -> Vec<f32> {
        let mut resampler = TouchResampler::default();
        resampler.push(sample(Phase::Down, 0., 0));
        let mut next_touch = touch;
        let mut shown = Vec::new();
        for n in 1..=30 {
            let frame_time = n * frame;
            // Every sample up to the frame has arrived.
            while next_touch <= frame_time {
                resampler.push(sample(
                    Phase::Move,
                    next_touch as f32 / MS as f32,
                    next_touch,
                ));
                next_touch += touch;
            }
            for moved in resampler.frame(frame_time) {
                shown.push(moved.y);
            }
        }
        shown.windows(2).map(|pair| pair[1] - pair[0]).collect()
    }

    #[test]
    fn steps_are_even_when_the_rates_do_not_divide() {
        // 8.33 ms frames over 6 ms samples: unresampled, frames collect one
        // or two samples and step 6 or 12 px.
        for step in steps(6 * MS, 8_333_333).into_iter().skip(2) {
            assert!((step - 8.333).abs() < 0.01, "step {step}");
        }
        for step in steps(4 * MS, 16_666_667).into_iter().skip(2) {
            assert!((step - 16.667).abs() < 0.01, "step {step}");
        }
    }

    #[test]
    fn a_position_is_interpolated_between_the_samples_around_the_frame() {
        let mut resampler = TouchResampler::default();
        resampler.push(sample(Phase::Down, 0., 0));
        resampler.push(sample(Phase::Move, 10., 10 * MS));
        resampler.push(sample(Phase::Move, 30., 20 * MS));
        // 20 ms frame → 15 ms target, halfway between 10 and 30.
        assert_eq!(
            resampler.frame(20 * MS),
            vec![sample(Phase::Move, 20., 15 * MS)]
        );
    }

    #[test]
    fn extrapolation_stops_at_the_prediction_limit() {
        let mut resampler = TouchResampler::default();
        resampler.push(sample(Phase::Down, 0., 0));
        resampler.push(sample(Phase::Move, 10., 10 * MS));
        resampler.push(sample(Phase::Move, 20., 20 * MS));
        // Target 27 ms, newest 20 ms: at most min(8, 10 / 2) = 5 ms past it.
        let moved = resampler.frame(32 * MS);
        assert_eq!(moved.len(), 1);
        assert!((moved[0].y - 25.).abs() < 1e-4);
        assert!(!resampler.pending());
    }

    #[test]
    fn samples_older_than_the_prediction_are_not_extrapolated() {
        let mut resampler = TouchResampler::default();
        resampler.push(sample(Phase::Down, 0., 0));
        resampler.push(sample(Phase::Move, 10., 10 * MS));
        resampler.push(sample(Phase::Move, 20., 20 * MS));
        assert_eq!(
            resampler.frame(50 * MS),
            vec![sample(Phase::Move, 20., 45 * MS)]
        );
        assert!(!resampler.pending());
    }

    #[test]
    fn a_resting_finger_does_not_move_the_content_back() {
        let mut resampler = TouchResampler::default();
        resampler.push(sample(Phase::Down, 0., 0));
        resampler.push(sample(Phase::Move, 10., 10 * MS));
        resampler.push(sample(Phase::Move, 20., 20 * MS));
        resampler.frame(32 * MS);
        assert!(resampler.frame(66 * MS).is_empty());
        assert!(resampler.frame(83 * MS).is_empty());
    }

    #[test]
    fn movement_behind_the_frame_waits_for_the_next_one() {
        let mut resampler = TouchResampler::default();
        resampler.push(sample(Phase::Down, 0., 0));
        resampler.push(sample(Phase::Move, 10., 10 * MS));
        resampler.push(sample(Phase::Move, 20., 20 * MS));
        resampler.frame(15 * MS);
        assert!(resampler.pending());
        assert_eq!(
            resampler.frame(25 * MS),
            vec![sample(Phase::Move, 20., 20 * MS)]
        );
        assert!(!resampler.pending());
    }

    #[test]
    fn down_and_cancel_pass_at_once() {
        let mut resampler = TouchResampler::default();
        let down = sample(Phase::Down, 3., 0);
        assert_eq!(resampler.push(down), Some(down));
        assert_eq!(resampler.push(sample(Phase::Move, 9., MS)), None);
        let cancel = sample(Phase::Cancel, 9., 2 * MS);
        assert_eq!(resampler.push(cancel), Some(cancel));
        assert!(resampler.frame(30 * MS).is_empty());
    }

    #[test]
    fn the_release_stays_on_the_trajectory_of_the_frames() {
        let mut resampler = TouchResampler::default();
        resampler.push(sample(Phase::Down, 0., 0));
        for t in 1..=20 {
            resampler.push(sample(Phase::Move, t as f32 * 4., t * 4 * MS));
        }
        let up = resampler.push(sample(Phase::Up, 84., 84 * MS)).unwrap();
        assert_eq!(up.phase, Phase::Up);
        // The finger at 79 ms, as the frames show it 5 ms late.
        assert!((up.y - 79.).abs() < 1e-4, "{}", up.y);
    }

    #[test]
    fn a_tap_is_untouched() {
        let mut resampler = TouchResampler::default();
        resampler.push(sample(Phase::Down, 5., 0));
        let up = resampler.push(sample(Phase::Up, 5., 80 * MS)).unwrap();
        assert_eq!(up.y, 5.);
    }

    #[test]
    fn pointers_are_resampled_apart() {
        let mut resampler = TouchResampler::default();
        resampler.push(Sample {
            id: 1,
            ..sample(Phase::Down, 0., 0)
        });
        resampler.push(Sample {
            id: 2,
            ..sample(Phase::Down, 100., 0)
        });
        resampler.push(Sample {
            id: 1,
            ..sample(Phase::Move, 10., 10 * MS)
        });
        resampler.push(Sample {
            id: 2,
            ..sample(Phase::Move, 80., 10 * MS)
        });
        let moved = resampler.frame(15 * MS);
        assert_eq!(
            moved.iter().map(|m| (m.id, m.y)).collect::<Vec<_>>(),
            vec![(1, 10.), (2, 80.)]
        );
    }
}
