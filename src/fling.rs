//! Scroll momentum on the frame's clock.
//!
//! GPUI's touch recognizer flings on its own: on release it fits the
//! finger's velocity and then, on every frame, scrolls by the distance its
//! fling curve covered *up to `Instant::now()`*. Under a finger the content
//! follows positions resampled at the frame's time (`touch_resampler`) and
//! moves evenly; after the release it follows the time the loop woke up, which
//! wanders by a millisecond or two from vsync to vsync. At 120 Hz that is a
//! fifth of a frame: the fling's steps are uneven while the drag's were not.
//! GPUI also stamps each touch sample with the time it was dispatched, so the
//! velocity it fits is measured on that wandering clock as well.
//!
//! This module flings in the platform instead: the release velocity from the
//! raw touch moves with their event times, as Android's `VelocityTracker`
//! does (the resampled positions run up to a few milliseconds ahead of the
//! finger near a release and overstate it), and each frame at its vsync. GPUI's own fling is switched off (`min_fling_velocity`), and each
//! frame gets one `ScrollWheel` step, the curve's progress between two
//! vsyncs. The recognizer's rules are kept: one contact at a time, its axis
//! decided by its movement past the slop, the same velocity fit, ceiling and
//! physics. A contact that stops a fling is not a tap, as in GPUI.
//!
//! [`Clock::Wall`] steps by the time the frame runs, as GPUI does, to compare
//! the two on a device.
//!
//! Main thread only, like everything in GPUI.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};

/// The recognizer's touch slop, in logical pixels.
const SLOP: f32 = 8.;

/// The slowest release that flings, in logical pixels per second (GPUI's
/// `min_fling_velocity`).
const MIN_VELOCITY: f32 = 50.;

/// The fastest fling, in logical pixels per second (GPUI's and Android's).
const MAX_VELOCITY: f32 = 8000.;

/// How far back the release velocity looks.
const VELOCITY_WINDOW_NS: i64 = 100_000_000;

/// A pause longer than this between samples means the finger stopped.
const STOPPED_GAP_NS: i64 = 40_000_000;

const MAX_SAMPLES: usize = 20;

/// The exponential model never stops; below this speed it has.
const STOP_VELOCITY: f32 = 10.;

/// How the fling decelerates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Physics {
    /// Android's `OverScroller` friction spline, at 160 logical pixels per
    /// inch (GPUI's `ScrollPhysics::android`).
    Android,
    /// `UIScrollView`'s exponential decay, 0.998 per millisecond: a fling
    /// coasts longer.
    Ios,
}

/// What the fling's progress is measured by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clock {
    /// The vsync the frame is drawn for, and touch times: steps follow the
    /// display.
    Vsync,
    /// The moment the frame runs, as GPUI's own fling.
    Wall,
}

/// Whether the platform flings in GPUI's place: unless the environment
/// variable `GPUI_FLING` is `gpui` when the window opens.
pub fn enabled() -> bool {
    static GPUI: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    !*GPUI.get_or_init(|| std::env::var("GPUI_FLING").is_ok_and(|value| value == "gpui"))
}

/// The last fling launched: its speed (logical px/s), how long it coasts
/// (ms) and how far (logical px), for measuring.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Launch {
    pub speed: f32,
    pub duration_ms: f64,
    pub distance: f32,
}

static LAST: std::sync::Mutex<Option<Launch>> = std::sync::Mutex::new(None);

/// The last fling launched.
pub fn last() -> Option<Launch> {
    *LAST.lock().unwrap_or_else(|poison| poison.into_inner())
}

static PHYSICS: AtomicU8 = AtomicU8::new(0);
static CLOCK: AtomicU8 = AtomicU8::new(0);

/// The physics flings from now on take.
pub fn physics() -> Physics {
    match PHYSICS.load(Ordering::Relaxed) {
        1 => Physics::Ios,
        _ => Physics::Android,
    }
}

/// Sets the physics the next fling takes.
pub fn set_physics(physics: Physics) {
    PHYSICS.store(physics as u8, Ordering::Relaxed);
}

/// The clock flings step by.
pub fn clock() -> Clock {
    match CLOCK.load(Ordering::Relaxed) {
        1 => Clock::Wall,
        _ => Clock::Vsync,
    }
}

