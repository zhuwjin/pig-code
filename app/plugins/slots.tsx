import { computed, Text, token, View, type JSX } from "nsh";
import { Button, Dialog, Popover } from "nsh/base";
import { appLocale } from "../i18n";
import type {
  PluginHostContext,
  PluginUiChoice,
  PluginUiContribution,
  PluginUiSlot,
  PluginUiState,
} from "../protocol";
import type { UiContext } from "../ui-context";
import { Icon, isIconName } from "../components/icon";
import { uiContributions, type PluginUiEntry } from "./ui-coordinator";
import { PluginSurface } from "./surface";

function localized(values: Record<string, string>): string {
  return values[appLocale()] ?? values.en ?? Object.values(values)[0] ?? "";
}

function stateFor(entry: PluginUiEntry, store: UiContext["store"]): PluginUiState {
  return store.pluginUiStates()[entry.instance]?.[entry.contribution.id] ?? {
    id: entry.contribution.id,
    visible: true,
    enabled: true,
    loading: false,
    value: null,
    details: [],
    segments: [],
  };
}

function label(contribution: PluginUiContribution, state: PluginUiState): string {
  return state.label ?? localized(contribution.label);
}

function displayValue(state: PluginUiState): string {
  if (typeof state.value === "string") return state.value;
  if (typeof state.value === "number" || typeof state.value === "boolean") return String(state.value);
  if (state.value && typeof state.value === "object") {
    const percent = (state.value as { percent?: unknown }).percent;
    if (typeof percent === "number") return `${Math.round(percent)}%`;
  }
  return "";
}

function sameContext(left: PluginHostContext, right: PluginHostContext): boolean {
  return (left.sessionId ?? null) === (right.sessionId ?? null) &&
    (left.turnId ?? null) === (right.turnId ?? null);
}

function openFor(entry: PluginUiEntry, context: PluginHostContext, store: UiContext["store"]): boolean {
  const current = store.pluginOverlay();
  return current?.instance === entry.instance && current.contribution === entry.contribution.id &&
    sameContext(current.context, context);
}

function publishOpen(
  entry: PluginUiEntry,
  context: PluginHostContext,
  store: UiContext["store"],
  open: boolean,
): void {
  store.setPluginOverlay(open ? { instance: entry.instance, contribution: entry.contribution.id, context } : null);
  if (entry.pkg.backend) {
    store.dispatchPluginUiEvent(entry.instance, entry.contribution.id, "change", open, context);
  }
}

function toneClass(tone: PluginUiState["segments"][number]["tone"]): string {
  if (tone === "accent") return "text-xs text-primary";
  if (tone === "success") return "text-xs text-success";
  if (tone === "warning") return "text-xs text-warning";
  if (tone === "danger") return "text-xs text-destructive";
  return "text-xs text-muted-foreground";
}

function Trigger(props: {
  entry: PluginUiEntry;
  state: PluginUiState;
  onPress?: () => void;
}) {
  const { contribution } = props.entry;
  const icon = props.state.icon ?? contribution.icon;
  return (
    <Button
      testID={`plugin-${contribution.slot}-${props.entry.pkg.id}-${contribution.id}`}
      accessibilityLabel={label(contribution, props.state)}
      disabled={!props.state.enabled || props.state.loading}
      selected={props.state.value === true}
      class="flex-row items-center gap-1 rounded-full px-2 py-0.5 hover:bg-accent active:bg-accent/70 disabled:opacity-40"
      onPress={props.onPress}
    >
      {icon && isIconName(icon) ? <Icon name={icon} size={13} color={token("muted-foreground")} /> : null}
      {props.state.segments.length === 0 ? (
        <Text class="text-xs text-muted-foreground">{displayValue(props.state) || label(contribution, props.state)}</Text>
      ) : (
        <Each of={props.state.segments} key={(segment) => segment.label}>
          {(segment) => <Text class={toneClass(segment().tone)}>{segment().label}</Text>}
        </Each>
      )}
    </Button>
  );
}

function Details(props: { state: PluginUiState }) {
  return (
    <View class="w-64 gap-2 p-1">
      <Each of={props.state.details} key={(detail) => `${detail.label}:${detail.value}`}>
        {(detail) => (
          <View class="flex-row items-center justify-between gap-3">
            <Text class="text-xs text-muted-foreground">{detail().label}</Text>
            <Text class="text-xs text-foreground">{detail().value}</Text>
          </View>
        )}
      </Each>
    </View>
  );
}

