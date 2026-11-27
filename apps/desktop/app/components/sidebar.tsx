import { Canvas, Clipboard, Easing, List, Scroll, Text, View, createAnimatedValue, effect, platform, signal, token } from "nsh";
import { Button, Collapsible, ContextMenu, Input, Menu, MenuItem, MenuSeparator } from "nsh/base";
import { titleBar } from "nsh/desktop";
import { t } from "../i18n";
import type { SessionMeta } from "../protocol";
import type { UiContext } from "../ui-context";
import { workspaceName } from "../ui-context";
import { SidebarBrand } from "./brand";
import { Icon } from "./icon";
import { SETTINGS_PAGES, type SettingsPageId } from "./settings/settings-page";

const WORKSPACE_PAGE_SIZE = 5;
const workspaceExpanded = signal(true);
const workspaceShown = signal(WORKSPACE_PAGE_SIZE);
const workspacePageOpen = signal(false);
const sidebarView = signal<"workspace" | "flat">("workspace");

export function formatSessionTime(updatedAt: number, nowSeconds = Math.floor(Date.now() / 1000)): string {
  const diff = Math.max(0, Math.floor(nowSeconds - updatedAt));
  if (diff < 60) return t("sidebar.time.just_now");
  if (diff < 3600) return t("sidebar.time.minutes", { count: Math.floor(diff / 60) });
  if (diff < 86400) return t("sidebar.time.hours", { count: Math.floor(diff / 3600) });
  const days = Math.floor(diff / 86400);
  if (days < 7) return t("sidebar.time.days", { count: days });
  if (days < 30) return t("sidebar.time.weeks", { count: Math.floor(days / 7) });
  return t("sidebar.time.months", { count: Math.floor(days / 30) });
}

export function workspaceSessionLimit(shown: number, open: boolean): number {
  return open ? shown : WORKSPACE_PAGE_SIZE;
}

export function nextWorkspaceSessionLimit(shown: number, open: boolean): number {
  return workspaceSessionLimit(shown, open) + WORKSPACE_PAGE_SIZE;
}

export function sessionTitleScroll(titleWidth: number, viewportWidth: number): { offset: number; duration: number } {
  const distance = Math.max(0, titleWidth - viewportWidth + 2);
  return { offset: -distance, duration: distance / 94 * 1000 };
}

function SessionTiming(props: { store: UiContext["store"]; session: () => SessionMeta }) {
  const running = () => props.store.running()[props.session().id] ?? false;
  const waitingApproval = () => (props.store.approvals()[props.session().id] ?? []).length > 0;
  return (
    <>
      <If when={waitingApproval()}>
        <View class="h-2 w-2 shrink-0 rounded-full" style={{ backgroundColor: token("warning") }} />
      </If>
      <If when={running()}>
        <Icon name="loader-circle" size={12} class="shrink-0 animate-spin" color={token("muted-foreground")} />
      </If>
      <If when={!running()}>
        <Text
          testID={`session-time-${props.session().id}`}
          class="shrink-0 text-xs text-muted-foreground"
        >
          {formatSessionTime(props.session().updated_at)}
        </Text>
      </If>
    </>
  );
}

