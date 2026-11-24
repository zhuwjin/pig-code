//! Repeat-call breaker (Kimi Code's `toolDedupe` semantics): a streak of
//! back-to-back identical tool calls — same tool name and canonical arguments
//! — appends escalating reminders to the tool result, and a streak that outlives
//! the final reminder ends the turn instead of burning tokens on a loop.
//!
//! Every call of a turn is counted once, in model order: a different call
//! resets the streak. Only results that went through the permission gate carry
//! a reminder; intercepted tools (Agent, AskUserQuestion, plan toggles) still
//! count, so a loop that alternates with them is not mistaken for a streak.

use std::hash::{Hash, Hasher};

use pig_protocol::CoreError;
use pig_provider::ToolCall;

/// Streak length at which each reminder first appears.
pub(crate) const REPEAT_REMINDER_FIRST: u32 = 3;
pub(crate) const REPEAT_REMINDER_SECOND: u32 = 5;
/// From here on every result asks for a final answer without further tool calls.
pub(crate) const REPEAT_REMINDER_FINAL: u32 = 8;
/// A streak longer than this ignored the final reminder: the turn is stopped.
pub(crate) const REPEAT_STOP_AFTER: u32 = 12;

const REMINDER_FIRST: &str = "<system-reminder>\n\
The same tool call has just been repeated several times in a row. Before your next call, state in one sentence what new information you expect from it. If that sentence names something this result does not already give you, choose the action most likely to provide it; otherwise continue with the evidence you already have.\n\
</system-reminder>";

const REMINDER_SECOND: &str = "<system-reminder>\n\
The same tool call has now been issued {streak} times in a row. Choose exactly one of the following and state your choice before acting:\n\
(1) Falsification check: run the cheapest test that could disprove your current approach, if such a test exists.\n\
(2) Missing input: tell the user precisely what information or decision you need to proceed, and ask for it.\n\
(3) Conclude: deliver your best result based on the evidence already gathered, listing anything that remains uncertain.\n\
</system-reminder>";

const REMINDER_FINAL: &str = "<system-reminder>\n\
Write your final response now, without any further tool calls. Cover: the current blocker, each approach you have tried and what it established, and the specific information or decision you need from the user to unblock progress. Text only.\n\
</system-reminder>";

/// The call that outlived the final reminder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoppedRepeat {
    pub tool: String,
    pub streak: u32,
}

#[derive(Default)]
pub(crate) struct RepeatBreaker {
    /// Key of the last counted call; `None` until the first call of a turn.
    last: Option<u64>,
    streak: u32,
    stopped: Option<StoppedRepeat>,
}

impl RepeatBreaker {
    /// Start of a turn: nothing has repeated yet.
    pub(crate) fn reset(&mut self) {
        self.last = None;
        self.streak = 0;
        self.stopped = None;
    }

    /// Count one tool call in model order and return the streak it extends.
    pub(crate) fn observe(&mut self, call: &ToolCall) -> u32 {
        let key = call_key(call);
        if self.last == Some(key) {
            self.streak = self.streak.saturating_add(1);
        } else {
            self.last = Some(key);
            self.streak = 1;
        }
        if self.streak > REPEAT_STOP_AFTER && self.stopped.is_none() {
            self.stopped = Some(StoppedRepeat {
                tool: call.name.clone(),
                streak: self.streak,
            });
        }
        self.streak
    }

    /// The call that outlived the final reminder, once the streak tripped.
    pub(crate) fn tripped(&self) -> Option<&StoppedRepeat> {
        self.stopped.as_ref()
    }
}

/// Append the reminder a call earns at `streak` to its result.
pub(crate) fn append_reminder(streak: u32, output: &mut String) {
    let reminder = if streak >= REPEAT_REMINDER_FINAL {
        REMINDER_FINAL
    } else if streak >= REPEAT_REMINDER_SECOND {
        &REMINDER_SECOND.replace("{streak}", &streak.to_string())
    } else if streak >= REPEAT_REMINDER_FIRST {
        REMINDER_FIRST
    } else {
        return;
    };
    output.push_str("\n\n");
    output.push_str(reminder);
}

/// English typed failure for a tripped breaker; surfaced as
/// `CoreError::Internal` so app's `CoreError` mapping needs no new variant.
pub(crate) fn repeat_stop_error(stopped: &StoppedRepeat) -> CoreError {
    CoreError::Internal {
        detail: repeat_stop_detail(stopped),
    }
}

pub(crate) fn repeat_stop_detail(stopped: &StoppedRepeat) -> String {
    format!(
        "Repeat breaker: `{}` was called {} times in a row with identical arguments and ignored the reminder to stop; the turn was ended instead of looping",
        stopped.tool, stopped.streak
    )
}

