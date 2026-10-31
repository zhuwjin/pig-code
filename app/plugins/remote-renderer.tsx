import { Scroll, Text, View, type JSX } from "nsh";
import { Button, Input } from "nsh/base";
import type { PluginPackageInfo, RemoteDocument, RemoteNode } from "../protocol";
import { pluginInstance, uiContributions, type PluginRuntimeStates } from "./ui-coordinator";
import type { PluginUiContribution } from "../protocol";

export type PluginEventHandler = (instance: string, node: string, kind: string, text?: string) => void;

function visualStyle(node: RemoteNode): Record<string, any> {
  const allowed = ["backgroundColor", "borderRadius", "opacity", "display", "color", "fontSize", "fontWeight", "lineHeight"];
  return Object.fromEntries(allowed.filter((key) => key in node.style).map((key) => [key, node.style[key]]));
}

function renderNode(
  document: RemoteDocument,
  node: RemoteNode,
  onEvent: PluginEventHandler,
  visited: Set<string>,
  hostChildren?: JSX.Element,
): JSX.Element {
  if (visited.has(node.id)) return <Text>远程 UI 存在循环引用</Text>;
  const nextVisited = new Set(visited);
  nextVisited.add(node.id);
  const children = node.children.map((id) => {
    const child = document.nodes.find((candidate) => candidate.id === id);
    return child ? <>{renderNode(document, child, onEvent, nextVisited)}</> : null;
  });
  if (hostChildren) children.push(hostChildren);
  const style = visualStyle(node);
  const testID = node.testId ?? undefined;
  if (node.kind === "text") return <Text testID={testID} style={style}>{node.text ?? ""}</Text>;
  if (node.kind === "scroll") return <Scroll testID={testID} class="min-h-0 flex-1" style={style}>{children}</Scroll>;
  if (node.kind === "input") {
    return (
      <Input
        testID={testID}
        value={node.text ?? ""}
        onChange={(text) => onEvent(document.instance, node.id, "changeText", text)}
        submitBehavior="submit"
        style={style}
      />
    );
  }
  if (node.press) {
    return (
      <Button testID={testID} class="rounded-(--radius) px-3 py-2 hover:bg-accent" style={style} onPress={() => onEvent(document.instance, node.id, "press")}>
        {children}
      </Button>
    );
  }
  return <View testID={testID} class="gap-2" style={style}>{children}</View>;
}

function exportedNode(document: RemoteDocument, exportName?: string): RemoteNode | null {
  if (exportName) {
    const found = document.nodes.find((node) => node.exportName === exportName);
    if (found) return found;
  }
  const root = document.roots[0];
  return document.nodes.find((node) => node.id === root) ?? document.nodes[0] ?? null;
}

export function RemoteContribution(props: {
  instance: string;
  document: RemoteDocument | null;
  exportName?: string;
  hostChildren?: JSX.Element;
  onEvent: PluginEventHandler;
}) {
  return (
    <If when={props.document} else={<Text class="text-xs text-muted-foreground">插件输出尚未就绪</Text>}>
      {(document) => {
        const node = exportedNode(document(), props.exportName);
        if (!node) return <Text class="text-xs text-muted-foreground">插件没有提供此界面</Text>;
        return renderNode(document(), node, props.onEvent, new Set(), props.hostChildren);
      }}
    </If>
  );
}

export function PluginPages(props: {
  target: string;
  packages: PluginPackageInfo[];
  documents: Record<string, RemoteDocument>;
  runtime: PluginRuntimeStates;
  dynamic: Record<string, PluginUiContribution[]>;
  onEvent: PluginEventHandler;
}) {
  const pages = () => uiContributions(props.packages, props.target, ["page", "panel"], props.runtime, props.dynamic);
  return (
    <Each of={pages()} key={({ pkg, contribution }) => `${pluginInstance(pkg)}:${contribution.id}`}>
      {(entry) => {
        const { pkg, contribution } = entry();
        return (
          <View testID={`plugin-page-${pkg.id}-${contribution.id}`} class="gap-2 rounded-xl border border-border bg-card p-3">
            <Text class="text-sm font-semibold text-foreground">{pkg.name}</Text>
            <RemoteContribution
              instance={pluginInstance(pkg)}
              document={props.documents[pluginInstance(pkg)] ?? null}
              exportName={contribution.export}
              onEvent={props.onEvent}
            />
          </View>
        );
      }}
    </Each>
  );
}
