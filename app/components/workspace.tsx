import { Text, View } from "nsh";
import type { UiContext } from "../ui-context";
import { Composer } from "./composer";
import { EmptyWorkspace } from "./hero";
import { MessageList } from "./messages";
import { PluginPages } from "../plugins/remote-renderer";
import { StatusBar } from "./status-bar";
import { UiView } from "../plugins/ui-node-factory";

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
  return (
    <View class="relative min-h-0 min-w-0 flex-1 bg-background">
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
      <UiView uiId="main.panelHost" store={store} class="absolute bottom-24 right-4 z-30 w-80 gap-2">
        <PluginPages
          target="main"
          packages={store.pluginPackages()}
          documents={store.pluginDocuments()}
          runtime={store.pluginRuntime()}
          dynamic={store.pluginDynamic()}
          onEvent={(instance, event) => store.dispatchPluginUiEvent(instance, event)}
        />
      </UiView>
    </View>
  );
}
