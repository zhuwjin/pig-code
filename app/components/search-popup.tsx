import { effect, keymap, Scroll, signal, Text, View } from "nsh";
import { Button, Input } from "nsh/base";
import type { UiContext } from "../ui-context";
import { workspaceName } from "../ui-context";
import { Icon } from "./icon";

type SearchResult = {
  id: string;
  kind: "session" | "workspace" | "command";
  title: string;
  detail: string;
  path: string;
  pluginInstance?: string;
  commandId?: string;
};

export function SearchPopup(props: UiContext & { onClose: () => void }) {
  const query = signal("");
  const selected = signal(0);
  const results = (): SearchResult[] => {
    const normalized = query().trim().toLowerCase();
    const matches = (value: string) => !normalized || value.toLowerCase().includes(normalized);
    const sessions = props.store.sessions()
      .filter((session) => matches(`${session.title} ${session.cwd}`))
      .map((session) => ({
        id: session.id,
        kind: "session" as const,
        title: session.title || "未命名会话",
        detail: session.cwd,
        path: session.cwd,
      }));
    const workspaces = Array.from(new Set([
      props.store.cwd(),
      ...props.store.sessions().map((session) => session.cwd),
    ]))
      .filter((cwd) => matches(cwd))
      .map((cwd) => ({
        id: `workspace:${cwd}`,
        kind: "workspace" as const,
        title: workspaceName(cwd),
        detail: cwd,
        path: cwd,
      }));
    const commands = props.store.pluginCommands()
      .filter((command) => matches(command.title))
      .map((command) => ({
        id: `command:${command.instance}:${command.command.id}`,
        kind: "command" as const,
        title: command.title,
        detail: "插件命令",
        path: "",
        pluginInstance: command.instance,
        commandId: command.command.id,
      }));
    return [...sessions, ...workspaces, ...commands].slice(0, 50);
  };
  const choose = (result: SearchResult) => {
    if (result.kind === "session") {
      props.store.openSession(result.id);
    } else if (result.kind === "workspace") {
      props.store.newTask();
      props.store.selectHeroWorkspace(result.path);
    } else if (result.pluginInstance && result.commandId) {
      props.store.runPluginCommand(result.pluginInstance, result.commandId);
    }
    props.onClose();
  };
  effect(() => {
    query();
    selected.set(0);
  });
  effect(() => keymap.bind({
    up: () => selected.update((index) => Math.max(0, index - 1)),
    down: () => selected.update((index) => Math.max(0, Math.min(results().length - 1, index + 1))),
    escape: props.onClose,
  }, { when: "always" }));
  return (
    <View class="absolute inset-0 z-50">
      <Button accessibilityLabel="关闭搜索" class="absolute inset-0 bg-black/30" onPress={props.onClose} />
      <View class="absolute inset-x-0 top-[15%] mx-auto max-h-[480px] w-[560px] max-w-[90%] overflow-hidden rounded-xl border border-border bg-popover shadow-xl">
        <View class="flex-row items-center gap-2 border-b border-border px-3 py-2">
          <Icon name="search" />
          <Input
            testID="global-search-input"
            autoFocus
            value={query()}
            onChange={(value) => query.set(value)}
            onSubmit={() => {
              const result = results()[selected()];
              if (result) choose(result);
            }}
            placeholder="搜索会话或工作区"
            class="min-w-0 flex-1 text-sm text-foreground selection:bg-foreground placeholder:text-muted-foreground"
          />
        </View>
        <Scroll class="max-h-[400px]">
          <View class="gap-1 p-2">
            <Each of={results()} key={(result) => result.id}>
              {(result, index) => (
                <Button
                  testID={`search-${result().kind}-${index()}`}
                  selected={results()[selected()]?.id === result().id}
                  class="rounded-(--radius) px-3 py-2 hover:bg-accent selected:bg-accent/60"
                  onPress={() => choose(result())}
                >
                  <View class="min-w-0 flex-1 gap-0.5">
                    <Text numberOfLines={1} class="text-sm text-foreground">{result().title}</Text>
                    <Text numberOfLines={1} class="text-xs text-muted-foreground">{result().detail}</Text>
                  </View>
                </Button>
              )}
            </Each>
            <If when={results().length === 0}>
              <Text class="px-3 py-6 text-center text-sm text-muted-foreground">没有匹配结果</Text>
            </If>
          </View>
        </Scroll>
      </View>
    </View>
  );
}
