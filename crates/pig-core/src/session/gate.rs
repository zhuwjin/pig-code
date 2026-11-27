use super::*;

/// Free-function implementation of gated execution (shared by
/// Session::exec_tool_gated and the background subagent drive): danger
/// classification → permissions deny/allow → approval gate → execution →
/// session-level side effects (TodoList snapshot persisted+pushed,
/// FileChanged persisted+pushed, file_originals persisted).
/// ToolCallBegin/history.push/RolloutRecord::ToolCall/ToolCallEnd are not
/// here — the caller owns them (parent and child each write their own
/// history and rollout). When `tool` is None (unknown tool name), the
/// approval/permission name matches all miss and flow reaches the execution
/// segment, which reports "unknown tool".
pub(crate) async fn exec_tool_gated_ctx(
    ctx: &mut GateCtx<'_>,
    incoming: &ToolCall,
    tool: Option<&dyn tool::Tool>,
    item_id: &str,
    turn_id: &str,
    tx: &async_channel::Sender<Event>,
    cancel: &CancellationToken,
) -> GatedToolOutcome {
    let _tool_phase = TurnTiming::optional_guard(&ctx.timing, TimingKind::ToolPhase);
    if cancel.is_cancelled() {
        return GatedToolOutcome::Cancelled;
    }

    // Unwrap `CallTool({ name, arguments })` into its real MCP/plugin target
    // BEFORE hooks, permission rules, approvals, and execution so every policy
    // check applies to the actual runtime tool with zero bypasses.
    let unwrapped_call: Option<ToolCall>;
    let (incoming, tool) = if let Some(unwrapped) = tool::unwrap_call_tool(incoming) {
        let (target_name, target_args) = match unwrapped {
            Ok(pair) => pair,
            Err(note) => return GatedToolOutcome::Rejected { note },
        };
        let Some(target_tool) = ctx
            .extra_tools
            .iter()
            .find(|extra| extra.name() == target_name.as_str())
        else {
            let note = if tool::all().iter().any(|t| t.name() == target_name.as_str()) {
                format!(
                    "`{target_name}` is a built-in tool; call it directly instead of via CallTool."
                )
            } else {
                format!(
                    "Unknown MCP/plugin tool `{target_name}`. Call ToolSearch to inspect available runtime tools."
                )
            };
            return GatedToolOutcome::Rejected { note };
        };
        if let Err(note) = tool::validate_tool_arguments(&target_tool.schema(), &target_args) {
            return GatedToolOutcome::Rejected { note };
        }
        unwrapped_call = Some(ToolCall {
            id: incoming.id.clone(),
            name: target_name,
            arguments: target_args,
        });
        (
            unwrapped_call.as_ref().expect("unwrapped call"),
            Some(target_tool.as_ref()),
        )
    } else {
        (incoming, tool)
    };

    // ---- Mandatory Bot capability & global-session boundary check ----
    // Enforced unconditionally at the execution gate (before hooks, approvals,
    // or execution) so neither prompts nor CallTool wrappers can bypass it.
    {
        let target_in_memory_root = if crate::bots::is_memory_io_tool(&incoming.name) {
            extract_file_path_arg(&incoming.arguments).is_some_and(|raw| {
                let candidate = {
                    let path = PathBuf::from(&raw);
                    if path.is_absolute() {
                        path
                    } else {
                        ctx.cwd.join(path)
                    }
                };
                path_within_any_root(&candidate, &ctx.state.extra_rw_roots)
            })
        } else {
            false
        };
        if let Err(note) = crate::bots::check_tool_capability(
            &incoming.name,
            incoming.name.starts_with("mcp__"),
            ctx.bot_capabilities.as_ref(),
            ctx.global_session,
            target_in_memory_root,
        ) {
            return GatedToolOutcome::Rejected { note };
        }
    }

    // ---- PreToolUse hooks (before any permission logic, so hooks observe
    // every call including ones the rules will deny). May reject the call,
    // rewrite the input, or adjust the permission path; contexts carry to the
    // tool result. Runs for subagent tool calls too (shared gate) ----
    let mut hook_permission: Option<crate::hooks::HookPermission> = None;
    let mut pre_contexts: Vec<String> = Vec::new();
    let mut rewritten_call: Option<ToolCall> = None;
    if ctx.hooks.is_active() {
        let run = crate::hooks::HookRunCtx {
            session_id: ctx.session_id.to_string(),
            cwd: ctx.cwd.to_path_buf(),
            tx: tx.clone(),
            cancel: cancel.clone(),
        };
        let payload = crate::hooks::HookPayload::tool(
            crate::hooks::HookEvent::PreToolUse,
            ctx.session_id,
            ctx.cwd,
            ctx.permissions,
            &incoming.name,
            &incoming.arguments,
            item_id,
        )
        .with_transcript(ctx.history);
        let _hook_timer = TurnTiming::optional_guard(&ctx.timing, TimingKind::Hook);
        let effects = crate::hooks::run_event(
            ctx.hooks,
            crate::hooks::HookEvent::PreToolUse,
            payload,
            &run,
        )
        .await;
        if let Some(reason) = effects.blocked {
            return GatedToolOutcome::Rejected {
                note: format!("Blocked by a PreToolUse hook: {reason}"),
            };
        }
        if let Some(updated) = effects.updated_input {
            let arguments =
                serde_json::to_string(&updated).unwrap_or_else(|_| incoming.arguments.clone());
            rewritten_call = Some(ToolCall {
                id: incoming.id.clone(),
                name: incoming.name.clone(),
                arguments,
            });
        }
        hook_permission = effects.permission;
        pre_contexts = effects.additional_contexts;
    }
    let call: &ToolCall = rewritten_call.as_ref().unwrap_or(incoming);

    // Dangerous commands are classified once and then handled by mode:
    // ReadOnly/Auto ask for approval, Full rejects outright.
    let bash_command = if crate::tool::is_exec_command(&call.name) {
        serde_json::from_str::<serde_json::Value>(&call.arguments)
            .ok()
            .and_then(|args| args["command"].as_str().map(str::to_string))
    } else {
        None
    };

    // Permission rules (the user level and the project level unioned): a deny
    // hit → hard rejection in all modes (including Full), checked before the
    // danger dialog (an explicit user-written deny is the strongest intent).
    // Note that ExecCommand matches the full command string (finer-grained than
    // always_allowed's first word).
    let plugin_tool = tool.is_some_and(|tool| tool.is_plugin())
        || ctx
            .extra_tools
            .iter()
            .any(|extra| extra.name() == call.name && extra.is_plugin());
    let perm_subject = match call.name.as_str() {
        name if crate::tool::is_exec_command(name) => bash_command.clone(),
        "Write" | "Edit" | tool::APPLY_PATCH_TOOL | "FetchURL" => {
            Some(tool::approval_subject(call))
        }
        name if name.starts_with("mcp__") || plugin_tool => Some(call.name.clone()),
        _ => None,
    };
    let allowed_by_rules = {
        let rules = ctx
            .permission_rules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(subject) = perm_subject.as_deref()
            && let Some(hit) = rules.deny_hit(&call.name, subject)
        {
            return GatedToolOutcome::Rejected { note: hit.note() };
        }
        // An allow hit skips approval (except dangerous commands — the danger
        // check below takes priority with its popup)
        perm_subject
            .as_deref()
            .is_some_and(|subject| rules.allow_hit(&call.name, subject))
    };

    if call.name == "FetchURL"
        && ctx.permissions.sandbox.mode != SandboxMode::DangerFullAccess
        && crate::task::network_blocked_for(ctx.state)
    {
        return GatedToolOutcome::Rejected {
            note: "Network access is blocked by the sandbox policy; FetchURL cannot make outbound requests."
                .to_string(),
        };
    }

    let danger_reason = bash_command.as_deref().and_then(tool::is_dangerous_command);
    if ctx.permissions.sandbox.mode == SandboxMode::DangerFullAccess
        && let Some(reason) = danger_reason
    {
        return GatedToolOutcome::Rejected {
            note: format!(
                "Blocked: high-risk command ({}). Full mode rejects destructive commands outright; \
                 use Full only for trusted automation, or switch to Auto/ReadOnly.",
                reason.en
            ),
        };
    }
    // "Always allowed in this session" is keyed down to (tool, subject):
    // ExecCommand = command first word, Write/Edit = path
    let approval_key = (call.name.clone(), tool::approval_subject(call));

    // Out-of-workspace write: a per-call approval request instead of a hard
    // error. Reads are never gated (the sandbox grants full-disk read, matching
    // Codex), so only Write/Edit reach here. Full runs without a sandbox, so
    // the workspace boundary does not exist there and no request is needed;
    // the sensitive-file filter still runs after resolve. In Auto/ReadOnly,
    // Allow runs this one call (the per-call fs_grant on ToolContext);
    // AlwaysAllow records the tool+path in always_allowed like any other
    // approval; Reject returns the boundary error as a tool error. tmp and the
    // extra roots stay exempt.
    let fs_outside = if ctx.permissions.sandbox.mode == SandboxMode::DangerFullAccess {
        None
    } else {
        tool::fs_outside_intent(ctx.state, ctx.cwd, &call.name, &call.arguments)
    };
    if !ctx.allow_scope_widening && fs_outside.is_some() {
        return GatedToolOutcome::Rejected {
            note: "The subagent's permission scope was fixed at launch and cannot grow from inside. This write needs an out-of-scope grant; ask the coordinating parent agent to handle it instead."
                .to_string(),
        };
    }

    // A Write/Edit into a protected credential directory: the same rules the
    // Bash widening path uses, applied to the file tools. One file is
    // approvable, a directory is not, and the approval is never remembered for
    // the session. The tool layer refuses the path anyway (the sandbox denies
    // reading it), so this dialog is the documented way to get the write made —
    // it converts the write into an explicit, named decision instead of an
    // opaque failure.
    // Protected credential directories are a wall for aggregates: a path that
    // names the directory (or a directory inside it) is refused outright,
    // because approving it would grant everything the user cannot see in the
    // dialog. A single file is the one approvable shape.
    //
    // Two request shapes reach this: a Bash widening (`sandbox_permissions`,
    // classified below) and a Write/Edit target (one path by construction).
    let mut credential_file_grant = false;
    let mut credential_high_risk = false;
    let mut credential_read_grant = false;
    let mut credential_listing = false;
    let mut file_tool_target: Option<PathBuf> = None;
    // Auto-detection, so the model does not have to know the request syntax: a
    // file tool whose single target lands inside a credential store asks the
    // user directly. Write/Edit ask to write, Read/ReadMediaFile ask to read a
    // file, Grep/Glob ask to list a directory (they never read one file's
    // content through this path — their per-file reads are filtered separately).
    let file_tool_is_write = matches!(
        call.name.as_str(),
        "Write" | "Edit" | tool::APPLY_PATCH_TOOL
    );
    let file_tool_lists = matches!(call.name.as_str(), "Grep" | "Glob");
    let file_targets = tool::file_tool_targets(ctx.cwd, call);
    let file_target = file_targets.first().cloned();
    for target in &file_targets {
        match crate::task::credential_grant(ctx.state, target) {
            crate::task::CredentialGrant::Directory => {
                if file_tool_is_write || !file_tool_lists {
                    return GatedToolOutcome::Rejected {
                        note: format!(
                            "`{}` sits inside a protected credential directory, and only a single \
                             file there can be read or written. Name the exact file instead — a \
                             directory approval would also cover the private keys and tokens next \
                             to it.",
                            target.display()
                        ),
                    };
                }
                // A listing: entry names only.
                credential_file_grant = true;
                credential_read_grant = true;
                credential_listing = true;
                file_tool_target = Some(target.clone());
            }
            crate::task::CredentialGrant::File => {
                if call.name == tool::APPLY_PATCH_TOOL {
                    return GatedToolOutcome::Rejected {
                        note: format!(
                            "`{}` sits inside a protected credential directory; ApplyPatch cannot modify credential files. Use Write for an explicit single-file replacement.",
                            target.display()
                        ),
                    };
                }
                credential_file_grant = true;
                credential_read_grant = !file_tool_is_write;
                credential_high_risk =
                    file_tool_is_write && crate::task::credential_write_is_high_risk(target);
                file_tool_target = Some(target.clone());
            }
            crate::task::CredentialGrant::Outside => {}
        }
    }
    if !ctx.allow_scope_widening && credential_file_grant {
        return GatedToolOutcome::Rejected {
            note: "The subagent's permission scope was fixed at launch and cannot grow from inside. This credential access needs a new grant; ask the coordinating parent agent to handle it instead."
                .to_string(),
        };
    }

    // A doomed outside write: deny rules and the sensitive-name list are the
    // no-appeal layer for the file tools too, so the fs_grant dialog could not
    // take effect. Reject upfront instead of an approval that must fail.
    // Credential targets have their own working channel and never reach this.
    if fs_outside.is_some()
        && let Some(target) = &file_target
        && crate::task::credential_grant(ctx.state, target) == crate::task::CredentialGrant::Outside
    {
        if let Some(reason) = crate::task::sandbox_read_denial(ctx.state, ctx.cwd, target) {
            return GatedToolOutcome::Rejected {
                note: format!(
                    "{reason}. Approvals cannot lift deny rules. Choose a different path or name."
                ),
            };
        }
        if tool::is_sensitive_file(target) {
            return GatedToolOutcome::Rejected {
                note: format!(
                    "`{}` is on the sensitive-file list (.env / private keys / cloud credentials), \
                     which the file tools never write and no approval lifts. Choose a different \
                     name.",
                    target.display()
                ),
            };
        }
    }

    // A model-requested widening of the sandbox for this one command
    // (`sandbox_permissions: with_additional_permissions`). This replaces
    // guessing from the command text: the model states what it needs, the user
    // approves, and the sandbox stays on with those roots added to its writable
    // set. A malformed or unsupported request is rejected with an explanation
    // so the model can correct it rather than retry blindly.
    let mut escalation = if crate::tool::is_exec_command(&call.name) {
        match tool::sandbox_escalation(&call.arguments) {
            Ok(request) => request,
            Err(reason) => return GatedToolOutcome::Rejected { note: reason },
        }
    } else {
        None
    };
    let mut require_escalated = escalation
        .as_ref()
        .is_some_and(|request| request.require_escalated);
    if escalation.is_some() && !ctx.allow_scope_widening {
        return GatedToolOutcome::Rejected {
            note: "The subagent's permission scope was fixed at launch and cannot grow from inside. Do not request sandbox escalation here; ask the coordinating parent agent to handle the operation."
                .to_string(),
        };
    }
    // Plan mode forbids widening: the profile exists to keep research read-only.
    if escalation.is_some() && ctx.plan_enabled {
        return GatedToolOutcome::Rejected {
            note: "Plan mode runs commands read-only: the sandbox cannot be widened while planning. Write the plan, then call ExitPlanMode to request approval."
                .to_string(),
        };
    }
    if escalation.is_some() && ctx.permissions.sandbox.mode == SandboxMode::DangerFullAccess {
        if require_escalated {
            // Codex's `RequireEscalated` is a no-op under an unrestricted
            // policy: Full already runs without a sandbox.
            escalation = None;
            require_escalated = false;
        } else {
            return GatedToolOutcome::Rejected {
                note: "Full mode runs commands without a sandbox, so additional write roots are unnecessary; drop `sandbox_permissions` from the call."
                    .to_string(),
            };
        }
    }

    // The shell widening goes through the same credential classification.
    let mut dropped_reads: Vec<PathBuf> = Vec::new();
    let mut dropped_writes: Vec<PathBuf> = Vec::new();
    if let Some(request) = &mut escalation
        && !request.require_escalated
    {
        // Roots the command can already write are redundant too: Auto grants
        // the workspace, tmp and the extra rw roots, so asking for them would
        // pop a dialog over nothing. ReadOnly reaches here with an empty set
        // (the read-only profile writes nothing), so every root there still
        // asks; plan and Full were rejected above.
        let already_writable = if ctx.permissions.sandbox.mode == SandboxMode::ReadOnly {
            Vec::new()
        } else {
            crate::task::session_write_roots(ctx.cwd, ctx.state)
        };
        // Compiled once for the doom checks below: a root a deny rule covers
        // can never be granted — sensitive denies stay on inside approved
        // roots — so the dialog could not take effect. Those requests are
        // rejected upfront instead of an approval that must fail.
        let denials = crate::task::SandboxDenialView::new(ctx.state, ctx.cwd);
        // Workspace metadata is read-only inside the writable root, so naming
        // one of those paths as a widening root cannot take effect. The
        // redundancy check below would call it "already writable" and drop it,
        // which reads as "the request was ignored" — the model then retries
        // with escalation. Reject with the real reason instead.
        let carve_outs = if request.write_roots.is_empty() {
            Vec::new()
        } else {
            crate::task::metadata_carve_outs(
                &crate::task::session_write_roots(ctx.cwd, ctx.state),
                ctx.cwd,
            )
        };
        let mut kept_writes = Vec::with_capacity(request.write_roots.len());
        for root in std::mem::take(&mut request.write_roots) {
            match crate::task::credential_grant(ctx.state, &root) {
                crate::task::CredentialGrant::Directory => {
                    let root_name = root.display();
                    return GatedToolOutcome::Rejected {
                        note: format!(
                            "`{root_name}` sits inside a protected credential directory, and only a \
                             single file there can be approved. Name the exact file the command \
                             needs — approving a directory would also grant the private keys and \
                             tokens next to it."
                        ),
                    };
                }
                crate::task::CredentialGrant::File => {
                    credential_file_grant = true;
                    credential_high_risk |= crate::task::credential_write_is_high_risk(&root);
                    kept_writes.push(root);
                }
                crate::task::CredentialGrant::Outside => {
                    let root = crate::task::canonical_root(root);
                    if let Some(reason) = denials.reason(&root) {
                        return GatedToolOutcome::Rejected {
                            note: format!(
                                "{reason}. Approving the widening cannot lift deny rules — they \
                                 stay on inside approved roots. Choose a different path or name."
                            ),
                        };
                    }
                    if let Some(carve) = carve_outs
                        .iter()
                        .find(|carve| crate::task::sandbox_within(&root, carve))
                    {
                        let root_name = root.display();
                        let carve_name = carve.display();
                        return GatedToolOutcome::Rejected {
                            note: format!(
                                "`{root_name}` is inside the read-only workspace metadata \
                                 `{carve_name}`. The sandbox denies every write there and adding \
                                 the path to the writable roots cannot lift it; the file tools \
                                 refuse it too. Explain the constraint to the user instead of \
                                 retrying."
                            ),
                        };
                    }
                    if already_writable
                        .iter()
                        .any(|w| crate::task::sandbox_within(&root, w))
                    {
                        dropped_writes.push(root);
                    } else {
                        kept_writes.push(root);
                    }
                }
            }
        }
        request.write_roots = kept_writes;
        // A read request only makes sense inside a credential store: reading is
        // otherwise full-disk, so a redundant read root is dropped and the
        // command still runs — rejecting the call would cost a model
        // round-trip over nothing.
        let mut kept_reads = Vec::with_capacity(request.read_roots.len());
        for root in std::mem::take(&mut request.read_roots) {
            match crate::task::credential_grant(ctx.state, &root) {
                // A **directory** read asks to list names only: the sandbox then
                // denies every child, so contents stay unreadable. That is
                // narrower than a file grant in what it reveals, so it is
                // approvable — and it is the answer to "what is in here?".
                crate::task::CredentialGrant::Directory => {
                    if root.is_dir() {
                        credential_file_grant = true;
                        credential_read_grant = true;
                        credential_listing = true;
                        kept_reads.push(root);
                    } else {
                        return GatedToolOutcome::Rejected {
                            note: format!(
                                "`{}` does not name one file or an existing directory inside a \
                                 protected credential directory.",
                                root.display()
                            ),
                        };
                    }
                }
                crate::task::CredentialGrant::File => {
                    credential_file_grant = true;
                    credential_read_grant = true;
                    kept_reads.push(root);
                }
                crate::task::CredentialGrant::Outside => {
                    let root = crate::task::canonical_root(root);
                    // Same doom check as the write side: a denied path stays
                    // denied after any approval.
                    if let Some(reason) = denials.reason(&root) {
                        return GatedToolOutcome::Rejected {
                            note: format!(
                                "{reason}. Approving the widening cannot lift deny rules — they \
                                 stay on inside approved roots. Choose a different path or name."
                            ),
                        };
                    }
                    dropped_reads.push(root);
                }
            }
        }
        request.read_roots = kept_reads;
        // Only redundant roots were requested: nothing left to approve.
        // (`require_escalated` carries no roots but is not redundant.)
        if !request.require_escalated
            && request.write_roots.is_empty()
            && request.read_roots.is_empty()
        {
            escalation = None;
        }
    }
    // Dropped redundant roots ride the tool output as guidance, so the model
    // stops requesting what is already granted.
    let dropped_note = if dropped_reads.is_empty() && dropped_writes.is_empty() {
        None
    } else {
        let paths = |roots: &[PathBuf]| {
            roots
                .iter()
                .map(|root| root.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut parts: Vec<String> = Vec::new();
        if !dropped_reads.is_empty() {
            parts.push(format!(
                "reading {} was already allowed (reads are full-disk outside credential \
                 directories)",
                paths(&dropped_reads)
            ));
        }
        if !dropped_writes.is_empty() {
            parts.push(format!(
                "writing {} was already covered by the session's writable roots (the workspace, \
                 tmp and the extra roots)",
                paths(&dropped_writes)
            ));
        }
        Some(format!(
            "Note: {}; the request was dropped and the command ran without it. Omit \
             `sandbox_permissions` when the access is already granted.",
            parts.join("; ")
        ))
    };
    // `require_escalated` is a boundary move: Codex asks whether read denials
    // are active, because an unsandboxed run would silently drop them. In that
    // case approval keeps the sandbox and grants a full-disk write instead.
    let require_escalated_denials = require_escalated
        && crate::task::SandboxDenialView::new(ctx.state, ctx.cwd).requires_sandbox_for_denials();
    // A file tool's target is its own approval subject, so the dialog detail is
    // built from it when there is no sandbox widening to describe.
    let credential_detail = credential_file_grant
        && escalation.is_none()
        && fs_outside.is_none()
        && file_tool_target.is_some();

    let mut needs_normal = tool.is_some_and(|t| tool::requires_approval(t, ctx.permissions))
        && !ctx
            .always_allowed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&approval_key)
        && !allowed_by_rules
        // kimi writesOnlyPlanFile: in plan mode, writing the plan file
        // passes through approval-free
        && !(ctx.plan_enabled && tool::is_plan_file_write(ctx.cwd, &call.arguments));
    // A sandbox widening always asks, in every mode: it is an explicit
    // escalation request, not part of the tool's normal permission tier.
    // (Fully redundant requests never reach here — the gate dropped them
    // above.) A credential file write does too — the tool layer refuses the
    // path, so the dialog is the only way the write can happen at all.
    if escalation.is_some() || credential_file_grant {
        needs_normal = true;
    }
    // PreToolUse permission adjustments: allow skips the approval, ask forces
    // it (Full stays prompt-free by definition; deny already returned above)
    match hook_permission {
        Some(crate::hooks::HookPermission::Allow) => needs_normal = false,
        Some(crate::hooks::HookPermission::Ask)
            if ctx.permissions.sandbox.mode != SandboxMode::DangerFullAccess =>
        {
            needs_normal = true;
        }
        _ => {}
    }

    // ---- PermissionRequest hooks: sequential pre-dialog (pig's simplified
    // shape; ZCode instead races the hook chain against the user dialog).
    // They run only when a dialog would actually show (the ask path): allow
    // answers this one call (not recorded in always_allowed), deny rejects,
    // anything else falls through to the dialog. Hook infrastructure failure
    // never auto-denies ----
    let mut hook_approved = false;
    if ctx.hooks.is_active() && (danger_reason.is_some() || fs_outside.is_some() || needs_normal) {
        let run = crate::hooks::HookRunCtx {
            session_id: ctx.session_id.to_string(),
            cwd: ctx.cwd.to_path_buf(),
            tx: tx.clone(),
            cancel: cancel.clone(),
        };
        let payload = crate::hooks::HookPayload::tool(
            crate::hooks::HookEvent::PermissionRequest,
            ctx.session_id,
            ctx.cwd,
            ctx.permissions,
            &call.name,
            &call.arguments,
            item_id,
        )
        .with_transcript(ctx.history);
        let _hook_timer = TurnTiming::optional_guard(&ctx.timing, TimingKind::Hook);
        let effects = crate::hooks::run_event(
            ctx.hooks,
            crate::hooks::HookEvent::PermissionRequest,
            payload,
            &run,
        )
        .await;
        if let Some(reason) = effects.blocked {
            return GatedToolOutcome::Rejected {
                note: format!("Blocked by a PermissionRequest hook: {reason}"),
            };
        }
        match effects.permission {
            Some(crate::hooks::HookPermission::Deny) => {
                return GatedToolOutcome::Rejected {
                    note: "Denied by a PermissionRequest hook.".to_string(),
                };
            }
            Some(crate::hooks::HookPermission::Allow) => hook_approved = true,
            _ => {}
        }
        pre_contexts.extend(effects.additional_contexts);
    }

    // A "remember to project rules" decision reports its outcome here (the
    // call itself is approved regardless); the note is prepended to the tool
    // output after execution.
    let mut project_rule_note: Option<String> = None;
    if !hook_approved && (danger_reason.is_some() || fs_outside.is_some() || needs_normal) {
        let request_id = format!("{}-{turn_id}-approval-{item_id}", ctx.session_id);
        // One-file credential explanation, shared by every dialog shape: a
        // production credential store is outside the workspace, so the
        // outside-write branch must carry the same one-file note and the
        // key-based-login warning instead of showing only the boundary line.
        let credential_explanation = |target: &str| -> String {
            let (verb, detail) = if credential_listing {
                (
                    "List the file names in",
                    "This is inside a protected credential directory: only the names of its \
                     entries become visible, reading any of them still fails, and the approval \
                     cannot be remembered for the session.",
                )
            } else if credential_read_grant {
                (
                    "Read",
                    "This is inside a protected credential directory; only read access to this \
                     one file is shared, and the approval cannot be remembered for the session.",
                )
            } else {
                (
                    "Replace",
                    "This is inside a protected credential directory: the sandbox keeps its \
                     content unreadable, so this write replaces the whole file and only this \
                     one file is shared. The approval cannot be remembered for the session.",
                )
            };
            let base = format!("{verb} {target}\n\n{detail}");
            if credential_high_risk {
                format!(
                    "{base}\n\nWarning: this file authorizes key-based logins. Writing it can \
                     let a key the agent controls log in to this machine."
                )
            } else {
                base
            }
        };
        // The out-of-workspace request leads with its own access line; a sandbox
        // widening names the directories the model asked for and its reason.
        let detail_text = match (&fs_outside, &escalation) {
            (Some((access, path)), _) => {
                let base = format!(
                    "{} outside the workspace: {path}",
                    match access {
                        tool::FsAccess::Read => "Read",
                        tool::FsAccess::Write => "Write",
                    }
                );
                if credential_file_grant {
                    format!("{base}\n\n{}", credential_explanation(path))
                } else {
                    base
                }
            }
            (None, Some(escalation)) if escalation.require_escalated => {
                let base = if require_escalated_denials {
                    "Run this command with broad filesystem access?

The sandbox stays on: ordinary paths become writable, but protected files, deny rules and the network policy remain enforced."
                } else {
                    "Run this command without the sandbox?"
                };
                match &escalation.justification {
                    Some(reason) => format!(
                        "{base}

{reason}"
                    ),
                    None => base.to_string(),
                }
            }
            (None, Some(escalation)) => {
                // The verb has to match what is actually granted: a read-only
                // request says "read", a write request says "write".
                let paths_of = |paths: &[PathBuf]| {
                    paths
                        .iter()
                        .map(|root| root.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let base = if escalation.is_read_only() {
                    let roots = paths_of(&escalation.read_roots);
                    let verb = if credential_listing {
                        "List the file names in"
                    } else {
                        "Grant read access to"
                    };
                    match &escalation.justification {
                        Some(reason) => format!("{verb} {roots}\n\n{reason}"),
                        None => format!("{verb} {roots}"),
                    }
                } else {
                    let roots = paths_of(&escalation.write_roots);
                    match &escalation.justification {
                        Some(reason) => format!("Widen the sandbox to write {roots}\n\n{reason}"),
                        None => format!("Widen the sandbox to write {roots}"),
                    }
                };
                if credential_high_risk {
                    format!(
                        "{base}\n\nWarning: this file authorizes key-based logins. Writing it can \
                         let a key the agent controls log in to this machine."
                    )
                } else if credential_listing {
                    format!(
                        "{base}\n\nThis is inside a protected credential directory: only the names \
                         of its entries become visible. Reading any of them still fails, and the \
                         approval cannot be remembered for the session."
                    )
                } else if credential_file_grant {
                    let access = if credential_read_grant {
                        "read"
                    } else {
                        "write"
                    };
                    format!(
                        "{base}\n\nThis is inside a protected credential directory; only {access} \
                         access to this one file is shared, and the approval cannot be remembered \
                         for the session."
                    )
                } else {
                    base
                }
            }
            (None, None) if credential_detail => credential_explanation(
                &file_tool_target
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
            ),
            (None, None) => approval_detail(call, ctx.cwd),
        };
        // Per-request approval: every waiter is answered only by its own
        // request_id. The full action (tool, arguments, workdir, requested
        // roots and danger class) is already carried by the event; a reply for
        // one request can never approve another — in this session or another.

        let danger_key = danger_reason.map(|reason| reason.key.to_string());
        // The dialog tiers, in display order.
        //
        // "Remember for the session" is meaningless for a high-risk command
        // (dangerous commands are never remembered) and for the plan
        // confirmation (a plan is a one-way decision). A grant inside a
        // credential store is one-shot for the same reason a whole-directory
        // approval is refused there: a standing grant would be a standing
        // capability the user approved after seeing exactly one file. A
        // sandbox widening is a boundary move: Codex offers only
        // approve/abort there so every one stays visible, and a widening
        // whose roots were granted earlier simply asks again.
        //
        // An outside-write approval is a one-call `fs_grant` by design: neither
        // a session pair nor a project rule can suppress the outside-write
        // prompt, so offering those tiers would promise a memory the gate does
        // not honor. Only per-call Allow is offered there.
        //
        // "Remember to project rules" writes a conservative glob into
        // `.pigcode/permissions.toml`; it is offered for the ordinary
        // tool+subject approvals (ExecCommand/Write/Edit) only — never where the
        // request is already a special case (widening, credential file,
        // outside write, high-risk command, plan confirmation).
        let session_scoped = danger_reason.is_none()
            && call.name != "ExitPlanMode"
            && !credential_file_grant
            && escalation.is_none()
            && fs_outside.is_none();
        // A project rule is only durable under a trusted workspace: an
        // untrusted session would write a file whose `allow` the next open
        // ignores, so the tier is not offered there (a legacy client that
        // still answers with it gets a one-shot approval and an honest note).
        let project_scoped = danger_reason.is_none()
            && escalation.is_none()
            && !credential_file_grant
            && fs_outside.is_none()
            && perm_subject.is_some()
            && ctx
                .permission_rules
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .project_allows_are_trusted()
            && (crate::tool::is_exec_command(&call.name)
                || matches!(call.name.as_str(), "Write" | "Edit"));
        let mut available_decisions = vec![ApprovalDecision::Allow];
        if project_scoped {
            available_decisions.push(ApprovalDecision::ApprovedForProject);
        }
        if session_scoped {
            available_decisions.push(ApprovalDecision::ApprovedForSession);
        }
        available_decisions.push(ApprovalDecision::Reject);
        // The roots this dialog asks about, for whichever access it grants; the
        // field name predates read requests and is kept for protocol stability.
        let requested_write_roots: Vec<String> = escalation
            .as_ref()
            .map(|escalation| {
                escalation
                    .write_roots
                    .iter()
                    .chain(escalation.read_roots.iter())
                    .map(|root| root.display().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let review = ctx.reviewer.review(
            ApprovalRequest {
                session_id: ctx.session_id.to_string(),
                agent_id: ctx.agent_id.to_string(),
                run_id: ctx.run_id.clone(),
                turn_id: Some(turn_id.to_string()),
                call_id: call.id.clone(),
                request_id,
                cwd: ctx.cwd.to_path_buf(),
                permissions: ctx.permissions,
                tool: call.name.clone(),
                action: serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone())),
                detail: detail_text,
                danger_key,
                requested_write_roots,
                available_decisions,
                timing: ctx.timing.clone(),
            },
            tx.clone(),
        );
        let ApprovalReply {
            decision,
            feedback: _,
        } = tokio::select! {
            result = review => match result {
                Ok(reply) => reply,
                Err(error) => return GatedToolOutcome::Rejected {
                    note: format!("Approval review failed: {}. The action was not executed.", error.detail),
                },
            },
            _ = cancel.cancelled() => return GatedToolOutcome::Cancelled,
        };
        match decision {
            ApprovalDecision::Abort => return GatedToolOutcome::Cancelled,
            ApprovalDecision::DeferToUser => {
                return GatedToolOutcome::Rejected {
                    note: "Approval review deferred to a user, but no user fallback returned a decision. The action was not executed.".to_string(),
                };
            }
            ApprovalDecision::Allow => {}
            ApprovalDecision::ApprovedForSession => {
                // A widening never offers the session tier (the dialog lists
                // [Allow, Reject]); a legacy client that still answers with it
                // gets a one-shot approval.
                if escalation.is_none() && danger_reason.is_none() && needs_normal {
                    // Dangerous commands are not recorded in always_allowed:
                    // allowed this time only, equivalent to Allow. Outside
                    // writes never reach this arm (the tier is not offered):
                    // their grant is per call, by design.
                    ctx.always_allowed
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(approval_key);
                }
            }
            ApprovalDecision::ApprovedForProject => {
                // Persist a conservative rule for future sessions and apply it
                // to the live set now (no reload). Only a request that was
                // offered the tier writes anything (a client answering with it
                // on a danger/widening/credential prompt gets a one-shot
                // approval), and the call is approved either way: a rule that
                // cannot be derived or saved rides the tool output as a note.
                if project_scoped && let Some(subject) = perm_subject.as_deref() {
                    project_rule_note = remember_project_rule(ctx, call, subject);
                }
            }
            ApprovalDecision::Reject => {
                let note = if let Some((access, path)) = &fs_outside {
                    format!(
                        "The user declined {} outside the workspace ({path}). Respect their decision; do not route around it via the shell, and continue within the workspace.",
                        fs_access_verb(*access)
                    )
                } else {
                    match danger_reason {
                        Some(reason) => format!(
                        "The user rejected this high-risk command ({}). Respect their decision; use a different approach, or explain why it is needed before continuing.",
                        reason.en
                    ),
                        None => "The user rejected this action. Respect their decision; use a different approach, or explain why it is needed before continuing."
                            .to_string(),
                    }
                };
                return GatedToolOutcome::Rejected { note };
            }
        }
    }

    // Plan mode stays read-only no matter what; Full deliberately runs model
    // commands without a sandbox. ReadOnly starts from the read-only base and
    // Auto from the workspace-write base; an approved request is an overlay of
    // `Write` entries on either, so "read-only plus one directory" is one
    // policy with an extra entry rather than a fourth profile.
    let mut sandbox_profile = crate::task::sandbox_profile_for(ctx.permissions, ctx.plan_enabled);
    let mut sandbox_override = crate::task::SandboxOverride::NoOverride;
    if require_escalated {
        if require_escalated_denials {
            // Codex's EscalatedSandboxWithRestrictions: root write, but the
            // deny rules (and metadata carve-outs) stay enforced.
            sandbox_override = crate::task::SandboxOverride::EscalatedWithRestrictions;
        } else {
            // Codex's BypassSandboxFirstAttempt: no denied reads, so the
            // approved command runs without a sandbox.
            sandbox_profile = crate::task::SandboxProfile::Unrestricted;
        }
    }
    // Roots this command may write: only the roots approved for this one (a
    // widening has no session tier). Only approved requests reach this point;
    // a rejected one returned above.
    let mut additional_permissions = crate::task::AdditionalPermissions::default();
    if let Some(escalation) = &escalation {
        additional_permissions.extend_write_roots(escalation.write_roots.iter().cloned());
        additional_permissions.extend_read_roots(escalation.read_roots.iter().cloned());
    }
    // An auto-detected file-tool approval carries no `SandboxEscalation`: the
    // grant lives in the credential flags, so it has to be added here or the
    // tool would still refuse the path it was just approved for.
    if escalation.is_none()
        && let Some(target) = &file_tool_target
    {
        if credential_read_grant {
            additional_permissions.extend_read_roots([target.clone()]);
        }
        if file_tool_is_write {
            additional_permissions.extend_write_roots([target.clone()]);
        }
    }
    let result = {
        let _tool_exec = TurnTiming::optional_guard(&ctx.timing, TimingKind::ToolExec);
        let tool_ctx = ToolContext {
            cwd: ctx.cwd,
            tracker: ctx.tracker,
            state: ctx.state,
            // Full has no workspace boundary, so file writes resolve anywhere;
            // the sensitive-file filter still runs in the tool.
            fs_grant: if ctx.permissions.sandbox.mode == SandboxMode::DangerFullAccess {
                Some(tool::FsAccess::Write)
            } else {
                fs_outside.map(|(access, _)| access)
            },
            sandbox_override,
            sandbox_profile,
            additional_permissions,
        };
        // Stream live incremental output chunks (stdout/stderr/PTY) to the UI
        // while this tool invocation is active. The active flag flips false as
        // soon as the tool returns or cancels so late background-task chunks
        // never leak onto a settled tool card.
        let stream_active = Arc::new(std::sync::atomic::AtomicBool::new(true));
        struct StreamActiveGuard(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for StreamActiveGuard {
            fn drop(&mut self) {
                self.0.store(false, std::sync::atomic::Ordering::Release);
            }
        }
        let _stream_guard = StreamActiveGuard(Arc::clone(&stream_active));
        let stream_tx = tx.clone();
        let stream_session_id = ctx.session_id.to_string();
        let stream_item_id = item_id.to_string();
        let output_sink: crate::task::OutputDeltaSink = Arc::new(move |chunk: &str| {
            if chunk.is_empty() || !stream_active.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            let _ = stream_tx.try_send(Event::ToolOutputDelta {
                session_id: stream_session_id.clone(),
                item_id: stream_item_id.clone(),
                delta: chunk.to_string(),
            });
        });
        tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            result = crate::task::with_tool_output_stream(
                output_sink,
                tool::execute_with_extra(call, tool_ctx, ctx.extra_tools),
            ) => Some(result),
        }
    };
    let Some((mut output, is_error, file_changes, edit, mut media)) = result else {
        return GatedToolOutcome::Cancelled;
    };

    // Codex's default approval policy (`OnRequest`) does not retry a failed
    // command automatically: the sandbox denial is returned to the model,
    // which can re-issue the call with an explicit `sandbox_permissions`
    // request. That keeps every boundary move visible and lets the model ask
    // for the narrow roots it actually needs instead of a blanket bypass.
    // A "remember to project rules" decision reports its outcome to the model
    // here; the call itself was approved either way, so the note rides the
    // tool output instead of turning the result into an error. A dropped
    // redundant-read note takes the same channel.
    for note in [dropped_note, project_rule_note].into_iter().flatten() {
        output = format!("{note}\n\n{output}");
    }

    // ---- PostToolUse / PostToolUseFailure hooks (after execution, before
    // the result becomes the model-facing output; a block decision is a no-op
    // here, logged only). pig's tool failures are is_error outputs (the cancel
    // path returned above), so is_interrupt is always false at this site ----
    if ctx.hooks.is_active() {
        let event = if is_error {
            crate::hooks::HookEvent::PostToolUseFailure
        } else {
            crate::hooks::HookEvent::PostToolUse
        };
        let run = crate::hooks::HookRunCtx {
            session_id: ctx.session_id.to_string(),
            cwd: ctx.cwd.to_path_buf(),
            tx: tx.clone(),
            cancel: cancel.clone(),
        };
        let payload = crate::hooks::HookPayload::tool(
            event,
            ctx.session_id,
            ctx.cwd,
            ctx.permissions,
            &call.name,
            &call.arguments,
            item_id,
        )
        .with_response(&output)
        .with_interrupt(false)
        .with_transcript(ctx.history);
        let _hook_timer = TurnTiming::optional_guard(&ctx.timing, TimingKind::Hook);
        let effects = crate::hooks::run_event(ctx.hooks, event, payload, &run).await;
        if effects.suppress_output {
            output = "(tool output suppressed by a hook)".to_string();
        }
        pre_contexts.extend(effects.additional_contexts);
    }
    for context in pre_contexts {
        output.push_str(&format!("\n\n[hook context]\n{context}"));
    }
    // Lazy subdirectory AGENTS.md discovery (Kimi/DeepSeek pattern): when a
    // successful file/search/shell tool accesses a subdirectory inside the
    // workspace, inject any previously unseen nested AGENTS.md files as a
    // `<system-reminder>` on the tool result without mutating the frozen
    // system prompt (zero prefix-cache invalidation).
    if !is_error {
        let mut accessed = tool::file_tool_targets(ctx.cwd, call);
        if tool::is_exec_command(&call.name)
            && let Some(workdir) = serde_json::from_str::<serde_json::Value>(&call.arguments)
                .ok()
                .and_then(|args| args["workdir"].as_str().map(str::to_string))
                .filter(|w| !w.trim().is_empty())
        {
            let raw = Path::new(&workdir);
            accessed.push(if raw.is_absolute() {
                raw.to_path_buf()
            } else {
                ctx.cwd.join(raw)
            });
        }
        if !accessed.is_empty() {
            let discovered = {
                let mut known = ctx
                    .state
                    .known_agents_md
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                // If the model explicitly Read an AGENTS.md file itself, mark
                // it known first so we never echo back a file it just read.
                if call.name == "Read" {
                    for path in &accessed {
                        if path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.eq_ignore_ascii_case("AGENTS.md"))
                        {
                            known.insert(path.canonicalize().unwrap_or_else(|_| path.clone()));
                        }
                    }
                }
                prompt::discover_subdir_agents_md(ctx.cwd, &accessed, &mut known)
            };
            if let Some(reminder) = prompt::format_subdir_agents_md_reminder(ctx.cwd, &discovered) {
                if !output.is_empty() {
                    output.push_str("\n\n");
                }
                output.push_str(&reminder);
            }
        }
    }
    // Media enters the model context via history; callers persist its
    // references alongside the ToolCall's output text, never base64 in rollout.
    // An unsupported input kind is degraded to deterministic text here, before
    // any media file is written.
    project_tool_media(&call.arguments, &mut output, &mut media, ctx.media);
    let chat_images = tool_images_to_chat(&call.arguments, media.images);
    let chat_documents = tool_documents_to_chat(media.documents);
    // After a successful root TodoList write, push the todo snapshot to the UI
    // (reads output the list itself; no need to push again). Children use a
    // child-local TodoHandle and never persist or emit root todo events.
    if ctx.owns_todo_state && call.name == "TodoList" && !is_error {
        let items = ctx.state.todos.lock().expect("todos lock").clone();
        // Write operations (with a todos argument) persist to the SQLite
        // todos table (upsert of current state); reads do not persist. The
        // event-stream JSONL no longer records state snapshots
        let is_write = serde_json::from_str::<serde_json::Value>(&call.arguments)
            .ok()
            .is_some_and(|v| v.get("todos").is_some());
        if is_write {
            let json = serde_json::to_string(&items).unwrap_or_default();
            ctx.store
                .lock()
                .expect("store lock")
                .set_todos(ctx.session_id, &json);
        }
        emit_bg(ctx.session_id, tx, |session_id| Event::TodoListChanged {
            session_id,
            items,
        });
    }
    for change in file_changes {
        // Current change state persists to the SQLite file_changes table
        // (upsert by path; rows deleted when the net delta reaches zero);
        // the JSONL keeps only the message/tool event stream
        {
            let store = ctx.store.lock().expect("store lock");
            if change.additions == 0 && change.deletions == 0 {
                store.delete_file_change(ctx.session_id, &change.path);
            } else {
                store.upsert_file_change(
                    ctx.session_id,
                    &change.path,
                    &change.unified_diff,
                    change.additions,
                    change.deletions,
                );
            }
        }
        emit_bg(ctx.session_id, tx, |session_id| Event::FileChanged {
            session_id,
            path: change.path,
            unified_diff: change.unified_diff,
            additions: change.additions,
            deletions: change.deletions,
        });
    }
    // Original snapshots newly added by this or any other driver persist to
    // the file_originals table (cross-restart diff baseline / revert); files
    // over 4MB are not persisted (the baseline falls back to process memory,
    // consistent with kimi-code).
    persist_tracker_originals(ctx.tracker, ctx.store, ctx.session_id);
    GatedToolOutcome::Executed {
        output,
        is_error,
        edit,
        images: chat_images,
        documents: chat_documents,
        delivered_notification_ids: media.delivered_notification_ids.clone(),
    }
}

fn extract_file_path_arg(arguments: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    value
        .get("path")
        .or_else(|| value.get("file_path"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn path_within_any_root(candidate: &Path, roots: &[PathBuf]) -> bool {
    if roots.is_empty() {
        return false;
    }
    let resolved = candidate.canonicalize().unwrap_or_else(|_| {
        if let Some(parent) = candidate.parent()
            && let Ok(canon_parent) = parent.canonicalize()
            && let Some(name) = candidate.file_name()
        {
            return canon_parent.join(name);
        }
        candidate.to_path_buf()
    });
    roots.iter().any(|root| {
        let canon_root = root.canonicalize().unwrap_or_else(|_| root.clone());
        resolved.starts_with(&canon_root)
    })
}
