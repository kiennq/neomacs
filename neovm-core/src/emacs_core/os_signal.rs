//! GNU's OS signal dispositions, and the one rule that decides their shape:
//! **what a signal handler is allowed to touch.**
//!
//! Before this module, `grep -rn 'sigaction|signal_hook|SIGWINCH|SIGINT'` over
//! `neovm-core`, `neomacs-bin`, `neomacs-display-runtime` and `neovm-worker`
//! returned **zero production hits** (ledger 180 §2, ledger 184).  This port
//! installed no OS signal handler at all, so every signal whose default
//! disposition is `Term` simply killed the editor.  Measured, `-Q --batch`,
//! at a process spinning in pure Lisp:
//!
//! ```text
//!              GNU 31.0.90                     this port, before
//!   SIGUSR2    rc=0, debug-on-quit t           rc=140, killed
//!   SIGUSR1    rc=0, nothing armed             rc=138, killed
//! ```
//!
//! # What GNU installs, and where
//!
//! `init_signals` (src/sysdep.c) is called from `main` after the image is in
//! memory, and its user-signal tail is:
//!
//! ```c
//!   /* SIGUSR1 and SIGUSR2 are used internally by the android_select
//!      function.  */
//!   #if !defined HAVE_ANDROID
//!   #ifdef SIGUSR1
//!     add_user_signal (SIGUSR1, "sigusr1");
//!   #endif
//!   #ifdef SIGUSR2
//!     add_user_signal (SIGUSR2, "sigusr2");
//!   #endif
//!   #endif
//! ```
//!
//! and `add_user_signal` (src/keyboard.c:8464-8483) ends with
//!
//! ```c
//!   emacs_sigaction_init (&action, deliver_user_signal);
//!   sigaction (sig, &action, 0);
//! ```
//!
//! # What a handler may legitimately do -- the whole design input
//!
//! GNU states the rule in the comment block above `handle_child_signal`
//! (src/process.c:7666-7688), and enforces it with a bug report inlined into
//! `child_signal_notify` (:7616-7650):
//!
//! ```text
//!    ** WARNING: this can be called during garbage collection.        /* :7673-7675 */
//!    ** Malloc WARNING: This should never call malloc either directly
//!    or indirectly; if it does, that is a bug.                        /* :7687-7688 */
//!
//!      But this calls `emacs_perror', which in turn invokes a localized
//!      version of strerror, which is not reentrant and must not be
//!      called within a signal handler:
//!        __lll_lock_wait_private () ... malloc () ... strerror_l () ...
//!      So we no longer check errors of emacs_write here.              /* :7630-7649 */
//! ```
//!
//! What survives at the bottom of GNU's handler is one
//! `emacs_write (fd, &dummy, 1)` to a self-pipe.  A fourth constraint is one
//! file over: `deliver_process_signal` (src/sysdep.c:1729-1751) exists because
//! *"POSIX says any thread can receive a signal that is associated with a
//! process"*, and forwards to the main thread with `pthread_kill`.
//!
//! # Why that makes the design, in a port with a render thread
//!
//! GNU's own `handle_user_signal` (src/keyboard.c:8487-8521) is **not**
//! restricted to the async-signal-safe list: it reads `Vdebug_on_event`,
//! calls `SSDATA (SYMBOL_NAME (...))` and writes four Lisp globals.  GNU can
//! do that only because `deliver_process_signal` has already forwarded the
//! signal to the thread that owns those globals.
//!
//! This port cannot forward.  Its Lisp state lives in a `&mut Context` owned
//! by the Lisp thread; there is no `pthread_kill`-shaped way to hand a
//! `&mut Context` to a handler, and a handler that reached the interpreter
//! from the render thread, a worker or the WPE `GMainContext` would be a data
//! race rather than a lock-order problem.  So the split is drawn one step
//! earlier than GNU draws it:
//!
//! * **the handler** does only what GNU's SIGCHLD handler does at its very
//!   bottom -- bump a lock-free counter and write one byte to a self-pipe.
//!   Both are on the POSIX.1-2024 §2.4.3 async-signal-safe list.  It is
//!   therefore correct on *any* thread, which is why this port needs no
//!   analogue of `FORWARD_SIGNAL_TO_MAIN_THREAD`.
//! * **`handle_user_signal`'s body** runs on the Lisp thread at the next safe
//!   point, [`drain_pending_os_signals`], which is where a `&mut Context`
//!   exists.
//!
//! # The type-level part: there is no place to write handler code
//!
//! The bad state this module exists to rule out is "a handler that can touch
//! a non-async-signal-safe thing".  It is made unrepresentable by leaving the
//! handler with nothing to be extended by:
//!
//! * [`HandledSignal`] is a closed, `#[repr(usize)]`, data-only enum whose
//!   `ALL` is declared with length `COUNT` derived from the last discriminant,
//!   so a variant that is not listed is a compile error (ledger 177's
//!   `post_image_init.rs` and ledger 180's `child_status.rs` shape).
//! * [`InstalledDisposition`] is **data, not code**: it carries GNU's
//!   `add_user_signal` NAME and nothing callable.  There is no per-signal
//!   handler hook, no function pointer and no closure anywhere in this
//!   module, so adding a signal adds a citation and a name -- never a line
//!   that runs in signal context.
//! * the one `extern "C"` handler is private, total over `HandledSignal`, and
//!   takes an [`AsyncSignalScope`]: a `!Send`/`!Sync` capability token with no
//!   public constructor whose only two methods are the two POSIX-listed
//!   operations.  **No method on it takes or returns a `Value`**, so a handler
//!   body that reached the interpreter would not type-check.
//!
//! The honest limit of that guarantee is worth stating: Rust lets any function
//! reach a `static`, so this is not a proof that no future author could lock a
//! mutex in signal context.  What it does buy is that the per-signal surface
//! is data and the code is written once -- the unsafe spelling is absent from
//! the type the way `child_status.rs` keeps `waitpid` out of `SweepableChild`.
//!
//! # Where the drain runs, and why that is GNU's own hot path
//!
//! GNU's safe point checks a `volatile bool` next to the quit flag:
//!
//! ```c
//!   /* src/lisp.h:3879, 3896-3900 */
//!   extern bool volatile pending_signals;
//!   INLINE void
//!   maybe_quit (void)
//!   {
//!     if (!NILP (Vquit_flag) || pending_signals)
//!       probably_quit ();
//!   }
//! ```
//!
//! so [`pending`] is one relaxed `'static` load in `Context::maybe_quit`'s
//! fast path -- exactly the shape and exactly the cost GNU pays, and the same
//! shape this port already uses for the Lisp profiler's watchdog flag.

