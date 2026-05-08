//! Graceful-shutdown policy types passed to [`ProcessHandle::terminate`] and friends.
//!
//! See [`GracefulShutdown`] for the cross-platform value, [`UnixGracefulShutdown`] for the Unix
//! phase model, and [`WindowsGracefulShutdown`] for the Windows single-phase model.
//!
//! [`ProcessHandle::terminate`]: crate::ProcessHandle::terminate

#![cfg(any(unix, windows))]

use std::marker::PhantomData;
use std::time::Duration;

/// Per-platform graceful-shutdown policy passed to [`ProcessHandle::terminate`] and related APIs.
///
/// Carries a sequence of one or more graceful-shutdown signals to dispatch before falling back to
/// the implicit forceful kill. The shape is platform-conditional because the available
/// graceful-shutdown signals themselves are platform-conditional:
///
/// - On Unix it carries a [`UnixGracefulShutdown`] of one or more [`UnixGracefulPhase`]s.
/// - On Windows it carries a single [`WindowsGracefulShutdown`] timeout for the only available
///   graceful signal, `CTRL_BREAK_EVENT`.
///
/// SIGKILL on Unix and `TerminateProcess` on Windows are always the implicit final fallback;
/// they are not configurable phases.
///
/// # Choosing the Unix sequence
///
/// See [`UnixGracefulShutdown`] for the recommended single-signal sequences and a discussion of
/// why mixing SIGINT and SIGTERM does not cover children with unknown signal handlers.
///
/// # Cross-platform construction
///
/// Use [`GracefulShutdown::builder`] to write a single cross-platform construction expression.
/// The setter for the platform that does not match the current target accepts its argument
/// without using it, so no cfg gates are needed at the call site:
///
/// ```rust
/// use std::time::Duration;
/// use tokio_process_tools::GracefulShutdown;
///
/// // Common case: single-signal sequences via the convenience shortcuts.
/// let shutdown = GracefulShutdown::builder()
///     .unix_sigterm(Duration::from_secs(10))
///     .windows_ctrl_break(Duration::from_secs(10))
///     .build();
/// ```
///
/// For the rare multiphase case, use the [`GracefulShutdownBuilder::unix`] setter, accepting a
/// custom-built `UnixGracefulShutdown`:
///
/// ```rust
/// use std::time::Duration;
/// use tokio_process_tools::{
///     GracefulShutdown, UnixGracefulPhase, UnixGracefulShutdown,
/// };
///
/// let shutdown = GracefulShutdown::builder()
///     .unix(UnixGracefulShutdown::from_phases([
///         UnixGracefulPhase::interrupt(Duration::from_secs(60)),
///         UnixGracefulPhase::terminate(Duration::from_secs(5)),
///     ]))
///     .windows_ctrl_break(Duration::from_secs(10))
///     .build();
/// ```
///
/// # Platform availability
///
/// This type is only available on Unix and Windows because the underlying graceful-shutdown
/// signals only exist there. On other Tokio-supported targets the spawn, wait, output-collection,
/// and [`ProcessHandle::kill`] APIs remain available; only the graceful-termination surface
/// (`terminate(...)`, `terminate_on_drop(...)`, `wait_for_completion_or_terminate(...)`, the
/// `send_*_signal(...)` methods, and this type) is gated out.
///
/// [`ProcessHandle::terminate`]: super::ProcessHandle::terminate
/// [`ProcessHandle::kill`]: super::ProcessHandle::kill
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GracefulShutdown {
    /// Unix graceful-shutdown phase sequence.
    #[cfg(unix)]
    pub unix: UnixGracefulShutdown,
    /// Windows graceful-shutdown timeout for the single `CTRL_BREAK_EVENT` phase.
    #[cfg(windows)]
    pub windows: WindowsGracefulShutdown,
}

