import { Text, View, nativeComponent, signal, type JSX } from "nsh";
import { t } from "../i18n";
import type { PluginPackageInfo, PluginUiContribution } from "../protocol";
import { uiContributions, type PluginRuntimeStates } from "./ui-coordinator";

type SurfaceAppearance = "system" | "light" | "dark";

type NativeSurfaceProps = {
  instance: string;
  contribution: string;
  revision: number;
  packageRoot: string;
  theme: string;
  locale: string;
  appearance: SurfaceAppearance;
};

type NativeSurfaceEvents = {
  ready: { instance: string; contribution: string; revision: number };
  error: { message: string };
};

const NativeSurface = nativeComponent<NativeSurfaceProps, NativeSurfaceEvents>("pig.pluginSurface", {
  props: ["instance", "contribution", "revision", "packageRoot", "theme", "locale", "appearance"],
  events: ["ready", "error"],
});

export function PluginSurface(props: NativeSurfaceProps & { testID: string }): JSX.Element {
  const ready = signal(false);
  const error = signal("");
  return (
    <View class="min-h-0 flex-1">
      <If when={ready()}>
        <Text testID={`${props.testID}-ready`} class="text-xs text-muted-foreground">
          {t("plugins.surface.ready")}
        </Text>
      </If>
      {error() ? <Text role="alert">{error()}</Text> : (
        <NativeSurface
          {...props}
          class="min-h-0 flex-1"
          onReady={() => {
            error.set("");
            ready.set(true);
          }}
          onError={(event) => {
            ready.set(false);
            error.set(event.message);
          }}
        />
      )}
    </View>
  );
}

export function PluginSurfacePages(props: {
  target: string;
  packages: PluginPackageInfo[];
  runtime: PluginRuntimeStates;
  dynamic: Record<string, PluginUiContribution[]>;
  pluginOrder?: readonly string[];
  theme: string;
  locale: string;
  appearance: SurfaceAppearance;
}): JSX.Element {
  const pages = () => uiContributions(
    props.packages,
    props.target,
    ["page", "panel", "route", "settings"],
    props.runtime,
    props.dynamic,
    props.pluginOrder ?? [],
  );
  return (
    <Each of={pages()} key={(entry) => `${entry.instance}:${entry.contribution.id}`}>
      {(entry) => {
        const { pkg, contribution, instance } = entry();
        const testID = `plugin-surface-${pkg.id}-${contribution.id}`;
        return (
          <View
            testID={`plugin-page-${pkg.id}-${contribution.id}`}
            class="shrink-0 gap-2 rounded-xl border border-border bg-card p-3"
            style={{ height: 320, minHeight: 320 }}
          >
            <Text class="text-sm font-semibold text-foreground">{pkg.name}</Text>
            <PluginSurface
              testID={testID}
              instance={instance}
              contribution={contribution.id}
              revision={(props.runtime[instance]?.attempt ?? 0) + 1}
              packageRoot={pkg.root}
              theme={props.theme}
              locale={props.locale}
              appearance={props.appearance}
            />
          </View>
        );
      }}
    </Each>
  );
}