use std::sync::OnceLock;
#[cfg(unix)]
use std::sync::atomic::AtomicI32;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// One of the OS signals this port installs a disposition for.
///
/// The list is closed and mechanically derivable: `grep -n 'add_user_signal ('
/// src/sysdep.c` gives exactly the two user-signal calls, and
/// `grep -n 'sigaction (SIGCHLD'` gives the third install
/// (`catch_child_signal`, src/process.c:8650).  Every other signal GNU names
/// in `init_signals` is unclaimed here and recorded as such in ledger 184 --
/// SIGWINCH, SIGINT, SIGHUP and SIGPIPE are all still in the hole ledger
/// 180 §9.6 opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(unix, repr(usize))]
pub(crate) enum HandledSignal {
    /// `add_user_signal (SIGUSR1, "sigusr1")`.
    #[cfg(unix)]
    Sigusr1 = 0,
    /// `add_user_signal (SIGUSR2, "sigusr2")`.  This is `debug-on-event`'s
    /// default value (src/keyboard.c:14358-14367).
    #[cfg(unix)]
    Sigusr2,
    /// `catch_child_signal` (src/process.c:8645-8660), which installs
    /// `deliver_child_signal` -> `handle_child_signal`.
    ///
    /// The one signal here whose handler does something for the EDITOR rather
    /// than for a Lisp program, and the reason it is in this enum at all is
    /// that its handler needs nothing the other two do not already have: GNU's
    /// own handler ends with `if (changed) child_signal_notify ()`, whose
    /// entire body is one `emacs_write` to a self-pipe (:7616-7650).  The walk
    /// that decides `changed` is what cannot go in a handler here, so it runs
    /// at the safe point instead (ledger 193).
    #[cfg(unix)]
    Sigchld,
}

impl HandledSignal {
    /// Derived from the last discriminant, so a variant missing from
    /// [`Self::ALL`] is a compile error rather than a silent omission.
    #[cfg(unix)]
    pub(crate) const COUNT: usize = Self::Sigchld as usize + 1;
    #[cfg(not(unix))]
    pub(crate) const COUNT: usize = 0;

    #[cfg(unix)]
    pub(crate) const ALL: [Self; Self::COUNT] = [Self::Sigusr1, Self::Sigusr2, Self::Sigchld];
    #[cfg(not(unix))]
    pub(crate) const ALL: [Self; Self::COUNT] = [];

