import {
  Accessor,
  Image,
  ImageAsset,
  List,
  Text,
  View,
  colorScheme,
  effect,
  mount,
  setColorScheme,
  signal,
  token,
} from "nsh";
import { Button, DiffView, Input, TextView } from "nsh/base";
import { Window, titleBar } from "nsh/desktop";
import arrowUpIcon from "../assets/icons/arrow-up.svg";
import chevronDownIcon from "../assets/icons/chevron-down.svg";
import folderIcon from "../assets/icons/folder.svg";
import moonIcon from "../assets/icons/moon.svg";
import moreHorizontalIcon from "../assets/icons/more-horizontal.svg";
import panelLeftIcon from "../assets/icons/panel-left.svg";
import plusIcon from "../assets/icons/plus.svg";
import searchIcon from "../assets/icons/search.svg";
import settingsIcon from "../assets/icons/settings.svg";
import shieldCheckIcon from "../assets/icons/shield-check.svg";
import stopIcon from "../assets/icons/stop.svg";
import sunIcon from "../assets/icons/sun.svg";
import { ChatItem, MarkdownItem, PendingApproval, ToolItem } from "./protocol";
import { AgentStore } from "./store";

let store: AgentStore;
const sidebarVisible = signal(true);
Window.setTitle("Pig Code");
Window.setTitleBar("hidden", 44);

function workspaceName(path: string): string {
  const normalized = path.replace(/[\\/]+$/, "");
  return normalized.split(/[\\/]/).filter(Boolean).pop() || path;
}

function Icon(props: { src: ImageAsset; color?: string }) {
  return (
    <Image
      src={props.src}
      class="h-4 w-4"
      resizeMode="contain"
      style={{ tintColor: props.color ?? token("muted-foreground") }}
    />
  );
}

function UserRow(props: { item: Accessor<ChatItem> }) {
  const item = () => props.item() as Extract<ChatItem, { kind: "user" }>;
  return (
    <View class="items-end px-1">
      <View class="max-w-[85%] rounded-2xl rounded-br-md bg-primary px-4 py-3">
        <Text selectable class="text-sm leading-6 text-primary-foreground">
          {item().text}
        </Text>
      </View>
    </View>
  );
}

function MarkdownRow(props: { item: Accessor<ChatItem> }) {
  const item = () => props.item() as MarkdownItem;
  return (
    <View class="px-1 py-1">
      <If when={item().kind === "reasoning"}>
        <View class="rounded-(--radius) border border-border bg-muted/30 px-3 py-2">
          <Text class="mb-1 text-xs font-medium text-muted-foreground">思考过程</Text>
          <Text selectable class="text-sm leading-6 text-muted-foreground">
            {item().text}
          </Text>
        </View>
      </If>
      <If when={item().kind === "markdown"}>
        <TextView
          markdown={item().text}
          options={{ streaming: item().streaming }}
          selectable
          classes={{ root: "text-sm leading-6 text-foreground" }}
          codeBlockActions={[{ id: "copy", label: "复制" }]}
        />
      </If>
    </View>
  );
}

function statusLabel(status: ToolItem["status"]): string {
  switch (status) {
    case "input":
      return "接收参数";
    case "running":
      return "执行中";
    case "done":
      return "完成";
    case "error":
      return "失败";
    case "stopped":
      return "已停止";
  }
}

function ToolRow(props: { item: Accessor<ChatItem> }) {
  const item = () => props.item() as ToolItem;
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
            {item().output.length > 5000
              ? `${item().output.slice(0, 5000)}\n… 输出已截断`
              : item().output}
          </Text>
        </If>
        <If when={item().edit}>
          <View class="h-80 overflow-hidden rounded-(--radius) border border-border">
            <DiffView
              patch={item().edit!.unified_diff}
              language={item().edit!.path}
              layout="unified"
              softWrap
            />
          </View>
        </If>
      </View>
    </View>
  );
}

function MessageRow(props: { item: Accessor<ChatItem> }) {
  return (
    <View class="w-full">
      <If when={props.item().kind === "user"}>
        <UserRow item={props.item} />
      </If>
      <If when={props.item().kind === "markdown" || props.item().kind === "reasoning"}>
        <MarkdownRow item={props.item} />
      </If>
      <If when={props.item().kind === "tool"}>
        <ToolRow item={props.item} />
      </If>
      <If when={props.item().kind === "note"}>
        <Text class="px-1 py-2 text-sm text-muted-foreground">
          {props.item().kind === "note"
            ? (props.item() as Extract<ChatItem, { kind: "note" }>).text
            : ""}
        </Text>
      </If>
    </View>
  );
}

