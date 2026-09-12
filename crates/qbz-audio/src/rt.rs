//! Real-time scheduling for the one thread that must never be late.
//!
//! # Why a renderer wants this
//!
//! The ALSA writer thread owes the DAC a period of audio before the previous
//! one finishes clocking out. Miss it and the hardware ring runs dry, which is
//! an xrun and an audible click. On a Raspberry Pi that thread shares four
//! small cores with WiFi softirqs, the SD card, and whatever else the box is
//! running — under `SCHED_OTHER` the scheduler owes it nothing in particular,
//! and the deadline it is trying to meet is invisible to the kernel.
//!
//! `SCHED_FIFO` makes the deadline visible: the writer runs ahead of every
//! ordinary thread on the box, for the few hundred microseconds per period it
//! needs. Every serious renderer does this — squeezelite takes a `-p` priority,
//! MPD raises its output thread, JACK runs its whole graph this way.
//!
//! # Why it is dangerous, and what makes it safe here
//!
//! A `SCHED_FIFO` thread that blocks — on a socket, on a disk read, on a
//! decoder — does not merely stall itself; it stalls whatever it is holding
//! while outranking every thread that could unblock it. **Promoting a thread
//! that can block converts a hiccup into a hang.**
//!
//! This is only called on the ALSA writer thread, and only because that thread
//! was first stripped of everything that can block: decode and network I/O live
//! on their own ordinary-priority thread and reach the writer through a
//! lock-free ring. The writer copies, scales, converts and hands bytes to ALSA.
//! Nothing else may be added to it.
//!
//! Two backstops sit under that promise:
//!
//! - The priority is low (5 by default) — above every `SCHED_OTHER` thread,
//!   far below the kernel's own IRQ threads, which typically run at 50. A
//!   runaway writer cannot starve the USB or network interrupt handling the
//!   audio itself depends on.
//! - The kernel's RT throttle (`/proc/sys/kernel/sched_rt_runtime_us`, 950 ms
//!   in every 1 s by default) caps total RT time regardless.
//!
//! # Failing is normal
//!
//! Setting `SCHED_FIFO` needs `RLIMIT_RTPRIO` (or `CAP_SYS_NICE`). A daemon
//! started from a unit without `LimitRTPRIO=` simply gets `EPERM`, which is not
//! an error condition: playback is exactly as good as it was before. Say so
//! once, at info, and carry on.
//!
//! **`LimitRTPRIO=` in the unit is necessary but, for a USER unit, not
//! sufficient**, and this is the case that matters because `qbzd` ships as a
//! user unit. An rlimit can only be lowered, never raised, so `LimitRTPRIO=20`
//! in a user unit is capped by the hard limit the `systemd --user` manager
//! itself inherited — which is **0 on stock Raspberry Pi OS**. The promotion
//! will return `EPERM` there no matter what the unit says, until one of:
//!
//! - `DefaultLimitRTPRIO=20` in `/etc/systemd/user.conf`, then
//!   `systemctl --user daemon-reexec` (or a reboot); or
//! - an `@audio - rtprio 20` line in `/etc/security/limits.d/` with the user in
//!   the `audio` group, which is what most audio distributions ship; or
//! - running qbzd from a SYSTEM unit, where `LimitRTPRIO=` is enough on its own.
//!
//! The log line below says this, because a feature that silently does nothing
//! on the hardware it was written for is worse than one that is switched off.

/// Process-wide `SCHED_FIFO` priority for the audio writer thread.
/// `0` disables the promotion. See [`set_writer_rt_priority`].
static WRITER_RT_PRIORITY: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Default `SCHED_FIFO` priority for the writer thread.
///
/// 5 is deliberately near the bottom of the RT range. The writer needs to beat
/// `SCHED_OTHER`, which any RT priority does; it must NOT beat the kernel
/// threads servicing the USB bus the audio is leaving through (`irq/*`,
/// conventionally 50) or the network the audio is arriving over. A number in
/// the tens would do that and buy nothing.
pub const DEFAULT_WRITER_RT_PRIORITY: u8 = 5;

/// Set the writer thread's `SCHED_FIFO` priority; `0` turns the promotion off.
///
/// Read once at player start from `audio.writer_rt_priority`, like the ALSA
/// buffer length beside it. Takes effect at the next stream open, since the
/// promotion happens when the writer thread starts.
pub fn set_writer_rt_priority(priority: u8) {
    WRITER_RT_PRIORITY.store(u32::from(priority), std::sync::atomic::Ordering::Relaxed);
}

/// Configured writer priority; `0` when the promotion is off.
pub fn writer_rt_priority() -> u8 {
    WRITER_RT_PRIORITY.load(std::sync::atomic::Ordering::Relaxed) as u8
}

/// Outcome of asking the kernel to make the calling thread real-time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtOutcome {
    /// The thread now runs `SCHED_FIFO` at this priority.
    Promoted(i32),
    /// The caller asked for `0`, or the platform has no such thing.
    Disabled,
    /// The kernel said no. The thread keeps its ordinary priority and
    /// playback is unaffected; the string is for one log line.
    Refused(String),
}

