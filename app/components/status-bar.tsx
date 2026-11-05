import { View } from "nsh";
import type { UiContext } from "../ui-context";
import { uiContributions } from "../plugins/ui-coordinator";
import { PluginSlot } from "../plugins/slots";

export function StatusBar(props: UiContext) {
  const mounted = () => uiContributions(
    props.store.pluginPackages(),
    "status",
    props.store.pluginRuntime(),
    props.store.pluginOrder(),
  ).length > 0;
  return (
    <If when={mounted()}>
      <View class="absolute inset-x-0 bottom-16 z-20 mx-auto flex min-h-8 w-full flex-row flex-wrap items-center justify-center px-4 pt-1" style={{ maxWidth: 860 }}>
        <PluginSlot {...props} slot="status" context={{ sessionId: props.store.current() }} />
      </View>
    </If>
  );
}