impl GracefulShutdown {
    /// Start a fluent specification of a `GracefulShutdown` value.
    ///
    /// Call [`unix`](GracefulShutdownBuilder::unix), then
    /// [`windows`](GracefulShutdownBuilder::windows), then
    /// [`build`](GracefulShutdownBuilder::build). The setter for the platform that does not
    /// match the current target accepts its argument without using it, which lets cross-platform
    /// code construct the value without cfg gates.
    #[must_use]
    pub fn builder() -> GracefulShutdownBuilder<UnixUnset> {
        GracefulShutdownBuilder {
            #[cfg(unix)]
            unix: None,
            #[cfg(windows)]
            windows: WindowsGracefulShutdown {
                timeout: Duration::ZERO,
            },
            _state: PhantomData,
        }
    }
}

/// Typestate marker indicating that the Unix-side sequence has not been provided yet.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct UnixUnset;

/// Typestate marker indicating that the Unix-side sequence has been provided but the
/// Windows-side budget has not.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct UnixSet;

/// Typestate marker indicating that both the Unix-side sequence and the Windows-side budget have
/// been provided. A builder in this state can be finished with
/// [`build`](GracefulShutdownBuilder::build).
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct BothSet;

/// Typestate builder for [`GracefulShutdown`]. Created via [`GracefulShutdown::builder`].
///
/// Both [`unix`](Self::unix) and [`windows`](Self::windows) must be called (in that order)
/// before [`build`](Self::build) becomes available. The setter for the platform that does not
/// match the current target accepts its argument without using it, so cross-platform code can
/// build a value without cfg gates.
#[derive(Debug, Clone)]
pub struct GracefulShutdownBuilder<State> {
    #[cfg(unix)]
    unix: Option<UnixGracefulShutdown>,
    #[cfg(windows)]
    windows: WindowsGracefulShutdown,
    _state: PhantomData<fn() -> State>,
}

impl GracefulShutdownBuilder<UnixUnset> {
    /// Set the Unix-side sequence to an explicit [`UnixGracefulShutdown`].
    ///
    /// Use this for the rare multi-phase case where you control the child and need a
    /// cooperative protocol that escalates through more than one signal. For the common
    /// single-signal cases prefer [`Self::unix_sigterm`] or [`Self::unix_sigint`], which
    /// take a `Duration` directly.
    ///
    /// On non-Unix targets the value is accepted but unused.
    #[must_use]
    #[cfg_attr(not(unix), allow(clippy::needless_pass_by_value))]
    pub fn unix(self, sequence: UnixGracefulShutdown) -> GracefulShutdownBuilder<UnixSet> {
        #[cfg(not(unix))]
        let _ = sequence;
        GracefulShutdownBuilder {
            #[cfg(unix)]
            unix: Some(sequence),
            #[cfg(windows)]
            windows: self.windows,
            _state: PhantomData,
        }
    }

    /// Shorthand for `.unix(UnixGracefulShutdown::terminate_only(timeout))`. The recommended
    /// default for service-like children. See [`UnixGracefulShutdown`] for the choice between
    /// `terminate_only`, `interrupt_only`, and `from_phases`.
    ///
    /// On non-Unix targets the value is accepted but unused.
    #[must_use]
    pub fn unix_sigterm(self, timeout: Duration) -> GracefulShutdownBuilder<UnixSet> {
        #[cfg(not(unix))]
        let _ = timeout;
        GracefulShutdownBuilder {
            #[cfg(unix)]
            unix: Some(UnixGracefulShutdown::terminate_only(timeout)),
            #[cfg(windows)]
            windows: self.windows,
            _state: PhantomData,
        }
    }

    /// Shorthand for `.unix(UnixGracefulShutdown::interrupt_only(timeout))`. Use this when
    /// forwarding a Ctrl-C / TTY interrupt to a CLI-like child. See [`UnixGracefulShutdown`]
    /// for the choice between `terminate_only`, `interrupt_only`, and `from_phases`.
    ///
    /// On non-Unix targets the value is accepted but unused.
    #[must_use]
    pub fn unix_sigint(self, timeout: Duration) -> GracefulShutdownBuilder<UnixSet> {
        #[cfg(not(unix))]
        let _ = timeout;
        GracefulShutdownBuilder {
            #[cfg(unix)]
            unix: Some(UnixGracefulShutdown::interrupt_only(timeout)),
            #[cfg(windows)]
            windows: self.windows,
            _state: PhantomData,
        }
    }
}

