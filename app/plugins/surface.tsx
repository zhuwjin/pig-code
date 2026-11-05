import { effect, nativeComponent, platform, signal, Text, View, type JSX, type NativeComponentRef } from "nsh";
import type { Agent } from "pig-code-ts/native";
import { t } from "../i18n";
import { errorMessage, type PluginHostContext } from "../protocol";

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
  sync: Record<string, never>;
};

const NativeSurface = nativeComponent<NativeSurfaceProps, NativeSurfaceEvents>("pig.pluginSurface", {
  props: ["surface", "theme", "locale", "appearance", "width", "height"],
  events: ["ready", "error", "sync"],
});

export function PluginSurface(props: {
  agent: Agent;
  instance: string;
  contribution: string;
  revision: number;
  context: PluginHostContext;
  theme: string;
  locale: string;
  appearance: SurfaceAppearance;
  testID: string;
  width?: number;
  height?: number;
  onReady?: (surface: string) => void;
  onDispose?: (surface: string) => void;
}): JSX.Element {
  const registration = signal<SurfaceRegistration | null>(null);
  const ready = signal(false);
  const error = signal("");
  let native: NativeComponentRef | undefined;
  effect(() => {
    const instance = props.instance;
    const contribution = props.contribution;
    props.revision;
    const context = { ...props.context };
    let mounted: SurfaceRegistration | null = null;
    let disposed = false;
    registration.set(null);
    ready.set(false);
    error.set("");
    try {
      const next = JSON.parse(props.agent.mountPluginSurface(
        instance,
        contribution,
        JSON.stringify(context),
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
      if (mounted) {
        props.onDispose?.(mounted.surface);
        props.agent.unmountPluginSurface(mounted.surface);
      }
    };
  });
  const supported = () => platform.os === "windows" || platform.os === "macos";
  return (
    <View
      testID={props.testID}
      class={props.width === undefined && props.height === undefined ? "min-h-0 min-w-0 flex-1" : "min-h-0 min-w-0"}
      style={{ width: props.width, height: props.height }}
    >
      {error() ? <Text role="alert">{error()}</Text> : registration() && supported() ? (
        <NativeSurface
          ref={(node) => native = node}
          surface={registration()!.surface}
          theme={props.theme}
          locale={props.locale}
          appearance={props.appearance}
          width={props.width}
          height={props.height}
          class="min-h-0 min-w-0 flex-1"
          onReady={(event) => {
            if (event.surface !== registration()?.surface) return;
            ready.set(true);
            props.onReady?.(event.surface);
          }}
          onError={(event) => error.set(event.message)}
          onSync={() => native?.command("sync", null)}
        />
      ) : registration() ? (
        <Text role="alert">{t("plugins.surface.unsupported")}</Text>
      ) : null}
      <If when={registration() && !ready() && !error() && supported()}>
        <Text testID={`${props.testID}-loading`} class="self-start text-xs text-muted-foreground">
          {t("plugins.surface.loading")}
        </Text>
      </If>
    </View>
  );
}
