//! How evenly frames are drawn, logged once a second while they are.
//!
//! Off unless the system property `debug.gpui.frames` is `1`
//! (`adb shell setprop debug.gpui.frames 1`, then restart the application).
//! Each line, under the `gpui_frames` target at warn level so that an
//! application's log filter lets it through:
//!
//! ```text
//! gpui frames: n=97 drawn=97 vsync=8.33ms gaps 1:80 2:15 3+:2 | tick avg 6.1 p90 9.0 max 14.2 | draw avg 2.0 max 7.5 | wake max 1.2 ms
//! ```
//!
//! - `n`, `drawn` — frames ticked, and of them drawn (a tick with nothing
//!   new to show draws nothing);
//! - `vsync` — the display's refresh interval, the shortest gap seen between
//!   the vsyncs two frames were drawn on;
//! - `gaps` — how many vsyncs apart consecutive frames were: all `1` is an
//!   even scroll, any `2` or more is a frame the screen showed twice;
//! - `tick` — the whole frame on the loop thread: input, layout, paint, draw;
//! - `draw` — the renderer's part of it: acquiring the swapchain image,
//!   encoding, submitting, presenting;
//! - `wake` — from the vsync to the frame starting.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Turned on by [`enable`], whatever the property says.
static FORCED: AtomicBool = AtomicBool::new(false);

/// The latest line, for a window that shows it.
static LATEST: Mutex<Option<String>> = Mutex::new(None);

/// Measures frames from now on, whatever the property says.
pub fn enable() {
    FORCED.store(true, Ordering::Relaxed);
}

/// The latest line measured, once a second has been.
pub fn latest() -> Option<String> {
    LATEST.lock().unwrap_or_else(|poison| poison.into_inner()).clone()
}

/// A frame further than this from the previous one starts a new burst: the
/// gap is idle time, not a missed vsync.
const IDLE_NS: i64 = 100_000_000;

/// How often a line is logged while frames are drawn.
const REPORT_NS: i64 = 1_000_000_000;

#[derive(Default)]
struct Window {
    started: i64,
    last_vsync: Option<i64>,
    gaps: Vec<i64>,
    ticks: Vec<i64>,
    draws: Vec<i64>,
    wakes: Vec<i64>,
}

struct Stats {
    window: Window,
    tick_start: Option<(i64, i64)>,
    draw_start: Option<i64>,
}

static STATS: Mutex<Option<Stats>> = Mutex::new(None);

fn enabled() -> bool {
    FORCED.load(Ordering::Relaxed) || property()
}

fn property() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        let mut value = [0u8; 92];
        // SAFETY: the buffer is PROP_VALUE_MAX (92) bytes, as the call needs.
        let len = unsafe {
            libc::__system_property_get(
                c"debug.gpui.frames".as_ptr(),
                value.as_mut_ptr().cast(),
            )
        };
        let on = len > 0 && value[0] == b'1';
        if on {
            log::warn!(target: "gpui_frames", "frame statistics on");
        }
        on
    })
}

fn with_stats(f: impl FnOnce(&mut Stats)) {
    if !enabled() {
        return;
    }
    let mut stats = STATS.lock().unwrap_or_else(|poison| poison.into_inner());
    let stats = stats.get_or_insert_with(|| Stats {
        window: Window::default(),
        tick_start: None,
        draw_start: None,
    });
    f(stats);
}

/// A frame starts, standing for the vsync at `frame_time` (see
/// `frame_source::frame_time`).
pub(crate) fn tick_begin(frame_time: i64) {
    with_stats(|stats| {
        let now = super::frame_source::now();
        stats.tick_start = Some((now, frame_time));
    });
}

/// The frame started by [`tick_begin`] ends.
pub(crate) fn tick_end() {
    with_stats(|stats| {
        let Some((start, frame_time)) = stats.tick_start.take() else {
            return;
        };
        let now = super::frame_source::now();
        let window = &mut stats.window;
        if window.ticks.is_empty() {
            window.started = start;
        }
        if let Some(last) = window.last_vsync {
            let gap = frame_time - last;
            if gap > 0 && gap < IDLE_NS {
                window.gaps.push(gap);
            }
        }
        window.last_vsync = Some(frame_time);
        window.ticks.push(now - start);
        window.wakes.push(start - frame_time);
        if now - window.started >= REPORT_NS {
            report(window);
            let last_vsync = window.last_vsync;
            *window = Window {
                last_vsync,
                ..Window::default()
            };
        }
    });
}

/// The renderer starts drawing the frame's scene.
pub(crate) fn draw_begin() {
    with_stats(|stats| stats.draw_start = Some(super::frame_source::now()));
}

/// The renderer has presented the frame's scene.
pub(crate) fn draw_end() {
    with_stats(|stats| {
        if let Some(start) = stats.draw_start.take() {
            stats.window.draws.push(super::frame_source::now() - start);
        }
    });
}

fn ms(ns: i64) -> f64 {
    ns as f64 / 1e6
}

fn report(window: &Window) {
    let (period, divisor) = super::frame_source::pacing();
    let (mut one, mut two, mut more) = (0, 0, 0);
    if period > 0 {
        for &gap in &window.gaps {
            match (gap as f64 / period as f64).round() as i64 {
                ..=1 => one += 1,
                2 => two += 1,
                _ => more += 1,
            }
        }
    }
    let mut ticks = window.ticks.clone();
    ticks.sort_unstable();
    let avg = |values: &[i64]| {
        if values.is_empty() {
            0.
        } else {
            ms(values.iter().sum::<i64>()) / values.len() as f64
        }
    };
    let max = |values: &[i64]| ms(values.iter().copied().max().unwrap_or(0));
    let p90 = ticks
        .get((ticks.len() * 9 / 10).min(ticks.len().saturating_sub(1)))
        .copied()
        .unwrap_or(0);
    let line = format!(
        "n={} drawn={} vsync={:.2}ms every {divisor} | gaps 1:{one} 2:{two} 3+:{more}\ntick avg {:.1} p90 {:.1} max {:.1} | draw avg {:.1} max {:.1} | wake max {:.1} ms",
        window.ticks.len(),
        window.draws.len(),
        ms(period),
        avg(&ticks),
        ms(p90),
        max(&ticks),
        avg(&window.draws),
        max(&window.draws),
        max(&window.wakes),
    );
    log::warn!(target: "gpui_frames", "gpui frames: {}", line.replace('\n', " | "));
    *LATEST.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(line);
}
