import { Text, View, token } from "nsh";
import { Button, Input } from "nsh/base";
import type { PendingApproval } from "../protocol";
import type { UiContext } from "../ui-context";
import { workspaceName } from "../ui-context";
import { Icon } from "./icon";
import { ExecutablePlugin, executablePlugins } from "./plugin-surface";

function ApprovalCard(props: UiContext & { approval: PendingApproval }) {
  return (
    <View class="mb-3 gap-3 rounded-(--radius) border border-warning/40 bg-warning/10 p-4">
      <Text class="text-sm font-semibold text-foreground">需要批准：{props.approval.tool}</Text>
      <Text selectable numberOfLines={6} class="font-mono text-xs leading-5 text-muted-foreground">
        {props.approval.detail}
      </Text>
      <View class="flex-row justify-end gap-2">
        <Button
          class="rounded-(--radius) border border-border px-4 py-2 active:bg-muted"
          onPress={() => props.store.answerApproval(false)}
        >
          <Text class="text-sm text-foreground">拒绝</Text>
        </Button>
        <Button
          class="rounded-(--radius) bg-primary px-4 py-2 active:bg-primary/90"
          onPress={() => props.store.answerApproval(true)}
        >
          <Text class="text-sm text-primary-foreground">允许</Text>
        </Button>
      </View>
    </View>
  );
}

function WorkspaceSelector(props: UiContext) {
  return (
    <View class="mb-1 flex-row items-center border-b border-border pb-1">
      <Button
        testID="hero-cwd"
        class="flex-row items-center gap-1 rounded-full bg-accent/50 px-3 py-1 hover:bg-accent"
      >
        <Icon name="folder-closed" color={token("muted-foreground")} />
        <Text class="text-sm text-muted-foreground">{workspaceName(props.store.cwd())}</Text>
        <Icon name="chevron-down" size={12} color={token("muted-foreground")} />
      </Button>
    </View>
  );
}

function DefaultComposer(props: UiContext & { hero?: boolean }) {
  const { store, themeStore } = props;
  const sessionId = () => store.current();
  const running = () => {
    const id = sessionId();
    return id ? (store.running()[id] ?? false) : false;
  };
  const approval = () => {
    const id = sessionId();
    return id ? (store.approvals()[id] ?? [])[0] : undefined;
  };
  return (
    <View class={props.hero ? "px-4" : "px-4 pb-4 pt-2"}>
      <View class="mx-auto w-full max-w-3xl">
        <If when={approval()}>{(value) => <ApprovalCard {...props} approval={value()} />}</If>
        <View
          class={themeStore.classes(
            "composer.root",
            "rounded-2xl border border-input bg-composer px-3 pb-2 pt-2 shadow-lg",
          )}
        >
          <If when={props.hero}><WorkspaceSelector {...props} /></If>
          <Input
            testID="composer-input"
            value={store.draft()}
            onChange={(text) => store.draft.set(text)}
            onSubmit={() => store.send()}
            multiline
            submitBehavior="submit"
            placeholder="输入消息，Enter 发送，Shift+Enter 换行"
            class={themeStore.classes(
              "composer.input",
              "max-h-40 min-h-16 px-1 py-2 text-sm leading-6 text-foreground placeholder:text-muted-foreground",
            )}
          />
          <View class="flex-row items-center gap-3 pt-1">
            <View class="flex-row items-center gap-1.5">
              <Icon name="shield-check" size={14} color={token("warning")} />
              <Text class="text-xs text-warning">自动</Text>
              <Icon name="chevron-down" size={12} color={token("muted-foreground")} />
            </View>
            <View class="flex-1" />
            <Text numberOfLines={1} class="text-xs text-muted-foreground">
              {store.currentModel() || "默认模型"}
            </Text>
            <Icon name="chevron-down" size={12} color={token("muted-foreground")} />
            <Button
              testID="send-button"
              accessibilityLabel={running() ? "停止" : "发送"}
              disabled={!running() && !store.draft().trim()}
              class="h-8 w-8 items-center justify-center rounded-full bg-primary active:bg-primary/90 disabled:opacity-30"
              onPress={() => store.send()}
            >
              <Icon
                name={running() ? "square" : "arrow-up"}
                size={running() ? 10 : 14}
                color={token("primary-foreground")}
              />
            </Button>
          </View>
        </View>
      </View>
    </View>
  );
}

export function Composer(props: UiContext & { hero?: boolean }) {
  const { store } = props;
  const approval = () => {
    const id = store.current();
    return id ? (store.approvals()[id] ?? [])[0] : undefined;
  };
  return (
    <View>
      <If when={executablePlugins}>
        <View class={props.hero ? "px-4" : "px-4 pb-4 pt-2"}>
          <View class="mx-auto w-full max-w-3xl">
            <If when={approval()}>{(value) => <ApprovalCard {...props} approval={value()} />}</If>
            <If when={props.hero}><WorkspaceSelector {...props} /></If>
            <ExecutablePlugin {...props} slot="composer" class="h-44" />
            <View class="flex-row items-center px-2 pt-1">
              <View class="flex-1" />
              <ExecutablePlugin {...props} slot="rate" class="h-6 w-24" />
            </View>
          </View>
        </View>
      </If>
      <If when={!executablePlugins}><DefaultComposer {...props} /></If>
    </View>
  );
}
