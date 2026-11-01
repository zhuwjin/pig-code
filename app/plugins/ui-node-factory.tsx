import { computed, Text, View, token, type JSX } from "nsh";
import type { AgentStore } from "../store";
import type { PluginUiContribution } from "../protocol";
import { Icon, type IconName } from "../components/icon";
import { BaseUiRegistry } from "./base-ui-registry";
import { createPluginHostEvent } from "./event-adapter";
import { RemoteContribution } from "./remote-renderer";
import { uiPlan, type PluginUiEntry, type PluginUiPlan } from "./ui-coordinator";
import { REMOTE_ACCESSIBILITY_KEYS, REMOTE_PROP_KEYS, safeHostStyle } from "./ui-protocol";

export interface HostPatch {
  style: Record<string, unknown>;
  props: Record<string, unknown>;
  accessibility: Record<string, unknown>;
  events: Record<string, (...args: any[]) => void>;
  preserve: ReadonlySet<string>;
}

type UiNodeFactoryProps = {
  uiId: string;
  store: AgentStore;
  children?: JSX.Element;
  captureFocus?: () => boolean;
  restoreFocus?: () => void;
  render: (patch: HostPatch, children?: JSX.Element) => JSX.Element;
};

const HOST_EVENTS: Record<string, string> = {
  onPress: "press",
  onChange: "changeText",
  onFocus: "focus",
  onBlur: "blur",
  onKeyPress: "keyPress",
};

function createHostPatch(props: UiNodeFactoryProps, plan: PluginUiPlan): HostPatch {
  const descriptor = BaseUiRegistry.require(props.uiId);
  const patches = plan.entries.filter((entry) => entry.contribution.kind === "patch");
  const patch: HostPatch = {
    style: Object.assign({}, ...patches.map((entry) => safeHostStyle(entry.contribution.style, descriptor.allowedStyles))),
    props: {},
    accessibility: {},
    events: {},
    preserve: new Set(patches.flatMap((entry) => entry.contribution.preserve)),
  };
  for (const entry of patches) {
    const { contribution, instance } = entry;
    for (const [key, value] of Object.entries(contribution.props ?? {})) {
      if (HOST_EVENTS[key] && value === true) {
        patch.events[key] = (...args: any[]) => {
          const kind = HOST_EVENTS[key];
          const payload = kind === "changeText"
            ? { text: typeof args[0] === "string" ? args[0] : (args[0]?.text ?? "") }
            : kind === "keyPress"
              ? { key: args[0]?.key ?? "" }
              : {};
          props.store.dispatchPluginHostEvent(instance, createPluginHostEvent({
            instance,
            contribution: contribution.id,
            node: props.uiId,
            kind,
            payload,
            surfaceRevision: BaseUiRegistry.revision(),
          }));
        };
      } else if (!key.startsWith("on") && REMOTE_PROP_KEYS.has(key) &&
        (key === "disabled" || key === "focusable" || descriptor.preservedBehavior.includes(key))) {
        patch.props[key] = value;
      }
    }
    for (const [key, value] of Object.entries(contribution.accessibility ?? {})) {
      if (REMOTE_ACCESSIBILITY_KEYS.has(key) && descriptor.preservedBehavior.includes(key)) {
        patch.accessibility[key] = value;
      }
    }
  }
  return patch;
}

export function UiNodeFactory(props: UiNodeFactoryProps): JSX.Element {
  BaseUiRegistry.require(props.uiId);
  const plan = computed(() => {
    props.store.pluginDocuments();
    return uiPlan(
      props.store.pluginPackages(),
      props.uiId,
      undefined,
      props.store.pluginRuntime(),
      props.store.pluginDynamic(),
      props.store.pluginOrder(),
    );
  }, {
    equals: (previous, next) => JSON.stringify(previous) === JSON.stringify(next),
  });
  let displaced = false;
  let restoreFocus = false;
  return (
    <Each of={[plan()]} key={(current) => JSON.stringify(current)}>
      {(current) => {
        const nextDisplaced = current().entries.some((entry) =>
          ["remove", "replace", "move"].includes(entry.contribution.kind)
        );
        if (nextDisplaced) {
          if (!displaced) restoreFocus = props.captureFocus?.() ?? false;
        } else if (displaced) {
          if (restoreFocus) props.restoreFocus?.();
          restoreFocus = false;
        }
        displaced = nextDisplaced;
        return <UiNodeRenderer {...props} plan={current()} />;
      }}
    </Each>
  );
}