function ApprovalCard(props: { approval: PendingApproval }) {
  return (
    <View class="mb-3 gap-3 rounded-(--radius) border border-warning/40 bg-warning/10 p-4">
      <Text class="text-sm font-semibold text-foreground">需要批准：{props.approval.tool}</Text>
      <Text selectable numberOfLines={6} class="font-mono text-xs leading-5 text-muted-foreground">
        {props.approval.detail}
      </Text>
      <View class="flex-row justify-end gap-2">
        <Button
          class="rounded-(--radius) border border-border px-4 py-2 active:bg-muted"
          onPress={() => store.answerApproval(false)}
        >
          <Text class="text-sm text-foreground">拒绝</Text>
        </Button>
        <Button
          class="rounded-(--radius) bg-primary px-4 py-2 active:bg-primary/90"
          onPress={() => store.answerApproval(true)}
        >
          <Text class="text-sm text-primary-foreground">允许</Text>
        </Button>
      </View>
    </View>
  );
}

function TitleBar() {
  return (
    <View
      desktop={{ windowDrag: true }}
      style={{
        height: titleBar().height || 44,
        paddingLeft: sidebarVisible() ? 8 : titleBar().left + 8,
        paddingRight: titleBar().right + 8,
      }}
      class="flex-row items-center gap-2 border-b border-border bg-title-bar"
    >
      <Button
        testID="sidebar-toggle"
        accessibilityLabel="切换侧边栏"
        class="h-8 w-8 items-center justify-center rounded-(--radius) hover:bg-muted active:bg-accent"
        onPress={() => sidebarVisible.update((visible) => !visible)}
      >
        <Icon src={panelLeftIcon} />
      </Button>
      <Text numberOfLines={1} class="text-sm font-medium text-foreground">
        {workspaceName(store.cwd())} / {store.currentTitle()}
      </Text>
      <View class="flex-1" />
      <If when={store.currentModel()}>
        <Text numberOfLines={1} class="text-xs text-muted-foreground">
          {store.currentModel()}
        </Text>
      </If>
      <Button
        testID={colorScheme() === "dark" ? "appearance-dark" : "appearance-light"}
        accessibilityLabel="切换明暗主题"
        class="h-8 w-8 items-center justify-center rounded-(--radius) hover:bg-muted active:bg-accent"
        onPress={() => setColorScheme(colorScheme() === "dark" ? "light" : "dark")}
      >
        <Icon src={colorScheme() === "dark" ? sunIcon : moonIcon} />
      </Button>
    </View>
  );
}

function Sidebar() {
  return (
    <View
      style={{ paddingTop: titleBar().height }}
      class="w-56 border-r border-sidebar-border bg-sidebar"
    >
      <View class="gap-1 p-2">
        <Button
          testID="new-task"
          accessibilityLabel="新任务"
          class="rounded-(--radius) px-3 py-2 hover:bg-sidebar-accent active:bg-accent"
          onPress={() => store.newTask()}
        >
          <View class="flex-row items-center gap-2">
            <Icon src={plusIcon} color={token("sidebar-foreground")} />
            <Text class="text-sm text-sidebar-foreground">新建任务</Text>
          </View>
        </Button>
        <Button
          disabled
          accessibilityLabel="搜索（尚未迁移）"
          class="rounded-(--radius) px-3 py-2 disabled:opacity-40"
        >
          <View class="flex-row items-center gap-2">
            <Icon src={searchIcon} color={token("sidebar-foreground")} />
            <Text class="text-sm text-sidebar-foreground">搜索</Text>
          </View>
        </Button>
      </View>
      <View class="flex-row items-center justify-between px-4 pb-2 pt-3">
        <Text class="text-xs text-muted-foreground">会话</Text>
        <Icon src={moreHorizontalIcon} />
      </View>
      <View class="flex-row items-center gap-1 px-4 pb-2">
        <Icon src={folderIcon} color={token("sidebar-foreground")} />
        <Text numberOfLines={1} class="text-sm text-sidebar-foreground">
          {workspaceName(store.cwd())}
        </Text>
        <Icon src={chevronDownIcon} />
      </View>
      <List
        each={store.sessions()}
        keyExtractor={(item) => item.id}
        class="flex-1"
        contentClass="gap-1 px-2 pb-2"
        empty={<Text class="p-4 text-xs text-muted-foreground">暂无历史会话</Text>}
      >
        {(session) => (
          <Button
            selected={store.current() === session().id}
            class="rounded-(--radius) px-3 py-2 hover:bg-sidebar-accent selected:bg-sidebar-accent"
            onPress={() => store.openSession(session().id)}
          >
            <View class="gap-1">
              <Text numberOfLines={1} class="text-sm text-sidebar-foreground">
                {session().title || "未命名会话"}
              </Text>
              <Text numberOfLines={1} class="text-xs text-muted-foreground">
                {session().cwd}
              </Text>
            </View>
          </Button>
        )}
      </List>
      <View class="border-t border-sidebar-border p-2">
        <Button
          disabled
          accessibilityLabel="设置（尚未迁移）"
          class="rounded-(--radius) px-3 py-2 disabled:opacity-40"
        >
          <View class="flex-row items-center gap-2">
            <Icon src={settingsIcon} color={token("sidebar-foreground")} />
            <Text class="text-sm text-sidebar-foreground">设置</Text>
          </View>
        </Button>
      </View>
    </View>
  );
}

