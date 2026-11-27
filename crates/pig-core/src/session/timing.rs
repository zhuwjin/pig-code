use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimingKind {
    Api,
    UserWait,
    ToolPhase,
    ToolExec,
    Compact,
    Hook,
    Review,
}

type Interval = (Instant, Instant);

#[derive(Debug, Default)]
struct TimingIntervals {
    api: Vec<Interval>,
    user_wait: Vec<Interval>,
    tool_phase: Vec<Interval>,
    tool_exec: Vec<Interval>,
    compact: Vec<Interval>,
    hook: Vec<Interval>,
    review: Vec<Interval>,
}

impl TimingIntervals {
    fn push(&mut self, kind: TimingKind, interval: Interval) {
        let target = match kind {
            TimingKind::Api => &mut self.api,
            TimingKind::UserWait => &mut self.user_wait,
            TimingKind::ToolPhase => &mut self.tool_phase,
            TimingKind::ToolExec => &mut self.tool_exec,
            TimingKind::Compact => &mut self.compact,
            TimingKind::Hook => &mut self.hook,
            TimingKind::Review => &mut self.review,
        };
        target.push(interval);
    }

    fn merged(&self, kinds: &[TimingKind]) -> Vec<Interval> {
        let mut intervals = Vec::new();
        for kind in kinds {
            let source = match kind {
                TimingKind::Api => &self.api,
                TimingKind::UserWait => &self.user_wait,
                TimingKind::ToolPhase => &self.tool_phase,
                TimingKind::ToolExec => &self.tool_exec,
                TimingKind::Compact => &self.compact,
                TimingKind::Hook => &self.hook,
                TimingKind::Review => &self.review,
            };
            intervals.extend(source.iter().copied());
        }
        merge(intervals)
    }
}

/// Monotonic intervals shared by root calls, concurrent tool waves, reviewers
/// and MCP elicitation. Guards record on Drop, including future cancellation.
#[derive(Debug, Default)]
pub(crate) struct TurnTiming {
    intervals: Mutex<TimingIntervals>,
}

pub(crate) type SharedTurnTiming = Arc<Mutex<Option<Arc<TurnTiming>>>>;

impl TurnTiming {
    pub(crate) fn guard(self: &Arc<Self>, kind: TimingKind) -> TimingGuard {
        TimingGuard {
            timing: Some(self.clone()),
            kind,
            started: Instant::now(),
        }
    }

    pub(crate) fn optional_guard(timing: &Option<Arc<Self>>, kind: TimingKind) -> TimingGuard {
        TimingGuard {
            timing: timing.clone(),
            kind,
            started: Instant::now(),
        }
    }

    pub(crate) fn record(&self, kind: TimingKind, started: Instant, ended: Instant) {
        if ended <= started {
            return;
        }
        self.intervals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(kind, (started, ended));
    }

    pub(crate) fn breakdown(&self, duration_ms: u64, api_ms: u64) -> TurnTimingBreakdown {
        let intervals = self
            .intervals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let user_wait = intervals.merged(&[TimingKind::UserWait]);
        let hook = intervals.merged(&[TimingKind::Hook]);
        let review = intervals.merged(&[TimingKind::Review]);
        let hook_and_wait = intervals.merged(&[TimingKind::UserWait, TimingKind::Hook]);
        let tool_phase = intervals.merged(&[TimingKind::ToolPhase]);
        let tool_exec = intervals.merged(&[TimingKind::ToolExec]);
        let compact = intervals.merged(&[TimingKind::Compact]);
        let tool_exclusions =
            intervals.merged(&[TimingKind::UserWait, TimingKind::Review, TimingKind::Hook]);
        let critical_exclusions = intervals.merged(&[
            TimingKind::Api,
            TimingKind::UserWait,
            TimingKind::Review,
            TimingKind::Hook,
            TimingKind::Compact,
        ]);
        let user_wait_ms = duration_ms_total(&user_wait);
        let hook_ms = duration_ms_total(&hook).saturating_sub(overlap_ms(&hook, &user_wait));
        let review_ms =
            duration_ms_total(&review).saturating_sub(overlap_ms(&review, &hook_and_wait));
        let tool_wall_ms = duration_ms_total(&tool_phase)
            .saturating_sub(overlap_ms(&tool_phase, &critical_exclusions));
        let tool_exec_ms =
            duration_ms_total(&tool_exec).saturating_sub(overlap_ms(&tool_exec, &tool_exclusions));
        let compact_ms =
            duration_ms_total(&compact).saturating_sub(overlap_ms(&compact, &user_wait));
        let overhead_ms = duration_ms
            .saturating_sub(api_ms)
            .saturating_sub(tool_wall_ms)
            .saturating_sub(user_wait_ms)
            .saturating_sub(review_ms)
            .saturating_sub(hook_ms)
            .saturating_sub(compact_ms);
        TurnTimingBreakdown {
            user_wait_ms,
            tool_exec_ms,
            tool_wall_ms,
            compact_ms,
            hook_ms,
            review_ms,
            overhead_ms,
        }
    }
}

