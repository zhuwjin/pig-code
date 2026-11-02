import { Image, Scroll, Text, View, type AccessibilityRole, type JSX } from "nsh";
import { Input } from "nsh/base";
import { t } from "../i18n";
import type {
  PluginPackageInfo,
  PluginUiContribution,
  PluginUiEvent,
  RemoteDocument,
  RemoteNode,
} from "../protocol";
import { emitPluginEvent, nextEditCount } from "./event-adapter";
import { pluginInstance, uiContributions, type PluginRuntimeStates } from "./ui-coordinator";
import { remoteAccessibility, remoteProps, remoteStyle, wants } from "./ui-protocol";

export type PluginEventHandler = (instance: string, event: PluginUiEvent) => void;

interface RenderContext {
  document: RemoteDocument;
  nodes: Map<string, RemoteNode>;
  onEvent: PluginEventHandler;
  packages: PluginPackageInfo[];
  contribution?: string;
}

function emit(context: RenderContext, node: RemoteNode, kind: string, payload: unknown = {}): void {
  emitPluginEvent((event) => context.onEvent(context.document.instance, event), {
    instance: context.document.instance,
    contribution: context.contribution,
    node,
    kind,
    payload,
    surfaceRevision: context.document.sequence,
  });
}

function eventProps(context: RenderContext, node: RemoteNode): Record<string, unknown> {
  const props: Record<string, unknown> = {};
  if (wants(node, "press")) props.onPress = (event: unknown) => emit(context, node, "press", event);
  if (wants(node, "longPress")) props.onLongPress = (event: unknown) => emit(context, node, "longPress", event);
  if (wants(node, "pressDown")) props.onPressDown = (event: unknown) => emit(context, node, "pressDown", event);
  if (wants(node, "pressUp")) props.onPressUp = (event: unknown) => emit(context, node, "pressUp", event);
  if (wants(node, "focus")) props.onFocus = () => emit(context, node, "focus");
  if (wants(node, "blur")) props.onBlur = () => emit(context, node, "blur");
  if (wants(node, "hoverIn")) props.onHoverIn = () => emit(context, node, "hoverIn");
  if (wants(node, "hoverOut")) props.onHoverOut = () => emit(context, node, "hoverOut");
  if (wants(node, "hoverMove")) props.onHoverMove = (event: unknown) => emit(context, node, "hoverMove", event);
  if (wants(node, "mouseMove")) props.onMouseMove = (event: unknown) => emit(context, node, "mouseMove", event);
  if (wants(node, "layout")) props.onLayout = (event: unknown) => emit(context, node, "layout", event);
  if (wants(node, "accessibilityAction")) {
    props.onAccessibilityAction = (event: unknown) => emit(context, node, "accessibilityAction", event);
  }
  return props;
}

function resource(context: RenderContext, source: string | null): string | null {
  if (!source) return null;
  if (source.startsWith("data:image/")) return source;
  if (!source.startsWith("plugin://")) return null;
  const pkg = context.packages.find((pkg) => pluginInstance(pkg) === context.document.instance);
  if (!pkg) return null;
  const prefix = `plugin://${pkg.id}/${pkg.digest}/`;
  if (!source.startsWith(prefix)) return null;
  const path = source.slice(prefix.length);
  if (!path || path.split("/").some((part) => !part || part === "." || part === "..")) return null;
  const root = pkg.root.replace(/[\\/]+$/, "");
  return `file://${root}/${path.split("/").map(encodeURIComponent).join("/")}`;
}