/// Sets the clock flings step by, the running one included.
pub fn set_clock(clock: Clock) {
    CLOCK.store(clock as u8, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Axis {
    Horizontal,
    Vertical,
}

/// A scroll step for GPUI, in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Step {
    /// Where the scrolled contact began: the hit-test point.
    pub position: (f32, f32),
    pub delta: (f32, f32),
    /// The last step of the fling.
    pub ended: bool,
}

struct Contact {
    id: i32,
    start: (f32, f32),
    axis: Option<Axis>,
    /// Whether this contact stopped a fling: it is no tap.
    caught: bool,
    /// `(time, x, y)`, oldest first.
    samples: VecDeque<(i64, f32, f32)>,
}

struct Momentum {
    position: (f32, f32),
    axis: Axis,
    /// +1 or −1 along the axis.
    sign: f32,
    speed: f32,
    physics: Physics,
    /// On the clock the fling was started by.
    started: i64,
    clock: Clock,
    /// Distance already stepped, in logical pixels.
    emitted: f32,
}

/// What GPUI should be told of a release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Release {
    /// The release as it is.
    Ended,
    /// A cancel in its place: the contact stopped a fling and did not move,
    /// so it must not tap.
    Cancelled,
}

/// One contact's flings.
#[derive(Default)]
pub(crate) struct Fling {
    contact: Option<Contact>,
    momentum: Option<Momentum>,
}

impl Fling {
    /// A contact begins at `time`. A running fling stops; the step returned
    /// closes its scroll stream.
    pub(crate) fn down(&mut self, id: i32, x: f32, y: f32, time: i64) -> Option<Step> {
        let stopped = self.momentum.take().map(|momentum| Step {
            position: momentum.position,
            delta: (0., 0.),
            ended: true,
        });
        if self.contact.is_none() {
            self.contact = Some(Contact {
                id,
                start: (x, y),
                axis: None,
                caught: stopped.is_some(),
                samples: VecDeque::from([(time, x, y)]),
            });
        }
        stopped
    }

    /// The contact is at `(x, y)` at `time`.
    pub(crate) fn moved(&mut self, id: i32, x: f32, y: f32, time: i64) {
        let Some(contact) = self.contact.as_mut().filter(|contact| contact.id == id) else {
            return;
        };
        if contact.axis.is_none() {
            let (dx, dy) = (x - contact.start.0, y - contact.start.1);
            if dx.hypot(dy) > SLOP {
                contact.axis = Some(if dx.abs() <= dy.abs() {
                    Axis::Vertical
                } else {
                    Axis::Horizontal
                });
            }
        }
        contact.samples.push_back((time, x, y));
        while contact.samples.len() > MAX_SAMPLES {
            contact.samples.pop_front();
        }
    }

    /// The contact lifts at `time`, its position on the touch clock; the
    /// fling, if it is fast enough, starts at `start`, on `clock`.
    pub(crate) fn up(&mut self, id: i32, time: i64, start: i64, clock: Clock) -> Release {
        let Some(contact) = self.contact.take_if(|contact| contact.id == id) else {
            return Release::Ended;
        };
        let Some(axis) = contact.axis else {
            return if contact.caught {
                Release::Cancelled
            } else {
                Release::Ended
            };
        };
        let stopped = contact
            .samples
            .back()
            .is_none_or(|(latest, ..)| time - latest > STOPPED_GAP_NS);
        if stopped {
            return Release::Ended;
        }
        let along: Vec<(i64, f32)> = contact
            .samples
            .iter()
            .map(|&(time, x, y)| {
                (
                    time,
                    match axis {
                        Axis::Horizontal => x,
                        Axis::Vertical => y,
                    },
                )
            })
            .collect();
        let velocity = velocity(&along);
        let speed = velocity.abs().min(MAX_VELOCITY);
        if speed >= MIN_VELOCITY {
            let physics = physics();
            let duration = duration(physics, speed);
            *LAST.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(Launch {
                speed: velocity.signum() * speed,
                duration_ms: duration * 1000.,
                distance: distance(physics, speed, duration),
            });
            self.momentum = Some(Momentum {
                position: contact.start,
                axis,
                sign: velocity.signum(),
                speed,
                physics,
                started: start,
                clock,
                emitted: 0.,
            });
        }
        Release::Ended
    }

    /// The contact is cancelled: nothing flings.
    pub(crate) fn cancel(&mut self, id: i32) {
        self.contact.take_if(|contact| contact.id == id);
    }

