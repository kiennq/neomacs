//! Ledger 184: the OS signal dispositions this port never installed.
//!
//! GNU's `init_signals` (src/sysdep.c) ends with
//!
//! ```c
//!   #ifdef SIGUSR1
//!     add_user_signal (SIGUSR1, "sigusr1");
//!   #endif
//!   #ifdef SIGUSR2
//!     add_user_signal (SIGUSR2, "sigusr2");
//!   #endif
//! ```
//!
//! and `add_user_signal` (src/keyboard.c:8464-8483) ends with
//! `emacs_sigaction_init (&action, deliver_user_signal); sigaction (sig,
//! &action, 0);`.  Without that install the kernel's default disposition for
//! both signals is `Term`, so the editor DIES.  Measured, `-Q --batch`,
//! `kill -USR1` / `kill -USR2` at a process spinning in pure Lisp:
//!
//! ```text
//!              GNU 31.0.90                    this port, before
//!   SIGUSR2    rc=0, debug-on-quit t          rc=140, killed
//!   SIGUSR1    rc=0, nothing armed            rc=138, killed
//! ```

#[cfg(windows)]
use super::{self as os_signal, HandledSignal};
#[cfg(unix)]
use super::{
    self as os_signal, HandledSignal, InstalledDisposition, PreviousDisposition, UserSignalAction,
};

/// Send SIG to this whole process, the way `kill -USR1 PID` does.
///
/// `libc::raise` targets the calling THREAD, which is a weaker question than
/// the one GNU answers: `deliver_process_signal` (src/sysdep.c:1729-1751)
/// exists precisely because "POSIX says any thread can receive a signal that
/// is associated with a process".
#[cfg(unix)]
fn kill_self(sig: libc::c_int) {
    // SAFETY: `kill` with the caller's own pid and a valid signal number.
    let rc = unsafe { libc::kill(libc::getpid(), sig) };
    assert_eq!(rc, 0, "kill(getpid(), {sig}) failed");
}

/// `kill -SIG PID`, then WAIT until the delivery has been recorded.
///
/// **The wait is the design's claim under test, not a workaround.**  `strace`
/// of this very test shows the `kill` issued from libtest's worker thread and
/// the signal delivered to the MAIN thread:
///
/// ```text
///   3682381 kill(3682377, SIGUSR1)  = 0
///   3682377 --- SIGUSR1 {si_signo=SIGUSR1, si_code=SI_USER, si_pid=3682377} ---
/// ```
///
/// POSIX promises delivery before `kill` returns only when the signal goes to
/// the CALLING thread, and here the kernel chose otherwise -- which is exactly
/// the case `deliver_process_signal` (src/sysdep.c:1729-1751) exists for in
/// GNU and that this port handles by making the handler correct on any thread.
/// `raise`/`pthread_kill` would take the wait away and the question with it.
#[cfg(unix)]
fn kill_self_and_wait(signal: HandledSignal) {
    let before = os_signal::pending_count(signal);
    kill_self(signal.number());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while os_signal::pending_count(signal) == before && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
}

/// The red this entry started from: with no handler installed, this test's
/// process is TERMINATED by the signal and nextest reports it killed rather
/// than failed.  It survives only because [`os_signal::install`] ran.
#[test]
#[cfg(unix)]
fn a_user_signal_does_not_terminate_this_process_like_gnu() {
    let report = os_signal::install();
    assert!(
        report.installed_count() > 0,
        "install() reported no dispositions: {report:?}"
    );

    kill_self_and_wait(HandledSignal::Sigusr1);
    kill_self_and_wait(HandledSignal::Sigusr2);

    // Reaching this line at all is the assertion GNU's `rc=0` column makes.
    let pending = os_signal::take_pending();
    assert_eq!(
        pending[HandledSignal::Sigusr1 as usize],
        1,
        "SIGUSR1 was survived but not recorded: {pending:?}"
    );
    assert_eq!(
        pending[HandledSignal::Sigusr2 as usize],
        1,
        "SIGUSR2 was survived but not recorded: {pending:?}"
    );
}

/// GNU installs these two and nothing else wanted them: the previous
/// disposition of both is `SIG_DFL`.
///
/// This is the control on the install itself.  GNU works around exactly one
/// library that claims a signal it also wants (`lib_child_handler`,
/// src/process.c:7654-7660, for Glib's SIGCHLD); there is no such library for
/// SIGUSR1/2 on this platform, and GNU's own reason for skipping them --
/// `#if !defined HAVE_ANDROID`, because `android_select` uses them -- does not
/// apply here.
#[test]
#[cfg(unix)]
fn the_two_user_signals_were_unclaimed_before_this_port_installed_them() {
    let report = os_signal::install();
    for signal in HandledSignal::ALL {
        assert_eq!(
            report.previous(signal),
            PreviousDisposition::Default,
            "{signal:?} was already claimed by something else"
        );
    }
}

