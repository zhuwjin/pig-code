import { Text, View } from "nsh";
import type { UiContext } from "../ui-context";
import { Composer } from "./composer";
import { EmptyWorkspace } from "./hero";
import { MessageList } from "./messages";
import { ExecutablePlugin, executablePlugins } from "./plugin-surface";
import { StatusBar } from "./status-bar";

export function Workspace(props: UiContext) {
  const { store } = props;
  const items = () => {
    const id = store.current();
    return id ? (store.items()[id] ?? []) : [];
  };
  const error = () => {
    const id = store.current();
    return (id && store.errors()[id]) || store.globalError();
  };
  const hero = () => items().length === 0;
  return (
    <View class="relative min-w-0 flex-1 bg-background">
      <If when={error()}>
        <View class="mx-auto mt-3 max-w-3xl rounded-(--radius) bg-destructive/10 px-3 py-2">
          <Text selectable class="text-sm text-destructive">{error()}</Text>
        </View>
      </If>
      <If when={!hero()}>
        <MessageList {...props} items={items()} />
        <Composer {...props} />
        <StatusBar {...props} />
      </If>
      <If when={hero()}><EmptyWorkspace {...props} /></If>
      <If when={executablePlugins}>
        <View class="absolute right-6 top-6 z-40 h-20 w-20 pointer-events-none">
          <ExecutablePlugin {...props} slot="pet" class="h-20 w-20" />
        </View>
      </If>
    </View>
  );
}
