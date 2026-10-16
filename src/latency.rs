//! Opt-in, content-free latency records. No GPUI dependency or work while disabled.
use std::collections::VecDeque;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

const MAX_EVENTS: usize = 32_768;
const MAX_PENDING: usize = 256;
const SLOW_US: u64 = 8_000;
pub const PROBE_INTERVAL: Duration = Duration::from_millis(200);

pub fn global() -> &'static Trace {
    static TRACE: OnceLock<Trace> = OnceLock::new();
    TRACE.get_or_init(Trace::default)
}

fn micros(duration: Duration) -> u64 {
    duration.as_micros().min(u64::MAX as u128) as u64
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Key,
    Pointer,
    Scroll,
    Edit,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    InputHandler,
    EventApply,
    HistoryReconcile,
    TranscriptProjection,
    MarkdownParse,
    RowMeasure,
    ImagePrepare,
    UiBuild,
    TraceCollect,
}

impl Operation {
    const ALL: [Self; 9] = [
        Self::InputHandler,
        Self::EventApply,
        Self::HistoryReconcile,
        Self::TranscriptProjection,
        Self::MarkdownParse,
        Self::RowMeasure,
        Self::ImagePrepare,
        Self::UiBuild,
        Self::TraceCollect,
    ];
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq)]
pub struct Context {
    pub transcript_rows: usize,
    pub measured_rows: usize,
    pub busy_tabs: usize,
    pub background_jobs: usize,
    pub sidebar_visible: bool,
    pub window_active: bool,
    pub window_width: f32,
    pub window_height: f32,
    pub display_scale: f32,
    pub zoom: f32,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event {
    Input {
        at_us: u64,
        id: u64,
        action: Action,
    },
    StateApplied {
        at_us: u64,
        id: u64,
        confirmed: bool,
    },
    InputFrame {
        at_us: u64,
        id: u64,
        frame_id: u64,
        input_to_draw_us: u64,
        input_to_submission_return_us: u64,
        state_to_draw_us: u64,
        paint_observed: bool,
    },
    Dispatch {
        at_us: u64,
        end_us: u64,
        action: Action,
        invalidated: bool,
    },
    SlowSpan {
        at_us: u64,
        duration_us: u64,
        operation: Operation,
        units: usize,
    },
    Probe {
        at_us: u64,
        due_us: u64,
        posted_us: u64,
        ui_delay_us: u64,
        timer_delay_us: u64,
    },
    Draw {
        at_us: u64,
        end_us: u64,
        frame_id: u64,
        invalidations: u64,
        dirty_us: Option<u64>,
    },
    Submission {
        at_us: u64,
        end_us: u64,
        frame_id: u64,
        draw_start_us: u64,
        draw_end_us: u64,
    },
    Gap {
        at_us: u64,
        lost: u64,
    },
    Context {
        at_us: u64,
        context: Context,
    },
}

#[derive(Default, Debug, Clone, Copy, Serialize)]
struct Totals {
    calls: u64,
    total_us: u64,
    max_us: u64,
    units: u64,
}

struct PendingInput {
    id: u64,
    received: Instant,
    ready: Option<Instant>,
    confirmed: bool,
}

struct Data {
    events: VecDeque<Event>,
    pending: VecDeque<PendingInput>,
    paints: VecDeque<Instant>,
    next_input: u64,
    dropped_events: u64,
    dropped_inputs: u64,
    journal_lost: u64,
    last_context: Option<Context>,
    operations: [Totals; 9],
}

impl Default for Data {
    fn default() -> Self {
        Self {
            events: VecDeque::with_capacity(MAX_EVENTS),
            pending: VecDeque::new(),
            paints: VecDeque::new(),
            next_input: 0,
            dropped_events: 0,
            dropped_inputs: 0,
            journal_lost: 0,
            last_context: None,
            operations: [Totals::default(); 9],
        }
    }
}

impl Data {
    fn push(&mut self, event: Event) {
        if self.events.len() == MAX_EVENTS {
            self.events.pop_front();
            self.dropped_events += 1;
        }
        self.events.push_back(event);
    }
}

#[derive(Clone, Default)]
pub struct Trace(Arc<Inner>);

#[derive(Default)]
struct Inner {
    enabled: AtomicBool,
    session: Mutex<Option<Arc<Session>>>,
}

struct Session {
    origin: Instant,
    unix_ms: u64,
    active: AtomicBool,
    data: Mutex<Data>,
}

impl Session {
    fn at(&self, at: Instant) -> u64 {
        micros(at.saturating_duration_since(self.origin))
    }

