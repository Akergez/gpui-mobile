//! Frame demand gated to a vsync source, for hosts that run their own loop
//! (the Android loops in `android::jni` and `android::host`).
//!
//! [`frame_demand::FrameDemand`](crate::frame_demand::FrameDemand) is the
//! shape for a host whose frame source ticks by itself (a `CADisplayLink`):
//! demand resumes the source, the source ticks until a tick finds no demand.
//! A loop that polls an `ALooper` has no source of its own; it wants to know
//! *when* to draw. `FramePacer` answers that: demand posts one vsync callback
//! (`AChoreographer` on Android), the callback marks a frame as due, and the
//! loop draws exactly one frame per marked vsync. Demand raised while a
//! callback is posted or a frame is already due is folded into it, so a
//! window that is notified fifty times between two vsyncs still draws once.
//!
//! Every frame is drawn on a vsync, and stands for that vsync's time
//! ([`frame_vsync`](FramePacer::frame_vsync)): content moved by a finger or
//! a fling advances by the time between two vsyncs, which the screen shows
//! evenly. Only the first frame after an idle period ([`IDLE`]) is served at
//! once, so that a tap answers without waiting.
//!
//! A frame that takes longer than the display's refresh interval cannot be
//! drawn on every vsync. Drawing it whenever the previous one is done gives
//! frames one, two or three vsyncs apart in turn, and the content jumps
//! unevenly. The pacer instead draws on every second (or third) vsync while
//! frames need it — the divisor follows the 90th percentile of the latest
//! frames' durations, rising at once and falling only after a calm stretch —
//! so a phone that cannot hold 120 Hz holds an even 60. The refresh interval
//! is measured from the vsyncs' own times.
//!
//! Without a vsync source (no looper on the thread, tests) the pacer falls
//! back to a wall clock: demand becomes due one frame interval after the
//! previous frame, and [`poll_timeout`](Self::poll_timeout) tells the loop
//! how long it may sleep before then.
//!
//! Main thread only, like everything in GPUI.

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    time::{Duration, Instant},
};

/// No frame for this long is an idle period: the next frame is served at
/// once instead of on a vsync.
pub(crate) const IDLE: Duration = Duration::from_millis(100);

/// How many of the latest frames' durations choose the divisor.
const DURATIONS: usize = 30;

/// Frames in a row that would fit a smaller divisor before it is taken.
const CALM: u32 = 60;

/// The most vsyncs a frame is spread over.
const MAX_DIVISOR: u32 = 3;

/// Posts one vsync callback that will call [`FramePacer::on_vsync`].
pub(crate) type PostVsync = Box<dyn Fn()>;

pub(crate) struct FramePacer {
    /// `None` paces by wall clock at `interval`.
    post_vsync: Option<PostVsync>,
    /// The spacing of the fallback clock, and the refresh interval assumed
    /// before vsyncs have measured it.
    interval: Duration,
    /// The display's refresh interval, in nanoseconds, as measured between
    /// vsyncs.
    period: Cell<i64>,
    /// The latest vsync seen.
    last_vsync: Cell<Option<i64>>,
    /// The vsync the due frame stands for (`None`: served at once).
    due_vsync: Cell<Option<i64>>,
    /// The vsync the latest frame taken stands for.
    frame_vsync: Cell<Option<i64>>,
    /// Frames are drawn on every `divisor`-th vsync.
    divisor: Cell<u32>,
    /// A fixed divisor instead of the measured one; 0 is measured.
    forced_divisor: Cell<u32>,
    /// The latest frames' durations, oldest first.
    durations: RefCell<VecDeque<Duration>>,
    /// Frames in a row that would have fit a smaller divisor.
    calm: Cell<u32>,
    /// A vsync callback is posted and has not fired yet.
    posted: Cell<bool>,
    /// A vsync fired (or the fallback clock ran out) and the frame it owes has
    /// not been taken yet.
    due: Cell<bool>,
    /// Fallback only: demand is waiting for this instant.
    fallback_due_at: Cell<Option<Instant>>,
    /// Fallback only: when the previous frame was taken, to space the next.
    last_frame_at: Cell<Option<Instant>>,
}