/// Every handled signal names its GNU install site, its Lisp name and its
/// disposition, and the table cannot be emptied.
///
/// The shape is ledger 177's `post_image_init.rs` and ledger 180's
/// `child_status.rs`: `ALL` is declared with length `COUNT`, derived from the
/// last discriminant, so a variant that is not listed is a compile error, and
/// an emptied table fails here rather than passing over nothing.
#[test]
#[cfg(unix)]
fn every_handled_signal_carries_its_gnu_citation_and_disposition() {
    assert_eq!(
        HandledSignal::COUNT,
        3,
        "GNU installs a user-signal handler for exactly SIGUSR1 and SIGUSR2 \
         (src/sysdep.c, init_signals) and a SIGCHLD one in catch_child_signal \
         (src/process.c:8650); a fourth needs its own citation"
    );
    assert_eq!(HandledSignal::ALL.len(), HandledSignal::COUNT);

    for signal in HandledSignal::ALL {
        assert!(
            signal.gnu().starts_with("src/"),
            "{signal:?} has no GNU citation"
        );
        assert!(signal.number() > 0, "{signal:?} has no signal number");
        // Exhaustive on purpose: a disposition added without an arm here is a
        // compile error rather than a silently unclassified signal.
        match signal.disposition() {
            InstalledDisposition::UserSignal { lisp_name } => assert!(
                !lisp_name.is_empty(),
                "{signal:?} has no `add_user_signal' NAME"
            ),
            InstalledDisposition::ChildStatus => assert_eq!(
                signal,
                HandledSignal::Sigchld,
                "only SIGCHLD records child statuses"
            ),
        }
    }

    assert_eq!(HandledSignal::Sigusr1.number(), libc::SIGUSR1);
    assert_eq!(HandledSignal::Sigusr2.number(), libc::SIGUSR2);
    assert_eq!(HandledSignal::Sigchld.number(), libc::SIGCHLD);
    assert_eq!(
        HandledSignal::Sigusr1.disposition(),
        InstalledDisposition::UserSignal {
            lisp_name: "sigusr1"
        }
    );
    assert_eq!(
        HandledSignal::Sigusr2.disposition(),
        InstalledDisposition::UserSignal {
            lisp_name: "sigusr2"
        }
    );
    assert_eq!(
        HandledSignal::Sigchld.disposition(),
        InstalledDisposition::ChildStatus
    );
}

/// GNU's `handle_user_signal` decides between two arms by comparing
/// `Vdebug_on_event`'s symbol name with the signal's `add_user_signal` name
/// (src/keyboard.c:8487-8508).  Here that comparison runs on the Lisp thread
/// at the safe point, because BOTH arms touch Lisp state.
#[test]
#[cfg(unix)]
fn debug_on_event_selects_the_debugger_arm_by_name_like_gnu() {
    // `debug-on-event' defaults to `sigusr2' (src/keyboard.c:14358-14367).
    assert_eq!(
        UserSignalAction::for_signal(HandledSignal::Sigusr2, Some("sigusr2")),
        UserSignalAction::EnterDebugger
    );
    assert_eq!(
        UserSignalAction::for_signal(HandledSignal::Sigusr1, Some("sigusr2")),
        UserSignalAction::QueueEvent {
            lisp_name: "sigusr1"
        }
    );
    // `if (SYMBOLP (Vdebug_on_event))' (:8492): a non-symbol selects no arm.
    assert_eq!(
        UserSignalAction::for_signal(HandledSignal::Sigusr2, None),
        UserSignalAction::QueueEvent {
            lisp_name: "sigusr2"
        }
    );
}

