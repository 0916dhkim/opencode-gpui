//! On-demand, content-free frame trace from GPUI's built-in CPU profiler.

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result};
use gpui_kit::{
    FrameEvent, WindowId,
    profiler::{self, FrameTimingCollector},
};
use serde_json::{Value, json};

pub struct Activity {
    pub busy_tabs: usize,
    pub active_busy: bool,
    pub sidebar_visible: bool,
    pub scroll_y: f32,
    pub draft_revision: u64,
    pub background_jobs: usize,
}

struct GpuCounters {
    at: Instant,
    app: Option<BTreeMap<String, u64>>,
    kwin: Option<BTreeMap<String, u64>>,
}

#[derive(Default)]
pub struct Measurement {
    collector: Option<FrameTimingCollector>,
    window_id: Option<WindowId>,
    started: Option<Instant>,
    started_unix_ms: u128,
    report: Option<PathBuf>,
    status: Option<String>,
    samples: Vec<Value>,
    previous_gpu: Option<GpuCounters>,
    kwin_pid: Option<u32>,
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
        self.samples.clear();
        self.previous_gpu = None;
        self.kwin_pid = find_kwin_pid();
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

    pub fn sample(&mut self, activity: Activity) {
        let Some(started) = self.started else { return };
        if !self.running() {
            return;
        }
        let now = Instant::now();
        let current = GpuCounters {
            at: now,
            app: process_gpu_engines(std::process::id()),
            kwin: self.kwin_pid.and_then(process_gpu_engines),
        };
        let interval_ns = self
            .previous_gpu
            .as_ref()
            .map(|old| now.duration_since(old.at).as_nanos());
        let app_busy = self
            .previous_gpu
            .as_ref()
            .and_then(|old| engine_busy(&old.app, &current.app, interval_ns));
        let kwin_busy = self
            .previous_gpu
            .as_ref()
            .and_then(|old| engine_busy(&old.kwin, &current.kwin, interval_ns));
        self.samples.push(json!({
            "type": "sample", "at_ms": now.duration_since(started).as_secs_f64() * 1000.,
            "interval_ms": interval_ns.map(|ns| ns as f64 / 1_000_000.),
            "app_gpu_engine_busy_pct": app_busy,
            "kwin_gpu_engine_busy_pct": kwin_busy,
            "system_gpu_busy_pct": system_gpu_busy(),
            "busy_tabs": activity.busy_tabs, "active_busy": activity.active_busy,
            "sidebar_visible": activity.sidebar_visible, "scroll_y": activity.scroll_y,
            "draft_revision": activity.draft_revision, "background_jobs": activity.background_jobs,
        }));
        self.previous_gpu = Some(current);
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
                "schema": 2,
                "started_unix_ms": start,
                "duration_ms": elapsed_ms,
                "units": "milliseconds",
                "notes": "GPU percentages are changes in Linux DRM per-process engine-time counters over each interval; system busy is the AMD sysfs gauge. Null means unavailable. They are not per-frame GPU timestamps. Activity contains counts and scroll offset, never text, prompts, transcripts or pixels."
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
        for sample in &self.samples {
            writeln!(file, "{sample}")?;
        }
        writeln!(
            file,
            "{}",
            json!({"type": "end", "draws": draws, "presents": presents, "gpu_samples": self.samples.len()})
        )?;
        file.flush()?;
        file.persist(&path)
            .with_context(|| format!("saving {}", path.display()))?;
        Ok((path, draws))
    }
}

fn process_gpu_engines(pid: u32) -> Option<BTreeMap<String, u64>> {
    let dir = fs::read_dir(format!("/proc/{pid}/fdinfo")).ok()?;
    let mut engines = BTreeMap::new();
    let mut seen = HashSet::new();
    for entry in dir.flatten() {
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let client_id = text
            .lines()
            .find_map(|line| line.strip_prefix("drm-client-id:\t"));
        let Some(client_id) = client_id else { continue };
        if !seen.insert(client_id.to_owned()) {
            continue;
        }
        for line in text.lines() {
            if let Some((name, value)) = line.split_once(':')
                && let Some(engine) = name.strip_prefix("drm-engine-")
                && let Some(ns) = value
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
            {
                *engines.entry(engine.to_owned()).or_insert(0) += ns;
            }
        }
    }
    (!engines.is_empty()).then_some(engines)
}

fn engine_busy(
    before: &Option<BTreeMap<String, u64>>,
    after: &Option<BTreeMap<String, u64>>,
    interval_ns: Option<u128>,
) -> Option<BTreeMap<String, f64>> {
    let (before, after, interval_ns) = (before.as_ref()?, after.as_ref()?, interval_ns?);
    if interval_ns == 0 {
        return None;
    }
    Some(
        after
            .iter()
            .filter_map(|(name, current)| {
                let previous = before.get(name)?;
                (*current >= *previous).then(|| {
                    (
                        name.clone(),
                        (*current - *previous) as f64 * 100. / interval_ns as f64,
                    )
                })
            })
            .collect(),
    )
}

fn find_kwin_pid() -> Option<u32> {
    fs::read_dir("/proc").ok()?.flatten().find_map(|entry| {
        let pid = entry.file_name().to_string_lossy().parse::<u32>().ok()?;
        (fs::read_to_string(entry.path().join("comm")).ok()?.trim() == "kwin_wayland")
            .then_some(pid)
    })
}

fn system_gpu_busy() -> Option<u32> {
    let cards = fs::read_dir(Path::new("/sys/class/drm")).ok()?;
    cards.flatten().find_map(|entry| {
        let name = entry.file_name();
        let name = name.to_str()?;
        if !name.starts_with("card") || !name[4..].chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let device = entry.path().join("device");
        (fs::read_to_string(device.join("vendor")).ok()?.trim() == "0x1002")
            .then(|| {
                fs::read_to_string(device.join("gpu_busy_percent"))
                    .ok()?
                    .trim()
                    .parse()
                    .ok()
            })
            .flatten()
    })
}