/// Clamp a requested priority into what the kernel allows for `SCHED_FIFO`.
///
/// Kept separate from the syscall so the arithmetic is testable on any host.
/// `min`/`max` are what `sched_get_priority_min`/`_max` report — 1 and 99 on
/// Linux, but read rather than assumed.
pub fn clamp_rt_priority(requested: i32, min: i32, max: i32) -> i32 {
    requested.clamp(min, max)
}

/// Promote the CALLING thread to `SCHED_FIFO` at `priority`.
///
/// `priority == 0` is "leave it alone" and returns [`RtOutcome::Disabled`].
///
/// Only ever call this from a thread that cannot block — see the module docs.
#[cfg(target_os = "linux")]
pub fn promote_current_thread(priority: u8) -> RtOutcome {
    if priority == 0 {
        return RtOutcome::Disabled;
    }

    // SAFETY: both calls take a policy constant and return a plain integer;
    // neither touches memory we own. A kernel without SCHED_FIFO limits
    // reports min > max, which the clamp below turns into a refusal.
    let (min, max) = unsafe {
        (
            libc::sched_get_priority_min(libc::SCHED_FIFO),
            libc::sched_get_priority_max(libc::SCHED_FIFO),
        )
    };
    if min < 0 || max < 0 || min > max {
        return RtOutcome::Refused("kernel reports no SCHED_FIFO priority range".to_string());
    }
    let wanted = clamp_rt_priority(i32::from(priority), min, max);

    // `sched_param` has private padding on some targets, so build it from the
    // zeroed representation rather than naming every field.
    // SAFETY: `sched_param` is a plain C struct of integers; all-zero is a
    // valid value for it, and the one field we care about is set below.
    let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
    param.sched_priority = wanted;

    // SAFETY: `pthread_self()` is always a live handle to the calling thread,
    // and `param` outlives the call. Unlike most libc functions this returns
    // the errno directly instead of setting `errno`.
    let rc = unsafe { libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param) };

    if rc == 0 {
        RtOutcome::Promoted(wanted)
    } else {
        RtOutcome::Refused(std::io::Error::from_raw_os_error(rc).to_string())
    }
}

/// Stub for hosts that are not Linux. `qbzd` is a Linux daemon; this exists so
/// the crate still compiles for a developer's `cargo check` on macOS.
#[cfg(not(target_os = "linux"))]
pub fn promote_current_thread(_priority: u8) -> RtOutcome {
    RtOutcome::Disabled
}

/// Promote the calling thread and say what happened, exactly once per process.
///
/// Once, because the answer cannot change between stream opens: either this
/// daemon has `RLIMIT_RTPRIO` or it does not. A line per track would be noise,
/// and silence would leave a user with clicks unable to tell whether the
/// feature is even on.
pub fn promote_writer_thread_and_log() -> RtOutcome {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);

    let outcome = promote_current_thread(writer_rt_priority());
    if !LOGGED.swap(true, Ordering::Relaxed) {
        match &outcome {
            RtOutcome::Promoted(p) => {
                log::info!("[Audio RT] writer thread promoted to SCHED_FIFO priority {p}");
            }
            RtOutcome::Disabled => {
                log::info!(
                    "[Audio RT] writer thread stays at normal priority (audio.writer_rt_priority = 0)"
                );
            }
            RtOutcome::Refused(why) => {
                log::info!(
                    "[Audio RT] writer thread stays at normal priority: {why}. Playback is \
                     unaffected — this only costs resilience to scheduling delay on a busy \
                     host. qbzd runs as a systemd USER unit, and `LimitRTPRIO=` there cannot \
                     exceed the limit the user manager inherited (0 on stock Raspberry Pi OS), \
                     so the unit alone is not enough: add `DefaultLimitRTPRIO=20` to \
                     /etc/systemd/user.conf and run `systemctl --user daemon-reexec`, or put \
                     the user in the `audio` group with an `@audio - rtprio 20` line in \
                     /etc/security/limits.d/. Set `audio.writer_rt_priority off` to stop trying."
                );
            }
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_priority_is_clamped_into_the_kernels_range() {
        // Linux's own range, as `sched_get_priority_{min,max}(SCHED_FIFO)`
        // reports it.
        assert_eq!(clamp_rt_priority(5, 1, 99), 5);
        assert_eq!(clamp_rt_priority(0, 1, 99), 1);
        assert_eq!(clamp_rt_priority(500, 1, 99), 99);
    }

    #[test]
    fn zero_means_leave_the_thread_alone() {
        assert_eq!(promote_current_thread(0), RtOutcome::Disabled);
    }

    /// Deliberately does NOT assert the initial value: this is process-wide
    /// state and cargo runs tests in threads of one process, so an initial-value
    /// assertion is a flake waiting for the next test that touches it. What
    /// matters is that the setter round-trips and that `0` is expressible.
    #[test]
    fn the_configured_priority_round_trips_and_can_be_turned_off() {
        let restore = writer_rt_priority();
        set_writer_rt_priority(DEFAULT_WRITER_RT_PRIORITY);
        assert_eq!(writer_rt_priority(), DEFAULT_WRITER_RT_PRIORITY);
        set_writer_rt_priority(0);
        assert_eq!(writer_rt_priority(), 0);
        set_writer_rt_priority(restore);
    }
}