impl GracefulShutdownBuilder<UnixSet> {
    /// Set the Windows-side sequence to an explicit [`WindowsGracefulShutdown`].
    ///
    /// For the common case prefer [`Self::windows_ctrl_break`], which takes a `Duration`
    /// directly. Windows currently has only one graceful signal (`CTRL_BREAK_EVENT`), so
    /// the structured form is rarely needed; it is kept for symmetry with the Unix side and
    /// to leave room for future Windows-specific knobs.
    ///
    /// On non-Windows targets the value is accepted but unused.
    #[must_use]
    #[cfg_attr(not(windows), allow(clippy::needless_pass_by_value))]
    pub fn windows(self, sequence: WindowsGracefulShutdown) -> GracefulShutdownBuilder<BothSet> {
        #[cfg(not(windows))]
        let _ = sequence;
        GracefulShutdownBuilder {
            #[cfg(unix)]
            unix: self.unix,
            #[cfg(windows)]
            windows: sequence,
            _state: PhantomData,
        }
    }

    /// Shorthand for `.windows(WindowsGracefulShutdown::new(timeout))`.
    ///
    /// `timeout` bounds the post-`CTRL_BREAK_EVENT` wait before escalating to
    /// `TerminateProcess`.
    ///
    /// On non-Windows targets the value is accepted but unused.
    #[must_use]
    pub fn windows_ctrl_break(self, timeout: Duration) -> GracefulShutdownBuilder<BothSet> {
        #[cfg(not(windows))]
        let _ = timeout;
        GracefulShutdownBuilder {
            #[cfg(unix)]
            unix: self.unix,
            #[cfg(windows)]
            windows: WindowsGracefulShutdown::new(timeout),
            _state: PhantomData,
        }
    }
}

impl GracefulShutdownBuilder<BothSet> {
    /// Finish the builder, producing a [`GracefulShutdown`].
    ///
    /// # Panics
    ///
    /// Should never panic: the builder's typestate requires [`Self::unix`] to have been called
    /// before this method becomes available.
    #[must_use]
    pub fn build(self) -> GracefulShutdown {
        GracefulShutdown {
            #[cfg(unix)]
            unix: self
                .unix
                .expect("UnixGracefulShutdown must be set before build()"),
            #[cfg(windows)]
            windows: self.windows,
        }
    }
}

/// Graceful-shutdown signal that can be the first or escalation step on Unix before the implicit
/// SIGKILL fallback.
///
/// Marked `#[non_exhaustive]` so additional Unix signals (e.g. `SIGHUP`, `SIGUSR1`) can be added
/// in future minor releases without forcing a breaking change on `match` users.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnixGracefulSignal {
    /// `SIGINT`. Default disposition is `Term` (kernel terminates the process when no handler is
    /// installed). Idiomatic Tokio applications using `tokio::signal::ctrl_c()` install a SIGINT
    /// handler, but service-style children (systemd, K8s, Docker) typically install only a
    /// SIGTERM handler, in which case the kernel default disposition kills the child without
    /// running the user's shutdown logic.
    Interrupt,

    /// `SIGTERM`. Default disposition is `Term` (kernel terminates the process when no handler is
    /// installed). The conventional "please shut down" signal in Unix; what `kill <pid>` (no args)
    /// sends and what systemd, K8s, Docker, runit, and supervisord all use.
    Terminate,
}

#[cfg(unix)]
impl UnixGracefulSignal {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
        }
    }
}

/// One step of a Unix graceful-shutdown sequence: a signal to send and a maximum time to wait
/// for the child to exit before escalating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnixGracefulPhase {
    /// Signal sent at the start of this phase.
    pub signal: UnixGracefulSignal,

    /// Maximum time to wait for the child to exit after `signal` is sent. When this elapses the
    /// sequence escalates to the next phase, or to the implicit SIGKILL fallback if this is the
    /// last phase.
    pub timeout: Duration,
}

impl UnixGracefulPhase {
    /// Convenience constructor for a `SIGINT` phase.
    #[must_use]
    pub const fn interrupt(timeout: Duration) -> Self {
        Self {
            signal: UnixGracefulSignal::Interrupt,
            timeout,
        }
    }