    /// The OS signal number.
    pub(crate) const fn number(self) -> libc::c_int {
        #[cfg(unix)]
        match self {
            Self::Sigusr1 => libc::SIGUSR1,
            Self::Sigusr2 => libc::SIGUSR2,
            Self::Sigchld => libc::SIGCHLD,
        }
        #[cfg(not(unix))]
        match self {}
    }

    /// `file:line` of the install in the GNU tree.
    pub(crate) const fn gnu(self) -> &'static str {
        #[cfg(unix)]
        match self {
            Self::Sigusr1 => "src/sysdep.c, init_signals: add_user_signal (SIGUSR1, \"sigusr1\")",
            Self::Sigusr2 => "src/sysdep.c, init_signals: add_user_signal (SIGUSR2, \"sigusr2\")",
            Self::Sigchld => {
                "src/process.c:8650, catch_child_signal: sigaction (SIGCHLD, &action, &old_action)"
            }
        }
        #[cfg(not(unix))]
        match self {}
    }

    /// What the Lisp thread does with a delivery -- **data only**.
    pub(crate) const fn disposition(self) -> InstalledDisposition {
        #[cfg(unix)]
        match self {
            Self::Sigusr1 => InstalledDisposition::UserSignal {
                lisp_name: "sigusr1",
            },
            Self::Sigusr2 => InstalledDisposition::UserSignal {
                lisp_name: "sigusr2",
            },
            Self::Sigchld => InstalledDisposition::ChildStatus,
        }
        #[cfg(not(unix))]
        match self {}
    }

    /// The inverse of [`Self::number`], used by the handler.
    ///
    /// `const fn` over the closed enum rather than a table lookup, so it needs
    /// no static storage and cannot fault.
    #[cfg(unix)]
    const fn from_raw(sig: libc::c_int) -> Option<Self> {
        if sig == libc::SIGUSR1 {
            Some(Self::Sigusr1)
        } else if sig == libc::SIGUSR2 {
            Some(Self::Sigusr2)
        } else if sig == libc::SIGCHLD {
            Some(Self::Sigchld)
        } else {
            None
        }
    }
}

/// What a delivered signal means to Lisp.
///
/// Deliberately data and not a callback: see the module docs.  A second
/// disposition (SIGWINCH's `change_frame_size`, SIGCHLD's `handle_child_signal`
/// sweep) is a new variant plus one arm in [`drain_pending_os_signals`], and
/// still no new code in signal context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstalledDisposition {
    /// GNU's `add_user_signal (sig, NAME)`: the delivery becomes either a
    /// debugger entry or a `USER_SIGNAL_EVENT` carrying `NAME`, decided by
    /// `handle_user_signal` (src/keyboard.c:8487-8521).
    UserSignal { lisp_name: &'static str },
    /// GNU's `handle_child_signal` (src/process.c:7691): walk the process
    /// alist and stamp every child whose status changed.
    ///
    /// The walk is a `HashMap` iteration and a `waitpid` per child here, so it
    /// cannot go in the handler -- GNU's own two warnings above the function
    /// forbid both ("this can be called during garbage collection",
    /// "This should never call malloc").  What the handler does for it is what
    /// GNU's handler does at its very END and nothing more, which is why this
    /// variant adds NO code that runs in signal context.
    ChildStatus,
}

/// The disposition that was in place before this port installed its own.
///
/// GNU works around exactly one library that claims a signal Emacs also wants
/// (`lib_child_handler`, src/process.c:7654-7660, for Glib's SIGCHLD).  This
/// records the same fact for the signals installed here, so "nothing else
/// wanted them" is a measurement rather than an assumption.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PreviousDisposition {
    /// `SIG_DFL` -- for SIGUSR1/2 that means `Term`, which is the bug.
    Default,
    /// `SIG_IGN`.
    Ignored,
    /// Some other handler was already installed.
    Handler,
    /// `sigaction` could not report it.
    Unknown,
}

/// What [`install`] did, so the install is inspectable rather than assumed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InstallReport {
    previous: [PreviousDisposition; HandledSignal::COUNT],
    installed: [bool; HandledSignal::COUNT],
    /// The read end of the self-pipe, or `-1` if it could not be created.
    ///
    /// **Not yet registered with the wait poller** -- ledger 184's declared
    /// residual.  It exists because the handler's wake must be a `write`
    /// (GNU's `child_signal_notify`, src/process.c:7648) and because the fd
    /// is what a future registration needs.
    self_pipe_read_fd: libc::c_int,
}