function UiNodeRenderer(props: UiNodeFactoryProps & { plan: PluginUiPlan }): JSX.Element {
  return renderUiNode(props, props.plan);
}

function renderUiNode(props: UiNodeFactoryProps, plan: PluginUiPlan): JSX.Element {
  props.store.reportPluginUiDiagnostics(props.uiId, plan.conflicts, plan.rejected);
  const patch = createHostPatch(props, plan);
  const structural = plan.entries.find((entry) =>
    ["replaceChildren", "replace", "wrap", "remove", "move"].includes(entry.contribution.kind)
  );
  const renderRemote = (entry: PluginUiEntry, hostChildren?: JSX.Element) => {
    if (entry.contribution.icon) {
      return <Icon testID={entry.contribution.kind === "replaceChildren" ? "plugin-send-icon" : `plugin-${entry.contribution.kind}-icon`} name={entry.contribution.icon as IconName} size={14} color={token("primary-foreground")} />;
    }
    if (!entry.contribution.export) return <Text role="alert">插件贡献 {entry.contribution.id} 没有可渲染内容</Text>;
    return (
      <RemoteContribution
        instance={entry.instance}
        document={props.store.pluginDocuments()[entry.instance] ?? null}
        exportName={entry.contribution.export}
        hostChildren={hostChildren}
        packages={props.store.pluginPackages()}
        contribution={entry.contribution.id}
        onEvent={(instance, event) => props.store.dispatchPluginUiEvent(instance, event)}
      />
    );
  };
  const before = plan.entries.filter((entry) => entry.contribution.kind === "insertBefore");
  const after = plan.entries.filter((entry) => ["insertAfter", "inline", "decoration"].includes(entry.contribution.kind));
  const first = plan.entries.filter((entry) => entry.contribution.kind === "insertFirst");
  const last = plan.entries.filter((entry) => ["insertLast", "create"].includes(entry.contribution.kind));
  const movedIn = plan.entries.filter((entry) => entry.contribution.kind === "move" && entry.contribution.moveTarget === props.uiId);
  const originalChildren = props.children;
  let children = originalChildren;
  if (structural?.contribution.kind === "replaceChildren") {
    children = renderRemote(structural);
  }
  if (first.length > 0 || last.length > 0) {
    children = (
      <>
        {first.map((entry) => renderRemote(entry))}
        {children}
        {last.map((entry) => renderRemote(entry))}
      </>
    );
  }
  const hidden = structural && (structural.contribution.kind === "remove" ||
    (structural.contribution.kind === "move" && structural.contribution.target === props.uiId));
  let content = hidden ? null : structural?.contribution.kind === "replace"
    ? renderRemote(structural)
    : props.render(patch, children);
  if (structural?.contribution.kind === "wrap") content = renderRemote(structural, content);
  const entries = [...before, ...after, ...movedIn];
  return (
    <>
      {before.map((entry) => renderRemote(entry))}
      {content}
      {entries.filter((entry) => !before.includes(entry)).map((entry) => renderRemote(entry))}
    </>
  );
}

export function UiView(props: {
  uiId: string;
  store: AgentStore;
  class?: string;
  style?: Record<string, unknown>;
  testID?: string;
  children?: JSX.Element;
  [key: string]: unknown;
}): JSX.Element {
  const { uiId, store, class: className, style, children, ...hostProps } = props;
  return (
    <UiNodeFactory
      uiId={uiId}
      store={store}
      render={(patch, content) => (
        <View
          {...hostPatchProps(patch, hostProps)}
          class={className}
          style={{ ...(style ?? {}), ...patch.style }}
        >
          {content}
        </View>
      )}
    >
      {children}
    </UiNodeFactory>
  );
}

export function hostPatchProps(
  patch: HostPatch,
  host: Record<string, unknown> = {},
): Record<string, unknown> {
  const result: Record<string, unknown> = {
    style: patch.style,
    ...host,
    ...patch.props,
    ...patch.accessibility,
  };
  for (const [key, handler] of Object.entries(patch.events)) {
    const original = host[key];
    result[key] = (...args: any[]) => {
      if (patch.preserve.has(key) && typeof original === "function") original(...args);
      handler(...args);
    };
  }
  return result;
}

export function contributionPreservesHostBehavior(contribution: PluginUiContribution): boolean {
  return contribution.preserve.includes("onPress") || contribution.preserve.includes("disabled");
}
