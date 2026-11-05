import { Text, View, effect, nativeComponent, platform, signal, type JSX } from "nsh";
import type { Agent } from "pig-code-ts/native";
import { t } from "../i18n";
import { errorMessage, type PluginPackageInfo, type PluginUiContribution } from "../protocol";
import { uiContributions, type PluginRuntimeStates } from "./ui-coordinator";

type SurfaceAppearance = "system" | "light" | "dark";

type SurfaceRegistration = {
  instance: string;
  surface: string;
  contribution: string;
  revision: number;
  packageRoot: string;
};

type NativeSurfaceProps = {
  surface: string;
  theme: string;
  locale: string;
  appearance: SurfaceAppearance;
};

type NativeSurfaceEvents = {
  ready: { surface: string; instance: string; contribution: string; revision: number };
  error: { message: string };
};

const NativeSurface = nativeComponent<NativeSurfaceProps, NativeSurfaceEvents>("pig.pluginSurface", {
  props: ["surface", "theme", "locale", "appearance"],
  events: ["ready", "error"],
});

export function PluginSurface(props: {
  agent: Agent;
  instance: string;
  contribution: string;
  revision: number;
  sessionId: string | null;
  theme: string;
  locale: string;
  appearance: SurfaceAppearance;
  testID: string;
}): JSX.Element {
  const registration = signal<SurfaceRegistration | null>(null);
  const ready = signal(false);
  const error = signal("");
  effect(() => {
    const instance = props.instance;
    const contribution = props.contribution;
    props.revision;
    const sessionId = props.sessionId;
    let mounted: SurfaceRegistration | null = null;
    let disposed = false;
    registration.set(null);
    ready.set(false);
    error.set("");
    try {
      const next = JSON.parse(props.agent.mountPluginSurface(
        instance,
        contribution,
        JSON.stringify({ sessionId }),
      )) as SurfaceRegistration;
      if (disposed) {
        props.agent.unmountPluginSurface(next.surface);
      } else {
        mounted = next;
        registration.set(next);
      }
    } catch (cause) {
      error.set(errorMessage(cause));
    }
    return () => {
      disposed = true;
      if (mounted) props.agent.unmountPluginSurface(mounted.surface);
    };
  });
  return (
    <View class="min-h-0 flex-1">
      <If when={ready()}>
        <Text testID={`${props.testID}-ready`} class="text-xs text-muted-foreground">
          {t("plugins.surface.ready")}
        </Text>
      </If>
      {error() ? <Text role="alert">{error()}</Text> : registration() && platform.os === "windows" ? (
        <NativeSurface
          surface={registration()!.surface}
          theme={props.theme}
          locale={props.locale}
          appearance={props.appearance}
          class="min-h-0 flex-1"
          onReady={(event) => {
            if (event.surface !== registration()?.surface) return;
            error.set("");
            ready.set(true);
          }}
          onError={(event) => {
            ready.set(false);
            error.set(event.message);
          }}
        />
      ) : null}
    </View>
  );
}

export function PluginSurfacePages(props: {
  target: string;
  packages: PluginPackageInfo[];
  runtime: PluginRuntimeStates;
  dynamic: Record<string, PluginUiContribution[]>;
  pluginOrder?: readonly string[];
  sessionId: string | null;
  theme: string;
  locale: string;
  appearance: SurfaceAppearance;
  agent: Agent;
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
              agent={props.agent}
              testID={testID}
              instance={instance}
              contribution={contribution.id}
              revision={props.runtime[instance]?.generation ?? 1}
              sessionId={props.sessionId}
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