    fn update(&self, work: impl FnOnce(&mut Data)) {
        let mut data = self.data.lock().unwrap();
        if self.active.load(Ordering::Acquire) {
            work(&mut data);
        }
    }
}

pub struct Span {
    session: Option<Arc<Session>>,
    start: Option<Instant>,
    operation: Operation,
    units: usize,
}

impl Span {
    pub fn units(&mut self, units: usize) {
        self.units = units;
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        let (Some(session), Some(start)) = (&self.session, self.start) else {
            return;
        };
        let duration_us = micros(start.elapsed());
        let at_us = session.at(start);
        session.update(|data| {
            let stats = &mut data.operations[self.operation as usize];
            stats.calls += 1;
            stats.total_us = stats.total_us.saturating_add(duration_us);
            stats.max_us = stats.max_us.max(duration_us);
            stats.units = stats.units.saturating_add(self.units as u64);
            if duration_us >= SLOW_US {
                data.push(Event::SlowSpan {
                    at_us,
                    duration_us,
                    operation: self.operation,
                    units: self.units,
                });
            }
        });
    }
}

impl Trace {
    pub fn enabled(&self) -> bool {
        self.0.enabled.load(Ordering::Relaxed)
    }

    fn session(&self) -> Option<Arc<Session>> {
        if !self.enabled() {
            return None;
        }
        self.0.session.lock().unwrap().clone()
    }

    pub fn start(&self) {
        let mut slot = self.0.session.lock().unwrap();
        if slot.is_some() {
            return;
        }
        let session = Session {
            origin: Instant::now(),
            unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            active: AtomicBool::new(true),
            data: Mutex::new(Data::default()),
        };
        *slot = Some(Arc::new(session));
        self.0.enabled.store(true, Ordering::Release);
    }

    pub fn span(&self, operation: Operation, units: usize) -> Span {
        let session = self.session();
        // A disabled call does not read the clock or acquire the recorder lock.
        let start = session.as_ref().map(|_| Instant::now());
        Span {
            session,
            start,
            operation,
            units,
        }
    }

    pub fn input(&self, action: Action) -> Option<InputToken> {
        let session = self.session()?;
        let received = Instant::now();
        let at_us = session.at(received);
        let mut id = None;
        session.update(|data| {
            data.next_input += 1;
            let next = data.next_input;
            if data.pending.len() == MAX_PENDING {
                data.pending.pop_front();
                data.dropped_inputs += 1;
            }
            data.pending.push_back(PendingInput {
                id: next,
                received,
                ready: None,
                confirmed: false,
            });
            data.push(Event::Input {
                at_us,
                id: next,
                action,
            });
            id = Some(next);
        });
        id.map(|id| InputToken { session, id })
    }

    /// Called after dispatch, not a claim that a widget value changed.
    pub fn dispatched(&self, token: InputToken) {
        self.ready(Some(token), false);
    }

    /// A real composer/state notification, including edits made by IME or paste.
    pub fn edited(&self) {
        // IME/paste notifications have no causal key token. Never guess from
        // the last pending key: its frame may already be in the journal.
        self.ready(self.input(Action::Edit), true);
    }

    fn ready(&self, token: Option<InputToken>, confirmed: bool) {
        let Some(InputToken { session, id }) = token else {
            return;
        };
        let ready = Instant::now();
        let at_us = session.at(ready);
        session.update(|data| {
            if let Some(input) = data.pending.iter_mut().find(|input| input.id == id)
                && (input.ready.is_none() || (confirmed && !input.confirmed))
            {
                input.ready = Some(ready);
                input.confirmed = confirmed;
                data.push(Event::StateApplied {
                    at_us,
                    id,
                    confirmed,
                });
            }
        });
    }

    pub fn paint(&self) {
        let Some(session) = self.session() else {
            return;
        };
        let at = Instant::now();
        session.update(|data| {
            if data.paints.len() == 1024 {
                data.paints.pop_front();
            }
            data.paints.push_back(at);
        });
    }

    pub fn context(&self, context: Context) {
        let Some(session) = self.session() else {
            return;
        };
        let at_us = session.at(Instant::now());
        session.update(|data| {
            if data.last_context != Some(context) {
                data.last_context = Some(context);
                data.push(Event::Context { at_us, context });
            }
        });
    }

    pub fn probe(&self, due: Instant, posted: Instant, handled: Instant) {
        let Some(session) = self.session() else {
            return;
        };
        session.update(|data| {
            data.push(Event::Probe {
                at_us: session.at(handled),
                due_us: session.at(due),
                posted_us: session.at(posted),
                ui_delay_us: micros(handled.saturating_duration_since(posted)),
                timer_delay_us: micros(posted.saturating_duration_since(due)),
            })
        });
    }

