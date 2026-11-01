import { effect, keymap, signal, Text, token, View, type YNode } from "nsh";
import { Button, Input, Popover } from "nsh/base";
import type { SlashCommand } from "../commands";
import { filterSlashCommands } from "../commands";
import type { UiContext } from "../ui-context";
import { workspaceName } from "../ui-context";
import { ApprovalCard } from "./approval-card";
import { ModelPicker, PermissionPicker, SlashCandidates } from "./composer-popups";
import { Icon } from "./icon";
import { hostPatchProps, UiNodeFactory, UiView } from "../plugins/ui-node-factory";
import { QuestionCard } from "./question-card";
import { QueuePanel } from "./queue-panel";

function WorkspaceSelector(props: UiContext) {
  const open = signal(false);
  const selected = () => props.store.heroCwd();
  const workspaces = () => Array.from(new Set([
    props.store.cwd(),
    ...props.store.sessions().map((session) => session.cwd),
    ...(selected() ? [selected()!] : []),
  ]));
  return (
    <View class="w-full flex-row items-center gap-2 border-b border-border pb-1">
      <Popover
        open={open()}
        onOpenChange={(value) => open.set(value)}
        placement="top"
        align="start"
        popupClass="w-80 rounded-xl border border-border bg-popover p-2 shadow-xl"
        trigger={
          <View role="button" testID="hero-cwd" class="flex-row items-center gap-1 rounded-full bg-accent/50 px-3 py-1 hover:bg-accent">
            <Icon name="folder-closed" color={token("muted-foreground")} />
            <Text class={selected() ? "text-sm text-foreground" : "text-sm text-muted-foreground"}>
              {selected() ? workspaceName(selected()!) : "选择工作区"}
            </Text>
            <Icon name="chevron-down" size={12} color={token("muted-foreground")} />
          </View>
        }
      >
        <View class="gap-1">
          <Each of={workspaces()} key={(path) => path}>
            {(path, index) => (
              <Button
                testID={`hero-cwd-option-${index()}`}
                selected={selected() === path()}
                class="rounded-(--radius) px-3 py-2 hover:bg-accent selected:bg-accent/60"
                onPress={() => {
                  props.store.selectHeroWorkspace(path());
                  open.set(false);
                }}
              >
                <View class="min-w-0 gap-0.5">
                  <Text numberOfLines={1} class="text-sm text-foreground">{workspaceName(path())}</Text>
                  <Text numberOfLines={1} class="text-xs text-muted-foreground">{path()}</Text>
                </View>
              </Button>
            )}
          </Each>
          <If when={selected()}>
            <Button
              testID="hero-cwd-clear"
              class="rounded-(--radius) px-3 py-2 hover:bg-accent"
              onPress={() => {
                props.store.selectHeroWorkspace(null);
                open.set(false);
              }}
            >
              <Text class="text-sm text-muted-foreground">清除工作区</Text>
            </Button>
          </If>
        </View>
      </Popover>
    </View>
  );
}