impl InstallReport {
    pub(crate) fn previous(&self, signal: HandledSignal) -> PreviousDisposition {
        self.previous[signal as usize]
    }

    pub(crate) fn installed_count(&self) -> usize {
        self.installed.iter().filter(|done| **done).count()
    }

    /// The self-pipe read end, for the poller registration ledger 184 leaves
    /// open.  `None` when the pipe could not be created.
    pub(crate) fn self_pipe_read_fd(&self) -> Option<libc::c_int> {
        (self.self_pipe_read_fd >= 0).then_some(self.self_pipe_read_fd)
    }
}

// ---------------------------------------------------------------------------
// The async-signal-safe half.  Everything below this line runs in signal
// context; everything it may touch is declared here and nowhere else.
// ---------------------------------------------------------------------------

/// Per-signal delivery counter, GNU's `p->npending` (src/keyboard.c:8456,
/// bumped at :8511).
///
/// `AtomicU32` rather than `sig_atomic_t` because a lock-free atomic RMW is
/// what makes the count correct when the signal lands on the render thread.
/// `AtomicU32::fetch_add` compiles to `lock xadd` on this target and takes no
/// lock; `the_pending_counters_are_lock_free` asserts the stable spelling of
/// that fact, `cfg!(target_has_atomic = "32")`, because an atomic that fell
/// back to `core`'s lock would put a lock in signal context.
static PENDING: [AtomicU32; HandledSignal::COUNT] =
    [const { AtomicU32::new(0) }; HandledSignal::COUNT];

/// GNU's `pending_signals` (src/keyboard.c:105, `bool volatile`), read by
/// `maybe_quit` (src/lisp.h:3896-3900).
static PENDING_ANY: AtomicBool = AtomicBool::new(false);

/// The write end of GNU's self-pipe (`child_signal_write_fd`,
/// src/process.c:7595).  `-1` until [`install`] creates it.
#[cfg(unix)]
static SELF_PIPE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// GNU's `lib_child_handler` (src/process.c:7657), the SIGCHLD handler that
/// was installed BEFORE this port's -- `0` for "there was none".
///
/// GNU captures it in `catch_child_signal` and calls it as the last line of
/// its own handler (:7769), *"On POSIXish systems lacking pidfd_open+waitid or
/// using Glib 2.73.1-, Glib needs this to keep track of its own children"*
/// (:7652-7655).  Without the chain, installing a disposition for SIGCHLD
/// silently breaks whatever library had one.
///
/// This is the one callable thing in this module, and the type is what keeps
/// it honest: the only value that can ever be stored here comes out of
/// `sigaction`'s `old` argument, so it is a handler some OTHER code installed
/// and already runs in signal context -- never a function written here.  GNU's
/// own precondition is reproduced at the store site: `SIG_DFL`, `SIG_IGN` and
/// an `SA_SIGINFO` handler are all refused, because they are not callable with
/// GNU's one-argument signature (`eassert` at src/process.c:8653-8655).
static PREVIOUS_SIGCHLD_HANDLER: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// GNU's `lib_child_handler (sig)` (src/process.c:7769).
///
/// Runs in signal context, and is allowed to because the thing it calls was
/// already a signal handler before this module replaced it.
fn chain_to_previous_sigchld_handler(sig: libc::c_int) {
    let previous = PREVIOUS_SIGCHLD_HANDLER.load(Ordering::Relaxed);
    if previous == 0 {
        // GNU's `dummy_handler`, which is `static void dummy_handler (int sig)
        // {}` (src/process.c:7656) -- the case where nothing else wanted the
        // signal, which is what this box measures.
        return;
    }
    // SAFETY: `previous` was read out of `sigaction`'s `old_action` at install
    // time and stored only after `classify_previous` reported
    // `PreviousDisposition::Handler` and the `SA_SIGINFO` flag was clear, so it
    // is a `void (*)(int)` the C library was already prepared to call in
    // signal context with this very signal number.
    let handler: extern "C" fn(libc::c_int) = unsafe { std::mem::transmute(previous) };
    handler(sig);
}

/// A capability token that exists only for the duration of a signal handler.
///
/// Its two methods are the only two operations this port performs in signal
/// context, and both are on the POSIX.1-2024 §2.4.3 async-signal-safe list.
/// It is `!Send` and `!Sync` (the `PhantomData<*const ()>`), has no public
/// constructor, and **no method takes or returns a Lisp `Value`** -- which is
/// the type-level statement that a handler cannot reach the interpreter.
#[cfg(unix)]
struct AsyncSignalScope(std::marker::PhantomData<*const ()>);

