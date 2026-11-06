import { cleanup, computed, effect, ErrorBoundary, nativeComponent, platform, signal, Text, untracked, View, type Component, type JSX } from "nsh";
import { t } from "../i18n";
import { errorMessage } from "../protocol";
import type { PluginSurfaceProps } from "./surface";

type ComponentRegistration = {
  instance: string;
  surface: string;
  contribution: string;
  revision: number;
  packageRoot: string;
};

const NativeComponent = nativeComponent<{ surface: string }, {
  ready: { surface: string; chunkId: string };
  error: { surface: string; message: string };
}>("pig.pluginComponent", {
  props: ["surface"],
  events: ["ready", "error"],
});

const importChunk = (id: string): Promise<Record<string, any>> =>
  (globalThis as any).__nsh_import(id, []);

export function PluginComponent(props: PluginSurfaceProps): JSX.Element {
  const registration = signal<ComponentRegistration | null>(null);
  const component = signal<Component | null>(null);
  const ready = signal(false);
  const error = signal("");
  const mountKey = computed(() => JSON.stringify([
    props.instance,
    props.contribution,
    props.exportName,
    props.revision,
    props.sessionId ?? null,
    props.turnId ?? null,
  ]));
  let mounted: ComponentRegistration | null = null;
  let mountedKey = "";
  let disposed = false;
  const unmount = () => {
    if (!mounted) return;
    const previous = mounted;
    mounted = null;
    mountedKey = "";
    props.onDispose?.(previous.surface);
    props.agent.unmountPluginSurface(previous.surface);
  };
  effect(() => {
    const nextKey = mountKey();
    untracked(() => {
      if (disposed || mountedKey === nextKey) return;
      unmount();
      mountedKey = nextKey;
      registration.set(null);
      component.set(null);
      ready.set(false);
      error.set("");
      const [instance, contribution, , , sessionId, turnId] = JSON.parse(nextKey) as [
        string,
        string,
        string,
        number,
        string | null,
        string | null,
      ];
      try {
        const next = JSON.parse(props.agent.mountPluginSurface(
          instance,
          contribution,
          JSON.stringify({ sessionId, turnId }),
        )) as ComponentRegistration;
        if (disposed) {
          props.agent.unmountPluginSurface(next.surface);
        } else {
          mounted = next;
          registration.set(next);
        }
      } catch (cause) {
        error.set(errorMessage(cause));
      }
    });
  });
  cleanup(() => {
    disposed = true;
    unmount();
  });
  const supported = platform.os === "windows" || platform.os === "macos" || platform.os === "linux";
  return (
    <View
      testID={props.testID}
      class={props.width === undefined && props.height === undefined ? "min-h-0 min-w-0 flex-1" : "min-h-0 min-w-0"}
      style={{ width: props.width, height: props.height }}
    >
      {error() ? <Text role="alert">{error()}</Text> : registration() && supported ? (
        <NativeComponent
          surface={registration()!.surface}
          onReady={(event) => {
            if (event.surface !== registration()?.surface) return;
            void importChunk(event.chunkId).then((module) => {
              if (disposed || registration()?.surface !== event.surface) return;
              const exported = props.exportName ? module[props.exportName] : module.default;
              if (typeof exported !== "function") {
                error.set(t("plugins.surface.component_unavailable"));
                return;
              }
              component.set(exported as Component);
              ready.set(true);
              props.onReady?.(event.surface);
            }).catch((cause: unknown) => {
              if (!disposed && registration()?.surface === event.surface) error.set(errorMessage(cause));
            });
          }}
          onError={(event) => {
            if (event.surface === registration()?.surface) error.set(event.message);
          }}
        />
      ) : registration() ? (
        <Text role="alert">{t("plugins.surface.unsupported")}</Text>
      ) : null}
      <ErrorBoundary fallback={(cause) => <Text role="alert">{errorMessage(cause)}</Text>}>
        {() => {
          const Exported = component();
          return Exported ? Exported({ sessionId: props.sessionId, turnId: props.turnId }) : null;
        }}
      </ErrorBoundary>
      <If when={registration() && !ready() && !error() && supported}>
        <Text testID={`${props.testID}-loading`} class="self-start text-xs text-muted-foreground">
          {t("plugins.surface.loading")}
        </Text>
      </If>
    </View>
  );
}