    /// Convenience constructor for a `SIGTERM` phase.
    #[must_use]
    pub const fn terminate(timeout: Duration) -> Self {
        Self {
            signal: UnixGracefulSignal::Terminate,
            timeout,
        }
    }
}

/// One or more graceful-shutdown phases dispatched on Unix before the implicit SIGKILL fallback.
///
/// Always followed by `SIGKILL` if every configured phase elapses without the child exiting.
///
/// # Choosing a sequence
///
/// **Recommended for service-like children (default for most orchestrators):**
/// [`UnixGracefulShutdown::terminate_only`]. `SIGTERM` is the standard Unix shutdown signal:
/// `kill <pid>`, systemd, K8s, Docker, runit, and supervisord all send it. Most well-behaved
/// long-running services install a SIGTERM handler.
///
/// **Recommended for CLI-like children:** [`UnixGracefulShutdown::interrupt_only`]. Use when the
/// orchestrator's user model is "I pressed Ctrl-C" and the child is expected to handle `SIGINT`
/// (Tokio `tokio::signal::ctrl_c()`, Python `KeyboardInterrupt`, and similar).
///
/// **Multi-phase sequences via [`from_phases`](Self::from_phases) are NOT a way to cover children
/// with unknown signal handlers.** If phase 1 sends a signal whose handler is missing in the
/// child, the kernel default disposition (`Term`) kills the child during phase 1 and later phases
/// are never dispatched. A two-phase `SIGINT` -> `SIGTERM` sequence does not "cover both
/// conventions": a child that handles only `SIGTERM` is killed by the `SIGINT` phase via kernel
/// default before its `SIGTERM` handler can run, and the symmetric problem occurs for
/// `SIGTERM` -> `SIGINT` against `SIGINT`-only-handler children.
///
/// Use multi-phase only when you control the child and use multiple signals as distinct
/// cooperative shutdown stages (for example: `SIGINT` "begin drain" followed by `SIGTERM` "abort
/// drain"). For unknown or heterogeneous children, pick `terminate_only` or `interrupt_only`.
///
/// # Construction
///
/// `UnixGracefulShutdown` cannot be constructed empty. The implicit `SIGKILL` fallback is not a
/// phase; a graceful sequence must contain at least one signal to dispatch. Use one of the named
/// single-signal constructors, or [`from_phases`](Self::from_phases) for a multi-phase
/// cooperative sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnixGracefulShutdown {
    phases: Vec<UnixGracefulPhase>,
}

impl UnixGracefulShutdown {
    /// Single-phase sequence sending `SIGTERM` only. The recommended default for service-like
    /// children; see the [type-level docs](Self#choosing-a-sequence) for the full discussion.
    /// The implicit `SIGKILL` fallback runs after `timeout` if the child has not exited.
    #[must_use]
    pub fn terminate_only(timeout: Duration) -> Self {
        Self::single(UnixGracefulPhase::terminate(timeout))
    }

    /// Single-phase sequence sending `SIGINT` only. Use this when forwarding a Ctrl-C / TTY
    /// interrupt to a CLI-like child; see the [type-level docs](Self#choosing-a-sequence) for
    /// the full discussion. The implicit `SIGKILL` fallback runs after `timeout` if the child
    /// has not exited.
    #[must_use]
    pub fn interrupt_only(timeout: Duration) -> Self {
        Self::single(UnixGracefulPhase::interrupt(timeout))
    }

    /// Single-phase sequence sending an arbitrary [`UnixGracefulSignal`].
    #[must_use]
    pub(crate) fn single(phase: UnixGracefulPhase) -> Self {
        Self {
            phases: vec![phase],
        }
    }