impl FramePacer {
    /// A pacer driven by a vsync source: `post_vsync` posts one callback that
    /// must end in [`on_vsync`](Self::on_vsync). `interval` is the display's
    /// refresh interval.
    pub(crate) fn with_vsync(post_vsync: PostVsync, interval: Duration) -> Self {
        Self {
            post_vsync: Some(post_vsync),
            interval,
            period: Cell::new(interval.as_nanos() as i64),
            last_vsync: Cell::new(None),
            due_vsync: Cell::new(None),
            frame_vsync: Cell::new(None),
            divisor: Cell::new(1),
            forced_divisor: Cell::new(0),
            durations: RefCell::new(VecDeque::with_capacity(DURATIONS)),
            calm: Cell::new(0),
            posted: Cell::new(false),
            due: Cell::new(false),
            fallback_due_at: Cell::new(None),
            last_frame_at: Cell::new(None),
        }
    }

    /// A pacer without a vsync source, spacing frames `interval` apart.
    pub(crate) fn with_clock(interval: Duration) -> Self {
        Self {
            post_vsync: None,
            interval,
            period: Cell::new(interval.as_nanos() as i64),
            last_vsync: Cell::new(None),
            due_vsync: Cell::new(None),
            frame_vsync: Cell::new(None),
            divisor: Cell::new(1),
            forced_divisor: Cell::new(0),
            durations: RefCell::new(VecDeque::with_capacity(DURATIONS)),
            calm: Cell::new(0),
            posted: Cell::new(false),
            due: Cell::new(false),
            fallback_due_at: Cell::new(None),
            last_frame_at: Cell::new(None),
        }
    }

    /// Records demand for a frame. The first demand since the last frame
    /// posts a vsync callback; later ones ride on it. Demand after an idle
    /// period is due at once (see the module docs).
    pub(crate) fn schedule(&self, now: Instant) {
        if self.posted.get() || self.due.get() || self.fallback_due_at.get().is_some() {
            return;
        }
        let idle = self
            .last_frame_at
            .get()
            .is_none_or(|last| now.duration_since(last) >= IDLE);
        match &self.post_vsync {
            Some(_) if idle => {
                self.due_vsync.set(None);
                self.due.set(true);
            }
            Some(post) => {
                self.posted.set(true);
                post();
            }
            None => {
                let earliest = self
                    .last_frame_at
                    .get()
                    .map_or(now, |last| last + self.interval);
                self.fallback_due_at.set(Some(earliest.max(now)));
            }
        }
    }

    /// The posted vsync callback fired for the vsync at `vsync`
    /// (nanoseconds): the frame it owes is now due, unless the divisor skips
    /// this vsync, in which case the demand waits for the next one.
    pub(crate) fn on_vsync(&self, vsync: i64) {
        self.posted.set(false);
        self.measure_period(vsync);
        let period = self.period.get();
        let divisor = i64::from(self.divisor());
        let skip = self
            .frame_vsync
            .get()
            .is_some_and(|drawn| vsync - drawn < divisor * period - period / 2);
        if skip && !self.due.get() {
            if let Some(post) = &self.post_vsync {
                self.posted.set(true);
                post();
            }
            return;
        }
        if !self.due.get() {
            self.due_vsync.set(Some(vsync));
        }
        self.due.set(true);
    }

    fn measure_period(&self, vsync: i64) {
        if let Some(previous) = self.last_vsync.replace(Some(vsync)) {
            let delta = vsync - previous;
            let period = self.period.get();
            // Two vsyncs in a row: one refresh interval. Further apart (a
            // frame skipped, an idle stretch) says nothing.
            if delta > 2_000_000 && delta < period * 3 / 2 {
                if delta < period * 9 / 10 {
                    self.period.set(delta);
                } else {
                    self.period.set((period * 7 + delta) / 8);
                }
            }
        }
    }

    /// The divisor in use: the forced one, or the measured one.
    pub(crate) fn divisor(&self) -> u32 {
        match self.forced_divisor.get() {
            0 => self.divisor.get(),
            forced => forced,
        }
    }

    /// Forces frames onto every `divisor`-th vsync; 0 measures it again.
    pub(crate) fn force_divisor(&self, divisor: u32) {
        self.forced_divisor.set(divisor.min(MAX_DIVISOR));
    }