    /// Whether a fling runs.
    pub(crate) fn active(&self) -> bool {
        self.momentum.is_some()
    }

    /// The clock the running fling steps by.
    pub(crate) fn running_clock(&self) -> Option<Clock> {
        self.momentum.as_ref().map(|momentum| momentum.clock)
    }

    /// The step a frame at `time`, on the fling's clock, shows.
    pub(crate) fn frame(&mut self, time: i64) -> Option<Step> {
        let momentum = self.momentum.as_mut()?;
        let elapsed = (time - momentum.started).max(0) as f64 / 1e9;
        let duration = duration(momentum.physics, momentum.speed);
        let distance = distance(momentum.physics, momentum.speed, elapsed.min(duration));
        let step = (distance - momentum.emitted).max(0.);
        momentum.emitted = momentum.emitted.max(distance);
        let along = momentum.sign * step;
        let delta = match momentum.axis {
            Axis::Horizontal => (along, 0.),
            Axis::Vertical => (0., along),
        };
        let position = momentum.position;
        let ended = elapsed >= duration;
        if ended {
            self.momentum = None;
        }
        Some(Step {
            position,
            delta,
            ended,
        })
    }
}

/// The velocity at the newest sample, in logical pixels per second: a
/// second-degree least-squares fit over the trailing window, as GPUI's
/// recognizer (and Android's `lsq2`) does.
fn velocity(samples: &[(i64, f32)]) -> f32 {
    let Some(&(newest, _)) = samples.last() else {
        return 0.;
    };
    let mut times = Vec::new();
    let mut values = Vec::new();
    let mut previous = newest;
    for &(time, value) in samples.iter().rev() {
        if newest - time > VELOCITY_WINDOW_NS || previous - time > STOPPED_GAP_NS {
            break;
        }
        previous = time;
        times.push((time - newest) as f64 / 1e9);
        values.push(f64::from(value));
    }
    let endpoint = || {
        let elapsed = -times.last().copied().unwrap_or(0.);
        if elapsed <= f64::EPSILON {
            return 0.;
        }
        ((values[0] - values[values.len() - 1]) / elapsed) as f32
    };
    if times.len() < 3 {
        return endpoint();
    }
    quadratic_velocity(&times, &values).map_or_else(endpoint, |velocity| velocity as f32)
}

/// The linear coefficient of `value = a0 + a1·t + a2·t²` fitted by least
/// squares: the velocity at `t = 0`.
fn quadratic_velocity(times: &[f64], values: &[f64]) -> Option<f64> {
    let count = times.len() as f64;
    let (mut t1, mut t2, mut t3, mut t4) = (0., 0., 0., 0.);
    let (mut v, mut vt, mut vt2) = (0., 0., 0.);
    for (&time, &value) in times.iter().zip(values) {
        let squared = time * time;
        t1 += time;
        t2 += squared;
        t3 += squared * time;
        t4 += squared * squared;
        v += value;
        vt += value * time;
        vt2 += value * squared;
    }
    let determinant = count * (t2 * t4 - t3 * t3) - t1 * (t1 * t4 - t3 * t2) + t2 * (t1 * t3 - t2 * t2);
    if determinant.abs() < 1e-12 {
        return None;
    }
    let linear = count * (vt * t4 - t3 * vt2) - v * (t1 * t4 - t3 * t2) + t2 * (t1 * vt2 - vt * t2);
    Some(linear / determinant)
}

/// How long a fling released at `speed` coasts, in seconds.
fn duration(physics: Physics, speed: f32) -> f64 {
    match physics {
        Physics::Ios => {
            if speed <= STOP_VELOCITY {
                return 0.;
            }
            (f64::from(STOP_VELOCITY / speed)).ln() / IOS_DECAY_PER_MS.ln() / 1000.
        }
        Physics::Android => {
            if speed <= 0. {
                return 0.;
            }
            (spline::deceleration(speed) / (spline::deceleration_rate() - 1.)).exp()
        }
    }
}

const IOS_DECAY_PER_MS: f64 = 0.998;

