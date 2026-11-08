//! What the daemon believes about one watched directory, as a pure state
//! machine. Kept in memory only: a restarted daemon starts every entry at
//! `None` (never checked) and re-learns it from one check.
//!
//! ```text
//!            Absent                       Absent, now ≥ deadline
//!  Present ─────────▶ Settling{since,deadline} ─────────────────▶ (trigger, gone_at = since)
//!     ▲  ◀──Present── │    │
//!     │   (cancelled) │    └──Error──▶ Unknown{error}
//!     └────Present────┴────────────────── │ ──Absent──▶ Settling{since: now}  (settle starts over)
//! ```
//!
//! The rule this encodes: **a teardown starts only after the directory was
//! seen absent (ENOENT/ENOTDIR) on every check for a full settle delay, with
//! no check in between that saw it present or failed.** An error (EACCES,
//! EIO, …) never triggers anything and, during settling, interrupts it: the
//! entry goes to `Unknown`, and the next `Absent` starts a fresh settle. So an
//! error can delay a teardown by one settle delay but can never cause one, and
//! an entry is never stuck: every state is left by the next check that
//! disagrees with it.

use crate::time::{Dur, Ts};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presence {
    Present,
    Settling { since: Ts, deadline: Ts },
    Unknown { error: String },
}

/// One check of the directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Present,
    /// ENOENT or ENOTDIR.
    Absent,
    /// Anything else; the text is for the log.
    Error(String),
}

/// What a check changed, for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// Seen absent; settling until `deadline`.
    Gone { deadline: Ts },
    /// Present again while settling: no teardown.
    Cancelled,
    /// A check failed (first time, or a different error than before).
    CheckError {
        error: String,
        interrupted_settle: bool,
    },
    /// Checks work again after an error.
    Recovered,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    /// The new state; `None` when the entry triggered (it stops being watched).
    pub next: Option<Presence>,
    pub notice: Option<Notice>,
    /// `Some(gone_at)`: start the teardown, counting delays from `gone_at`.
    pub trigger: Option<Ts>,
}

fn stay(p: Presence) -> Observed {
    Observed {
        next: Some(p),
        notice: None,
        trigger: None,
    }
}

fn start_settling(now: Ts, settle: Dur) -> Observed {
    let deadline = now.plus(settle);
    if deadline <= now {
        // No settle delay: gone on first sight.
        return Observed {
            next: None,
            notice: Some(Notice::Gone { deadline }),
            trigger: Some(now),
        };
    }
    Observed {
        next: Some(Presence::Settling {
            since: now,
            deadline,
        }),
        notice: Some(Notice::Gone { deadline }),
        trigger: None,
    }
}

/// The transition function. `state` is `None` for an entry never checked
/// since the daemon started (treated like `Present`: the first absent check
/// starts settling).
pub fn observe(state: Option<&Presence>, probe: Probe, now: Ts, settle: Dur) -> Observed {
    use Presence as S;
    match (state, probe) {
        (None | Some(S::Present), Probe::Present) => stay(S::Present),
        (None | Some(S::Present), Probe::Absent) => start_settling(now, settle),
        (None | Some(S::Present), Probe::Error(error)) => Observed {
            next: Some(S::Unknown {
                error: error.clone(),
            }),
            notice: Some(Notice::CheckError {
                error,
                interrupted_settle: false,
            }),
            trigger: None,
        },

        (Some(S::Settling { .. }), Probe::Present) => Observed {
            next: Some(S::Present),
            notice: Some(Notice::Cancelled),
            trigger: None,
        },
        (Some(&S::Settling { since, deadline }), Probe::Absent) => {
            if now >= deadline {
                Observed {
                    next: None,
                    notice: None,
                    trigger: Some(since),
                }
            } else {
                stay(S::Settling { since, deadline })
            }
        }
        (Some(S::Settling { .. }), Probe::Error(error)) => Observed {
            next: Some(S::Unknown {
                error: error.clone(),
            }),
            notice: Some(Notice::CheckError {
                error,
                interrupted_settle: true,
            }),
            trigger: None,
        },

        (Some(S::Unknown { .. }), Probe::Present) => Observed {
            next: Some(S::Present),
            notice: Some(Notice::Recovered),
            trigger: None,
        },
        (Some(S::Unknown { .. }), Probe::Absent) => start_settling(now, settle),
        (Some(S::Unknown { error: old }), Probe::Error(error)) => {
            let changed = *old != error;
            Observed {
                next: Some(S::Unknown {
                    error: error.clone(),
                }),
                notice: changed.then_some(Notice::CheckError {
                    error,
                    interrupted_settle: false,
                }),
                trigger: None,
            }
        }
    }
}