#[cfg(unix)]
impl AsyncSignalScope {
    /// GNU's `p->npending++` (src/keyboard.c:8511) and its
    /// `pending_signals = true` (:8431).
    ///
    /// A lock-free atomic RMW: no allocation, no lock, no reentrancy hazard.
    fn record(&self, signal: HandledSignal) {
        PENDING[signal as usize].fetch_add(1, Ordering::Relaxed);
        PENDING_ANY.store(true, Ordering::Release);
    }

    /// GNU's `child_signal_notify`, whose entire surviving body is
    /// `emacs_write (fd, &dummy, 1)` (src/process.c:7648) -- and whose comment
    /// is the evidence for why nothing more may go here.
    ///
    /// The result is deliberately discarded, for GNU's own stated reason: the
    /// error path it used to have reached `strerror_l` and therefore `malloc`.
    fn wake(&self) {
        let fd = SELF_PIPE_WRITE_FD.load(Ordering::Relaxed);
        if fd < 0 {
            return;
        }
        let dummy: u8 = 0;
        // SAFETY: `write` is async-signal-safe (POSIX.1-2024 §2.4.3); `fd` is
        // the self-pipe's O_NONBLOCK write end, owned for the process's
        // lifetime, and the buffer is one byte of stack.  A short write or
        // EAGAIN is the coalescing this wake wants, so the result is dropped
        // exactly as GNU drops it.
        unsafe {
            libc::write(fd, std::ptr::addr_of!(dummy).cast(), 1);
        }
    }
}

/// GNU's `deliver_user_signal` (src/keyboard.c:8524-8531) with the forwarding
/// removed, because this handler is correct on any thread.
///
/// There is exactly one of these in the crate and it is total over
/// [`HandledSignal`], so a new signal adds no code that runs here.
#[cfg(unix)]
extern "C" fn deliver_user_signal(sig: libc::c_int) {
    // GNU preserves errno around the handler ("Races can occur even in
    // single-threaded hosts", src/sysdep.c:1734-1735).
    // SAFETY: `__errno_location` is the thread-local errno slot; reading and
    // restoring it is what GNU does at src/sysdep.c:1733 and :1750.
    let saved_errno = unsafe { *libc::__errno_location() };

    if let Some(signal) = HandledSignal::from_raw(sig) {
        let scope = AsyncSignalScope(std::marker::PhantomData);
        match signal.disposition() {
            // Every arm may use only `scope`, and `scope` has only these two
            // operations.  This `match` is the whole handler.
            //
            // The two arms are the same two operations, and that is the
            // finding rather than a coincidence: GNU's user-signal handler
            // ends with `p->npending++; pending_signals = true;`
            // (src/keyboard.c:8511-8512) and GNU's SIGCHLD handler ends with
            // `if (changed) child_signal_notify ();` (:7766-7767), whose body
            // is one `emacs_write`.  Everything GNU does BEFORE those lines
            // needs Lisp state, and is therefore at the safe point here.
            InstalledDisposition::UserSignal { .. } | InstalledDisposition::ChildStatus => {
                scope.record(signal);
                scope.wake();
            }
        }

        // GNU's `lib_child_handler (sig)` (src/process.c:7769), the last line
        // of `handle_child_signal`.  See [`chain_to_previous_sigchld_handler`].
        if matches!(signal, HandledSignal::Sigchld) {
            chain_to_previous_sigchld_handler(sig);
        }
    }

    // SAFETY: as above.
    unsafe { *libc::__errno_location() = saved_errno };
}

// ---------------------------------------------------------------------------
// Install (runs once, on the Lisp thread, from post_image_init)
// ---------------------------------------------------------------------------

static INSTALL: OnceLock<InstallReport> = OnceLock::new();

/// GNU's `init_signals` user-signal tail, run once per process.
///
/// Called from `post_image_init` because GNU calls `init_signals` from `main`
/// after the image is in memory (`PostImageInit::Signals`, src/emacs.c:1951);
/// idempotent, because `add_user_signal` opens by returning early for a signal
/// it has already added (src/keyboard.c:8470-8473).
pub(crate) fn install() -> &'static InstallReport {
    INSTALL.get_or_init(install_once)
}