function Composer() {
  const sessionId = () => store.current();
  const running = () => {
    const id = sessionId();
    return id ? (store.running()[id] ?? false) : false;
  };
  const approval = () => {
    const id = sessionId();
    return id ? (store.approvals()[id] ?? [])[0] : undefined;
  };
  const usage = () => {
    const id = sessionId();
    return id ? store.contextUsage()[id] : undefined;
  };
  return (
    <View class="px-4 pb-4 pt-2">
      <View class="mx-auto w-full max-w-3xl">
        <If when={approval()}>{(value) => <ApprovalCard approval={value()} />}</If>
        <View class="rounded-2xl border border-input bg-composer px-3 pb-2 pt-2 shadow-lg">
          <Input
            testID="composer-input"
            value={store.draft()}
            onChange={(text) => store.draft.set(text)}
            onSubmit={() => store.send()}
            multiline
            submitBehavior="submit"
            placeholder="输入消息，Enter 发送，Shift+Enter 换行"
            class="max-h-40 min-h-16 px-1 py-2 text-sm leading-6 text-foreground placeholder:text-muted-foreground"
          />
          <View class="flex-row items-center gap-3 pt-1">
            <View class="flex-row items-center gap-1">
              <Icon src={shieldCheckIcon} color={token("warning")} />
              <Text class="text-xs text-warning">自动</Text>
            </View>
            <View class="flex-1" />
            <Text numberOfLines={1} class="text-xs text-muted-foreground">
              {store.currentModel() || "默认模型"}
            </Text>
            <Button
              testID="send-button"
              accessibilityLabel={running() ? "停止" : "发送"}
              disabled={!running() && !store.draft().trim()}
              class="h-8 w-8 items-center justify-center rounded-full bg-primary active:bg-primary/90 disabled:opacity-30"
              onPress={() => store.send()}
            >
              <Icon src={running() ? stopIcon : arrowUpIcon} color={token("primary-foreground")} />
            </Button>
          </View>
        </View>
        <If when={usage()}>
          <View class="flex-row items-center px-2 pt-2">
            <Text class="flex-1 text-xs text-muted-foreground">{usage()}</Text>
            <Text class="text-xs text-muted-foreground">Rust agent</Text>
          </View>
        </If>
      </View>
    </View>
  );
}

function Workspace() {
  const items = () => {
    const id = store.current();
    return id ? (store.items()[id] ?? []) : [];
  };
  const error = () => {
    const id = store.current();
    return (id && store.errors()[id]) || store.globalError();
  };
  return (
    <View class="min-w-0 flex-1 bg-background">
      <If when={error()}>
        <View class="mx-auto mt-3 max-w-3xl rounded-(--radius) bg-destructive/10 px-3 py-2">
          <Text selectable class="text-sm text-destructive">{error()}</Text>
        </View>
      </If>
      <View class="flex-1 items-center">
        <View class="w-full max-w-4xl flex-1 px-4">
          <List
            each={items()}
            keyExtractor={(item) => item.id}
            estimatedItemSize={120}
            maintainVisibleContentPosition={{ autoscrollToBottomThreshold: 120 }}
            class="flex-1"
            contentClass="gap-4 py-4"
            empty={
              <View class="flex-1 items-center justify-center gap-3 p-8">
                <Text class="text-2xl font-semibold text-foreground">Pig Code</Text>
                <Text class="text-center text-sm leading-6 text-muted-foreground">
                  输入一个任务，Rust agent 会创建会话并流式返回答复。
                </Text>
              </View>
            }
          >
            {(item) => <MessageRow item={item} />}
          </List>
        </View>
      </View>
      <Composer />
    </View>
  );
}

function App() {
  effect(() => {
    store.start();
  });
  return (
    <View class="flex-1 flex-row bg-background pt-[env(safe-area-inset-top)] pr-[env(safe-area-inset-right)] pb-[env(safe-area-inset-bottom)] pl-[env(safe-area-inset-left)]">
      <If when={sidebarVisible()}>
        <Sidebar />
      </If>
      <View class="min-w-0 flex-1">
        <TitleBar />
        <Workspace />
      </View>
    </View>
  );
}

export function mountApp(selftest = false): void {
  store = new AgentStore(selftest);
  mount(App);
}