/// Whether the daemon should check this entry now. Entries it has no settled
/// belief about (never checked, settling, or erroring) are checked on every
/// pass; present ones only when an event touched them or a sweep is due.
pub fn wants_check(state: Option<&Presence>, touched: bool, sweep: bool) -> bool {
    match state {
        None | Some(Presence::Settling { .. }) | Some(Presence::Unknown { .. }) => true,
        Some(Presence::Present) => touched || sweep,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTLE: Dur = Dur(10_000);

    fn err(s: &str) -> Probe {
        Probe::Error(s.into())
    }

    #[test]
    fn absent_for_a_full_settle_triggers_with_the_first_sighting() {
        let o = observe(None, Probe::Absent, Ts(100), SETTLE);
        assert_eq!(
            o.next,
            Some(Presence::Settling {
                since: Ts(100),
                deadline: Ts(10_100)
            })
        );
        let s = o.next.unwrap();
        let o = observe(Some(&s), Probe::Absent, Ts(10_099), SETTLE);
        assert_eq!(o.trigger, None);
        let o = observe(Some(&s), Probe::Absent, Ts(10_100), SETTLE);
        assert_eq!((o.next, o.trigger), (None, Some(Ts(100))));
    }

    #[test]
    fn present_during_settle_cancels() {
        let s = observe(None, Probe::Absent, Ts(0), SETTLE).next.unwrap();
        let o = observe(Some(&s), Probe::Present, Ts(5), SETTLE);
        assert_eq!(
            (o.next, o.notice),
            (Some(Presence::Present), Some(Notice::Cancelled))
        );
    }

    #[test]
    fn an_error_interrupts_settling_and_absent_restarts_it() {
        let s = observe(None, Probe::Absent, Ts(0), SETTLE).next.unwrap();
        let o = observe(Some(&s), err("EACCES"), Ts(9_000), SETTLE);
        assert_eq!(o.trigger, None);
        assert!(matches!(
            o.notice,
            Some(Notice::CheckError {
                interrupted_settle: true,
                ..
            })
        ));
        let u = o.next.unwrap();
        // Absent again after the old deadline: settle starts over.
        let o = observe(Some(&u), Probe::Absent, Ts(20_000), SETTLE);
        assert_eq!(o.trigger, None);
        assert_eq!(
            o.next,
            Some(Presence::Settling {
                since: Ts(20_000),
                deadline: Ts(30_000)
            })
        );
    }

    #[test]
    fn errors_never_trigger_and_log_once_per_distinct_error() {
        let o = observe(None, err("a"), Ts(0), SETTLE);
        assert!(o.notice.is_some() && o.trigger.is_none());
        let u = o.next.unwrap();
        let o = observe(Some(&u), err("a"), Ts(99_999), SETTLE);
        assert_eq!((o.notice, o.trigger), (None, None));
        let o = observe(Some(&u), err("b"), Ts(99_999), SETTLE);
        assert!(o.notice.is_some());
        let o = observe(Some(&u), Probe::Present, Ts(1), SETTLE);
        assert_eq!(o.notice, Some(Notice::Recovered));
    }

    #[test]
    fn zero_settle_triggers_on_first_sight() {
        let o = observe(Some(&Presence::Present), Probe::Absent, Ts(7), Dur::ZERO);
        assert_eq!(o.trigger, Some(Ts(7)));
    }

    #[test]
    fn checks_are_planned_by_state() {
        assert!(wants_check(None, false, false));
        assert!(!wants_check(Some(&Presence::Present), false, false));
        assert!(wants_check(Some(&Presence::Present), true, false));
        assert!(wants_check(Some(&Presence::Present), false, true));
        let s = Presence::Settling {
            since: Ts(0),
            deadline: Ts(1),
        };
        assert!(wants_check(Some(&s), false, false));
    }
}