#[cfg(unix)]
fn install_once() -> InstallReport {
    let self_pipe_read_fd = create_self_pipe();

    let mut previous = [PreviousDisposition::Unknown; HandledSignal::COUNT];
    let mut installed = [false; HandledSignal::COUNT];

    for signal in HandledSignal::ALL {
        // GNU's `emacs_sigaction_init` (src/sysdep.c:1678-1710) blocks the
        // nonfatal signals Emacs catches while this one is being handled, "so
        // race conditions are less likely".  The set is reproduced here for
        // the signals this port installs; the rest of GNU's set names signals
        // nothing here catches, so adding them would block a disposition that
        // does not exist.
        //
        // SAFETY: `sigaction` is called with a fully initialised `struct
        // sigaction` and a valid signal number, before any thread that could
        // race with it is started.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: `sigemptyset`/`sigaddset` write only through `sa_mask`.
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
            for other in HandledSignal::ALL {
                libc::sigaddset(&mut action.sa_mask, other.number());
            }
        }
        let handler: extern "C" fn(libc::c_int) = deliver_user_signal;
        action.sa_sigaction = handler as usize;
        // SA_RESTART unconditionally, where GNU takes it only when
        // `noninteractive` (`emacs_sigaction_flags`, src/sysdep.c:1660-1673).
        // GNU's interactive reason for omitting it is "we need to poll for
        // pending input so we need long-running syscalls to be interrupted";
        // this port's wake is the self-pipe instead, and the wait it must
        // interrupt is `epoll_wait`, which signal(7) lists among the calls
        // that are "never restarted after being interrupted by a signal
        // handler, regardless of the use of SA_RESTART".  So SA_RESTART costs
        // nothing here and spares every other blocking call in the render
        // thread and the workers a spurious EINTR.
        action.sa_flags = libc::SA_RESTART;

        // Seeded with a sentinel `sigaction` cannot produce, so
        // "the previous disposition was SIG_DFL" is a reading and not the
        // zero this struct would have had anyway.
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        old.sa_sigaction = usize::MAX;
        // SAFETY: both pointers are to fully initialised locals.
        let rc = unsafe { libc::sigaction(signal.number(), &action, &mut old) };
        let index = signal as usize;
        if rc == 0 {
            installed[index] = true;
            previous[index] = classify_previous(&old);
            // GNU's `catch_child_signal` tail (src/process.c:8656-8659), with
            // GNU's own precondition on the shape of what it will call:
            //
            //   eassert (old_action.sa_handler == SIG_DFL
            //            || old_action.sa_handler == SIG_IGN
            //            || ! (old_action.sa_flags & SA_SIGINFO));
            //
            // GNU asserts it; here it decides, because an `SA_SIGINFO` handler
            // has three parameters and calling it with one would be undefined
            // rather than merely wrong.  Refusing to chain is the conservative
            // half of that -- it loses the other library's notification, which
            // is recorded as this entry's residual rather than papered over.
            if matches!(signal, HandledSignal::Sigchld) {
                let chainable = matches!(previous[index], PreviousDisposition::Handler)
                    && old.sa_flags & libc::SA_SIGINFO == 0;
                if chainable {
                    PREVIOUS_SIGCHLD_HANDLER.store(old.sa_sigaction, Ordering::Release);
                }
                tracing::debug!(
                    previous = ?previous[index],
                    sa_siginfo = old.sa_flags & libc::SA_SIGINFO != 0,
                    chained = chainable,
                    "catch_child_signal: recorded lib_child_handler"
                );
            }
        }
    }

    let report = InstallReport {
        previous,
        installed,
        self_pipe_read_fd,
    };

    // The install is the kind of fact that is invisible until it is wrong, so
    // it says what it did once, with GNU's line for each disposition and the
    // disposition it replaced.  `PreviousDisposition::Handler` here would mean
    // something else in this process wanted the signal -- GNU has exactly one
    // such case and works around it by hand (`lib_child_handler`,
    // src/process.c:7654-7660).
    for signal in HandledSignal::ALL {
        tracing::debug!(
            signal = ?signal,
            number = signal.number(),
            gnu = signal.gnu(),
            previous = ?report.previous(signal),
            self_pipe_read_fd = ?report.self_pipe_read_fd(),
            "init_signals: installed an OS signal disposition"
        );
    }

    report
}

#[cfg(not(unix))]
fn install_once() -> InstallReport {
    InstallReport {
        previous: [],
        installed: [],
        self_pipe_read_fd: -1,
    }
}

