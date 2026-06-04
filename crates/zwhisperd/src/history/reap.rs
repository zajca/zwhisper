// `libc::kill` is the minimal surface needed to (a) test whether a
// recorded whisper-cli pid is still alive and (b) tear down an orphaned
// subprocess group left behind by an OOM/SIGKILL of a previous daemon
// (RFC-daemon-role F2.3). The workspace denies `unsafe_code` globally;
// it is allowed here, scoped to this one module, because there is no
// safe std API to signal an arbitrary pid and we deliberately avoid
// pulling `nix` for two FFI calls. Every call is documented inline.
#![allow(unsafe_code)]

//! Orphan-subprocess reaping for startup recovery (F2.3).
//!
//! When a daemon dies hard (SIGKILL/OOM) mid-transcribe, the
//! `whisper-cli` child it spawned can survive as an orphan. On restart,
//! before marking the interrupted entry, we check whether the recorded
//! pid is still alive and, if so, terminate its process group so a
//! later `Retry` cannot collide with a surviving writer.
//!
//! All pids handled here are values the daemon itself recorded into
//! `history.json` for processes it spawned in their own process group
//! (`process_group(0)` in `zwhisper_core::transcribe::whisper_cpp`).

use tracing::{info, warn};

/// Returns `true` when a process with `pid` currently exists.
///
/// Uses `kill(pid, 0)`: signal `0` performs the permission/existence
/// check without delivering a signal. `Ok` (0) means the process
/// exists; `ESRCH` means it does not.
#[must_use]
pub(crate) fn is_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: `kill` with sig 0 only probes existence/permission and
    // never delivers a signal. `pid` is a plain integer; no memory is
    // dereferenced.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0
}

/// Best-effort terminate the process group led by `pid` (the daemon
/// spawns `whisper-cli` as its own group leader, so `pgid == pid`).
///
/// Sends `SIGTERM` to `-pid` (the negative pid addresses the whole
/// group). Failures are logged, never propagated — recovery must
/// proceed even if the orphan is already gone or unkillable.
pub(crate) fn reap_group(pid: u32) {
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    if pid <= 1 {
        // Never signal pid 0/1 or the whole session (-1); a bogus
        // recorded value must not become a mass kill.
        warn!(pid, "refusing to reap implausible pid");
        return;
    }
    if !is_alive(pid as u32) {
        return;
    }
    info!(
        pid,
        "reaping orphaned whisper-cli process group from a prior daemon"
    );
    // SAFETY: targeting the negative pid signals the process group led
    // by `pid`. `pid > 1` is guarded above, so this can never become a
    // broadcast (`kill(-1, …)`) nor hit the init process. SIGTERM is a
    // graceful request; we do not escalate to SIGKILL.
    let rc = unsafe { libc::kill(-pid, libc::SIGTERM) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        warn!(pid, error = %err, "failed to reap orphaned process group");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn is_alive_true_for_self() {
        let me = std::process::id();
        assert!(is_alive(me));
    }

    #[test]
    fn is_alive_false_for_zero_and_implausible() {
        assert!(!is_alive(0));
        // u32::MAX is not a valid i32 pid → try_from fails → false.
        assert!(!is_alive(u32::MAX));
    }

    #[test]
    fn reap_group_noop_on_dead_pid_does_not_panic() {
        // A pid that is almost certainly not alive; reap must be a
        // silent no-op (is_alive guards the kill).
        reap_group(0x7FFF_FFF0);
    }

    #[test]
    fn reap_group_refuses_pid_one() {
        // Guard: must never signal init. No assertion beyond "does not
        // panic and returns"; the warn! path is exercised.
        reap_group(1);
    }
}
