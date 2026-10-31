import { List, Text, View, platform, signal, token } from "nsh";
import { Button, Popover } from "nsh/base";
import { titleBar } from "nsh/desktop";
import type { UiContext } from "../ui-context";
import { workspaceName } from "../ui-context";
import { SidebarBrand } from "./brand";
import { Icon } from "./icon";

const workspaceExpanded = signal(true);
const sidebarView = signal<"workspace" | "flat">("workspace");

export function Sidebar(props: UiContext & {
  onToggle: () => void;
  onOpenSettings: () => void;
  onOpenSearch: () => void;
}) {
  const { store, themeStore } = props;
  const manageOpen = signal(false);
  const sessions = () => store.sessions().filter((session) => session.cwd === store.cwd());
  const shownSessions = () => sidebarView() === "workspace"
    ? sessions()
    : [...store.sessions()].sort((a, b) =>
      Number(b.pinned) - Number(a.pinned) || b.updated_at - a.updated_at
    );
  return (
    <View
      style={themeStore.styles("sidebar.root")}
      class={themeStore.classes("sidebar.root", "relative h-full w-full shrink-0 bg-sidebar")}
    >
      <View class="absolute inset-0 bg-foreground/5 pointer-events-none" />
      <View
        desktop={{ windowDrag: true }}
        style={{ height: titleBar().height || 44 }}
        class={`shrink-0 flex-row items-center pr-2 ${platform.os === "macos" ? "justify-end pl-[80px]" : "justify-between pl-3"}`}
      >
        <If when={platform.os !== "macos"}><SidebarBrand /></If>
        <Button
          testID="sidebar-toggle"
          accessibilityLabel="折叠侧边栏"
          class="h-8 w-8 items-center justify-center rounded-(--radius) hover:bg-sidebar-accent active:bg-accent"
          onPress={props.onToggle}
        >
          <Icon name="panel-left" size={14} color={token("secondary-foreground")} />
        </Button>
      </View>
      <View class="gap-1 p-2">
        <Button
          testID="new-task"
          accessibilityLabel="新任务"
          class="group rounded-(--radius) px-2 py-1 hover:bg-sidebar-accent active:bg-accent"
          onPress={() => store.newTask()}
        >
          <View class="flex-row items-center gap-2">
            <Icon name="plus" />
            <Text class="text-sm text-sidebar-foreground">新建任务</Text>
            <View class="flex-1" />
            <Text class="rounded border border-border px-1 text-[10px] text-muted-foreground opacity-0 group-hover:opacity-100">
              {platform.os === "macos" ? "⌘N" : "Ctrl+N"}
            </Text>
          </View>
        </Button>
        <Button
          testID="open-search"
          accessibilityLabel="搜索"
          class="group rounded-(--radius) px-2 py-1 hover:bg-sidebar-accent active:bg-accent"
          onPress={props.onOpenSearch}
        >
          <View class="flex-row items-center gap-2">
            <Icon name="search" />
            <Text class="text-sm text-sidebar-foreground">搜索</Text>
            <View class="flex-1" />
            <Text class="rounded border border-border px-1 text-[10px] text-muted-foreground opacity-0 group-hover:opacity-100">
              {platform.os === "macos" ? "⌘K" : "Ctrl+K"}
            </Text>
          </View>
        </Button>
      </View>
      <View class="flex-row items-center gap-1 pl-3 pr-2 py-1">
        <Text class="flex-1 text-xs text-muted-foreground">会话</Text>
        <If when={sidebarView() === "workspace"}>
          <Button
            testID="toggle-workspace"
            accessibilityLabel={workspaceExpanded() ? "折叠工作区" : "展开工作区"}
            class="h-6 w-6 items-center justify-center rounded-(--radius) hover:bg-sidebar-accent"
            onPress={() => workspaceExpanded.update((expanded) => !expanded)}
          >
            <Icon
              name={workspaceExpanded() ? "fold-vertical" : "unfold-vertical"}
              size={12}
              color={token("secondary-foreground")}
            />
          </Button>
        </If>
        <Popover
          open={manageOpen()}
          onOpenChange={(value) => manageOpen.set(value)}
          placement="bottom"
          align="end"
          popupClass="w-48 rounded-xl border border-border bg-popover p-2 shadow-xl"
          trigger={
            <View role="button" testID="list-manage" class="h-6 w-6 items-center justify-center rounded-(--radius) hover:bg-sidebar-accent">
              <Icon name="sliders-horizontal" size={12} color={token("secondary-foreground")} />
            </View>
          }
        >
          <View class="gap-1">
            <Button
              testID="list-view-workspace"
              selected={sidebarView() === "workspace"}
              class="flex-row items-center rounded-(--radius) px-3 py-2 hover:bg-accent selected:bg-accent/60"
              onPress={() => {
                sidebarView.set("workspace");
                manageOpen.set(false);
              }}
            >
              <Text class="flex-1 text-sm text-foreground">按工作区分组</Text>
              <If when={sidebarView() === "workspace"}><Text class="text-sm">✓</Text></If>
            </Button>
            <Button
              testID="list-view-flat"
              selected={sidebarView() === "flat"}
              class="flex-row items-center rounded-(--radius) px-3 py-2 hover:bg-accent selected:bg-accent/60"
              onPress={() => {
                sidebarView.set("flat");
                manageOpen.set(false);
              }}
            >
              <Text class="flex-1 text-sm text-foreground">全部会话</Text>
              <If when={sidebarView() === "flat"}><Text class="text-sm">✓</Text></If>
            </Button>
          </View>
        </Popover>
      </View>
      <If when={sidebarView() === "workspace"}>
        <Button
          testID="workspace-row"
          accessibilityLabel={workspaceExpanded() ? "折叠当前工作区" : "展开当前工作区"}
          class="mx-2 flex-row items-center gap-2 rounded-(--radius) px-2 py-1 hover:bg-sidebar-accent"
          onPress={() => workspaceExpanded.update((expanded) => !expanded)}
        >
          <Icon
            name={workspaceExpanded() ? "folder-open" : "folder-closed"}
            color={token("muted-foreground")}
          />
          <Text numberOfLines={1} class="flex-1 text-sm text-sidebar-foreground">
            {workspaceName(store.cwd())}
          </Text>
        </Button>
      </If>
      <If when={sidebarView() === "flat" || workspaceExpanded()}>
        <List
          each={shownSessions()}
          keyExtractor={(item) => item.id}
          class="flex-1"
          contentClass="gap-1 px-2 pb-2"
        >
          {(session) => (
            <Button
              selected={store.current() === session().id}
              class={themeStore.classes(
                "sidebar.session",
                `rounded-(--radius) py-2 hover:bg-sidebar-accent selected:bg-sidebar-accent ${sidebarView() === "workspace" ? "pl-8 pr-3" : "px-3"}`,
              )}
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
      </If>
      <If when={sidebarView() === "workspace" && !workspaceExpanded()}><View class="flex-1" /></If>
      <View class="border-t border-sidebar-border p-2">
        <Button
          testID="settings-open"
          accessibilityLabel="模型与应用设置"
          class="rounded-(--radius) px-2 py-1 hover:bg-sidebar-accent active:bg-accent"
          onPress={props.onOpenSettings}
        >
          <View class="flex-row items-center gap-2">
            <Icon name="settings" />
            <Text class="text-sm text-sidebar-foreground">设置</Text>
          </View>
        </Button>
      </View>
    </View>
  );
}