#[cfg(unix)]
fn classify_previous(old: &libc::sigaction) -> PreviousDisposition {
    match old.sa_sigaction {
        // The seed survived: `sigaction` reported success without writing the
        // old action, which no implementation should do -- but if one does,
        // the answer must not be the sentinel-shaped `SIG_DFL`.
        handler if handler == usize::MAX => PreviousDisposition::Unknown,
        handler if handler == libc::SIG_DFL => PreviousDisposition::Default,
        handler if handler == libc::SIG_IGN => PreviousDisposition::Ignored,
        _ => PreviousDisposition::Handler,
    }
}

/// GNU's `child_signal_init` (src/process.c:7580-7597): a nonblocking,
/// close-on-exec pipe whose read end the wait registers and whose write end
/// the handler pokes.
#[cfg(unix)]
fn create_self_pipe() -> libc::c_int {
    let mut fds: [libc::c_int; 2] = [-1, -1];
    // SAFETY: `pipe2` writes exactly two ints through the array.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if rc != 0 {
        return -1;
    }
    SELF_PIPE_WRITE_FD.store(fds[1], Ordering::Release);
    fds[0]
}

// ---------------------------------------------------------------------------
// The safe-point half (runs on the Lisp thread, where anything is allowed)
// ---------------------------------------------------------------------------

/// GNU's `pending_signals` (src/keyboard.c:105), read by `maybe_quit`
/// (src/lisp.h:3896-3900).  One relaxed `'static` load.
#[inline(always)]
pub(crate) fn pending() -> bool {
    PENDING_ANY.load(Ordering::Relaxed)
}

/// The pending count for one signal, without consuming it.
///
/// Exists because a delivery can land on a thread other than the one that
/// asked for it: `kill (getpid (), sig)` only promises delivery before it
/// returns when the kernel picks the CALLING thread, and it is free not to.
/// That is the property this module is built around -- the handler is correct
/// wherever it lands -- so the tests observe it rather than sidestepping it
/// with `raise`.
#[cfg(test)]
pub(crate) fn pending_count(signal: HandledSignal) -> u32 {
    PENDING[signal as usize].load(Ordering::Acquire)
}

/// GNU's `store_user_signal_events` drain (src/keyboard.c:8546-8570): take the
/// pending counts and reset them, so a delivery is consumed exactly once.
///
/// `#[cfg(test)]` on purpose, and the reason is the entry's finding rather
/// than an oversight: production does NOT take the counts wholesale.
/// [`drain_pending_os_signals`] consumes only the deliveries whose action is
/// the debugger arm and leaves the rest in `p->npending`, because the port of
/// `store_user_signal_events` -- the half that would queue a
/// `USER_SIGNAL_EVENT` for `special-event-map` -- is ledger 184's declared
/// residual.  When that lands, this becomes its drain and the attribute goes.
#[cfg(test)]
pub(crate) fn take_pending() -> [u32; HandledSignal::COUNT] {
    PENDING_ANY.store(false, Ordering::Release);
    let mut taken = [0u32; HandledSignal::COUNT];
    for signal in HandledSignal::ALL {
        taken[signal as usize] = PENDING[signal as usize].swap(0, Ordering::AcqRel);
    }
    taken
}

/// GNU's `handle_user_signal` body (src/keyboard.c:8487-8521), as a decision
/// over data.
///
/// GNU makes this decision *in the handler*, which it can do only because
/// `deliver_process_signal` already forwarded the signal to the thread that
/// owns `Vdebug_on_event`.  Here it is made at the safe point instead, for the
/// reason in the module docs, and the comparison is GNU's: the printed name of
/// `debug-on-event` against the signal's `add_user_signal` NAME.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UserSignalAction {
    /// `debug-on-event` names this signal: arm the debugger four ways and EAT
    /// the event (src/keyboard.c:8498-8508).
    EnterDebugger,
    /// Otherwise `p->npending++`, and `store_user_signal_events` later queues
    /// a `USER_SIGNAL_EVENT` whose Lisp form is `(intern NAME)`
    /// (src/keyboard.c:7251-7258) for `special-event-map`.
    QueueEvent { lisp_name: &'static str },
    /// GNU's `handle_child_signal` body (src/process.c:7734-7763): stamp every
    /// child whose status changed, and run NO sentinel.
    ///
    /// GNU does this IN the handler; here it is the safe point's work, for the
    /// reason `child_status.rs` gives at length -- the walk allocates and the
    /// process table is owned by the Lisp thread.  What that costs is
    /// LATENESS, and lateness in this direction is the safe direction: GNU's
    /// record is made when the signal is delivered and this one at the first
    /// safe point after, so this port's record can never be EARLIER than GNU's
    /// for any program.  That is the property ledger 180's synchronous sweep
    /// lacked, and it is why this is a trigger rather than a poll.
    RecordChildStatuses,
}

