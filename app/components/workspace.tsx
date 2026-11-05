import { Text, View } from "nsh";
import type { UiContext } from "../ui-context";
import { Composer } from "./composer";
import { EmptyWorkspace } from "./hero";
import { MessageList } from "./messages";
import { StatusBar } from "./status-bar";
import { pluginPages } from "../plugins/ui-coordinator";
import { PluginPageView } from "../plugins/pages";

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
  const hero = () => {
    const id = store.current();
    return items().length === 0 && !(id && store.running()[id]);
  };
  const pluginPage = () => pluginPages(
    store.pluginPackages(),
    "workspace",
    store.pluginRuntime(),
    store.pluginOrder(),
  ).find((entry) => {
    const active = store.activePluginPage();
    return active?.instance === entry.instance && active.pageId === entry.page.id;
  });
  return (
    <View class="relative min-h-0 min-w-0 flex-1 bg-background">
      <If when={pluginPage()}>
        {(entry) => <PluginPageView {...props} entry={entry()} />}
      </If>
      <If when={!pluginPage()}>
        <If when={error()}>
          <View class="mx-auto mt-3 max-w-3xl rounded-(--radius) bg-destructive/10 px-3 py-2">
            <Text style={{ userSelect: "text" }} class="text-sm text-destructive">{error()}</Text>
          </View>
        </If>
        <If when={!hero()}>
          <MessageList {...props} items={items()} />
          <Composer {...props} />
          <StatusBar {...props} />
        </If>
        <If when={hero()}><EmptyWorkspace {...props} /></If>
      </If>
    </View>
  );
}