    /// The display's refresh interval as measured, in nanoseconds.
    pub(crate) fn period(&self) -> i64 {
        self.period.get()
    }

    /// The vsync the frame being drawn stands for; `None` for a frame served
    /// at once.
    pub(crate) fn frame_vsync(&self) -> Option<i64> {
        self.frame_vsync.get()
    }

    /// The frame taken last took `duration`: the divisor follows the 90th
    /// percentile of the latest ones, rising at once, falling after
    /// [`CALM`] frames that would all have fit the smaller one.
    pub(crate) fn end_frame(&self, duration: Duration) {
        let mut durations = self.durations.borrow_mut();
        if durations.len() == DURATIONS {
            durations.pop_front();
        }
        durations.push_back(duration);
        let mut sorted: Vec<Duration> = durations.iter().copied().collect();
        sorted.sort_unstable();
        let p90 = sorted[(sorted.len() * 9 / 10).min(sorted.len() - 1)];
        // A frame fits a vsync with a little to spare for the loop around it.
        let budget = self.period.get() as f64 * 0.9;
        let needed = ((p90.as_nanos() as f64 / budget).ceil() as u32).clamp(1, MAX_DIVISOR);
        let divisor = self.divisor.get();
        if needed > divisor {
            self.divisor.set(needed);
            self.calm.set(0);
        } else if needed < divisor {
            let calm = self.calm.get() + 1;
            if calm >= CALM {
                self.divisor.set(divisor - 1);
                self.calm.set(0);
            } else {
                self.calm.set(calm);
            }
        } else {
            self.calm.set(0);
        }
    }

    /// Takes the due frame, if any. The loop draws when this returns `true`
    /// and only then; a frame left untaken (window inactive) stays due.
    pub(crate) fn take_frame(&self, now: Instant) -> bool {
        if self.due.replace(false) {
            self.last_frame_at.set(Some(now));
            self.frame_vsync.set(self.due_vsync.take());
            return true;
        }
        match self.fallback_due_at.get() {
            Some(due_at) if due_at <= now => {
                self.fallback_due_at.set(None);
                self.last_frame_at.set(Some(now));
                self.frame_vsync.set(None);
                true
            }
            _ => false,
        }
    }

    /// Whether a frame is wanted: posted, due, or waiting on the fallback
    /// clock.
    #[cfg(test)]
    fn has_demand(&self) -> bool {
        self.posted.get() || self.due.get() || self.fallback_due_at.get().is_some()
    }

    /// Forgets a posted callback (the vsync source may have been paused
    /// while the app was in the background) and posts afresh so demand that
    /// piled up meanwhile is served. A stale callback that fires later marks
    /// one extra frame due, which GPUI answers with a no-op when nothing is
    /// dirty.
    pub(crate) fn resume(&self, now: Instant) {
        self.posted.set(false);
        self.schedule(now);
    }