/// **Why rows 2 and 3 of ledger 184 are declined**, measured rather than
/// argued.
///
/// `(process-attributes pid)` answers `"Z"` here and `nil` in GNU, and
/// `(signal-process p 0)` answers `0` here and `-1` in GNU, because GNU's
/// SIGCHLD handler REAPS: `child_status_changed` is `waitpid`
/// (src/process.c:7741-7742), so GNU's exited child is gone from the OS within
/// microseconds and this port's is a zombie until something waits.
///
/// Both rows therefore need a reaper that runs with nobody waiting -- ledger
/// 180 §9.1's "dedicated reaper" -- and its cost is not the thread:
///
/// > `waitpid` must then have exactly ONE owner, and today every
/// > `try_wait`/`poll_child_status` path reaps on the Lisp thread, so a second
/// > reaper is a double-reap hazard across the whole file.
///
/// This is that hazard as a measurement.  `std::process::Child` owns its
/// child's reap; a second reaper that gets there first takes the status the
/// owner would have reported, and the owner is left with `ECHILD` and no exit
/// code -- which is exactly the `(exit . 7)` this port's `process-status`
/// answers from.  Five call sites reach a reap today (`process.rs:975`,
/// `:984`, `:6340`, `:6359`, and `sys::poll_child_status`), three of them
/// through `std::process::Child`.
#[test]
#[cfg(unix)]
fn a_second_reaper_takes_the_exit_status_the_owner_would_have_reported() {
    let mut child = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("exit 7")
        .spawn()
        .expect("spawn /bin/sh");
    let pid = child.id() as libc::pid_t;

    // The reaper GNU's SIGCHLD handler is, arriving first.
    let mut raw: libc::c_int = 0;
    // SAFETY: a blocking `waitpid` on a child of this process.
    let reaped = unsafe { libc::waitpid(pid, &mut raw, 0) };
    assert_eq!(reaped, pid, "the second reaper did not get the child");
    assert!(libc::WIFEXITED(raw));
    assert_eq!(
        libc::WEXITSTATUS(raw),
        7,
        "the second reaper holds the exit code"
    );

    // And the owner can no longer report it.  This is the whole reason rows 2
    // and 3 are their own entry: closing them means giving `waitpid` ONE owner
    // across every site above, not adding a thread.
    let owner_says = child.try_wait();
    assert!(
        !matches!(owner_says, Ok(Some(_))),
        "the owning std::process::Child still reported a status ({owner_says:?}); \
         if that ever becomes true the double-reap hazard is gone and ledger \
         184's rows 2 and 3 can be reconsidered"
    );
}

/// The handler's wake is GNU's, and the fd it needs exists.
///
/// GNU's `child_signal_init` (src/process.c:7580-7597) makes a nonblocking
/// pipe, `add_read_fd`s the read end, and `child_signal_notify` writes one
/// byte to the write end from signal context -- the ONE thing left in GNU's
/// handler after `emacs_perror` had to be deleted for reaching `malloc`
/// through `strerror_l` (:7630-7649).
///
/// This asserts the mechanism rather than the wiring, and the difference is
/// ledger 184's declared residual: the read end is created and the byte is
/// written, but the fd is **not yet registered with the wait poller**, so a
/// signal delivered while the Lisp thread is blocked in `poller.wait` is
/// noticed through `epoll_wait`'s EINTR (which signal(7) says is never
/// restarted) rather than through a readable fd.
#[test]
#[cfg(unix)]
fn the_handler_has_gnus_self_pipe_and_it_carries_a_byte() {
    let report = os_signal::install();
    let read_fd = report
        .self_pipe_read_fd()
        .expect("install created GNU's self-pipe");

    // Drain anything an earlier test in this process left behind.
    let mut sink = [0u8; 64];
    loop {
        // SAFETY: a nonblocking read of the pipe's own read end.
        let n = unsafe { libc::read(read_fd, sink.as_mut_ptr().cast(), sink.len()) };
        if n <= 0 {
            break;
        }
    }

    kill_self_and_wait(HandledSignal::Sigusr1);
    let _ = os_signal::take_pending();

    // SAFETY: as above.
    let n = unsafe { libc::read(read_fd, sink.as_mut_ptr().cast(), sink.len()) };
    let errno = std::io::Error::last_os_error();
    assert!(
        n >= 1,
        "the handler wrote no wake byte to the self-pipe \
         (read fd {read_fd} returned {n}, errno {errno:?})"
    );
}

