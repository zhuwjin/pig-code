pub const COMPACTION_MARKER: &str = "[COMPACTION]";

/// Which conversation is being summarized. The sections and safety wording
/// are shared; only the subject and continuation phrase differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstructionFlavor {
    Root,
    Child,
}

/// Summary request instruction. Root manual compaction may add a user focus;
/// the child path is automatic and has no manual focus.
pub(crate) fn compaction_instruction(
    flavor: InstructionFlavor,
    instruction: Option<&str>,
) -> String {
    let (subject, continuation) = match flavor {
        InstructionFlavor::Root => ("coding-assistant", "continue working"),
        InstructionFlavor::Child => ("subagent", "continue this task"),
    };
    let custom = instruction
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| {
            format!("\nThe user added a special request for this summary — emphasize it: {text}\n")
        })
        .unwrap_or_default();
    format!(
        "{COMPACTION_MARKER} Compress the {subject} conversation history above into a handoff summary for the same assistant to {continuation} with after compaction. Write it in the language the conversation has been using, with the following sections (skip sections that have no content):
1. User goals: what the user wants, including explicitly stated requirements and preferences.
2. Completed: work that is done and verified conclusions.
3. File changes: files touched (path + one-line description of the change).
4. Key decisions: technical choices and their rationale; directions the user rejected or corrected.
5. In progress and pending: unfinished steps, next-step plans, known blockers; if an implementation plan has been approved or is being drafted, its steps and conclusions must be preserved in full (execution continues from it after compaction).
6. Important context: running commands/background tasks, verbatim key error messages, environment essentials.
Keep only what is needed to continue the work; preserve hard facts such as file paths, commands, and verbatim error messages — do not invent details. Keep the whole summary within 1200 characters.{custom}
Do not call any tools; output the summary text directly.
"
    )
}
