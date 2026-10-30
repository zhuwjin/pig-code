import { List, Text, View, signal, token } from "nsh";
import { Button } from "nsh/base";
import { titleBar } from "nsh/desktop";
import type { UiContext } from "../ui-context";
import { workspaceName } from "../ui-context";
import { SidebarBrand } from "./brand";
import { Icon } from "./icon";

const workspaceExpanded = signal(true);

export function Sidebar(props: UiContext & {
  onToggle: () => void;
  onOpenThemes: () => void;
}) {
  const { store, themeStore } = props;
  const sessions = () => store.sessions().filter((session) => session.cwd === store.cwd());
  return (
    <View
      style={themeStore.styles("sidebar.root")}
      class={themeStore.classes("sidebar.root", "w-[332px] shrink-0 border-r border-sidebar-border bg-sidebar")}
    >
      <View
        desktop={{ windowDrag: true }}
        style={{ height: titleBar().height || 45 }}
        class="shrink-0 flex-row items-center justify-between pl-3 pr-2"
      >
        <SidebarBrand />
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
          class="rounded-(--radius) px-2 py-1.5 hover:bg-sidebar-accent active:bg-accent"
          onPress={() => store.newTask()}
        >
          <View class="flex-row items-center gap-2">
            <Icon name="plus" />
            <Text class="text-sm text-sidebar-foreground">新建任务</Text>
          </View>
        </Button>
        <Button
          disabled
          accessibilityLabel="搜索（尚未迁移）"
          class="rounded-(--radius) px-2 py-1.5 disabled:opacity-40"
        >
          <View class="flex-row items-center gap-2">
            <Icon name="search" />
            <Text class="text-sm text-sidebar-foreground">搜索</Text>
          </View>
        </Button>
      </View>
      <View class="flex-row items-center gap-1 px-3 py-1.5">
        <Text class="flex-1 text-xs text-muted-foreground">会话</Text>
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
        <Button
          disabled
          accessibilityLabel="会话管理（尚未迁移）"
          class="h-6 w-6 items-center justify-center rounded-(--radius) disabled:opacity-40"
        >
          <Icon name="sliders-horizontal" size={12} color={token("secondary-foreground")} />
        </Button>
      </View>
      <View class="mx-2 flex-row items-center gap-2 rounded-(--radius) px-2 py-1.5">
        <Icon
          name={workspaceExpanded() ? "folder-open" : "folder-closed"}
          color={token("muted-foreground")}
        />
        <Text numberOfLines={1} class="flex-1 text-sm text-sidebar-foreground">
          {workspaceName(store.cwd())}
        </Text>
        <Icon name="chevron-down" size={12} color={token("muted-foreground")} />
      </View>
      <If when={workspaceExpanded()}>
        <List
          each={sessions()}
          keyExtractor={(item) => item.id}
          class="flex-1"
          contentClass="gap-1 px-2 pb-2"
          empty={<Text class="p-4 text-xs text-muted-foreground">暂无历史会话</Text>}
        >
          {(session) => (
            <Button
              selected={store.current() === session().id}
              class={themeStore.classes(
                "sidebar.session",
                "rounded-(--radius) px-3 py-2 pl-8 hover:bg-sidebar-accent selected:bg-sidebar-accent",
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
      <If when={!workspaceExpanded()}><View class="flex-1" /></If>
      <View class="border-t border-sidebar-border p-2">
        <Button
          testID="theme-settings"
          accessibilityLabel="外观与主题设置"
          class="rounded-(--radius) px-2 py-1.5 hover:bg-sidebar-accent active:bg-accent"
          onPress={props.onOpenThemes}
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