    /// How long the loop may block before it has something to do: zero while
    /// a frame is due, the time until the fallback frame otherwise, and
    /// `None` when only the vsync callback (delivered through the looper) or
    /// an unrelated wakeup can bring work.
    pub(crate) fn poll_timeout(&self, now: Instant) -> Option<Duration> {
        if self.due.get() {
            return Some(Duration::ZERO);
        }
        self.fallback_due_at
            .get()
            .map(|due_at| due_at.saturating_duration_since(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    const INTERVAL: Duration = Duration::from_millis(16);
    /// Some vsync's time, nanoseconds.
    const VSYNC: i64 = 1_000_000_000;
    const MS: i64 = 1_000_000;

    /// A vsync pacer that has just drawn a frame at `now`, so the next
    /// demand is inside the interval and waits for a vsync.
    fn vsync_pacer_after_frame(now: Instant) -> (FramePacer, Rc<Cell<u32>>) {
        let posts = Rc::new(Cell::new(0));
        let counter = Rc::clone(&posts);
        let pacer =
            FramePacer::with_vsync(Box::new(move || counter.set(counter.get() + 1)), INTERVAL);
        pacer.schedule(now);
        assert!(pacer.take_frame(now), "the first frame is served at once");
        (pacer, posts)
    }

    #[test]
    fn first_frame_does_not_wait_for_a_vsync() {
        let posts = Rc::new(Cell::new(0));
        let counter = Rc::clone(&posts);
        let pacer =
            FramePacer::with_vsync(Box::new(move || counter.set(counter.get() + 1)), INTERVAL);
        let now = Instant::now();

        pacer.schedule(now);

        assert_eq!(posts.get(), 0);
        assert_eq!(pacer.poll_timeout(now), Some(Duration::ZERO));
        assert!(pacer.take_frame(now));
    }

    #[test]
    fn demand_posts_one_callback_until_it_fires() {
        let now = Instant::now();
        let (pacer, posts) = vsync_pacer_after_frame(now);
        let now = now + Duration::from_millis(1);

        pacer.schedule(now);
        pacer.schedule(now);
        pacer.schedule(now);

        assert_eq!(posts.get(), 1);
        assert!(pacer.has_demand());
        assert!(!pacer.take_frame(now), "nothing is due before the vsync");
        assert_eq!(
            pacer.poll_timeout(now),
            None,
            "the looper delivers the vsync"
        );
    }

    #[test]
    fn vsync_makes_one_frame_due_and_the_next_demand_posts_again() {
        let now = Instant::now();
        let (pacer, posts) = vsync_pacer_after_frame(now);
        let now = now + Duration::from_millis(1);

        pacer.schedule(now);
        pacer.on_vsync(VSYNC);
        assert_eq!(pacer.poll_timeout(now), Some(Duration::ZERO));
        assert!(pacer.take_frame(now));
        assert!(!pacer.take_frame(now), "a vsync owes exactly one frame");
        assert!(!pacer.has_demand());

        pacer.schedule(now + Duration::from_millis(1));
        assert_eq!(posts.get(), 2);
    }

    #[test]
    fn demand_while_a_frame_is_due_does_not_post() {
        let now = Instant::now();
        let (pacer, posts) = vsync_pacer_after_frame(now);
        let now = now + Duration::from_millis(1);

        pacer.schedule(now);
        pacer.on_vsync(VSYNC);
        pacer.schedule(now);

        assert_eq!(posts.get(), 1);
        assert!(pacer.take_frame(now));
        assert!(!pacer.take_frame(now));
    }

    #[test]
    fn a_frame_that_overran_the_interval_still_waits_for_a_vsync() {
        let now = Instant::now();
        let (pacer, posts) = vsync_pacer_after_frame(now);

        // The frame taken at `now` took 40 ms: still not idle, so the demand
        // it re-raised waits for a vsync rather than drawing off-beat.
        let later = now + Duration::from_millis(40);
        pacer.schedule(later);

        assert_eq!(posts.get(), 1);
        assert!(!pacer.take_frame(later));
        pacer.on_vsync(VSYNC);
        assert!(pacer.take_frame(later));
        assert_eq!(pacer.frame_vsync(), Some(VSYNC));
    }

    #[test]
    fn demand_after_an_idle_period_is_served_at_once() {
        let now = Instant::now();
        let (pacer, posts) = vsync_pacer_after_frame(now);
        let later = now + IDLE;
        pacer.schedule(later);
        assert_eq!(posts.get(), 0);
        assert!(pacer.take_frame(later));
        assert_eq!(pacer.frame_vsync(), None);
    }

    #[test]
    fn the_period_is_measured_between_vsyncs() {
        let now = Instant::now();
        let (pacer, _posts) = vsync_pacer_after_frame(now);
        // 165 Hz: 6.06 ms apart.
        for n in 0..20 {
            pacer.schedule(now);
            pacer.on_vsync(VSYNC + n * 6_060_606);
            assert!(pacer.take_frame(now));
        }
        assert!((pacer.period() - 6_060_606).abs() < 50_000, "{}", pacer.period());
    }

    #[test]
    fn slow_frames_are_drawn_on_every_second_vsync() {
        let now = Instant::now();
        let (pacer, posts) = vsync_pacer_after_frame(now);
        let period = 8_333_333;
        let mut vsync = VSYNC;
        // Measure the period and settle on 10 ms frames: past 8.3 ms.
        for _ in 0..10 {
            pacer.schedule(now);
            pacer.on_vsync(vsync);
            if pacer.take_frame(now) {
                pacer.end_frame(Duration::from_millis(10));
            }
            vsync += period;
        }
        assert_eq!(pacer.divisor(), 2);
        // Now frames land two vsyncs apart: the vsync in between is skipped
        // and the callback posted again.
        let mut drawn = Vec::new();
        for _ in 0..8 {
            pacer.schedule(now);
            let before = posts.get();
            pacer.on_vsync(vsync);
            if pacer.take_frame(now) {
                drawn.push(pacer.frame_vsync().unwrap());
                pacer.end_frame(Duration::from_millis(10));
            } else {
                assert_eq!(posts.get(), before + 1, "a skipped vsync posts again");
            }
            vsync += period;
        }
        assert!(drawn.windows(2).all(|pair| pair[1] - pair[0] == 2 * period), "{drawn:?}");
    }

    #[test]
    fn the_divisor_falls_only_after_a_calm_stretch() {
        let now = Instant::now();
        let (pacer, _posts) = vsync_pacer_after_frame(now);
        let period = 8 * MS;
        let mut vsync = VSYNC;
        let mut frame = |pacer: &FramePacer, duration: Duration| loop {
            pacer.schedule(now);
            pacer.on_vsync(vsync);
            vsync += period;
            if pacer.take_frame(now) {
                pacer.end_frame(duration);
                break;
            }
        };
        for _ in 0..DURATIONS {
            frame(&pacer, Duration::from_millis(12));
        }
        assert_eq!(pacer.divisor(), 2);
        // Fast frames: the slow ones leave the window after DURATIONS, then
        // CALM more frames are needed.
        for _ in 0..DURATIONS {
            frame(&pacer, Duration::from_millis(3));
        }
        assert_eq!(pacer.divisor(), 2);
        for _ in 0..CALM {
            frame(&pacer, Duration::from_millis(3));
        }
        assert_eq!(pacer.divisor(), 1);
    }

    #[test]
    fn untaken_frame_stays_due_for_an_inactive_window() {
        let now = Instant::now();
        let (pacer, _posts) = vsync_pacer_after_frame(now);
        let now = now + Duration::from_millis(1);

        pacer.schedule(now);
        pacer.on_vsync(VSYNC);
        // The loop skips drawing while inactive …
        assert!(pacer.has_demand());
        // … and draws the owed frame once it is active again.
        assert!(pacer.take_frame(now + Duration::from_secs(5)));
    }

    #[test]
    fn resume_reposts_a_callback_that_may_never_fire() {
        let now = Instant::now();
        let (pacer, posts) = vsync_pacer_after_frame(now);
        let now = now + Duration::from_millis(1);

        pacer.schedule(now);
        assert_eq!(posts.get(), 1);

        pacer.resume(now);
        assert_eq!(posts.get(), 2);

        // The stale callback and the fresh one both fire: still one frame.
        pacer.on_vsync(VSYNC);
        pacer.on_vsync(VSYNC);
        assert!(pacer.take_frame(now));
        assert!(!pacer.take_frame(now));
    }

    #[test]
    fn clock_fallback_spaces_frames_by_the_interval() {
        let pacer = FramePacer::with_clock(INTERVAL);
        let t0 = Instant::now();

        pacer.schedule(t0);
        assert_eq!(pacer.poll_timeout(t0), Some(Duration::ZERO));
        assert!(pacer.take_frame(t0), "the first frame is due at once");

        pacer.schedule(t0 + Duration::from_millis(1));
        assert_eq!(
            pacer.poll_timeout(t0 + Duration::from_millis(1)),
            Some(Duration::from_millis(15))
        );
        assert!(!pacer.take_frame(t0 + Duration::from_millis(10)));
        assert!(pacer.take_frame(t0 + INTERVAL));
        assert!(!pacer.has_demand());
    }

    #[test]
    fn clock_fallback_folds_demand_into_the_pending_frame() {
        let pacer = FramePacer::with_clock(INTERVAL);
        let t0 = Instant::now();

        pacer.schedule(t0);
        pacer.schedule(t0 + Duration::from_millis(5));
        assert!(pacer.take_frame(t0 + Duration::from_millis(5)));
        assert!(!pacer.take_frame(t0 + Duration::from_millis(5)));
    }
}
