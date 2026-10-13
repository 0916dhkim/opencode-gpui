//! On-demand, content-free frame trace from GPUI's built-in CPU profiler.

use std::{
    fs,
    io::Write,
    path::PathBuf,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result};
use gpui_kit::{
    FrameEvent, WindowId,
    profiler::{self, FrameTimingCollector},
};
use serde_json::json;

#[derive(Default)]
pub struct Measurement {
    collector: Option<FrameTimingCollector>,
    window_id: Option<WindowId>,
    started: Option<Instant>,
    started_unix_ms: u128,
    report: Option<PathBuf>,
    status: Option<String>,
}

impl Measurement {
    pub fn running(&self) -> bool {
        self.collector.is_some()
    }

    pub fn summary(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub fn report_path(&self) -> Option<String> {
        self.report.as_ref().map(|path| path.display().to_string())
    }

    pub fn start(&mut self, window_id: WindowId) {
        self.status = None;
        self.report = None;
        self.window_id = Some(window_id);
        self.started = Some(Instant::now());
        self.started_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        profiler::set_trace_enabled(true);
        self.collector = Some(FrameTimingCollector::new());
    }

    pub fn stop(&mut self) {
        let Some(mut collector) = self.collector.take() else {
            return;
        };
        let elapsed_ms = self
            .started
            .expect("measurement has a start")
            .elapsed()
            .as_secs_f64()
            * 1000.;
        let events = collector.collect_unseen();
        profiler::set_trace_enabled(false);
        match self.write_trace(&events, elapsed_ms) {
            Ok((path, count)) => {
                self.status = Some(format!("CPU frame trace saved · {count} draws"));
                self.report = Some(path);
            }
            Err(error) => self.status = Some(format!("CPU frame trace failed: {error:#}")),
        }
        self.started = None;
    }

    fn write_trace(&self, events: &[FrameEvent], elapsed_ms: f64) -> Result<(PathBuf, usize)> {
        let directory = dirs::data_dir()
            .context("XDG data directory unavailable")?
            .join("opencode-gpui/measurements");
        fs::create_dir_all(&directory)?;
        let path = directory.join(format!(
            "frame-{}-{}.jsonl",
            self.started_unix_ms,
            std::process::id()
        ));
        let mut file = tempfile::NamedTempFile::new_in(&directory)?;
        let start = self.started_unix_ms;
        let window_id = self.window_id.expect("measurement has a window");
        writeln!(
            file,
            "{}",
            json!({
                "type": "capture",
                "schema": 1,
                "started_unix_ms": start,
                "duration_ms": elapsed_ms,
                "units": "milliseconds",
                "notes": "CPU draw and platform present timing only. Present is not GPU execution or compositor time. No prompt, transcript, or pixel content is recorded."
            })
        )?;
        let mut draws = 0;
        let mut presents = 0;
        for event in events {
            let row = match event {
                FrameEvent::Draw(frame) if frame.window_id == window_id => {
                    draws += 1;
                    json!({
                        "type": "draw",
                        "index": draws,
                        "at_ms": frame.draw_start.duration_since(self.started.expect("start available during write")).as_secs_f64() * 1000.,
                        "duration_ms": frame.draw_duration().as_secs_f64() * 1000.,
                        "dirty_to_draw_ms": frame.dirty_to_draw_duration().map(|duration| duration.as_secs_f64() * 1000.),
                        "invalidations": frame.invalidations,
                    })
                }
                FrameEvent::Present(frame) if frame.window_id == window_id => {
                    presents += 1;
                    json!({
                        "type": "present",
                        "index": presents,
                        "at_ms": frame.present_start.duration_since(self.started.expect("start available during write")).as_secs_f64() * 1000.,
                        "duration_ms": frame.present_duration().as_secs_f64() * 1000.,
                        "animation_interval_ms": frame.animation_interval.map(|duration| duration.as_secs_f64() * 1000.),
                    })
                }
                _ => continue,
            };
            writeln!(file, "{row}")?;
        }
        writeln!(
            file,
            "{}",
            json!({"type": "end", "draws": draws, "presents": presents})
        )?;
        file.flush()?;
        file.persist(&path)
            .with_context(|| format!("saving {}", path.display()))?;
        Ok((path, draws))
    }
}