    /// Multi-phase sequence dispatched in iteration order. Use only for cooperative
    /// shutdown protocols against a child you control, where each signal in the sequence has
    /// a distinct handler. For unknown children pick [`Self::terminate_only`] or
    /// [`Self::interrupt_only`] instead; see the [type-level docs](Self#choosing-a-sequence)
    /// for why a multi-phase sequence does not "cover both conventions".
    ///
    /// # Panics
    ///
    /// Panics if `phases` produces no elements. A graceful sequence must contain at least one
    /// phase; the implicit `SIGKILL` fallback is not a phase.
    #[must_use]
    pub fn from_phases(phases: impl IntoIterator<Item = UnixGracefulPhase>) -> Self {
        let phases: Vec<_> = phases.into_iter().collect();
        assert!(
            !phases.is_empty(),
            "UnixGracefulShutdown must contain at least one phase",
        );
        Self { phases }
    }

    /// Phases in dispatch order.
    #[must_use]
    pub fn phases(&self) -> &[UnixGracefulPhase] {
        &self.phases
    }
}

/// Single-phase Windows graceful-shutdown sequence.
///
/// `CTRL_BREAK_EVENT` is dispatched to the child's console process group, then `TerminateProcess`
/// runs as the kill fallback if the child has not exited within `timeout`. Windows has
/// no second graceful signal: `GenerateConsoleCtrlEvent` accepts only `CTRL_BREAK_EVENT` for
/// nonzero process groups, so a second graceful phase would just duplicate the first send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowsGracefulShutdown {
    /// Maximum time to wait after sending `CTRL_BREAK_EVENT` before escalating to
    /// `TerminateProcess`.
    pub timeout: Duration,
}

impl WindowsGracefulShutdown {
    /// Construct a Windows graceful sequence with the given timeout for the single
    /// `CTRL_BREAK_EVENT` phase.
    #[must_use]
    pub const fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assertr::prelude::*;

    mod builder {
        use super::*;

        #[test]
        fn populates_unix_terminate_only_phase() {
            let shutdown = GracefulShutdown::builder()
                .unix(UnixGracefulShutdown::terminate_only(Duration::from_secs(5)))
                .windows(WindowsGracefulShutdown::new(Duration::from_secs(7)))
                .build();

            #[cfg(unix)]
            {
                let phases = shutdown.unix.phases();
                assert_that!(phases.len()).is_equal_to(1);
                assert_that!(phases[0].timeout).is_equal_to(Duration::from_secs(5));
            }
            #[cfg(windows)]
            {
                assert_that!(shutdown.windows.timeout).is_equal_to(Duration::from_secs(7));
            }
        }

        #[test]
        fn uses_interrupt_signal_for_unix_interrupt_only() {
            let shutdown = GracefulShutdown::builder()
                .unix(UnixGracefulShutdown::interrupt_only(Duration::from_secs(3)))
                .windows(WindowsGracefulShutdown::new(Duration::from_secs(7)))
                .build();

            #[cfg(unix)]
            {
                let phases = shutdown.unix.phases();
                assert_that!(phases.len()).is_equal_to(1);
                assert_that!(phases[0].timeout).is_equal_to(Duration::from_secs(3));
            }
            #[cfg(windows)]
            {
                assert_that!(shutdown.windows.timeout).is_equal_to(Duration::from_secs(7));
            }
        }
    }

    #[cfg(unix)]
    mod unix_sequence {
        use super::*;
        use crate::{UnixGracefulPhase, UnixGracefulSignal};

        #[test]
        fn from_phases_preserves_iteration_order() {
            let sequence = UnixGracefulShutdown::from_phases([
                UnixGracefulPhase::interrupt(Duration::from_secs(1)),
                UnixGracefulPhase::terminate(Duration::from_secs(2)),
            ]);

            let phases = sequence.phases();
            assert_that!(phases.len()).is_equal_to(2);
            assert_that!(phases[0].signal).is_equal_to(UnixGracefulSignal::Interrupt);
            assert_that!(phases[0].timeout).is_equal_to(Duration::from_secs(1));
            assert_that!(phases[1].signal).is_equal_to(UnixGracefulSignal::Terminate);
            assert_that!(phases[1].timeout).is_equal_to(Duration::from_secs(2));
        }

        #[test]
        #[should_panic(expected = "UnixGracefulShutdown must contain at least one phase")]
        fn from_phases_panics_on_empty_input() {
            let _ = UnixGracefulShutdown::from_phases(std::iter::empty());
        }
    }
}
