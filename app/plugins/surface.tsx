import { Text, View, effect, nativeComponent, platform, signal, type JSX } from "nsh";
import { Popover } from "nsh/base";
import type { Agent } from "pig-code-ts/native";
import { appLocale, t } from "../i18n";
import { errorMessage, type PluginPackageInfo, type PluginUiContribution, type StatusBarItem } from "../protocol";
import type { UiContext } from "../ui-context";
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
  width?: number;
  height?: number;
};

type NativeSurfaceEvents = {
  ready: { surface: string; instance: string; contribution: string; revision: number };
  error: { message: string };
};

const NativeSurface = nativeComponent<NativeSurfaceProps, NativeSurfaceEvents>("pig.pluginSurface", {
  props: ["surface", "theme", "locale", "appearance", "width", "height"],
  events: ["ready", "error"],
});

export function PluginSurface(props: {
  agent: Agent;
  instance: string;
  contribution: string;
  revision: number;
  sessionId: string | null;
  turnId?: string | null;
  turnIndex?: number | null;
  displayMode?: StatusBarItem;
  theme: string;
  locale: string;
  appearance: SurfaceAppearance;
  testID: string;
  showStatus?: boolean;
  width?: number;
  height?: number;
}): JSX.Element {
  const registration = signal<SurfaceRegistration | null>(null);
  const ready = signal(false);
  const error = signal("");
  effect(() => {
    const instance = props.instance;
    const contribution = props.contribution;
    props.revision;
    const sessionId = props.sessionId;
    const turnId = props.turnId ?? null;
    const turnIndex = props.turnIndex ?? null;
    const displayMode = props.displayMode;
    let mounted: SurfaceRegistration | null = null;
    let disposed = false;
    registration.set(null);
    ready.set(false);
    error.set("");
    try {
      const next = JSON.parse(props.agent.mountPluginSurface(
        instance,
        contribution,
        JSON.stringify({ sessionId, turnId, turnIndex, displayMode }),
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
    <View testID={props.testID} class="min-h-0 flex-1">
      <If when={props.showStatus === true && ready()}>
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
          width={props.width}
          height={props.height}
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

export function PluginSurfaceSlot(props: UiContext & {
  target: string;
  sessionId: string | null;
  turnId?: string | null;
  turnIndex?: number | null;
  detail?: string | string[];
  align?: "start" | "center" | "end";
  mode?: StatusBarItem;
  testID?: string;
  width: number;
  height: number;
}): JSX.Element {
  if (props.mode === "hidden") return <View />;
  const { store, themeStore } = props;
  const entries = () => uiContributions(
    store.pluginPackages(),
    props.target,
    ["surface"],
    store.pluginRuntime(),
    store.pluginDynamic(),
    store.pluginOrder(),
  );
  const details = () => Array.isArray(props.detail) ? props.detail : props.detail ? [props.detail] : [];
  const current = () => {
    const overlay = store.pluginOverlay();
    return overlay && overlay.anchor === props.target && overlay.sessionId === props.sessionId &&
      overlay.turnId === (props.turnId ?? null) && overlay.turnIndex === (props.turnIndex ?? null) &&
      details().includes(overlay.contribution)
      ? overlay
      : null;
  };
  return (
    <Each of={entries()} key={(entry) => `${entry.instance}:${entry.contribution.id}:${props.turnId ?? ""}`}>
      {(entry) => {
        const { pkg, instance, contribution } = entry();
        const surfaceTestID = `plugin-surface-${instance}-${contribution.id}`;
        const summary = (
          <View
            testID={props.testID ?? surfaceTestID}
            role={details().length > 0 ? "button" : undefined}
            accessibilityLabel={details().length > 0 ? pkg.name : undefined}
            onPress={details().length > 0 ? () => store.togglePluginOverlay(
              instance,
              details()[0],
              props.target,
              props.turnId ?? null,
              props.turnIndex ?? null,
            ) : undefined}
            class="min-h-0 flex-1"
            style={{ width: props.width, height: props.height }}
          >
            <PluginSurface
              agent={store.agent}
              testID={surfaceTestID}
              instance={instance}
              contribution={contribution.id}
              revision={store.pluginRuntime()[instance]?.generation ?? 1}
              sessionId={props.sessionId}
              turnId={props.turnId}
              turnIndex={props.turnIndex}
              displayMode={props.mode}
              theme={themeStore.serialized()}
              locale={appLocale()}
              appearance={themeStore.mode()}
              width={props.width}
              height={props.height}
            />
          </View>
        );
        return (
          <If when={current()} else={summary}>
            <Popover
              testID={`plugin-overlay-${contribution.id}`}
              open={current() !== null}
              onOpenChange={({ open }) => {
                if (!open && current()) store.pluginOverlay.set(null);
              }}
              placement="top"
              align={props.align ?? "center"}
              popupClass="w-72 rounded-xl border border-border bg-popover p-0 shadow-xl"
              trigger={summary}
            >
              <View testID="plugin-overlay" style={{ width: 288, height: 320 }}>
                <PluginSurface
                  agent={store.agent}
                  testID={`plugin-detail-${current()!.contribution}`}
                  instance={instance}
                  contribution={current()!.contribution}
                  revision={store.pluginRuntime()[instance]?.generation ?? 1}
                  sessionId={props.sessionId}
                  turnId={props.turnId}
                  turnIndex={props.turnIndex}
                  displayMode="detailed"
                  theme={themeStore.serialized()}
                  locale={appLocale()}
                  appearance={themeStore.mode()}
                  width={288}
                  height={320}
                />
              </View>
            </Popover>
          </If>
        );
      }}
    </Each>
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
              showStatus={true}
            />
          </View>
        );
      }}
    </Each>
  );
}
