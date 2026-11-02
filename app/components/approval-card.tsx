import { effect, keymap, Scroll, signal, Text, View } from "nsh";
import { Button, Input, TextView } from "nsh/base";
import { t } from "../i18n";
import type { MessageKey } from "../i18n";
import type { PendingApproval } from "../protocol";
import type { UiContext } from "../ui-context";

function dangerText(key?: string | null): string | null {
  const reasons: Record<string, MessageKey> = {
    fork_bomb: "approval.danger.fork_bomb",
    rm_rf_root: "approval.danger.rm_rf_root",
    disk_format: "approval.danger.disk_format",
    dd_block: "approval.danger.dd_block",
    shutdown: "approval.danger.shutdown",
    chmod_root: "approval.danger.chmod_root",
  };
  return key && reasons[key] ? t("approval.danger.prefix", { reason: t(reasons[key]) }) : null;
}

export function ApprovalCard(props: UiContext & { approval: PendingApproval }) {
  const revise = signal(false);
  const feedback = signal("");
  const plan = () => props.approval.tool === "ExitPlanMode";
  const offersSession = () => props.approval.availableDecisions.includes("ApprovedForSession");
  const offersProject = () => props.approval.availableDecisions.includes("approved_for_project");
  const warning = () => dangerText(props.approval.dangerKey);
  const submitRevision = () => {
    const text = feedback().trim();
    if (text) props.store.answerApproval("Reject", text);
  };
  effect(() => {
    props.approval.requestId;
    revise.set(false);
    feedback.set("");
  });
  effect(() => keymap.bind({
    enter: () => {
      if (revise()) submitRevision();
      else props.store.answerApproval("Allow");
    },
    "ctrl+enter": () => props.store.answerApproval(offersSession() ? "ApprovedForSession" : "Allow"),
    escape: () => props.store.answerApproval("Reject"),
  }, { when: "always" }));
  return (
    <View
      testID="approval-card"
      class={props.themeStore.classes("approval.root", "gap-3 rounded-xl border border-warning/40 bg-warning/10 p-4")}
    >
      <View class="flex-row items-center gap-2">
        <View class="h-2 w-2 rounded-full bg-warning" />
        <Text class="text-sm font-semibold text-foreground">
          {plan() ? t("approval.plan_prompt") : t("approval.tool_prompt", { tool: props.approval.tool })}
        </Text>
      </View>
      <If when={warning()}>
        <Text class="text-sm text-destructive">{warning()}</Text>
      </If>
      <If when={plan()}>
        <View class="max-h-64 overflow-hidden rounded-(--radius) bg-background p-3">
          <TextView
            markdown={props.approval.detail}
            selectable
            classes={{ root: "text-sm leading-6 text-foreground", codeBlock: "bg-tool-surface" }}
          />
        </View>
      </If>
      <If when={!plan()}>
        <Scroll class="max-h-52 rounded-(--radius) bg-background">
          <Text selectable class="p-3 font-mono text-xs leading-5 text-foreground">
            {props.approval.tool === "ExecCommand" ? `$ ${props.approval.detail}` : props.approval.detail}
          </Text>
        </Scroll>
      </If>
      <If when={props.approval.requestedWriteRoots.length > 0}>
        <View class="gap-1">
          <Text class="text-xs font-medium text-foreground">{t("approval.write_dirs")}</Text>
          <Each of={props.approval.requestedWriteRoots} key={(root) => root}>
            {(root) => <Text selectable class="font-mono text-xs text-muted-foreground">{root()}</Text>}
          </Each>
        </View>
      </If>
      <If when={!plan()}>
        <Text numberOfLines={1} class="text-xs text-muted-foreground">{t("approval.cwd", { cwd: props.store.cwd() })}</Text>
      </If>
      <If when={revise()}>
        <Input
          testID="plan-revision-input"
          value={feedback()}
          onChange={(text) => feedback.set(text)}
          onSubmit={submitRevision}
          submitBehavior="submit"
          autoFocus
          placeholder={t("approval.revise_placeholder")}
          class="rounded-(--radius) border border-border bg-background px-3 py-2 selection:bg-foreground text-sm text-foreground placeholder:text-muted-foreground"
        />
      </If>
      <View class="flex-row flex-wrap items-center justify-end gap-2">
        <If when={plan()}>
          <Button
            testID="plan-revise"
            class="rounded-(--radius) border border-border px-3 py-2 hover:bg-muted"
            onPress={() => revise.update((value) => !value)}
          >
            <Text class="text-sm text-foreground">{t(revise() ? "approval.cancel_revise" : "approval.revise_plan")}</Text>
          </Button>
        </If>
        <If when={offersProject()}>
          <Button
            testID="approval-project"
            class="rounded-(--radius) border border-border px-3 py-2 hover:bg-muted"
            onPress={() => props.store.answerApproval("approved_for_project")}
          >
            <Text class="text-sm text-foreground">{t("approval.allow_project")}</Text>
          </Button>
        </If>
        <If when={offersSession()}>
          <Button
            testID="approval-session"
            class="rounded-(--radius) border border-border px-3 py-2 hover:bg-muted"
            onPress={() => props.store.answerApproval("ApprovedForSession")}
          >
            <Text class="text-sm text-foreground">{t("approval.allow_session")}</Text>
          </Button>
        </If>
        <If when={!revise()}>
          <Button
            testID="approval-reject"
            class="rounded-(--radius) border border-border px-3 py-2 hover:bg-muted"
            onPress={() => props.store.answerApproval("Reject")}
          >
            <Text class="text-sm text-foreground">{t("approval.deny")}</Text>
          </Button>
          <Button
            testID="approval-allow"
            class="rounded-(--radius) bg-primary px-4 py-2"
            onPress={() => props.store.answerApproval("Allow")}
          >
            <Text class="text-sm text-primary-foreground">{t(plan() ? "approval.approve_plan" : "approval.allow")}</Text>
          </Button>
        </If>
        <If when={revise()}>
          <Button
            testID="plan-revision-submit"
            disabled={!feedback().trim()}
            class="rounded-(--radius) bg-primary px-4 py-2 disabled:opacity-40"
            onPress={submitRevision}
          >
            <Text class="text-sm text-primary-foreground">{t("approval.submit_revisions")}</Text>
          </Button>
        </If>
      </View>
    </View>
  );
}
