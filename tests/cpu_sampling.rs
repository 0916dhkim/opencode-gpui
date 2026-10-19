//! Non-GUI, isolated process check of real native unwinding and signal cleanup.
#![cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
use opencode_gpui::cpu_profile::{Capture, Status};
use std::time::{Duration, Instant};

#[inline(never)]
fn profile_known_hotspot(mut value: u64) -> u64 {
    for _ in 0..100_000 {
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        value = std::hint::black_box(value);
    }
    value
}

unsafe extern "C" {
    fn getitimer(which: libc::c_int, value: *mut libc::itimerval) -> libc::c_int;
}

fn timer_active() -> bool {
    let mut value = std::mem::MaybeUninit::<libc::itimerval>::uninit();
    assert_eq!(
        unsafe { getitimer(libc::ITIMER_PROF, value.as_mut_ptr()) },
        0
    );
    let value = unsafe { value.assume_init() };
    value.it_value.tv_sec != 0
        || value.it_value.tv_usec != 0
        || value.it_interval.tv_sec != 0
        || value.it_interval.tv_usec != 0
}

#[test]
fn native_cpu_sampling_symbolizes_ui_hotspot_and_restores_signal() {
    let mut original = std::mem::MaybeUninit::<libc::sigaction>::uninit();
    assert_eq!(
        unsafe { libc::sigaction(libc::SIGPROF, std::ptr::null(), original.as_mut_ptr()) },
        0
    );
    let original = unsafe { original.assume_init() };
    assert!(!timer_active());
    for iteration in 0..2 {
        let capture = Capture::start(Instant::now());
        let deadline = Instant::now() + Duration::from_secs(10);
        while capture.status() == Status::Starting && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(capture.status(), Status::Recording);
        if iteration == 0 {
            let other = Capture::start(Instant::now()).finish();
            assert_eq!(other.reason, Some("sampler_busy"));
            assert!(timer_active());
        }
        let deadline = Instant::now() + Duration::from_millis(900);
        let mut value = 123;
        while Instant::now() < deadline {
            value = profile_known_hotspot(value);
        }
        std::hint::black_box(value);
        // Queue a signal on a blocked thread before teardown. SIG_IGN must
        // discard it process-wide before restoring the original disposition.
        let mut previous_mask = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        if iteration == 1 {
            let mut blocked = unsafe { std::mem::zeroed::<libc::sigset_t>() };
            unsafe {
                libc::sigemptyset(&mut blocked);
                libc::sigaddset(&mut blocked, libc::SIGPROF);
                assert_eq!(
                    libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, previous_mask.as_mut_ptr()),
                    0
                );
                assert_eq!(libc::pthread_kill(libc::pthread_self(), libc::SIGPROF), 0);
                let mut pending = std::mem::zeroed::<libc::sigset_t>();
                assert_eq!(libc::sigpending(&mut pending), 0);
                assert_eq!(libc::sigismember(&pending, libc::SIGPROF), 1);
            }
        }
        let profile = capture.finish();
        if iteration == 1 {
            unsafe {
                let mut pending = std::mem::zeroed::<libc::sigset_t>();
                assert_eq!(libc::sigpending(&mut pending), 0);
                assert_eq!(libc::sigismember(&pending, libc::SIGPROF), 0);
                assert_eq!(
                    libc::pthread_sigmask(
                        libc::SIG_SETMASK,
                        &previous_mask.assume_init(),
                        std::ptr::null_mut()
                    ),
                    0
                );
            }
        }
        assert_eq!(profile.status, "recorded", "{profile:?}");
        assert!(profile.ui_samples > 0, "{profile:?}");
        assert!(profile.usable_ui_samples > 0, "{profile:?}");
        assert!(
            profile.stacks.iter().any(|s| s.thread == "ui"
                && s.functions
                    .iter()
                    .any(|f| f.contains("profile_known_hotspot"))),
            "{profile:?}"
        );
        assert!(!timer_active());
        let mut restored = std::mem::MaybeUninit::<libc::sigaction>::uninit();
        assert_eq!(
            unsafe { libc::sigaction(libc::SIGPROF, std::ptr::null(), restored.as_mut_ptr()) },
            0
        );
        assert_eq!(
            unsafe { restored.assume_init() }.sa_sigaction,
            original.sa_sigaction
        );
        let json = serde_json::to_string(&profile).unwrap();
        assert!(!json.contains("thread_id"));
        assert!(!json.contains("thread_name"));
        assert!(!json.contains("/home/"));
        assert!(!json.contains("/app/"));
    }
    let capture = Capture::start_bounded(Instant::now(), Duration::from_millis(250));
    let deadline = Instant::now() + Duration::from_secs(10);
    while capture.status() == Status::Starting && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(capture.status(), Status::Recording);
    let deadline = Instant::now() + Duration::from_millis(400);
    while Instant::now() < deadline {
        std::hint::black_box(profile_known_hotspot(123));
    }
    let profile = capture.finish();
    assert_eq!(profile.status, "limited");
    assert!(!timer_active());
}
