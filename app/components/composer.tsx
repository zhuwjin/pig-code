import { effect, keymap, signal, Text, token, View } from "nsh";
import { Button, Input } from "nsh/base";
import type { SlashCommand } from "../commands";
import { filterSlashCommands } from "../commands";
import type { UiContext } from "../ui-context";
import { workspaceName } from "../ui-context";
import { ApprovalCard } from "./approval-card";
import { ModelPicker, PermissionPicker, SlashCandidates } from "./composer-popups";
import { Icon } from "./icon";
import { ExecutablePlugin, executablePlugins } from "./plugin-surface";
import { QuestionCard } from "./question-card";
import { QueuePanel } from "./queue-panel";

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

export function Composer(props: UiContext & { hero?: boolean }) {
  const { store, themeStore } = props;
  const slashIndex = signal(0);
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
  effect(() => keymap.bind({
    enter: () => submit(),
    "ctrl+enter": () => submit(true),
    "cmd+enter": () => submit(true),
  }, { when: "always" }));
  effect(() => {
    if (slashCommands().length === 0) return;
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
  return (
    <View class={props.hero ? "px-4" : "px-4 pb-4 pt-2"}>
      <View class="mx-auto w-full max-w-3xl">
        <If when={showQueue()}><QueuePanel {...props} /></If>
        <View
          class={themeStore.classes(
            "composer.root",
            "rounded-2xl border border-input bg-composer px-3 pb-2 pt-2 shadow-lg",
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
              <If when={executablePlugins}>
                <ExecutablePlugin {...props} slot="composer" class="h-44" />
              </If>
              <If when={!executablePlugins}>
                <Input
                  testID="composer-input"
                  value={store.draft()}
                  onChange={(text) => store.draft.set(text)}
                  onSubmit={() => submit()}
                  multiline
                  submitBehavior="submit"
                  placeholder={running()
                    ? store.config()?.default_delivery === "steer"
                      ? "Enter 引导当前回合 · Ctrl+Enter 加入队列 · Shift+Enter 换行"
                      : "Enter 加入队列 · Ctrl+Enter 引导当前回合 · Shift+Enter 换行"
                    : "输入消息，Enter 发送，Shift+Enter 换行"}
                  class={themeStore.classes(
                    "composer.input",
                    "max-h-40 min-h-16 px-1 py-2 text-sm leading-6 text-foreground placeholder:text-muted-foreground",
                  )}
                />
              </If>
            </View>
            <View class="flex-row items-center gap-3 pt-1">
              <PermissionPicker {...props} />
              <View class="flex-1" />
              <ModelPicker {...props} />
              <Button
                testID="send-button"
                accessibilityLabel={running() ? "停止" : "发送"}
                disabled={!running() && !store.draft().trim()}
                class="h-8 w-8 items-center justify-center rounded-full bg-primary active:bg-primary/90 disabled:opacity-30"
                onPress={() => store.submitOrStop()}
              >
                <Icon
                  name={running() ? "square" : "arrow-up"}
                  size={running() ? 10 : 14}
                  color={token("primary-foreground")}
                />
              </Button>
            </View>
          </If>
          <If when={executablePlugins && !question() && !approval()}>
            <View class="flex-row items-center px-2 pt-1">
              <View class="flex-1" />
              <ExecutablePlugin {...props} slot="rate" class="h-6 w-24" />
            </View>
          </If>
        </View>
      </View>
    </View>
  );
}