    pub fn dispatch(&self, start: Instant, end: Instant, action: Action, invalidated: bool) {
        let Some(session) = self.session() else {
            return;
        };
        if start < session.origin {
            return;
        }
        session.update(|data| {
            data.push(Event::Dispatch {
                at_us: session.at(start),
                end_us: session.at(end),
                action,
                invalidated,
            })
        });
    }

    pub fn draw(&self, start: Instant, end: Instant, invalidations: u64, dirty: Option<Instant>) {
        let Some(session) = self.session() else {
            return;
        };
        if start < session.origin {
            return;
        }
        session.update(|data| {
            data.push(Event::Draw {
                at_us: session.at(start),
                end_us: session.at(end),
                frame_id: session.at(start),
                invalidations,
                dirty_us: dirty.map(|at| session.at(at)),
            })
        });
    }

    /// A paired GPUI draw/platform-call interval. This is NOT GPU/display completion.
    pub fn submitted(&self, draw_start: Instant, draw_end: Instant, start: Instant, end: Instant) {
        let Some(session) = self.session() else {
            return;
        };
        if draw_start < session.origin {
            return;
        }
        let draw_start_us = session.at(draw_start);
        let draw_end_us = session.at(draw_end);
        let end_us = session.at(end);
        session.update(|data| {
            data.push(Event::Submission {
                at_us: session.at(start),
                end_us,
                frame_id: draw_start_us,
                draw_start_us,
                draw_end_us,
            });
            let paint_observed = data
                .paints
                .iter()
                .any(|paint| *paint >= draw_start && *paint <= draw_end);
            while data.paints.front().is_some_and(|paint| *paint <= draw_end) {
                data.paints.pop_front();
            }
            let pending = std::mem::take(&mut data.pending);
            for input in pending {
                if let Some(ready) = input.ready.filter(|ready| *ready <= draw_start) {
                    data.push(Event::InputFrame {
                        at_us: end_us,
                        id: input.id,
                        frame_id: draw_start_us,
                        input_to_draw_us: micros(
                            draw_start.saturating_duration_since(input.received),
                        ),
                        input_to_submission_return_us: micros(
                            end.saturating_duration_since(input.received),
                        ),
                        state_to_draw_us: micros(draw_start.saturating_duration_since(ready)),
                        paint_observed,
                    });
                } else {
                    data.pending.push_back(input);
                }
            }
        });
    }

    pub fn gap(&self, lost: u64) {
        let Some(session) = self.session() else {
            return;
        };
        let at_us = session.at(Instant::now());
        session.update(|data| {
            data.journal_lost = data.journal_lost.saturating_add(lost);
            data.dropped_inputs += data.pending.len() as u64;
            data.pending.clear();
            data.paints.clear();
            data.push(Event::Gap { at_us, lost });
        });
    }