export function Composer(props: UiContext & { hero?: boolean }) {
  const { store, themeStore } = props;
  const slashIndex = signal(0);
  const inputFocused = signal(false);
  let inputNode: YNode | undefined;
  const sessionId = () => store.current();
  const running = () => {
    const id = sessionId();
    return id ? (store.running()[id] ?? false) : false;
  };
  const approval = () => {
    const id = sessionId();
    return id ? (store.approvals()[id] ?? [])[0] : undefined;
  };
  const question = () => {
    const id = sessionId();
    return id ? store.questions()[id] : null;
  };
  const slashQuery = () => /^\/([^\s]*)$/.exec(store.draft())?.[1] ?? null;
  const slashCommands = () => {
    const query = slashQuery();
    return query === null || approval() || question() ? [] : filterSlashCommands(query);
  };
  const chooseSlash = (name: SlashCommand["name"]) => {
    store.draft.set(`${name} `);
  };
  const submit = (opposite = false) => {
    if (approval() || question()) return;
    const commands = slashCommands();
    if (!opposite && commands.length > 0) {
      chooseSlash(commands[Math.min(slashIndex(), commands.length - 1)].name);
      return;
    }
    if (opposite) store.sendWithOppositeDelivery();
    else store.send();
  };
  effect(() => {
    store.draft();
    slashIndex.set(0);
  });
  effect(() => {
    if (!inputFocused()) return;
    return keymap.bind({
      enter: () => submit(),
      "ctrl+enter": () => submit(true),
      "cmd+enter": () => submit(true),
    }, { when: "always" });
  });
  effect(() => {
    if (!inputFocused() || slashCommands().length === 0) return;
    return keymap.bind({
      up: () => slashIndex.update((index) => (index - 1 + slashCommands().length) % slashCommands().length),
      down: () => slashIndex.update((index) => (index + 1) % slashCommands().length),
      escape: () => {
        const value = store.draft();
        if (/^\/[^\s]*$/.test(value)) store.draft.set("");
      },
    }, { when: "always" });
  });
  const queue = () => store.currentQueue();
  const showQueue = () => !props.hero && !approval() && !question() && (
    queue().pending.length > 0 || queue().steer.length > 0 || queue().paused || queue().held
  );
  const sendWithContributions = () => (
    <UiNodeFactory
      uiId="composer.sendButton"
      store={store}
      render={(patch, children) => (
        <Button
          testID="send-button"
          accessibilityLabel={(patch.accessibility.accessibilityLabel as string | undefined) ?? (running() ? "停止" : "发送")}
          disabled={patch.props.disabled === undefined ? !running() && !store.draft().trim() : Boolean(patch.props.disabled)}
          class="h-6 w-6 items-center justify-center rounded-full bg-primary active:bg-primary/90 disabled:opacity-30"
          style={patch.style}
          {...hostPatchProps(patch, { onPress: () => store.submitOrStop() })}
        >
          {children ?? <Icon name={running() ? "square" : "arrow-up"} size={running() ? 10 : 14} color={token("primary-foreground")} />}
        </Button>
      )}
    />
  );
  return (
    <View class={props.hero ? "p-3 pb-1" : "px-4 pb-4 pt-2"}>
      <View class="mx-auto w-full max-w-[860px]">
        <If when={showQueue()}><QueuePanel {...props} /></If>
        <UiView
          uiId="composer.root"
          store={store}
          class={themeStore.classes(
            "composer.root",
            "gap-2 rounded-2xl border border-input bg-composer px-3 pb-2 pt-2",
          )}
        >
          <If when={props.hero}><WorkspaceSelector {...props} /></If>
          <If when={question()}>
            {(value) => <QuestionCard {...props} question={value()} />}
          </If>
          <If when={approval()}>
            {(value) => <ApprovalCard {...props} approval={value()} />}
          </If>
          <If when={!question() && !approval()}>
            <View class="relative">
              <If when={slashCommands().length > 0}>
                <SlashCandidates query={slashQuery() ?? ""} selected={slashIndex()} onSelect={chooseSlash} />
              </If>
              <UiNodeFactory
                uiId="composer.input"
                store={store}
                captureFocus={() => inputFocused()}
                restoreFocus={() => setTimeout(() => inputNode?.focus(), 0)}
                render={(patch) => (
                  <Input
                    ref={(node) => inputNode = node}
                    testID="composer-input"
                    value={store.draft()}
                    {...hostPatchProps(patch, {
                      onChange: (text: string) => store.draft.set(text),
                      onSubmit: () => submit(),
                      onFocus: () => inputFocused.set(true),
                      onBlur: () => inputFocused.set(false),
                    })}
                    multiline={patch.props.multiline === undefined ? true : Boolean(patch.props.multiline)}
                    disabled={patch.props.disabled as boolean | undefined}
                    submitBehavior="submit"
                    placeholder={(patch.props.placeholder as string | undefined) ?? (running()
                      ? store.config()?.default_delivery === "steer"
                        ? "继续输入以引导后续修改"
                        : "继续输入以排队后续修改"
                      : store.config()?.default_delivery === "steer"
                        ? "按 Enter 发送引导消息 · 按 Ctrl+Enter 加入队列"
                        : "按 Enter 加入队列 · 按 Ctrl+Enter 发送引导消息")}
                    class={themeStore.classes(
                      "composer.input",
                      "max-h-40 min-h-16 px-1 py-2 text-sm leading-6 text-foreground placeholder:text-muted-foreground",
                    )}
                    style={patch.style}
                    {...patch.accessibility}
                  />
                )}
              />
            </View>
            <UiView uiId="composer.actions" store={store} class="flex-row items-center gap-2">
              <PermissionPicker {...props} />
              <View class="flex-1" />
              <ModelPicker {...props} />
              {sendWithContributions()}
            </UiView>
          </If>
        </UiView>
      </View>
    </View>
  );
}
