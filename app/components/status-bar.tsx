import { View } from "nsh";
import type { UiContext } from "../ui-context";
import { uiContributions } from "../plugins/ui-coordinator";
import { PluginSurfaceSlot } from "../plugins/surface";

export function StatusBar(props: UiContext) {
  const { store } = props;
  const mounted = () => ["status.activity", "status.usage", "status.context"].some((target) =>
    uiContributions(
      store.pluginPackages(),
      target,
      ["surface"],
      store.pluginRuntime(),
      store.pluginDynamic(),
      store.pluginOrder(),
    ).length > 0
  );
  const mode = (key: "activity" | "usage" | "context") => store.config()?.status_bar?.[key] ?? "detailed";
  return (
    <If when={mounted()}>
      <View class="absolute inset-x-0 bottom-16 z-20 mx-auto flex min-h-8 w-full flex-row flex-wrap items-center justify-center gap-2 px-4 pt-1" style={{ maxWidth: 860 }}>
        <PluginSurfaceSlot
          {...props}
          target="status.activity"
          detail="stats-activity-detail"
          sessionId={store.current()}
          mode={mode("activity")}
          testID="stats-summary-SessionActivity"
          width={220}
          height={24}
        />
        <PluginSurfaceSlot
          {...props}
          target="status.usage"
          detail="stats-usage-detail"
          sessionId={store.current()}
          mode={mode("usage")}
          testID="stats-summary-SessionUsage"
          width={240}
          height={24}
        />
        <PluginSurfaceSlot
          {...props}
          target="status.context"
          detail="stats-context-detail"
          sessionId={store.current()}
          mode={mode("context")}
          testID="stats-summary-SessionContext"
          width={100}
          height={24}
        />
      </View>
    </If>
  );
}