    pub fn stop(&self) -> Option<Report> {
        let mut slot = self.0.session.lock().unwrap();
        self.0.enabled.store(false, Ordering::Release);
        let session = slot.take()?;
        let mut data = session.data.lock().unwrap();
        session.active.store(false, Ordering::Release);
        Some(Report {
            unix_ms: session.unix_ms,
            duration_us: micros(session.origin.elapsed()),
            events: std::mem::take(&mut data.events),
            operations: data.operations,
            dropped_events: data.dropped_events,
            dropped_inputs: data.dropped_inputs,
            journal_lost: data.journal_lost,
            pending_inputs: data.pending.len(),
            last_context: data.last_context,
        })
    }
}

pub struct InputToken {
    session: Arc<Session>,
    id: u64,
}

pub struct Report {
    unix_ms: u64,
    duration_us: u64,
    events: VecDeque<Event>,
    operations: [Totals; 9],
    dropped_events: u64,
    dropped_inputs: u64,
    journal_lost: u64,
    pending_inputs: usize,
    last_context: Option<Context>,
}

impl Report {
    pub fn write(&self, directory: &Path) -> io::Result<PathBuf> {
        std::fs::create_dir_all(directory)?;
        let mut file = tempfile::Builder::new()
            .prefix(&format!("lag-{}-", self.unix_ms))
            .suffix(".jsonl")
            .tempfile_in(directory)?;
        {
            let mut output = BufWriter::new(file.as_file_mut());
            let header = serde_json::json!({
                "type": "header", "schema": "opencode-gpui-latency-v1",
                "app_version": env!("CARGO_PKG_VERSION"),
                "build_profile": if cfg!(debug_assertions) { "debug" } else { "release" },
                "build_fingerprint": env!("OPENCODE_BUILD_FINGERPRINT"),
                "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
                "started_unix_ms": self.unix_ms, "duration_us": self.duration_us,
                "event_limit": MAX_EVENTS, "slow_span_threshold_us": SLOW_US,
                "probe_interval_us": micros(PROBE_INTERVAL),
                "dropped_events": self.dropped_events, "dropped_inputs": self.dropped_inputs,
                "journal_lost": self.journal_lost, "pending_inputs_at_stop": self.pending_inputs,
                "last_context": self.last_context,
                "semantics": {
                    "input": "app receipt, never physical key-press time; action categories only",
                    "state": "confirmed=true is an edit notification; false is post-dispatch only",
                    "edit_causality": "edit notifications have independent IDs; no last-key causality is inferred",
                    "input_frame": "first paired draw starting after state/post-dispatch marker; not proof of visible change",
                    "paint_observed": "root canvas executed during that draw; cached painting may skip it",
                    "submission": "synchronous platform draw call returned, not guaranteed submission or GPU/compositor/display completion",
                    "spans": "elapsed wall time (including waiting/preemption); inclusive and may overlap; summaries include fast calls, individual spans only when slow",
                    "ordering": "completion-order journal; use timestamps, not line order",
                    "dispatch_scope": "GPUI dispatch intervals are process-wide; own input IDs and frames are captured in the main client window",
                    "image_prepare": "synchronous inline-byte preparation only; GPUI asynchronous image decode is not separately timed",
                    "limits": "bounded rolling event buffer; no correlation across journal gaps",
                    "disabled": "custom recording/probes disabled; compiled GPUI journal retains its own bounded bookkeeping"
                }
            });
            serde_json::to_writer(&mut output, &header)?;
            writeln!(output)?;
            for (operation, totals) in Operation::ALL.into_iter().zip(self.operations) {
                serde_json::to_writer(
                    &mut output,
                    &serde_json::json!({
                        "type": "operation_totals", "operation": operation, "totals": totals,
                    }),
                )?;
                writeln!(output)?;
            }
            for event in &self.events {
                serde_json::to_writer(&mut output, event)?;
                writeln!(output)?;
            }
            output.flush()?;
        }
        let (_, path) = file.keep().map_err(|error| error.error)?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_and_stopped_traces_ignore_late_work() {
        let trace = Trace::default();
        assert!(trace.input(Action::Key).is_none());
        assert!(trace.stop().is_none());
        trace.start();
        let span = trace.span(Operation::EventApply, 1);
        let report = trace.stop().unwrap();
        drop(span);
        assert_eq!(report.operations[Operation::EventApply as usize].calls, 0);
        trace.start();
        assert_eq!(trace.stop().unwrap().events.len(), 0);
    }

    #[test]
    fn correlation_uses_actual_draw_times_not_drain_time_and_respects_gaps() {
        let trace = Trace::default();
        trace.start();
        let old_draw = Instant::now();
        let id = trace.input(Action::Key).unwrap();
        trace.dispatched(id);
        trace.submitted(old_draw, Instant::now(), Instant::now(), Instant::now());
        assert_eq!(
            trace.session().unwrap().data.lock().unwrap().pending.len(),
            1
        );
        let draw = Instant::now();
        trace.paint();
        trace.submitted(draw, Instant::now(), Instant::now(), Instant::now());
        let id = trace.input(Action::Scroll).unwrap();
        trace.dispatched(id);
        trace.gap(2);
        let draw = Instant::now();
        trace.submitted(draw, draw, draw, draw);
        let report = trace.stop().unwrap();
        assert_eq!(
            report
                .events
                .iter()
                .filter(|event| matches!(event, Event::InputFrame { .. }))
                .count(),
            1
        );
        assert_eq!(report.journal_lost, 2);
        assert_eq!(report.dropped_inputs, 1);
        assert!(report.events.iter().any(|event| matches!(
            event,
            Event::InputFrame {
                paint_observed: true,
                ..
            }
        )));
    }

    #[test]
    fn buffering_is_bounded_and_counts_overwrites() {
        let trace = Trace::default();
        trace.start();
        for _ in 0..MAX_EVENTS + 7 {
            trace.input(Action::Pointer);
        }
        let report = trace.stop().unwrap();
        assert_eq!(report.events.len(), MAX_EVENTS);
        assert_eq!(report.dropped_events, 7);
        assert_eq!(report.pending_inputs, MAX_PENDING);
        assert_eq!(report.dropped_inputs, (MAX_EVENTS + 7 - MAX_PENDING) as u64);
    }

    #[test]
    fn independent_edit_never_confirms_an_uncollected_earlier_key() {
        let trace = Trace::default();
        trace.start();
        let key = trace.input(Action::Key).unwrap();
        trace.dispatched(key);
        let earlier_draw = Instant::now();
        let earlier_end = Instant::now();
        trace.edited();
        trace.submitted(earlier_draw, earlier_end, earlier_end, earlier_end);
        let session = trace.session().unwrap();
        let data = session.data.lock().unwrap();
        assert_eq!(data.pending.len(), 1);
        assert_eq!(data.pending[0].id, 2);
        assert!(data.pending[0].confirmed);
        assert!(
            data.events
                .iter()
                .any(|event| matches!(event, Event::InputFrame { id: 1, .. }))
        );
        assert!(!data.events.iter().any(|event| matches!(
            event,
            Event::StateApplied {
                id: 1,
                confirmed: true,
                ..
            }
        )));
    }

    #[test]
    fn sub_microsecond_ordering_is_preserved_until_serialization() {
        let trace = Trace::default();
        trace.start();
        let token = trace.input(Action::Key).unwrap();
        trace.dispatched(token);
        let session = trace.session().unwrap();
        let base = session.origin + Duration::from_millis(1);
        {
            let mut data = session.data.lock().unwrap();
            data.pending[0].received = base;
            data.pending[0].ready = Some(base + Duration::from_nanos(500));
            data.paints.push_back(base + Duration::from_nanos(650));
        }
        trace.submitted(
            base + Duration::from_nanos(400),
            base + Duration::from_nanos(450),
            base,
            base,
        );
        assert_eq!(session.data.lock().unwrap().pending.len(), 1);
        trace.submitted(
            base + Duration::from_nanos(600),
            base + Duration::from_nanos(700),
            base,
            base,
        );
        assert_eq!(session.data.lock().unwrap().pending.len(), 0);
        assert!(trace.stop().unwrap().events.iter().any(|event| matches!(
            event,
            Event::InputFrame {
                paint_observed: true,
                ..
            }
        )));
    }

    #[test]
    fn deferred_input_tokens_cannot_write_into_a_new_capture() {
        let trace = Trace::default();
        trace.start();
        let stale = trace.input(Action::Key).unwrap();
        trace.stop();
        trace.start();
        let current = trace.input(Action::Key).unwrap();
        trace.dispatched(stale);
        assert!(
            trace.session().unwrap().data.lock().unwrap().pending[0]
                .ready
                .is_none()
        );
        trace.dispatched(current);
        assert!(
            trace.session().unwrap().data.lock().unwrap().pending[0]
                .ready
                .is_some()
        );
    }

    #[test]
    fn slow_work_is_individual_and_failed_exports_can_be_retried() {
        let trace = Trace::default();
        trace.start();
        let mut span = trace.span(Operation::RowMeasure, 4);
        span.start = Some(Instant::now() - Duration::from_millis(20));
        drop(span);
        let report = trace.stop().unwrap();
        assert!(report.events.iter().any(|event| matches!(
            event,
            Event::SlowSpan {
                operation: Operation::RowMeasure,
                units: 4,
                ..
            }
        )));
        assert_eq!(report.operations[Operation::RowMeasure as usize].calls, 1);
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(report.write(file.path()).is_err());
        let directory = tempfile::tempdir().unwrap();
        assert!(report.write(directory.path()).is_ok());
    }

    #[test]
    fn export_is_private_unique_and_content_free_with_individual_events() {
        let dir = tempfile::tempdir().unwrap();
        let trace = Trace::default();
        trace.start();
        let id = trace.input(Action::Key).unwrap();
        trace.edited();
        trace.dispatched(id);
        let now = Instant::now();
        trace.probe(now, now, now + Duration::from_millis(80));
        let draw = Instant::now();
        trace.submitted(draw, draw, draw, draw);
        let report = trace.stop().unwrap();
        let first = report.write(dir.path()).unwrap();
        let second = report.write(dir.path()).unwrap();
        assert_ne!(first, second);
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(&first)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows[0]["schema"], "opencode-gpui-latency-v1");
        assert!(
            rows.iter()
                .any(|row| row["type"] == "probe" && row["ui_delay_us"] == 80_000)
        );
        assert!(
            rows.iter()
                .any(|row| row["type"] == "state_applied" && row["confirmed"] == true)
        );
        assert!(rows.iter().any(|row| row["type"] == "input_frame"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(first).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