/// Identity of a call for streak counting: the tool name plus its arguments,
/// insensitive to key order and whitespace.
fn call_key(call: &ToolCall) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    call.name.hash(&mut hasher);
    match serde_json::from_str::<serde_json::Value>(&call.arguments) {
        Ok(value) => canonical_json(&value).hash(&mut hasher),
        Err(_) => call.arguments.trim().hash(&mut hasher),
    }
    hasher.finish()
}

/// Key-order- and whitespace-independent spelling of a JSON value. Object keys
/// are sorted explicitly: `serde_json`'s map order depends on whether any
/// workspace dependency enables `preserve_order`.
fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            let body: Vec<String> = entries
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        canonical_json(value)
                    )
                })
                .collect();
            format!("{{{}}}", body.join(","))
        }
        serde_json::Value::Array(items) => {
            let body: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", body.join(","))
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: String::new(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    fn read(path: &str) -> ToolCall {
        call("Read", &format!("{{\"path\":\"{path}\"}}"))
    }

    #[test]
    fn a_different_call_resets_the_streak() {
        let mut breaker = RepeatBreaker::default();
        assert_eq!(breaker.observe(&read("a")), 1);
        assert_eq!(breaker.observe(&read("a")), 2);
        assert_eq!(breaker.observe(&read("b")), 1);
        assert_eq!(breaker.observe(&read("b")), 2);
        assert_eq!(breaker.observe(&call("Grep", "{\"pattern\":\"x\"}")), 1);
        assert_eq!(breaker.tripped(), None);
    }

    #[test]
    fn key_order_and_whitespace_do_not_split_a_streak() {
        let mut breaker = RepeatBreaker::default();
        assert_eq!(breaker.observe(&call("Read", "{\"path\":\"a\",\"limit\":5}")), 1);
        assert_eq!(breaker.observe(&call("Read", " { \"limit\": 5, \"path\": \"a\" } ")), 2);
        // A different tool with the same arguments is a different call.
        assert_eq!(breaker.observe(&call("Grep", "{\"limit\":5,\"path\":\"a\"}")), 1);
    }

    #[test]
    fn malformed_arguments_still_count_by_raw_text() {
        let mut breaker = RepeatBreaker::default();
        assert_eq!(breaker.observe(&call("Read", "{\"path\":")), 1);
        assert_eq!(breaker.observe(&call("Read", " {\"path\": ")), 2);
    }

    #[test]
    fn reminders_escalate_and_stop_only_after_the_final_one_is_ignored() {
        let mut output = String::new();
        for streak in 1..=REPEAT_REMINDER_FIRST - 1 {
            append_reminder(streak, &mut output);
        }
        assert!(output.is_empty(), "no reminder before the first threshold");

        append_reminder(REPEAT_REMINDER_FIRST, &mut output);
        assert_eq!(output.matches("<system-reminder>").count(), 1);
        assert!(output.contains("state in one sentence"));

        let mut second = String::new();
        append_reminder(REPEAT_REMINDER_SECOND, &mut second);
        assert!(
            second.contains(&format!("issued {REPEAT_REMINDER_SECOND} times")),
            "{second}"
        );
        assert!(second.contains("Falsification check"));

        let mut final_reminder = String::new();
        append_reminder(REPEAT_STOP_AFTER, &mut final_reminder);
        assert!(final_reminder.contains("Write your final response now"));

        let mut breaker = RepeatBreaker::default();
        for _ in 1..=REPEAT_STOP_AFTER {
            breaker.observe(&read("a"));
        }
        assert_eq!(breaker.tripped(), None, "the final reminder gets a chance");
        breaker.observe(&read("a"));
        assert_eq!(
            breaker.tripped(),
            Some(&StoppedRepeat {
                tool: "Read".into(),
                streak: REPEAT_STOP_AFTER + 1,
            })
        );
    }

    #[test]
    fn a_reset_clears_the_streak_and_the_trip() {
        let mut breaker = RepeatBreaker::default();
        for _ in 0..=REPEAT_STOP_AFTER {
            breaker.observe(&read("a"));
        }
        assert!(breaker.tripped().is_some());
        breaker.reset();
        assert_eq!(breaker.tripped(), None);
        assert_eq!(breaker.observe(&read("a")), 1);
    }

    #[test]
    fn the_trip_survives_a_later_different_call() {
        let mut breaker = RepeatBreaker::default();
        for _ in 0..=REPEAT_STOP_AFTER {
            breaker.observe(&read("a"));
        }
        assert_eq!(breaker.observe(&call("Grep", "{\"pattern\":\"x\"}")), 1);
        assert_eq!(breaker.tripped().map(|stopped| stopped.tool.as_str()), Some("Read"));
    }
}