impl UserSignalAction {
    /// `debug_on_event_name` is `Some` only when `debug-on-event` holds a
    /// SYMBOL, which is GNU's `if (SYMBOLP (Vdebug_on_event))` at :8492.
    pub(crate) fn for_signal(signal: HandledSignal, debug_on_event_name: Option<&str>) -> Self {
        match signal.disposition() {
            InstalledDisposition::ChildStatus => Self::RecordChildStatuses,
            InstalledDisposition::UserSignal { lisp_name } => match debug_on_event_name {
                Some(name) if name == lisp_name => Self::EnterDebugger,
                _ => Self::QueueEvent { lisp_name },
            },
        }
    }
}

/// How many deliveries each arm of [`UserSignalAction`] consumed.
///
/// An engagement counter, not a convenience: ledger P5.2's skip was 100% green
/// and fired ZERO times, so a mechanism that can silently never run has to be
/// able to say how often it ran.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct UserSignalDrain {
    /// Deliveries that took GNU's debugger arm (src/keyboard.c:8498-8508).
    pub(crate) armed_debugger: u32,
    /// Deliveries left in `p->npending` for the input path to queue.
    pub(crate) left_pending: u32,
    /// SIGCHLD deliveries that ran `handle_child_signal`'s body.
    ///
    /// A trigger that never fires is indistinguishable from no trigger while
    /// every test happens to observe a status anyway -- ledger P5.2's skip was
    /// 100% green and fired ZERO times -- so the count is reported rather than
    /// inferred.
    pub(crate) swept_child_statuses: u32,
}

/// GNU's `handle_user_signal` body (src/keyboard.c:8487-8521), run at the Lisp
/// thread's safe point.
///
/// GNU reaches it from `probably_quit`'s `else if (pending_signals)`
/// (src/eval.c:1868-1876), and clears the flag first
/// (`process_pending_signals`, src/keyboard.c:8367-8372) -- both reproduced
/// here, so a delivery cannot make every later safe point take the cold path.
///
/// **Only the debugger arm is consumed.**  GNU "eats the event" there (:8506)
/// and never bumps `npending`; a `QueueEvent` delivery stays in the counter --
/// which IS GNU's `p->npending` storage, not a second one -- until
/// `store_user_signal_events` (:8546-8570) takes it while reading input.  That
/// half is ledger 184's declared residual, and leaving the count in place is
/// what keeps it a residual rather than a lost event.
pub(crate) fn drain_pending_os_signals(
    eval: &mut crate::emacs_core::eval::Context,
) -> UserSignalDrain {
    // GNU's `process_pending_signals` opens with `pending_signals = false;`.
    PENDING_ANY.store(false, Ordering::Release);

    let debug_on_event = eval.debug_on_event_signal_name();
    let mut drain = UserSignalDrain::default();
    for signal in HandledSignal::ALL {
        let slot = &PENDING[signal as usize];
        let pending_here = slot.load(Ordering::Acquire);
        if pending_here == 0 {
            continue;
        }
        match UserSignalAction::for_signal(signal, debug_on_event.as_deref()) {
            UserSignalAction::EnterDebugger => {
                // GNU decides per DELIVERY, inside the handler; this decides
                // per drain window, because the decision needs Lisp state a
                // handler must not read.  The two differ only if
                // `debug-on-event` changes between two deliveries of the same
                // signal inside one window, which no arm of GNU's own code
                // does.
                slot.fetch_sub(pending_here, Ordering::AcqRel);
                eval.arm_debugger_for_debug_on_event();
                drain.armed_debugger += pending_here;
            }
            UserSignalAction::QueueEvent { .. } => {
                drain.left_pending += pending_here;
            }
            UserSignalAction::RecordChildStatuses => {
                // Consumed, not left pending: there is no later queue for this
                // one.  GNU's handler makes the record and the delivery is
                // then spent; `status_notify` takes it from the process table,
                // not from `npending`.
                slot.fetch_sub(pending_here, Ordering::AcqRel);
                eval.processes.record_child_status_changes();
                drain.swept_child_statuses += pending_here;
            }
        }
    }
    drain
}

#[cfg(test)]
#[path = "os_signal_test.rs"]
mod os_signal_test;