function renderNode(
  context: RenderContext,
  node: RemoteNode,
  visited: ReadonlySet<string>,
  hostChildren?: JSX.Element,
): JSX.Element {
  if (visited.has(node.id)) return <Text role="alert">{t("plugins.remote.cycle")}</Text>;
  const nextVisited = new Set(visited);
  nextVisited.add(node.id);
  const children: JSX.Element[] = node.children.map((id) => {
    const child = context.nodes.get(id);
    return child ? <>{renderNode(context, child, nextVisited)}</> : null;
  });
  if (hostChildren) children.push(hostChildren);
  const style = remoteStyle(node);
  const props = remoteProps(node);
  const accessibility = remoteAccessibility(node);
  const events = eventProps(context, node);
  const testID = node.testId ?? undefined;
  if (node.kind === "text") {
    return <Text testID={testID} style={style} {...props} {...accessibility} {...events}>{node.text ?? ""}</Text>;
  }
  if (node.kind === "scroll") {
    const scrollEvents: Record<string, unknown> = {};
    if (wants(node, "scroll")) scrollEvents.onScroll = (event: unknown) => emit(context, node, "scroll", event);
    if (wants(node, "scrollEnd")) scrollEvents.onScrollEnd = (event: unknown) => emit(context, node, "scrollEnd", event);
    for (const kind of ["scrollDragStart", "scrollDragEnd", "momentumStart", "momentumEnd"] as const) {
      if (wants(node, kind)) scrollEvents[`on${kind[0].toUpperCase()}${kind.slice(1)}`] = (event: unknown) => emit(context, node, kind, event);
    }
    return <Scroll testID={testID} class="min-h-0 flex-1" style={style} {...props} {...accessibility} {...events} {...scrollEvents}>{children}</Scroll>;
  }
  if (node.kind === "input") {
    const inputEvents: Record<string, unknown> = { ...events };
    if (wants(node, "changeText")) inputEvents.onChange = (text: string) => emit(context, node, "changeText", { text, count: nextEditCount(context.document.instance, node.id) });
    if (wants(node, "submit")) inputEvents.onSubmit = (event: { text: string }) => emit(context, node, "submit", { text: event.text, count: nextEditCount(context.document.instance, node.id) });
    if (wants(node, "selectionChange")) inputEvents.onSelectionChange = (event: unknown) => emit(context, node, "selectionChange", event);
    if (wants(node, "keyPress")) inputEvents.onKeyPress = (event: unknown) => emit(context, node, "keyPress", event);
    if (wants(node, "compositionChange")) inputEvents.onCompositionChange = (event: unknown) => emit(context, node, "compositionChange", event);
    if (wants(node, "contentSizeChange")) inputEvents.onContentSizeChange = (event: unknown) => emit(context, node, "contentSizeChange", event);
    if (wants(node, "caretMove")) inputEvents.onCaretMove = (event: unknown) => emit(context, node, "caretMove", event);
    return (
      <Input
        testID={testID}
        class="selection:bg-foreground"
        value={node.text ?? ""}
        submitBehavior="submit"
        style={style}
        {...props}
        {...accessibility}
        {...inputEvents}
      />
    );
  }
  if (node.kind === "image") {
    const source = resource(context, node.source);
    if (!source) return <Text role="alert">{t("plugins.remote.unresolved_resource", { source: node.source ?? t("common.not_provided") })}</Text>;
    return <Image testID={testID} src={source} style={style} {...props} {...accessibility} {...events} />;
  }
  return (
    <View
      testID={testID}
      class="gap-2"
      style={style}
      {...props}
      {...accessibility}
      {...events}
      role={node.press ? "button" : (accessibility.role as AccessibilityRole | undefined)}
    >
      {children}
    </View>
  );
}

function exportedNode(document: RemoteDocument, exportName?: string): RemoteNode | null | undefined {
  if (exportName) return document.nodes.find((node) => node.exportName === exportName) ?? undefined;
  const root = document.roots[0];
  return document.nodes.find((node) => node.id === root) ?? document.nodes[0] ?? null;
}

type RemoteContributionProps = {
  instance: string;
  document: RemoteDocument | null;
  exportName?: string;
  hostChildren?: JSX.Element;
  packages?: PluginPackageInfo[];
  contribution?: string;
  onEvent: PluginEventHandler;
};

export function RemoteContribution(props: RemoteContributionProps) {
  return <>{renderRemoteContribution(props)}</>;
}

function renderRemoteContribution(props: RemoteContributionProps): JSX.Element {
  const value = props.document;
  if (!value) return <Text class="text-xs text-muted-foreground">{t("plugins.remote.not_ready")}</Text>;
  const node = exportedNode(value, props.exportName);
  if (node === undefined) return <Text role="alert">{t("plugins.remote.missing_export", { name: props.exportName ?? "" })}</Text>;
  if (!node) return <Text class="text-xs text-muted-foreground">{t("plugins.remote.missing_view")}</Text>;
  const context: RenderContext = {
    document: value,
    nodes: new Map(value.nodes.map((node) => [node.id, node])),
    onEvent: props.onEvent,
    packages: props.packages ?? [],
    contribution: props.contribution,
  };
  return (
    <>
      {value.warnings.length > 0 ? <Text role="alert">{value.warnings.join("\n")}</Text> : null}
      {renderNode(context, node, new Set(), props.hostChildren)}
    </>
  );
}

export function PluginPages(props: {
  target: string;
  packages: PluginPackageInfo[];
  documents: Record<string, RemoteDocument>;
  runtime: PluginRuntimeStates;
  dynamic: Record<string, PluginUiContribution[]>;
  pluginOrder?: readonly string[];
  onEvent: PluginEventHandler;
}) {
  const pages = () => uiContributions(
    props.packages,
    props.target,
    ["page", "panel", "route", "settings", "overlay", "decoration"],
    props.runtime,
    props.dynamic,
    props.pluginOrder ?? [],
  );
  return (
    <Each of={pages()} key={(entry) => `${entry.instance}:${entry.contribution.id}`}>
      {(entry) => {
        const { pkg, contribution, instance } = entry();
        return (
          <View testID={`plugin-page-${pkg.id}-${contribution.id}`} class="gap-2 rounded-xl border border-border bg-card p-3">
            <Text class="text-sm font-semibold text-foreground">{pkg.name}</Text>
            <RemoteContribution
              instance={instance}
              document={props.documents[instance] ?? null}
              exportName={contribution.export ?? undefined}
              packages={props.packages}
              contribution={contribution.id}
              onEvent={props.onEvent}
            />
          </View>
        );
      }}
    </Each>
  );
}