/// How far a fling released at `speed` has gone after `elapsed` seconds, in
/// logical pixels. In closed form, so that the path does not depend on when
/// frames come.
fn distance(physics: Physics, speed: f32, elapsed: f64) -> f32 {
    let total = duration(physics, speed);
    if total <= 0. {
        return 0.;
    }
    let elapsed = elapsed.min(total);
    match physics {
        Physics::Ios => {
            let milliseconds = elapsed * 1000.;
            (f64::from(speed) / 1000. * (IOS_DECAY_PER_MS.powf(milliseconds) - 1.)
                / IOS_DECAY_PER_MS.ln()) as f32
        }
        Physics::Android => {
            let rate = spline::deceleration_rate();
            let distance = spline::FRICTION
                * spline::physical_coefficient()
                * (rate / (rate - 1.) * spline::deceleration(speed)).exp();
            distance as f32 * spline::position((elapsed / total) as f32)
        }
    }
}

/// The fling model of Android's `OverScroller.SplineOverScroller`, as GPUI
/// transcribes it from AOSP (Apache-2.0).
mod spline {
    use std::sync::LazyLock;

    const SAMPLES: usize = 100;
    const INFLEXION: f32 = 0.35;
    const START_TENSION: f32 = 0.5;
    const END_TENSION: f32 = 1.0;
    const P1: f32 = START_TENSION * INFLEXION;
    const P2: f32 = 1.0 - END_TENSION * (1.0 - INFLEXION);

    /// `ViewConfiguration.getScrollFriction()`.
    pub(super) const FRICTION: f64 = 0.015;

    /// Logical pixels are density-independent pixels: 160 per inch.
    const PIXELS_PER_INCH: f64 = 160.;

    /// `ln(0.78) / ln(0.9)`.
    pub(super) fn deceleration_rate() -> f64 {
        0.78f64.ln() / 0.9f64.ln()
    }

    /// Gravity in pixels, times AOSP's look-and-feel factor.
    pub(super) fn physical_coefficient() -> f64 {
        9.80665 * 39.37 * PIXELS_PER_INCH * 0.84
    }

    pub(super) fn deceleration(speed: f32) -> f64 {
        (f64::from(INFLEXION) * f64::from(speed) / (FRICTION * physical_coefficient())).ln()
    }

    /// The fractional distance at 101 evenly spaced fractions of the
    /// duration, from AOSP's cubic Bézier.
    static POSITION: LazyLock<[f32; SAMPLES + 1]> = LazyLock::new(|| {
        let mut position = [0f32; SAMPLES + 1];
        let mut x_min = 0f32;
        for (i, sample) in position.iter_mut().take(SAMPLES).enumerate() {
            let alpha = i as f32 / SAMPLES as f32;
            let mut x_max = 1f32;
            let (x, coefficient) = loop {
                let x = x_min + (x_max - x_min) / 2.;
                let coefficient = 3. * x * (1. - x);
                let time = coefficient * ((1. - x) * P1 + x * P2) + x * x * x;
                if (time - alpha).abs() < 1e-5 {
                    break (x, coefficient);
                }
                if time > alpha {
                    x_max = x;
                } else {
                    x_min = x;
                }
            };
            *sample = coefficient * ((1. - x) * START_TENSION + x) + x * x * x;
        }
        position[SAMPLES] = 1.;
        position
    });