function Choices(props: {
  entry: PluginUiEntry;
  state: PluginUiState;
  context: PluginHostContext;
  store: UiContext["store"];
}) {
  const close = () => props.store.pluginOverlay.set(null);
  return (
    <View class="w-56 gap-0.5 p-1">
      <Each of={props.state.choices ?? props.entry.contribution.choices} key={(choice) => choice.id}>
        {(choice: () => PluginUiChoice) => (
          <Button
            testID={`plugin-choice-${props.entry.contribution.id}-${choice().id}`}
            disabled={!choice().enabled}
            class="flex-row items-center gap-2 rounded-(--radius) px-2 py-1.5 hover:bg-accent disabled:opacity-40"
            onPress={() => {
              props.store.dispatchPluginUiEvent(
                props.entry.instance,
                props.entry.contribution.id,
                "change",
                choice().id,
                props.context,
              );
              close();
            }}
          >
            <Text class="text-xs text-foreground">{localized(choice().label)}</Text>
          </Button>
        )}
      </Each>
    </View>
  );
}

type ControlProps = {
  entry: PluginUiEntry;
  context: PluginHostContext;
  store: UiContext["store"];
  themeStore: UiContext["themeStore"];
};

function Control(props: ControlProps): JSX.Element {
  return (
    <If when={stateFor(props.entry, props.store).visible}>
      <ControlContent {...props} />
    </If>
  );
}

function ControlContent(props: ControlProps): JSX.Element {
  const entry = props.entry;
  const { contribution } = entry;
  const store = props.store;
  const context = () => props.context;
  const state = () => stateFor(entry, store);
  const revision = computed(() => store.pluginRuntime()[entry.instance]?.generation ?? 1);
  const surface = () => (
    <PluginSurface
      agent={store.agent}
      instance={entry.instance}
      contribution={contribution.id}
      revision={revision()}
      context={context()}
      theme={props.themeStore.serialized()}
      locale={appLocale()}
      appearance={props.themeStore.mode()}
      testID={`plugin-surface-${entry.pkg.id}-${contribution.id}`}
      width={contribution.width ?? 320}
      height={contribution.height ?? 240}
      onReady={(id) => store.attachPluginOverlaySurface(entry.instance, contribution.id, id)}
      onDispose={(id) => store.detachPluginOverlaySurface(entry.instance, contribution.id, id)}
    />
  );
  if (contribution.kind === "button") {
    return <Trigger entry={entry} state={state()} onPress={() => store.dispatchPluginUiEvent(entry.instance, contribution.id, "invoke", null, context())} />;
  }
  if (contribution.kind === "toggle") {
    return <Trigger entry={entry} state={state()} onPress={() => store.dispatchPluginUiEvent(entry.instance, contribution.id, "change", state().value !== true, context())} />;
  }
  const trigger = <Trigger entry={entry} state={state()} />;
  if (contribution.kind === "metric" || contribution.kind === "indicator") {
    return (
      <If when={state().details.length > 0} else={trigger}>
        <Popover
          open={openFor(entry, context(), store)}
          onOpenChange={({ open }) => store.setPluginOverlay(open ? { instance: entry.instance, contribution: contribution.id, context: context() } : null)}
          placement="top"
          popupClass="rounded-xl border border-border bg-popover p-2 shadow-xl"
          trigger={trigger}
        >
          <Details state={state()} />
        </Popover>
      </If>
    );
  }
  if (contribution.kind === "select" || contribution.kind === "menu") {
    return (
      <Popover
        open={openFor(entry, context(), store)}
        onOpenChange={({ open }) => store.setPluginOverlay(open ? { instance: entry.instance, contribution: contribution.id, context: context() } : null)}
        placement="top"
        popupClass="rounded-xl border border-border bg-popover p-1 shadow-xl"
        trigger={trigger}
      >
        <Choices entry={entry} state={state()} context={context()} store={store} />
      </Popover>
    );
  }
  if (contribution.kind === "dialog") {
    return (
      <>
        <Trigger entry={entry} state={state()} onPress={() => publishOpen(entry, context(), store, true)} />
        <Dialog
          open={openFor(entry, context(), store)}
          onOpenChange={({ open }) => publishOpen(entry, context(), store, open)}
          backdropClass="bg-black/40"
        >
          <View class="rounded-xl border border-border bg-popover shadow-xl">{surface()}</View>
        </Dialog>
      </>
    );
  }
  return (
    <Popover
      open={openFor(entry, context(), store)}
      onOpenChange={({ open }) => publishOpen(entry, context(), store, open)}
      placement="top"
      popupClass="rounded-xl border border-border bg-popover p-0 shadow-xl"
      trigger={trigger}
    >
      {surface()}
    </Popover>
  );
}

export function PluginSlot(props: UiContext & {
  slot: PluginUiSlot;
  context?: PluginHostContext;
  class?: string;
}): JSX.Element {
  return (
    <View class={props.class ?? "flex-row items-center gap-1"}>
      <Each
        of={uiContributions(
          props.store.pluginPackages(),
          props.slot,
          props.store.pluginRuntime(),
          props.store.pluginOrder(),
        )}
        key={(entry) => `${entry.instance}:${entry.contribution.id}`}
      >
        {(entry) => (
          <Control
            entry={entry()}
            context={props.context ?? {}}
            store={props.store}
            themeStore={props.themeStore}
          />
        )}
      </Each>
    </View>
  );
}
