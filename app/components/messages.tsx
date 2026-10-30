import { Accessor, List, Text, View } from "nsh";
import { DiffView, TextView } from "nsh/base";
import type { ChatItem, MarkdownItem, ToolItem } from "../protocol";
import type { UiContext } from "../ui-context";

function UserRow(props: UiContext & { item: Accessor<ChatItem> }) {
  const item = () => props.item() as Extract<ChatItem, { kind: "user" }>;
  return (
    <View class="items-end px-1">
      <View
        class={props.themeStore.classes(
          "chat.user",
          "max-w-[85%] rounded-2xl rounded-br-md bg-primary px-4 py-3",
        )}
      >
        <Text selectable class="text-sm leading-6 text-primary-foreground">{item().text}</Text>
      </View>
    </View>
  );
}

function MessageMetrics(props: UiContext) {
  const rate = () => {
    const id = props.store.current();
    return id ? (props.store.outputRate()[id] ?? 0) : 0;
  };
  return (
    <If when={rate() > 0}>
      <View class="mt-1 flex-row items-center gap-2">
        <Text class="text-xs text-muted-foreground">≈ {rate()} token/s</Text>
      </View>
    </If>
  );
}

function MarkdownRow(props: UiContext & { item: Accessor<ChatItem> }) {
  const item = () => props.item() as MarkdownItem;
  const { themeStore } = props;
  return (
    <View class="px-1 py-1">
      <If when={item().kind === "reasoning"}>
        <View class="rounded-(--radius) border border-border bg-muted/30 px-3 py-2">
          <Text class="mb-1 text-xs font-medium text-muted-foreground">思考过程</Text>
          <Text selectable class="text-sm leading-6 text-muted-foreground">{item().text}</Text>
        </View>
      </If>
      <If when={item().kind === "markdown"}>
        <TextView
          markdown={item().text}
          options={{ streaming: item().streaming }}
          selectable
          classes={{
            root: themeStore.classes("chat.markdown", "text-sm leading-6 text-foreground"),
            codeBlock: themeStore.classes("markdown.code-block", "border-border bg-tool-surface"),
            codeBlockHeader: "text-muted-foreground",
            code: themeStore.classes("markdown.inline-code", "rounded bg-muted text-foreground"),
            link: "text-info",
            table: "border-border",
            tableHead: "bg-muted",
            rule: "border-border",
            alertNote: "border-info/40 bg-info/10 text-info",
            alertTip: "border-success/40 bg-success/10 text-success",
            alertImportant: "border-primary/40 bg-primary/10 text-foreground",
            alertWarning: "border-warning/40 bg-warning/10 text-warning",
            alertCaution: "border-destructive/40 bg-destructive/10 text-destructive",
          }}
          codeBlockActions={[{ id: "copy", label: "复制" }]}
        />
        <MessageMetrics {...props} />
      </If>
    </View>
  );
}

function statusLabel(status: ToolItem["status"]): string {
  switch (status) {
    case "input": return "接收参数";
    case "running": return "执行中";
    case "done": return "完成";
    case "error": return "失败";
    case "stopped": return "已停止";
  }
}

function ToolRow(props: UiContext & { item: Accessor<ChatItem> }) {
  const item = () => props.item() as ToolItem;
  const { themeStore } = props;
  return (
    <View class="overflow-hidden rounded-(--radius) border border-border bg-tool-surface">
      <View class="flex-row items-center gap-2 border-b border-border px-3 py-2">
        <Text class="flex-1 text-sm font-medium text-foreground">{item().tool}</Text>
        <Text class="text-xs text-muted-foreground">{statusLabel(item().status)}</Text>
      </View>
      <View class="gap-2 px-3 py-3">
        <If when={item().summary}>
          <Text selectable numberOfLines={4} class="font-mono text-xs leading-5 text-muted-foreground">
            {item().summary}
          </Text>
        </If>
        <If when={item().detail && item().status === "input"}>
          <Text numberOfLines={5} class="font-mono text-xs leading-5 text-muted-foreground">
            {item().detail}
          </Text>
        </If>
        <If when={item().output}>
          <Text selectable numberOfLines={10} class="font-mono text-xs leading-5 text-foreground">
            {item().output.length > 5000 ? `${item().output.slice(0, 5000)}\n… 输出已截断` : item().output}
          </Text>
        </If>
        <If when={item().edit}>
          <View class={themeStore.classes("diff.root", "h-80 overflow-hidden rounded-(--radius) border border-border")}>
            <DiffView
              patch={item().edit!.unified_diff}
              language={item().edit!.path}
              layout="unified"
              softWrap
              classes={{
                gutter: themeStore.classes("diff.gutter", "bg-tool-surface"),
                lineAdded: themeStore.classes("diff.line-added", "bg-diff-added"),
                lineDeleted: themeStore.classes("diff.line-deleted", "bg-diff-deleted"),
                wordAdded: themeStore.classes("diff.word-added", "bg-diff-added-word"),
                wordDeleted: themeStore.classes("diff.word-deleted", "bg-diff-deleted-word"),
                indicatorAdded: "bg-success",
                indicatorDeleted: "bg-destructive",
                separator: "bg-muted text-muted-foreground",
                fileHeader: "bg-secondary text-secondary-foreground",
              }}
            />
          </View>
        </If>
      </View>
    </View>
  );
}

function MessageRow(props: UiContext & { item: Accessor<ChatItem> }) {
  return (
    <View class="w-full">
      <If when={props.item().kind === "user"}><UserRow {...props} /></If>
      <If when={props.item().kind === "markdown" || props.item().kind === "reasoning"}>
        <MarkdownRow {...props} />
      </If>
      <If when={props.item().kind === "tool"}><ToolRow {...props} /></If>
      <If when={props.item().kind === "note"}>
        <Text class="px-1 py-2 text-sm text-muted-foreground">
          {props.item().kind === "note" ? (props.item() as Extract<ChatItem, { kind: "note" }>).text : ""}
        </Text>
      </If>
    </View>
  );
}

export function MessageList(props: UiContext & { items: ChatItem[] }) {
  return (
    <View class="flex-1 items-center">
      <View class="w-full max-w-4xl flex-1 px-4">
        <List
          each={props.items}
          keyExtractor={(item) => item.id}
          estimatedItemSize={120}
          maintainVisibleContentPosition={{ autoscrollToBottomThreshold: 120 }}
          class="flex-1"
          contentClass="gap-4 py-4"
        >
          {(item) => <MessageRow {...props} item={item} />}
        </List>
      </View>
    </View>
  );
}