pub(crate) struct TimingGuard {
    timing: Option<Arc<TurnTiming>>,
    kind: TimingKind,
    started: Instant,
}

impl Drop for TimingGuard {
    fn drop(&mut self) {
        if let Some(timing) = &self.timing {
            timing.record(self.kind, self.started, Instant::now());
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TurnTimingBreakdown {
    pub user_wait_ms: u64,
    pub tool_exec_ms: u64,
    pub tool_wall_ms: u64,
    pub compact_ms: u64,
    pub hook_ms: u64,
    pub review_ms: u64,
    pub overhead_ms: u64,
}

fn merge(mut intervals: Vec<Interval>) -> Vec<Interval> {
    intervals.sort_by_key(|interval| interval.0);
    let mut merged: Vec<Interval> = Vec::with_capacity(intervals.len());
    for (started, ended) in intervals {
        if ended <= started {
            continue;
        }
        if let Some(last) = merged.last_mut()
            && started <= last.1
        {
            last.1 = last.1.max(ended);
            continue;
        }
        merged.push((started, ended));
    }
    merged
}

fn duration_ms_total(intervals: &[Interval]) -> u64 {
    intervals
        .iter()
        .fold(Duration::ZERO, |total, (started, ended)| {
            total.saturating_add(ended.duration_since(*started))
        })
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn overlap_ms(left: &[Interval], right: &[Interval]) -> u64 {
    let left = merge(left.to_vec());
    let right = merge(right.to_vec());
    let mut left_index = 0;
    let mut right_index = 0;
    let mut total = Duration::ZERO;
    while left_index < left.len() && right_index < right.len() {
        let (left_start, left_end) = left[left_index];
        let (right_start, right_end) = right[right_index];
        let started = left_start.max(right_start);
        let ended = left_end.min(right_end);
        if ended > started {
            total = total.saturating_add(ended.duration_since(started));
        }
        if left_end < right_end {
            left_index += 1;
        } else {
            right_index += 1;
        }
    }
    total.as_millis().min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breakdown_merges_concurrency_and_subtracts_nested_wait_and_review() {
        let timing = TurnTiming::default();
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        timing.record(TimingKind::Api, at(0), at(100));
        timing.record(TimingKind::ToolPhase, at(0), at(200));
        timing.record(TimingKind::ToolPhase, at(50), at(180));
        timing.record(TimingKind::ToolExec, at(10), at(180));
        timing.record(TimingKind::UserWait, at(50), at(150));
        timing.record(TimingKind::Review, at(20), at(60));
        timing.record(TimingKind::Hook, at(20), at(40));
        timing.record(TimingKind::Compact, at(500), at(520));

        let breakdown = timing.breakdown(1_000, 100);
        assert_eq!(breakdown.user_wait_ms, 100);
        assert_eq!(breakdown.review_ms, 10);
        assert_eq!(breakdown.hook_ms, 20);
        assert_eq!(breakdown.tool_exec_ms, 40);
        assert_eq!(breakdown.tool_wall_ms, 50);
        assert_eq!(breakdown.compact_ms, 20);
        assert_eq!(breakdown.overhead_ms, 700);
    }

    #[test]
    fn guard_records_when_dropped_and_overhead_saturates() {
        let timing = Arc::new(TurnTiming::default());
        {
            let _guard = timing.guard(TimingKind::UserWait);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(timing.breakdown(0, 0).user_wait_ms > 0);
        assert_eq!(timing.breakdown(0, 100).overhead_ms, 0);
    }
}
