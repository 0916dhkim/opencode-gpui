//! Opt-in aggregate CPU stacks. All sampler work and symbolization is off the UI.
//! No sampler singleton, timer or worker is initialized until Start is requested.
use std::{
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use serde::Serialize;

pub const FREQUENCY_HZ: i32 = 99;
pub const LIMIT: Duration = Duration::from_secs(30);
const MAX_STACKS: usize = 512;
const MAX_FRAMES: usize = 96;
const MAX_SYMBOL_CHARS: usize = 512;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Starting,
    Recording,
    Finalizing,
    Complete,
    Limited,
    Unavailable,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "CPU stacks: starting…",
            Self::Recording => "CPU stacks: recording (30s max)",
            Self::Finalizing => "CPU stacks: finalizing…",
            Self::Complete => "CPU stacks: ready",
            Self::Limited => "CPU limit reached; stop/save capture",
            Self::Unavailable => "CPU stacks unavailable; timings retained",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Stack {
    pub thread: &'static str,
    pub samples: u64,
    /// First RESOLVED frame first, not necessarily the real leaf. No paths/addresses.
    pub functions: Vec<String>,
    pub truncated: bool,
    pub native_depth: usize,
    pub native_depth_limited: bool,
    pub unresolved_or_filtered_native_frames: usize,
}

#[derive(Debug, Serialize)]
pub struct Profile {
    pub backend: &'static str,
    pub backend_revision: &'static str,
    pub status: &'static str,
    pub reason: Option<&'static str>,
    pub frequency_hz: i32,
    pub limit_us: u64,
    pub sampling_started_us: Option<u64>,
    pub snapshot_started_us: Option<u64>,
    pub sampling_stopped_us: Option<u64>,
    pub ui_samples: u64,
    pub usable_ui_samples: u64,
    pub other_samples: u64,
    pub empty_stack_samples: u64,
    pub omitted_stack_samples: u64,
    pub truncated_symbols: u64,
    pub stacks: Vec<Stack>,
    pub loss_accounting: &'static str,
    pub semantics: &'static str,
}

impl Profile {
    pub fn has_ui_stacks(&self) -> bool {
        self.stacks.iter().any(|stack| {
            stack.thread == "ui"
                && stack
                    .functions
                    .iter()
                    .any(|f| !f.is_empty() && f != "Unknown" && !f.starts_with("[path-bearing"))
        })
    }

    fn unavailable(reason: &'static str) -> Self {
        Self {
            backend: "pprof-rs/framehop/ITIMER_PROF",
            backend_revision: "01cff82dbe6fe110a707bf2b38d8ebb1d14a18f8",
            status: "unavailable",
            reason: Some(reason),
            frequency_hz: FREQUENCY_HZ,
            limit_us: LIMIT.as_micros() as u64,
            sampling_started_us: None,
            snapshot_started_us: None,
            sampling_stopped_us: None,
            ui_samples: 0,
            usable_ui_samples: 0,
            other_samples: 0,
            empty_stack_samples: 0,
            omitted_stack_samples: 0,
            truncated_symbols: 0,
            stacks: Vec::new(),
            loss_accounting: "unavailable: contention, blocklisting and collector failures can skip samples",
            semantics: "aggregate process-CPU-triggered observed stacks, not unbiased per-thread CPU accounting or timestamped per-stall stacks; UI identified by pthread identity; first RESOLVED frame first, not guaranteed real leaf: upstream drops unresolved native frames and profiler frames; native_depth_limited flags possible upstream truncation at 128 frames; truncated flags the separate 96-name export cap; CPU samples do not explain off-CPU waits; 30s sampler deadline, fixed buffers may spill to a private temporary file; export capped at 512 stacks/4MiB of stack JSON; initialization/finalization perturbs capture",
        }
    }
}

pub struct Capture {
    stop: mpsc::Sender<()>,
    result: Option<mpsc::Receiver<Profile>>,
    status: Arc<Mutex<Status>>,
}

impl Capture {
    /// Call on the GPUI thread; only its identity is captured here, not its stack.
    pub fn start(origin: Instant) -> Self {
        Self::start_bounded(origin, LIMIT)
    }

    /// Shorter windows for non-GUI diagnostic validation; never exceeds 30s.
    #[doc(hidden)]
    pub fn start_bounded(origin: Instant, limit: Duration) -> Self {
        let limit = limit.min(LIMIT);
        let (stop, commands) = mpsc::channel();
        let (sender, result) = mpsc::channel();
        let status = Arc::new(Mutex::new(Status::Starting));
        let state = status.clone();
        #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
        // SAFETY: pthread_self has no preconditions and is used only for matching.
        let ui_thread = unsafe { libc::pthread_self() as u64 };
        let spawn = std::thread::Builder::new()
            .name("lag-cpu-worker".into())
            .spawn(move || {
                let profile = std::panic::catch_unwind(|| {
                    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
                    {
                        native::record(origin, ui_thread, commands, &state, limit)
                    }
                    #[cfg(not(all(
                        target_os = "linux",
                        target_arch = "x86_64",
                        target_env = "gnu"
                    )))]
                    {
                        let _ = (origin, commands, limit);
                        Profile::unavailable("unsupported_platform")
                    }
                })
                .unwrap_or_else(|_| Profile::unavailable("worker_panicked"));
                *state.lock().unwrap_or_else(|e| e.into_inner()) = match profile.status {
                    _ if !profile.has_ui_stacks() => Status::Unavailable,
                    "recorded" => Status::Complete,
                    "limited" => Status::Limited,
                    _ => Status::Unavailable,
                };
                let _ = sender.send(profile);
            });
        if spawn.is_err() {
            *status.lock().unwrap() = Status::Unavailable;
        }
        Self {
            stop,
            result: Some(result),
            status,
        }
    }

    pub fn status(&self) -> Status {
        *self.status.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Nonblocking stop request; finalization must be awaited on an export worker.
    pub fn request_stop(&self) {
        let _ = self.stop.send(());
    }

    pub fn finish(mut self) -> Profile {
        self.request_stop();
        self.result
            .take()
            .unwrap()
            .recv()
            .unwrap_or_else(|_| Profile::unavailable("worker_disconnected"))
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Closing a window must not join or symbolize on the GPUI thread.
        self.request_stop();
    }
}