export function blendColors(background: string, foreground: string, amount: number): string {
  const channels = (color: string): number[] | null => {
    if (!/^#[0-9a-f]{6}(?:[0-9a-f]{2})?$/i.test(color)) return null;
    return [1, 3, 5].map((index) => parseInt(color.slice(index, index + 2), 16));
  };
  const base = channels(background);
  const overlay = channels(foreground);
  if (!base || !overlay) return background;
  return `#${base.map((channel, index) =>
    Math.round(channel * (1 - amount) + overlay[index] * amount).toString(16).padStart(2, "0")
  ).join("")}`;
}

function SessionTitle(props: { session: () => SessionMeta; selected: () => boolean; rowHovered?: () => boolean; fontFamily?: string | null }) {
  const offset = createAnimatedValue(0);
  const hovering = signal(false);
  const fade = signal(false);
  const text = () => props.session().title || t("sidebar.untitled_session");
  const titleWidth = signal(0);
  let viewportWidth = 0;
  let started = false;
  let currentTitle = "";
  let run = 0;
  let scheduled: number | undefined;
  const start = () => {
    const scroll = sessionTitleScroll(titleWidth(), viewportWidth);
    fade.set(scroll.offset < -1);
    if (started || !hovering()) return;
    if (scroll.offset >= -1) {
      fade.set(false);
      return;
    }
    started = true;
    fade.set(true);
    const currentRun = run;
    void offset.timing(scroll.offset, {
      duration: scroll.duration,
      easing: Easing.linear,
      delay: 480,
    }).then((finished) => {
      if (finished && run === currentRun) fade.set(false);
    });
  };
  effect(() => {
    const titleKey = `${props.session().id}:${props.session().title}`;
    const active = hovering();
    if (currentTitle !== titleKey) {
      currentTitle = titleKey;
      titleWidth.set(0);
    }
    started = false;
    run += 1;
    offset.stop();
    offset.set(0);
    fade.set(sessionTitleScroll(titleWidth(), viewportWidth).offset < -1);
    if (active) scheduled = requestAnimationFrame(start);
    return () => {
      run += 1;
      if (scheduled !== undefined) cancelAnimationFrame(scheduled);
      scheduled = undefined;
      offset.stop();
    };
  });
  return (
    <View
      class="relative min-w-0 flex-1 overflow-hidden"
      onLayout={({ bounds }) => {
        viewportWidth = bounds.size.width;
        start();
      }}
      onMouseEnter={() => hovering.set(true)}
      onMouseLeave={() => hovering.set(false)}
    >
      <Canvas
        class="absolute h-px w-px opacity-0"
        style={{ pointerEvents: "none", width: 1, height: 1 }}
        draw={(cx) => {
          cx.font = `14px ${props.fontFamily ? JSON.stringify(props.fontFamily) : "system-ui"}`;
          titleWidth.set(cx.measureText(text()).width);
          start();
        }}
      />
      <Text
        testID="session-title-text"
        class="shrink-0 whitespace-nowrap text-left text-sm text-sidebar-foreground"
        style={{ numberOfLines: 1, textOverflow: "clip", width: titleWidth() > 0 ? titleWidth() : undefined, transform: [{ translateX: offset }] }}
      >
        {text()}
      </Text>
      <If when={fade()}>
        <Canvas
          testID="session-title-fade"
          class="absolute right-0 top-0 h-5 w-5"
          style={{ pointerEvents: "none", width: 20, height: 20 }}
          draw={(cx, { width, height }) => {
            const gradient = cx.createLinearGradient(0, 0, width, 0);
            const background = hovering() || props.rowHovered?.() || props.selected()
              ? token("sidebar-accent")
              : blendColors(token("sidebar"), token("foreground"), 0.05);
            gradient.addColorStop(0, background.length === 7 ? `${background}00` : `${background.slice(0, 7)}00`);
            gradient.addColorStop(1, background);
            cx.fillStyle = gradient;
            cx.fillRect(0, 0, width, height);
          }}
        />
      </If>
    </View>
  );
}

export function Sidebar(props: UiContext & {
  mode: "workspace" | "settings";
  settingsPage: SettingsPageId;
  onNavigateSettings: (page: SettingsPageId) => void;
  onBackSettings: () => void;
  onToggle: () => void;
  onOpenSettings: () => void;
  onOpenSearch: () => void;
}) {
  const { store, themeStore } = props;
  const manageOpen = signal(false);
  const deviceMenuOpen = signal(false);
  const workspaceMenuOpen = signal(false);
  const hoveredSession = signal<string | null>(null);
  const renaming = signal<{ kind: "workspace" | "session"; id: string } | null>(null);
  const renameValue = signal("");
  const workspaceMeta = () => store.workspaces().find((workspace) => workspace.path === store.cwd());
  const currentWorkspaceName = () => workspaceMeta()?.alias || workspaceName(store.cwd());
  const workspaceHidden = () => workspaceMeta()?.hidden ?? false;
  const startRename = (kind: "workspace" | "session", id: string, value: string) => {
    renaming.set({ kind, id });
    renameValue.set(value);
  };
  const commitRename = () => {
    const target = renaming();
    if (!target) return;
    const value = renameValue().trim();
    renaming.set(null);
    if (target.kind === "workspace") {
      store.renameWorkspace(target.id, !value || value === workspaceName(target.id) ? null : value);
    } else if (value) {
      store.renameSession(target.id, value);
    }
  };
  const pinnedSessions = () => store.sessions()
    .filter((session) => session.pinned)
    .sort((a, b) => b.updated_at - a.updated_at);
  const sessions = () => store.sessions()
    .filter((session) => session.cwd === store.cwd() && !session.pinned)
    .sort((a, b) => b.updated_at - a.updated_at);
  const flatSessions = () => [...store.sessions()].sort((a, b) =>
    Number(b.pinned) - Number(a.pinned) || b.updated_at - a.updated_at
  );
  const firstPageSessions = () => sessions().slice(0, WORKSPACE_PAGE_SIZE);
  const morePages = () => {
    const remaining = sessions().slice(WORKSPACE_PAGE_SIZE);
    const pages: SessionMeta[][] = [];
    for (let index = 0; index < remaining.length; index += WORKSPACE_PAGE_SIZE) {
      pages.push(remaining.slice(index, index + WORKSPACE_PAGE_SIZE));
    }
    return pages;
  };
  const canShowMore = () => sessions().length > workspaceSessionLimit(workspaceShown(), workspacePageOpen());
  const showMore = () => {
    workspaceShown.set(nextWorkspaceSessionLimit(workspaceShown(), workspacePageOpen()));
    workspacePageOpen.set(true);
  };
  const collapseSessions = () => {
    workspacePageOpen.set(false);
    workspaceShown.set(WORKSPACE_PAGE_SIZE);
  };
  const workspaceItems = () => (
    <>
      <MenuItem testID="workspace-new-task" class="flex-row items-center gap-2 rounded-(--radius) px-2 py-0.5 hover:bg-accent" onSelect={() => store.newTaskInWorkspace(store.cwd())}>
        <Icon testID="workspace-new-task-icon" name="plus" size={13} color={token("muted-foreground")} />
        <Text class="flex-1 text-sm text-foreground">{t("sidebar.new_task")}</Text>
      </MenuItem>
      <MenuSeparator class="my-0.5 h-px bg-border" />
      <MenuItem testID="workspace-copy-path" class="flex-row items-center gap-2 rounded-(--radius) px-2 py-0.5 hover:bg-accent" onSelect={() => void Clipboard.writeText(store.cwd())}>
        <Icon testID="workspace-copy-path-icon" name="copy" size={13} color={token("muted-foreground")} />
        <Text class="flex-1 text-sm text-foreground">{t("sidebar.copy_path")}</Text>
      </MenuItem>
      <MenuItem
        testID="workspace-rename"
        class="flex-row items-center gap-2 rounded-(--radius) px-2 py-0.5 hover:bg-accent"
        onSelect={() => startRename("workspace", store.cwd(), currentWorkspaceName())}
      >
        <Icon testID="workspace-rename-icon" name="pencil" size={13} color={token("muted-foreground")} />
        <Text class="flex-1 text-sm text-foreground">{t("sidebar.rename")}</Text>
      </MenuItem>
      <MenuSeparator class="my-0.5 h-px bg-border" />
      <MenuItem testID="workspace-remove" class="flex-row items-center gap-2 rounded-(--radius) px-2 py-0.5 hover:bg-accent" onSelect={() => store.removeWorkspace(store.cwd())}>
        <Icon testID="workspace-remove-icon" name="folder-minus" size={13} color={token("muted-foreground")} />
        <Text class="flex-1 text-sm text-foreground">{t("sidebar.remove_workspace")}</Text>
      </MenuItem>
    </>
  );
  const sessionItems = (session: () => SessionMeta) => (
    <>
      <MenuItem
        testID={`session-rename-${session().id}`}
        class="flex-row items-center gap-2 rounded-(--radius) px-2 py-0.5 hover:bg-accent"
        onSelect={() => startRename("session", session().id, session().title)}
      >
        <Icon testID="session-rename-icon" name="pencil" size={13} color={token("muted-foreground")} />
        <Text class="flex-1 text-sm text-foreground">{t("sidebar.rename")}</Text>
      </MenuItem>
      <MenuItem
        testID={`session-pin-${session().id}`}
        class="flex-row items-center gap-2 rounded-(--radius) px-2 py-0.5 hover:bg-accent"
        onSelect={() => store.setSessionPinned(session().id, !session().pinned)}
      >
        <Icon testID="session-pin-icon" name={session().pinned ? "star-off" : "star"} size={13} color={token("muted-foreground")} />
        <Text class="flex-1 text-sm text-foreground">{t(session().pinned ? "sidebar.unpin" : "sidebar.pin")}</Text>
      </MenuItem>
      <MenuItem
        testID={`session-archive-${session().id}`}
        class="flex-row items-center gap-2 rounded-(--radius) px-2 py-0.5 hover:bg-accent"
        onSelect={() => store.setSessionArchived(session().id, !session().archived)}
      >
        <Icon testID="session-archive-icon" name={session().archived ? "undo-2" : "inbox"} size={13} color={token("muted-foreground")} />
        <Text class="flex-1 text-sm text-foreground">{t(session().archived ? "sidebar.unarchive" : "sidebar.archive")}</Text>
      </MenuItem>
      <MenuSeparator class="my-0.5 h-px bg-border" />
      <MenuItem
        testID={`session-delete-${session().id}`}
        class="flex-row items-center gap-2 rounded-(--radius) px-2 py-0.5 hover:bg-accent"
        onSelect={() => store.deleteSession(session().id)}
      >
        <Icon testID="session-delete-icon" name="trash" size={13} color={token("muted-foreground")} />
        <Text class="flex-1 text-sm text-foreground">{t("sidebar.delete_session")}</Text>
      </MenuItem>
    </>
  );
  const renderWorkspaceRow = () => (
    <>
      <If when={renaming()?.kind === "workspace"}>
        <View class="mb-0 flex-row items-center rounded-(--radius) px-2 py-1.5">
          <Input
            testID="workspace-rename-input"
            autoFocus
            value={renameValue()}
            onChange={({ value }) => renameValue.set(value)}
            onSubmit={commitRename}
            onBlur={commitRename}
            class="min-w-0 flex-1 rounded-(--radius) border border-border bg-background px-2 py-1.5 text-sm text-foreground"
          />
        </View>
      </If>
      <If when={renaming()?.kind !== "workspace"}>
        <ContextMenu
          popupClass="w-44 rounded-xl border border-border bg-popover p-1 shadow-xl"
          trigger={
            <View class="group relative mb-0">
              <Button
                testID="workspace-row"
                accessibilityLabel={t(workspaceExpanded() ? "sidebar.collapse_current_workspace" : "sidebar.expand_current_workspace")}
                class={`w-full flex-row items-center gap-2 rounded-(--radius) px-2 py-1.5 ${workspaceMenuOpen() ? "bg-sidebar-accent pr-14" : "hover:bg-sidebar-accent group-hover:bg-sidebar-accent group-hover:pr-14"}`}
                onPress={() => workspaceExpanded.update((expanded) => !expanded)}
              >
                <Icon
                  name={workspaceExpanded() ? "folder-open" : "folder-closed"}
                  color={token("muted-foreground")}
                />
                <Text style={{ numberOfLines: 1 }} class="min-w-0 flex-1 text-left text-sm text-sidebar-foreground">
                  {currentWorkspaceName()}
                </Text>
              </Button>
              <View class={`absolute right-1 top-0 bottom-0 items-center gap-1 ${workspaceMenuOpen() ? "flex" : "hidden group-hover:flex"}`}>
                <Menu
                  open={workspaceMenuOpen()}
                  onOpenChange={({ open: value }) => workspaceMenuOpen.set(value)}
                  placement="bottom"
                  align="end"
                  popupClass="w-44 rounded-xl border border-border bg-popover p-1 shadow-xl"
                  trigger={
                    <View role="button" testID="workspace-menu" accessibilityLabel={t("sidebar.workspace_menu")} class="h-6 w-6 flex-row items-center justify-center rounded-(--radius) hover:bg-foreground/10">
                      <Icon name="ellipsis" size={14} color={token("muted-foreground")} />
                    </View>
                  }
                >
                  {workspaceItems()}
                </Menu>
                <Button
                  testID="workspace-add"
                  accessibilityLabel={t("sidebar.new_task")}
                  class="h-6 w-6 flex-row items-center justify-center rounded-(--radius) hover:bg-foreground/10"
                  onPress={() => store.newTaskInWorkspace(store.cwd())}
                >
                  <Icon name="plus" size={14} color={token("muted-foreground")} />
                </Button>
              </View>
            </View>
          }
        >
          {workspaceItems()}
        </ContextMenu>
      </If>
    </>
  );
  const renderSessionRow = (session: () => SessionMeta, detailed: () => boolean) => (
    <>
      <If when={renaming()?.kind === "session" && renaming()?.id === session().id}>
        <View class={`flex-row items-center py-1.5 ${detailed() ? "px-2" : "pl-8 pr-2"}`}>
          <Input
            testID="session-rename-input"
            autoFocus
            value={renameValue()}
            onChange={({ value }) => renameValue.set(value)}
            onSubmit={commitRename}
            onBlur={commitRename}
            class="min-w-0 flex-1 rounded-(--radius) border border-border bg-background px-2 py-1.5 text-sm text-foreground"
          />
        </View>
      </If>
      <If when={renaming()?.kind !== "session" || renaming()?.id !== session().id}>
        <ContextMenu
          popupClass="w-44 rounded-xl border border-border bg-popover p-1 shadow-xl"
          trigger={
            <View
              class="group relative"
              onMouseEnter={() => hoveredSession.set(session().id)}
              onMouseLeave={() => hoveredSession.update((current) => current === session().id ? null : current)}
            >
              <Button
                testID={`session-${session().id}`}
                selected={store.current() === session().id}
                class={themeStore.classes(
                  "sidebar.session",
                  `w-full flex-row items-center rounded-(--radius) py-1.5 hover:bg-sidebar-accent group-hover:bg-sidebar-accent group-hover:pr-14 selected:bg-sidebar-accent ${detailed() ? "px-2" : "pl-8 pr-2"}`,
                )}
                onPress={() => store.openSession(session().id)}
              >
                <If when={!detailed()}>
                  <View class="min-w-0 flex-1 flex-row items-center gap-2">
                    <SessionTitle session={session} selected={() => store.current() === session().id} rowHovered={() => hoveredSession() === session().id} fontFamily={themeStore.fonts().ui} />
                    <View class="flex-row items-center gap-2 group-hover:hidden">
                      <SessionTiming store={store} session={session} />
                    </View>
                  </View>
                </If>
                <If when={detailed()}>
                  <View testID="session-detail" class="min-w-0 flex-1 gap-0.5">
                    <View class="flex-row items-center gap-2">
                      <SessionTitle session={session} selected={() => store.current() === session().id} rowHovered={() => hoveredSession() === session().id} fontFamily={themeStore.fonts().ui} />
                      <View class="flex-row items-center gap-2 group-hover:hidden">
                        <SessionTiming store={store} session={session} />
                      </View>
                    </View>
                    <View class="flex-row items-center gap-1">
                      <Icon name="folder-closed" size={12} color={token("muted-foreground")} />
                      <Text
                        testID={`session-workspace-${session().id}`}
                        style={{ numberOfLines: 1 }}
                        class="min-w-0 flex-1 text-left text-xs text-muted-foreground"
                      >
                        {workspaceName(session().cwd)}
                      </Text>
                    </View>
                  </View>
                </If>
              </Button>
              <View class="absolute right-1 top-0 bottom-0 hidden items-center gap-1 group-hover:flex">
                <Button
                  testID={`pin-${session().id}`}
                  accessibilityLabel={t(session().pinned ? "sidebar.unpin" : "sidebar.pin")}
                  class="h-6 w-6 flex-row items-center justify-center rounded-(--radius) hover:bg-foreground/10"
                  onPress={() => store.setSessionPinned(session().id, !session().pinned)}
                >
                  <Icon name={session().pinned ? "star-fill" : "star"} size={14} color={token("muted-foreground")} />
                </Button>
                <Button
                  testID={`archive-${session().id}`}
                  accessibilityLabel={t(session().archived ? "sidebar.unarchive" : "sidebar.archive")}
                  class="h-6 w-6 flex-row items-center justify-center rounded-(--radius) hover:bg-foreground/10"
                  onPress={() => store.setSessionArchived(session().id, !session().archived)}
                >
                  <Icon name={session().archived ? "undo-2" : "inbox"} size={14} color={token("muted-foreground")} />
                </Button>
              </View>
            </View>
          }
        >
          {sessionItems(session)}
        </ContextMenu>
      </If>
    </>
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
          accessibilityLabel={t("sidebar.collapse")}
          class="h-8 w-8 items-center justify-center rounded-(--radius) hover:bg-sidebar-accent active:bg-accent"
          onPress={props.onToggle}
        >
          <Icon name="panel-left" size={14} color={token("secondary-foreground")} />
        </Button>
      </View>
      <If when={props.mode === "workspace"}>
        <View class="gap-1 p-2">
          <Button
            testID="new-task"
            accessibilityLabel={t("sidebar.new_task")}
            class="group rounded-(--radius) px-2 py-1.5 hover:bg-sidebar-accent active:bg-accent"
            onPress={() => store.newTask()}
          >
            <View class="flex-row items-center gap-2">
              <Icon name="plus" />
              <Text class="text-sm text-sidebar-foreground">{t("sidebar.new_task")}</Text>
              <View class="flex-1" />
              <Text class="rounded border border-border px-1 text-[10px] text-muted-foreground opacity-0 group-hover:opacity-100">
                {platform.os === "macos" ? "⌘N" : "Ctrl+N"}
              </Text>
            </View>
          </Button>
          <Button
            testID="open-search"
            accessibilityLabel={t("sidebar.search")}
            class="group rounded-(--radius) px-2 py-1.5 hover:bg-sidebar-accent active:bg-accent"
            onPress={props.onOpenSearch}
          >
            <View class="flex-row items-center gap-2">
              <Icon name="search" />
              <Text class="text-sm text-sidebar-foreground">{t("sidebar.search")}</Text>
              <View class="flex-1" />
              <Text class="rounded border border-border px-1 text-[10px] text-muted-foreground opacity-0 group-hover:opacity-100">
                {platform.os === "macos" ? "⌘K" : "Ctrl+K"}
              </Text>
            </View>
          </Button>
        </View>
      </If>
      <If when={props.mode === "workspace" && sidebarView() === "workspace" && pinnedSessions().length > 0}>
        <View testID="pinned-sessions" class="gap-0.5 px-2 pb-1">
          <Text class="px-1 py-0.5 text-xs text-muted-foreground">{t("sidebar.pinned")}</Text>
          <Each of={pinnedSessions()} key={(session) => session.id}>
            {(session) => renderSessionRow(session, () => true)}
          </Each>
        </View>
      </If>
      <If when={props.mode === "workspace"}>
      <View class="flex-row items-center gap-1 pl-3 pr-2 py-0.5">
        <Text class="flex-1 text-xs text-muted-foreground">{t("sidebar.sessions")}</Text>
        <If when={sidebarView() === "workspace"}>
          <Button
            testID="toggle-workspace"
            accessibilityLabel={t(workspaceExpanded() ? "sidebar.collapse_workspace" : "sidebar.expand_workspace")}
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
        <Menu
          open={manageOpen()}
          onOpenChange={({ open: value }) => manageOpen.set(value)}
          placement="bottom"
          align="end"
          popupClass="w-44 rounded-xl border border-border bg-popover p-1 shadow-xl"
          trigger={
            <View role="button" testID="list-manage" class="h-6 w-6 items-center justify-center rounded-(--radius) hover:bg-sidebar-accent">
              <Icon name="sliders-horizontal" size={12} color={token("secondary-foreground")} />
            </View>
          }
        >
          <View class="gap-0.5">
            <Text class="px-2 pb-0.5 pt-1 text-xs text-muted-foreground">{t("sidebar.view")}</Text>
            <MenuItem
              testID="list-view-flat"
              class="flex-row items-center gap-2 rounded-(--radius) px-2 py-0.5 hover:bg-accent"
              onSelect={() => {
                sidebarView.set("flat");
                manageOpen.set(false);
              }}
            >
              <Icon name="list" size={14} color={token("muted-foreground")} />
              <Text class="flex-1 text-sm text-foreground">{t("sidebar.view_flat")}</Text>
              <If when={sidebarView() === "flat"}>
                <Icon name="check" size={14} color={token("foreground")} />
              </If>
            </MenuItem>
            <MenuItem
              testID="list-view-workspace"
              class="flex-row items-center gap-2 rounded-(--radius) px-2 py-0.5 hover:bg-accent"
              onSelect={() => {
                sidebarView.set("workspace");
                manageOpen.set(false);
              }}
            >
              <Icon name="folder-kanban" size={14} color={token("muted-foreground")} />
              <Text class="flex-1 text-sm text-foreground">{t("sidebar.group_by_workspace")}</Text>
              <If when={sidebarView() === "workspace"}>
                <Icon name="check" size={14} color={token("foreground")} />
              </If>
            </MenuItem>
          </View>
        </Menu>
      </View>
      </If>
      <If when={props.mode === "workspace" && sidebarView() === "flat"}>
        <List
          testID="session-list"
          each={flatSessions()}
          keyExtractor={(item) => item.id}
          class="flex-1"
          contentClass="gap-0.5 px-2 pb-1"
        >
          {(session) => renderSessionRow(session, () => true)}
        </List>
      </If>
      <If when={props.mode === "workspace" && sidebarView() === "workspace"}>
        <Scroll testID="session-list" class="flex-1" contentClass="gap-0.5 px-2 pb-1">
          <If when={!workspaceHidden()}>
            <Collapsible
              open={workspaceExpanded()}
              reveal
              content={
                <View>
                  <View class="gap-0.5 pt-0.5">
                    <Each of={firstPageSessions()} key={(session) => session.id}>
                      {(session) => renderSessionRow(session, () => false)}
                    </Each>
                  </View>
                  <Each of={morePages()} key="index">
                    {(page, index) => (
                      <Collapsible
                        open={workspacePageOpen() && workspaceShown() >= (index() + 2) * WORKSPACE_PAGE_SIZE}
                        reveal
                        content={<View class="gap-0.5 pt-0.5">
                          <Each of={page()} key={(session) => session.id}>
                            {(session) => renderSessionRow(session, () => false)}
                          </Each>
                        </View>}
                      />
                    )}
                  </Each>
                  <If when={canShowMore() || workspacePageOpen()}>
                    <View class="pl-5 pt-0.5">
                      <View class="flex-row items-center gap-3 px-2 py-0.5">
                        <If when={canShowMore()}>
                          <Button
                            testID="workspace-more"
                            accessibilityLabel={t("sidebar.show_more")}
                            class="flex-row items-center gap-1 rounded-(--radius) px-1 hover:bg-sidebar-accent"
                            onPress={showMore}
                          >
                            <Icon name="chevron-down" size={12} color={token("muted-foreground")} />
                            <Text class="text-xs text-muted-foreground">{t("sidebar.show_more")}</Text>
                          </Button>
                        </If>
                        <If when={canShowMore() && workspacePageOpen()}>
                          <Text class="text-xs text-muted-foreground">·</Text>
                        </If>
                        <If when={workspacePageOpen()}>
                          <Button
                            testID="workspace-collapse"
                            accessibilityLabel={t("sidebar.collapse_sessions")}
                            class="flex-row items-center gap-1 rounded-(--radius) px-1 hover:bg-sidebar-accent"
                            onPress={collapseSessions}
                          >
                            <Icon name="chevron-up" size={12} color={token("muted-foreground")} />
                            <Text class="text-xs text-muted-foreground">{t("sidebar.collapse_sessions")}</Text>
                          </Button>
                        </If>
                      </View>
                    </View>
                  </If>
                </View>
              }
            >
              {renderWorkspaceRow()}
            </Collapsible>
          </If>
        </Scroll>
      </If>
      <If when={props.mode === "settings"}>
        <View testID="settings-navigation" class="flex-1">
          <View class="gap-1 p-2">
            <Text class="px-2 pb-2 pt-1 text-xs font-medium text-muted-foreground">{t("sidebar.settings")}</Text>
            <Each of={SETTINGS_PAGES} key={(item) => item.id}>
              {(item) => (
                <Button
                  testID={`settings-${item().id}`}
                  selected={props.settingsPage === item().id}
                  accessibilityLabel={t("sidebar.open_page", { name: t(item().titleKey) })}
                  class="flex-row items-center gap-2 rounded-(--radius) px-3 py-2 hover:bg-sidebar-accent selected:bg-sidebar-accent"
                  onPress={() => props.onNavigateSettings(item().id)}
                >
                  <Icon name={item().icon} size={15} />
                  <Text class="text-sm text-sidebar-foreground">{t(item().titleKey)}</Text>
                </Button>
              )}
            </Each>
          </View>
        </View>
      </If>
      <View class="gap-1 border-t border-sidebar-border p-2">
        <If when={props.mode === "workspace" && (Boolean(store.relayStatus()?.configured) || store.activeDevice() !== null)}>
          <Menu
            open={deviceMenuOpen()}
            onOpenChange={({ open: value }) => {
              deviceMenuOpen.set(value);
              if (value) void store.refreshRelayDevices();
            }}
            placement="top"
            align="start"
            popupClass="w-56 rounded-xl border border-border bg-popover p-1 shadow-xl"
            trigger={
              <View
                role="button"
                testID="device-switcher"
                accessibilityLabel={t("sidebar.device.label")}
                class="flex-row items-center gap-2 rounded-(--radius) px-2 py-1 hover:bg-sidebar-accent"
              >
                <Icon
                  name={store.activeDevice() ? "wifi" : "terminal"}
                  size={14}
                  color={store.activeDevice() ? token("primary") : token("muted-foreground")}
                />
                <Text style={{ numberOfLines: 1 }} class="min-w-0 flex-1 text-left text-xs text-sidebar-foreground">
                  {store.switchingDevice()
                    ? t("sidebar.device.switching")
                    : (store.activeDevice()?.name ?? t("sidebar.device.local_short"))}
                </Text>
                <If when={store.activeDevice() !== null}>
                  <Text class="rounded bg-primary/15 px-1 text-[10px] text-primary">
                    {t("sidebar.device.remote_badge")}
                  </Text>
                </If>
              </View>
            }
          >
            <View class="gap-0.5">
              <MenuItem
                testID="device-switch-local"
                class="flex-row items-center gap-2 rounded-(--radius) px-2 py-1 hover:bg-accent"
                onSelect={() => {
                  deviceMenuOpen.set(false);
                  void store.selectDevice(null);
                }}
              >
                <Icon name="terminal" size={14} color={token("muted-foreground")} />
                <Text class="flex-1 text-sm text-foreground">{t("sidebar.device.local")}</Text>
                <If when={store.activeDevice() === null}>
                  <Icon name="check" size={14} color={token("foreground")} />
                </If>
              </MenuItem>
              <Each
                of={store.relayDevices().filter((d) => d.device_id !== store.relayStatus()?.device_id)}
                key={(device) => device.device_id}
              >
                {(device) => (
                  <MenuItem
                    testID={`device-switch-${device().device_id}`}
                    disabled={!device().online}
                    class="flex-row items-center gap-2 rounded-(--radius) px-2 py-1 hover:bg-accent disabled:opacity-40"
                    onSelect={() => {
                      if (!device().online) return;
                      deviceMenuOpen.set(false);
                      void store.selectDevice(device());
                    }}
                  >
                    <Icon
                      name={device().online ? "wifi" : "wifi-off"}
                      size={14}
                      color={device().online ? token("primary") : token("muted-foreground")}
                    />
                    <Text style={{ numberOfLines: 1 }} class="min-w-0 flex-1 text-sm text-foreground">
                      {device().name}
                    </Text>
                    <If when={!device().online}>
                      <Text class="text-[10px] text-muted-foreground">{t("sidebar.device.offline")}</Text>
                    </If>
                    <If when={store.activeDevice()?.device_id === device().device_id}>
                      <Icon name="check" size={14} color={token("foreground")} />
                    </If>
                  </MenuItem>
                )}
              </Each>
              <MenuSeparator class="my-0.5 h-px bg-border" />
              <MenuItem
                testID="device-manage-remote"
                class="flex-row items-center gap-2 rounded-(--radius) px-2 py-1 hover:bg-accent"
                onSelect={() => {
                  deviceMenuOpen.set(false);
                  props.onNavigateSettings("remote");
                }}
              >
                <Icon name="settings" size={14} color={token("muted-foreground")} />
                <Text class="flex-1 text-sm text-foreground">{t("sidebar.device.manage")}</Text>
              </MenuItem>
            </View>
          </Menu>
        </If>
        <Button
          testID={props.mode === "settings" ? "settings-back" : "settings-open"}
          accessibilityLabel={t(props.mode === "settings" ? "sidebar.return_workspace" : "sidebar.open_settings")}
          class="rounded-(--radius) px-2 py-0.5 hover:bg-sidebar-accent active:bg-accent"
          onPress={props.mode === "settings" ? props.onBackSettings : props.onOpenSettings}
        >
          <View class="flex-row items-center gap-2">
            <Icon name="settings" />
            <Text class="text-sm text-sidebar-foreground">{t(props.mode === "settings" ? "sidebar.return_workspace" : "sidebar.settings")}</Text>
          </View>
        </Button>
      </View>
    </View>
  );
}