/// The counter the handler bumps must be lock-free, or the handler is not
/// async-signal-safe no matter what it is written in.
///
/// GNU's is a plain `int` (`p->npending`, src/keyboard.c:8456) reached only
/// from the thread `deliver_process_signal` forwarded to.  This port's
/// handler runs on whatever thread the kernel picked, so the counter has to
/// be an atomic -- and an atomic that fell back to a lock would put a lock in
/// signal context, which is exactly the state this module exists to exclude.
#[test]
#[cfg(unix)]
fn the_pending_counters_are_lock_free() {
    // `Atomic*::is_lock_free` is still unstable, and `target_has_atomic` is
    // the stable spelling of the same fact: rustc sets it only for widths the
    // target implements natively, and for any other width `core` would fall
    // back to a lock -- which is the state this asserts against.
    assert!(
        cfg!(target_has_atomic = "32"),
        "AtomicU32 is not native on this target, so the pending-signal counter \
         would take a lock in signal context"
    );
    assert!(
        cfg!(target_has_atomic = "8"),
        "AtomicBool is not native on this target, so the pending-signal flag \
         would take a lock in signal context"
    );
}
/// The trigger's ENGAGEMENT counter, and the previous disposition it replaced.
///
/// Ledger P5.2's skip was 100% green and fired ZERO times, so a mechanism that
/// can silently never run has to be able to say how often it ran.  This asks
/// the drain directly: deliver a real SIGCHLD (with `kill`, to the PROCESS, so
/// the kernel may pick any thread -- the property this module is built around)
/// and assert that the safe point consumed it and reports the sweep.
#[cfg(unix)]
#[test]
fn a_delivered_sigchld_is_consumed_by_the_safe_point_and_counted() {
    let report = os_signal::install();
    assert!(
        report.installed_count() >= HandledSignal::COUNT,
        "install() did not install every disposition: {report:?}"
    );

    // The evaluator is built BEFORE the delivery, and that ordering is a
    // measurement rather than tidiness: with `Context::new()` after the
    // `kill`, this test failed with `swept_child_statuses: 0`, because
    // building an evaluator runs Lisp, Lisp reaches `maybe_quit`, and
    // `maybe_quit` had already drained the delivery.  Which is the trigger
    // working -- so the reorder keeps the pin measuring the DRAIN rather than
    // racing the safe point it is about.
    let mut eval = crate::emacs_core::eval::Context::new();

    kill_self_and_wait(HandledSignal::Sigchld);
    assert!(
        os_signal::pending_count(HandledSignal::Sigchld) > 0,
        "the handler recorded nothing"
    );
    assert!(os_signal::pending(), "GNU's `pending_signals' must be set");

    let drain = os_signal::drain_pending_os_signals(&mut eval);

    assert!(
        drain.swept_child_statuses > 0,
        "the SIGCHLD arm did not run: {drain:?}"
    );
    assert_eq!(
        os_signal::pending_count(HandledSignal::Sigchld),
        0,
        "GNU's handler spends the delivery; there is no later queue for it"
    );
    assert!(
        !os_signal::pending(),
        "`process_pending_signals' clears the flag first (src/keyboard.c:8367-8372)"
    );
}

/// GNU's SIGCHLD disposition before Emacs installs one, and the
/// `lib_child_handler` question that goes with it.
///
/// `catch_child_signal` (src/process.c:8645-8660) keeps whatever handler was
/// already installed and calls it as the last line of its own
/// (`lib_child_handler (sig)`, :7769), *"On POSIXish systems lacking
/// pidfd_open+waitid or using Glib 2.73.1-, Glib needs this to keep track of
/// its own children"*.  On a kernel with `pidfd_open` and a Glib newer than
/// 2.73.2 the hack is not needed and GNU's own `glib_installs_sigchld_handler`
/// stays false (:8705-8731).
///
/// This asserts the MEASUREMENT rather than the assumption: in this build
/// nothing else had claimed SIGCHLD, so the chain is `dummy_handler` and the
/// question does not arise here.  If a future build links a library that does
/// claim it, this pin is what turns that into a failing test rather than a
/// silently broken trigger.
#[cfg(unix)]
#[test]
fn sigchld_was_unclaimed_before_this_port_installed_it() {
    let report = os_signal::install();
    assert_eq!(
        report.previous(HandledSignal::Sigchld),
        PreviousDisposition::Default,
        "something else in this process wanted SIGCHLD; GNU's answer is \
         lib_child_handler (src/process.c:7657, 8656-8659) and this port's \
         chain would have to be exercised rather than merely present"
    );
}

/// Windows has no POSIX SIGUSR dispositions, so this module must expose no
/// signal entries and must not create an install-time wake mechanism.
#[cfg(windows)]
#[test]
fn windows_does_not_advertise_or_install_user_signals() {
    assert!(HandledSignal::ALL.is_empty());

    let report = os_signal::install();
    assert_eq!(report.installed_count(), 0);
    assert_eq!(report.self_pipe_read_fd(), None);
    assert!(!os_signal::pending());
}