fn clean_symbol(name: &str) -> (String, bool) {
    // Names, not filenames, are admitted. Defend against path-bearing symbols too.
    if name.contains('/') || name.contains('\\') {
        return ("[path-bearing symbol redacted]".into(), false);
    }
    let truncated = name.chars().count() > MAX_SYMBOL_CHARS;
    (
        name.chars()
            .filter(|c| !c.is_control())
            .take(MAX_SYMBOL_CHARS)
            .collect(),
        truncated,
    )
}

fn finish_stacks(profile: &mut Profile, mut stacks: Vec<Stack>) {
    // Keep UI stacks first, then the most frequent observed stacks in each group.
    stacks.sort_by(|a, b| {
        (a.thread != "ui")
            .cmp(&(b.thread != "ui"))
            .then_with(|| b.samples.cmp(&a.samples))
            .then_with(|| a.functions.cmp(&b.functions))
    });
    let mut bytes = 0;
    for stack in stacks {
        let size = serde_json::to_vec(&stack).expect("stack serializes").len() + 1;
        if profile.stacks.len() == MAX_STACKS || bytes + size > 4 * 1024 * 1024 {
            profile.omitted_stack_samples += stack.samples;
        } else {
            bytes += size;
            profile.stacks.push(stack);
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
mod native {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    static OWNED: AtomicBool = AtomicBool::new(false);
    struct Owner;
    impl Drop for Owner {
        fn drop(&mut self) {
            OWNED.store(false, Ordering::Release);
        }
    }

    unsafe extern "C" {
        fn getitimer(which: libc::c_int, value: *mut libc::itimerval) -> libc::c_int;
        fn setitimer(
            which: libc::c_int,
            value: *const libc::itimerval,
            old: *mut libc::itimerval,
        ) -> libc::c_int;
    }

    /// Disarming alone does not discard pending SIGPROF. Ignore/discard before
    /// upstream restores the original (usually terminating) default disposition.
    struct SamplingGuard<'a>(Option<pprof::ProfilerGuard<'a>>);

    impl SamplingGuard<'_> {
        fn shutdown(&mut self) -> bool {
            let Some(guard) = self.0.take() else {
                return true;
            };
            // SAFETY: zero is a valid disabled timer and empty signal action.
            let timer: libc::itimerval = unsafe { std::mem::zeroed() };
            let mut ignore: libc::sigaction = unsafe { std::mem::zeroed() };
            ignore.sa_sigaction = libc::SIG_IGN;
            let disarmed =
                unsafe { setitimer(libc::ITIMER_PROF, &timer, std::ptr::null_mut()) } == 0;
            let discarded =
                unsafe { libc::sigaction(libc::SIGPROF, &ignore, std::ptr::null_mut()) } == 0;
            if !disarmed || !discarded {
                // Fail closed if kernel signal/timer operations unexpectedly fail:
                // retain the valid handler/collector rather than restore SIG_DFL
                // over a possibly pending signal. Further capture will be refused.
                std::mem::forget(guard);
                return false;
            }
            drop(guard);
            true
        }

        fn stop(mut self) -> bool {
            self.shutdown()
        }
    }

    impl Drop for SamplingGuard<'_> {
        fn drop(&mut self) {
            let _ = self.shutdown();
        }
    }

    fn timer_active() -> Result<bool, &'static str> {
        let mut timer = std::mem::MaybeUninit::<libc::itimerval>::uninit();
        // SAFETY: valid output storage; read only after successful initialization.
        if unsafe { getitimer(libc::ITIMER_PROF, timer.as_mut_ptr()) } != 0 {
            return Err("timer_query_failed");
        }
        let timer = unsafe { timer.assume_init() };
        Ok(timer.it_value.tv_sec != 0
            || timer.it_value.tv_usec != 0
            || timer.it_interval.tv_sec != 0
            || timer.it_interval.tv_usec != 0)
    }

    pub(super) fn record(
        origin: Instant,
        ui_thread: u64,
        commands: mpsc::Receiver<()>,
        state: &Mutex<Status>,
        limit: Duration,
    ) -> Profile {
        if OWNED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Profile::unavailable("sampler_busy");
        }
        let _owner = Owner;
        if !matches!(commands.try_recv(), Err(mpsc::TryRecvError::Empty)) {
            return Profile::unavailable("stopped_before_initialization");
        }
        let mut action = std::mem::MaybeUninit::<libc::sigaction>::uninit();
        // SAFETY: null new action queries rather than modifies signal ownership.
        if unsafe { libc::sigaction(libc::SIGPROF, std::ptr::null(), action.as_mut_ptr()) } != 0 {
            return Profile::unavailable("signal_query_failed");
        }
        let action = unsafe { action.assume_init() };
        if action.sa_sigaction != libc::SIG_DFL && action.sa_sigaction != libc::SIG_IGN {
            return Profile::unavailable("signal_already_owned");
        }
        match timer_active() {
            Ok(false) => {}
            Ok(true) => return Profile::unavailable("timer_already_owned"),
            Err(reason) => return Profile::unavailable(reason),
        }
        let guard = match pprof::ProfilerGuardBuilder::default()
            .frequency(FREQUENCY_HZ)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
        {
            Ok(guard) => SamplingGuard(Some(guard)),
            Err(_) => return Profile::unavailable("initialization_failed"),
        };
        let mut profile = Profile::unavailable("no_samples");
        profile.limit_us = limit.as_micros() as u64;
        profile.sampling_started_us = Some(origin.elapsed().as_micros() as u64);
        if !matches!(timer_active(), Ok(true)) {
            let clean = guard.stop();
            profile.reason = Some(if clean {
                "timer_did_not_start"
            } else {
                "sampler_shutdown_failed"
            });
            return profile;
        }
        *state.lock().unwrap() = Status::Recording;
        let limited = matches!(
            commands.recv_timeout(limit),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        *state.lock().unwrap() = Status::Finalizing;
        profile.snapshot_started_us = Some(origin.elapsed().as_micros() as u64);
        let snapshot = guard.0.as_ref().unwrap().report().build_unresolved();
        // Disarm, discard pending signals, restore, then symbolize/export.
        if !guard.stop() {
            profile.reason = Some("sampler_shutdown_failed");
            return profile;
        }
        profile.sampling_stopped_us = Some(origin.elapsed().as_micros() as u64);
        let snapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(_) => {
                profile.reason = Some("snapshot_failed");
                return profile;
            }
        };
        let mut stacks = Vec::new();
        for (unresolved, count) in snapshot.data {
            let count = count.max(0) as u64;
            let ui = unresolved.thread_id == ui_thread;
            if ui {
                profile.ui_samples += count;
            } else {
                profile.other_samples += count;
            }
            let native_depth = unresolved.frames.len();
            let frames = pprof::Frames::from(unresolved);
            let missing = native_depth.saturating_sub(frames.frames.len());
            let mut functions = Vec::new();
            let mut truncated = false;
            for symbol in frames.frames.iter().flatten() {
                if functions.len() == MAX_FRAMES {
                    truncated = true;
                    break;
                }
                let (name, cut) = clean_symbol(&symbol.name());
                profile.truncated_symbols += u64::from(cut);
                functions.push(name);
            }
            if functions.is_empty() {
                profile.empty_stack_samples += count;
            }
            if ui
                && functions
                    .iter()
                    .any(|f| !f.is_empty() && f != "Unknown" && !f.starts_with("[path-bearing"))
            {
                profile.usable_ui_samples += count;
            }
            stacks.push(Stack {
                thread: if ui { "ui" } else { "other" },
                samples: count,
                functions,
                truncated,
                native_depth,
                native_depth_limited: native_depth >= 128,
                unresolved_or_filtered_native_frames: missing,
            });
        }
        if profile.ui_samples + profile.other_samples > 0 {
            profile.status = if limited { "limited" } else { "recorded" };
            profile.reason = if profile.ui_samples == 0 {
                Some("no_ui_samples")
            } else if profile.usable_ui_samples == 0 {
                Some("no_ui_symbols")
            } else {
                None
            };
        }
        finish_stacks(&mut profile, stacks);
        if profile.usable_ui_samples > 0 && !profile.has_ui_stacks() {
            profile.reason = Some("no_exported_ui_stacks");
        }
        profile
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usable_samples_without_exported_ui_stacks_do_not_claim_success() {
        let mut profile = Profile::unavailable("test");
        profile.ui_samples = 7;
        profile.usable_ui_samples = 7;
        assert!(!profile.has_ui_stacks());
    }

    #[test]
    fn symbol_export_is_bounded_and_excludes_paths() {
        assert_eq!(
            clean_symbol("/home/danny/private.rs").0,
            "[path-bearing symbol redacted]"
        );
        assert_eq!(clean_symbol("gpui::text::shape\n").0, "gpui::text::shape");
        let (s, cut) = clean_symbol(&"λ".repeat(1000));
        assert!(cut);
        assert_eq!(s.chars().count(), MAX_SYMBOL_CHARS);
    }

    #[test]
    fn export_caps_stacks_and_counts_omitted_samples() {
        let mut profile = Profile::unavailable("test");
        let stacks = (0..MAX_STACKS + 7)
            .map(|i| Stack {
                thread: if i == MAX_STACKS + 6 { "ui" } else { "other" },
                samples: 1,
                functions: vec![i.to_string()],
                truncated: false,
                native_depth: 1,
                native_depth_limited: false,
                unresolved_or_filtered_native_frames: 0,
            })
            .collect();
        finish_stacks(&mut profile, stacks);
        assert_eq!(profile.stacks.len(), MAX_STACKS);
        assert_eq!(profile.stacks[0].thread, "ui");
        assert_eq!(profile.omitted_stack_samples, 7);
        let json = serde_json::to_string(&profile).unwrap();
        assert!(!json.contains("thread_id"));
        assert!(!json.contains("filename"));
        assert_eq!(
            profile.loss_accounting.split(':').next(),
            Some("unavailable")
        );
    }
}