    /// The fraction of the distance covered at fraction `time` of the
    /// duration.
    pub(super) fn position(time: f32) -> f32 {
        if time >= 1. {
            return 1.;
        }
        let index = ((SAMPLES as f32 * time) as usize).min(SAMPLES - 1);
        let lower = index as f32 / SAMPLES as f32;
        let upper = (index + 1) as f32 / SAMPLES as f32;
        let slope = (POSITION[index + 1] - POSITION[index]) / (upper - lower);
        POSITION[index] + (time - lower) * slope
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    /// A swipe down at 2 px/ms, one sample every 8 ms, released at 80 ms.
    fn swipe(fling: &mut Fling) {
        fling.down(0, 100., 100., 0);
        for n in 1..=10 {
            fling.moved(0, 100., 100. + 16. * n as f32, n * 8 * MS);
        }
    }

    #[test]
    fn a_swipe_flings_at_the_fingers_speed() {
        let mut fling = Fling::default();
        swipe(&mut fling);
        assert_eq!(fling.up(0, 82 * MS, 82 * MS, Clock::Vsync), Release::Ended);
        let momentum = fling.momentum.as_ref().expect("a fling");
        assert_eq!(momentum.axis, Axis::Vertical);
        assert_eq!(momentum.sign, 1.);
        assert!((momentum.speed - 2000.).abs() < 1., "{}", momentum.speed);
    }

    #[test]
    fn steps_follow_the_frames_time_only() {
        // Frames 8 ms apart give the curve's progress over 8 ms each,
        // whenever they run.
        let mut fling = Fling::default();
        swipe(&mut fling);
        fling.up(0, 80 * MS, 80 * MS, Clock::Vsync);
        let speed = fling.momentum.as_ref().expect("a fling").speed;
        let steps: Vec<f32> = (1..=5)
            .map(|n| fling.frame(80 * MS + n * 8 * MS).expect("a step").delta.1)
            .collect();
        for n in 1..=5 {
            let elapsed = |n: i64| (n * 8) as f64 / 1000.;
            // The spline's table starts a hair above zero: the first step
            // is from the release, not from the curve's value at zero.
            let before = if n == 1 { 0. } else { distance(Physics::Android, speed, elapsed(n - 1)) };
            let expected = distance(Physics::Android, speed, elapsed(n)) - before;
            assert!((steps[n as usize - 1] - expected).abs() < 1e-3, "{n}: {} vs {expected}", steps[n as usize - 1]);
        }
        assert!(steps.windows(2).all(|pair| pair[1] < pair[0]), "{steps:?}");
    }

    #[test]
    fn the_fling_ends_on_its_whole_distance() {
        let mut fling = Fling::default();
        swipe(&mut fling);
        fling.up(0, 80 * MS, 80 * MS, Clock::Vsync);
        let total = distance(Physics::Android, fling.momentum.as_ref().unwrap().speed, 10.);
        let mut travelled = 0.;
        let mut time = 80 * MS;
        loop {
            time += 8 * MS;
            let step = fling.frame(time).expect("a step");
            travelled += step.delta.1;
            assert_eq!(step.delta.0, 0.);
            if step.ended {
                break;
            }
        }
        assert!((travelled - total).abs() < 0.5, "{travelled} of {total}");
        assert!(!fling.active());
    }

    #[test]
    fn a_resting_finger_does_not_fling() {
        let mut fling = Fling::default();
        swipe(&mut fling);
        fling.up(0, 80 * MS + STOPPED_GAP_NS + 1, 0, Clock::Vsync);
        assert!(!fling.active());
    }

    #[test]
    fn a_tap_does_not_fling() {
        let mut fling = Fling::default();
        fling.down(0, 10., 10., 0);
        fling.moved(0, 12., 13., 8 * MS);
        assert_eq!(fling.up(0, 16 * MS, 16 * MS, Clock::Vsync), Release::Ended);
        assert!(!fling.active());
    }

    #[test]
    fn a_contact_that_stops_a_fling_is_no_tap() {
        let mut fling = Fling::default();
        swipe(&mut fling);
        fling.up(0, 80 * MS, 80 * MS, Clock::Vsync);
        let stop = fling.down(1, 50., 50., 100 * MS).expect("the fling's end");
        assert!(stop.ended);
        assert_eq!(stop.delta, (0., 0.));
        assert!(!fling.active());
        assert_eq!(fling.up(1, 150 * MS, 150 * MS, Clock::Vsync), Release::Cancelled);
    }

    #[test]
    fn a_sideways_swipe_flings_sideways() {
        let mut fling = Fling::default();
        fling.down(0, 300., 100., 0);
        for n in 1..=10 {
            fling.moved(0, 300. - 20. * n as f32, 100. + n as f32, n * 8 * MS);
        }
        fling.up(0, 80 * MS, 80 * MS, Clock::Vsync);
        let step = fling.frame(88 * MS).expect("a step");
        assert!(step.delta.0 < 0.);
        assert_eq!(step.delta.1, 0.);
        assert_eq!(step.position, (300., 100.));
    }

    #[test]
    fn a_second_finger_is_not_the_contact() {
        let mut fling = Fling::default();
        fling.down(0, 0., 0., 0);
        fling.down(1, 0., 0., 0);
        for n in 1..=10 {
            fling.moved(1, 0., 20. * n as f32, n * 8 * MS);
        }
        fling.up(1, 80 * MS, 80 * MS, Clock::Vsync);
        assert!(!fling.active());
    }

    #[test]
    fn ios_physics_coasts_longer() {
        assert!(duration(Physics::Ios, 2000.) > duration(Physics::Android, 2000.));
        assert!(distance(Physics::Ios, 2000., 10.) > distance(Physics::Android, 2000., 10.));
    }
}
