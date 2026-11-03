import { View } from "nsh";
import type { UiContext } from "../ui-context";
import { UiNodeFactory } from "../plugins/ui-node-factory";
import { uiContributions } from "../plugins/ui-coordinator";

export function StatusBar(props: UiContext) {
  const { store } = props;
  const mounted = () => ["status.activity", "status.usage", "status.context"].some((target) =>
    uiContributions(
      store.pluginPackages(),
      target,
      undefined,
      store.pluginRuntime(),
      store.pluginDynamic(),
      store.pluginOrder(),
    ).length > 0
  );
  const context = (mode: "activity" | "usage" | "context") => () => ({
    sessionId: store.current(),
    mode: store.config()?.status_bar?.[mode] ?? "detailed",
  });
  return (
    <If when={mounted()}>
      <View class="mx-auto flex min-h-8 w-full flex-row flex-wrap items-center justify-center gap-2 px-4 pt-1" style={{ maxWidth: 860 }}>
        <UiNodeFactory uiId="status.activity" store={store} context={context("activity")} exportName={() => "SessionActivity"} render={() => <View />} />
        <UiNodeFactory uiId="status.usage" store={store} context={context("usage")} exportName={() => "SessionUsage"} render={() => <View />} />
        <UiNodeFactory uiId="status.context" store={store} context={context("context")} exportName={() => "SessionContext"} render={() => <View />} />
      </View>
    </If>
  );
}
