//! On-demand CPU frame costs from GPUI's built-in profiler.

use std::time::Instant;

use gpui_kit::{
    FrameEvent, WindowId,
    profiler::{self, FrameTimingCollector},
};

#[derive(Default)]
pub struct Measurement {
    collector: Option<FrameTimingCollector>,
    window_id: Option<WindowId>,
    started: Option<Instant>,
    summary: Option<String>,
}

impl Measurement {
    pub fn running(&self) -> bool {
        self.collector.is_some()
    }

    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    pub fn start(&mut self, window_id: WindowId) {
        self.summary = None;
        self.window_id = Some(window_id);
        self.started = Some(Instant::now());
        profiler::set_trace_enabled(true);
        self.collector = Some(FrameTimingCollector::new());
    }

    pub fn stop(&mut self) {
        let Some(mut collector) = self.collector.take() else {
            return;
        };
        let window_id = self.window_id.expect("active measurement has a window");
        let elapsed = self
            .started
            .take()
            .expect("active measurement has a start")
            .elapsed();
        let mut draws = Vec::new();
        let mut presents = Vec::new();
        for event in collector.collect_unseen() {
            match event {
                FrameEvent::Draw(frame) if frame.window_id == window_id => {
                    draws.push(frame.draw_duration().as_secs_f64() * 1000.);
                }
                FrameEvent::Present(frame) if frame.window_id == window_id => {
                    presents.push(frame.present_duration().as_secs_f64() * 1000.);
                }
                _ => {}
            }
        }
        profiler::set_trace_enabled(false);
        self.summary = Some(summarize(&mut draws, &mut presents, elapsed.as_secs_f64()));
    }
}

fn summarize(draws: &mut [f64], presents: &mut [f64], seconds: f64) -> String {
    if draws.is_empty() && presents.is_empty() {
        return "No frames recorded. Try measuring while scrolling or typing.".into();
    }
    let p95 = |durations: &mut [f64]| -> f64 {
        if durations.is_empty() {
            return 0.;
        }
        durations.sort_by(f64::total_cmp);
        durations[((durations.len() - 1) as f64 * 0.95).ceil() as usize]
    };
    format!(
        "CPU · {:.0}s · {} frames · draw p95 {:.1}ms · present p95 {:.1}ms",
        seconds,
        draws.len(),
        p95(draws),
        p95(presents),
    )
}

#[cfg(test)]
mod tests {
    use super::summarize;

    #[test]
    fn reports_cpu_frame_tail_and_empty_capture() {
        let summary = summarize(&mut [1., 4., 2.], &mut [0.5, 3., 1.], 2.);
        assert!(summary.contains("CPU · 2s · 3 frames"));
        assert!(summary.contains("draw p95 4.0ms"));
        assert!(summary.contains("present p95 3.0ms"));
        assert!(summarize(&mut [], &mut [], 0.).contains("No frames recorded"));
    }
}
