import { Animated, Easing, Image, LayoutAnimation, View, createAnimatedValue, effect, keymap, mount, reduceMotion, signal, untracked } from "nsh";
import type { JSX, Signal } from "nsh";
import { ResizablePanel, ResizablePanelGroup } from "nsh/base";
import { Window } from "nsh/desktop";
import { AgentStore } from "./store";
import { ThemeStore } from "./theme";
import { appLocale, setLanguagePreference } from "./i18n";
import type { UiContext } from "./ui-context";
import { Sidebar } from "./components/sidebar";
import { TitleBar } from "./components/title-bar";
import { Workspace } from "./components/workspace";
import { RightPanel } from "./components/right-panel";
import { TerminalPanel } from "./components/terminal-panel";
import { SearchPopup } from "./components/search-popup";
import { SettingsPage, type SettingsPageId } from "./components/settings/settings-page";

Window.setTitle("Pig Code");
Window.setTitleBar({ style: "hidden", height: 44 });

function ThemeBackground(props: UiContext) {
  const background = () => props.themeStore.background();
  return (
    <If when={background()}>
      {(value) => {
        const background = value();
        return (
          <View class="absolute inset-0 pointer-events-none">
            <Image
              src={background.asset}
              class={props.themeStore.classes(
                "app.background",
                `absolute inset-0 ${background.blur ? `blur-[${background.blur}px]` : ""}`,
              )}
              style={props.themeStore.styles("app.background", { objectFit: "cover", opacity: background.opacity ?? 1 })}
            />
            <If when={background.overlay}>
              <View class={`absolute inset-0 ${background.overlay ?? ""}`} />
            </If>
          </View>
        );
      }}
    </If>
  );
}

function AnimatedResizablePanel(props: {
  side: "left" | "right" | "bottom";
  open: boolean;
  size: number;
  minSize: number;
  animating: Signal<boolean>;
  onResize: (size: number) => void;
  children: JSX.Element;
}) {
  const direction = props.side === "left" ? -1 : 1;
  const presented = signal(props.open);
  const animating = props.animating;
  const slide = createAnimatedValue(props.open ? 0 : direction * props.size);
  let shown = props.open;
  let generation = 0;
  effect(() => {
    const open = props.open;
    if (shown === open) return;
    shown = open;
    const size = untracked(() => props.size);
    generation += 1;
    const current = generation;
    if (reduceMotion()) {
      slide.set(open ? 0 : direction * size);
      presented.set(open);
      animating.set(false);
      return;
    }
    const finish = () => {
      if (generation !== current) return;
      animating.set(false);
      presented.set(open);
    };
    animating.set(true);
    LayoutAnimation.configureNext({ duration: 220, update: { type: "easeOut" } });
    Animated.timing(slide, {
      toValue: open ? 0 : direction * size,
      duration: 220,
      easing: Easing.bezier(0, 0, 0.58, 1),
    }).start();
    if (open) presented.set(true);
    setTimeout(finish, 220);
  });
  return (
    <ResizablePanel
      size={props.open ? props.size : 0}
      minSize={props.open ? props.minSize : 0}
      fixed
      visible={presented()}
      onResize={({ size }) => props.onResize(size)}
    >
      <View
        class="relative h-full w-full overflow-hidden"
        importantForAccessibility={props.open ? "yes" : "no-hide-descendants"}
      >
        <View
          class={props.side === "bottom" ? "absolute bottom-0 left-0" : "absolute bottom-0 top-0"}
          style={props.side === "bottom"
            ? { left: 0, width: "100%", height: animating() ? props.size : "100%", transform: [{ translateY: slide }] }
            : props.side === "left"
              ? { left: 0, width: animating() ? props.size : "100%", transform: [{ translateX: slide }] }
              : { right: 0, width: animating() ? props.size : "100%", transform: [{ translateX: slide }] }}
        >
          {props.children}
        </View>
      </View>
    </ResizablePanel>
  );
}

const HANDLE_THUMB = "h-11 w-[3px] rounded-full bg-muted-foreground opacity-0 scale-y-0 transition duration-150 ease-out group-hover:opacity-40 group-hover:scale-y-[0.45] group-active:opacity-90 group-active:scale-y-100";
const HANDLE_THUMB_HIDDEN = "h-11 w-[3px] rounded-full bg-muted-foreground opacity-0 scale-y-0";

function App(props: UiContext) {
  const { store, themeStore } = props;
  const sidebarVisible = signal(true);
  const sidebarWidth = signal(220);
  const sidebarAnimating = signal(false);
  const rightVisible = signal(false);
  const rightWidth = signal(300);
  const rightAnimating = signal(false);
  const terminalVisible = signal(false);
  const terminalHeight = signal(300);
  const terminalAnimating = signal(false);
  const terminalStarted = signal(false);
  const settingsVisible = signal(false);
  const settingsPage = signal<SettingsPageId>("models");
  const searchVisible = signal(false);
  const panelsAnimating = () => sidebarAnimating() || rightAnimating();
  const saveTheme = () => store.saveThemePreference(themeStore.activeId(), themeStore.mode());
  const openAppearance = () => {
    settingsPage.set("appearance");
    settingsVisible.set(true);
  };
  const toggleSidebar = (visible: boolean) => {
    sidebarVisible.set(visible);
  };
  const toggleRight = () => {
    rightVisible.update((visible) => !visible);
  };
  const toggleTerminal = () => {
    terminalStarted.set(true);
    terminalVisible.update((visible) => !visible);
  };
  effect(() => {
    store.start();
  });
  effect(() => {
    store.agent.setPluginLocale(appLocale());
  });
  effect(() => {
    const target = store.pluginNavigation();
    if (!target) return;
    if (target !== "main") settingsPage.set(target as SettingsPageId);
    settingsVisible.set(target !== "main");
    store.pluginNavigation.set(null);
  });
  effect(() => {
    const request = store.rightPanelRequest();
    if (!request) return;
    settingsVisible.set(false);
    rightVisible.set(true);
  });
  effect(() => {
    const config = store.config();
    if (config) {
      themeStore.hydrate(config.theme, config.theme_mode);
      themeStore.hydrateFonts(config.ui_font, config.mono_font);
      setLanguagePreference(config.language === "zh-CN" || config.language === "en" ? config.language : "system");
    }
  });
  effect(() => {
    themeStore.syncPlugins(store.pluginThemes());
  });
  effect(() => keymap.bind({
    "mod+k": () => searchVisible.update((visible) => !visible),
    "mod+n": () => {
      searchVisible.set(false);
      settingsVisible.set(false);
      store.newTask();
    },
  }, { when: "always" }));
  return (
    <View
      // @ts-expect-error NSH's reactive uiTheme prop is not in its public TS declarations yet.
      uiTheme={themeStore.serialized()}
      class={themeStore.classes(
        "app.root",
        "relative flex-1 bg-background text-foreground selection:bg-selection pt-[env(safe-area-inset-top)] pr-[env(safe-area-inset-right)] pb-[env(safe-area-inset-bottom)] pl-[env(safe-area-inset-left)]",
      )}
    >
      <ThemeBackground {...props} />
      <View class="relative flex-1">
        <ResizablePanelGroup
          class="flex-1"
          disabled={panelsAnimating()}
          handleClass={panelsAnimating() ? "w-px bg-transparent" : "w-px bg-border"}
          handleThumbClass={panelsAnimating() ? HANDLE_THUMB_HIDDEN : HANDLE_THUMB}
          handleAccessibilityLabel="Resize the sidebar"
        >
          <AnimatedResizablePanel
            side="left"
            open={sidebarVisible()}
            size={sidebarWidth()}
            minSize={200}
            animating={sidebarAnimating}
            onResize={(width) => sidebarWidth.set(width)}
          >
            <Sidebar
              {...props}
              mode={settingsVisible() ? "settings" : "workspace"}
              settingsPage={settingsPage()}
              onNavigateSettings={(page) => settingsPage.set(page)}
              onBackSettings={() => settingsVisible.set(false)}
              onToggle={() => toggleSidebar(false)}
              onOpenSettings={() => settingsVisible.set(true)}
              onOpenSearch={() => searchVisible.set(true)}
            />
          </AnimatedResizablePanel>
          <ResizablePanel minSize={480}>
            <View class={themeStore.classes("app.content", "min-w-0 flex-1")}>
              <TitleBar
                {...props}
                sidebarVisible={sidebarVisible()}
                rightPanelShown={rightVisible() && !settingsVisible()}
                terminalVisible={terminalVisible()}
                onToggleSidebar={() => toggleSidebar(true)}
                onToggleRight={toggleRight}
                onToggleTerminal={toggleTerminal}
                onOpenThemes={openAppearance}
                onSaveTheme={saveTheme}
              />
              <ResizablePanelGroup
                axis="vertical"
                class="min-h-0 flex-1"
                disabled={terminalAnimating()}
                handleClass={terminalAnimating() ? "h-px w-full bg-transparent" : "h-px w-full bg-border"}
                handleThumbClass={terminalAnimating() ? HANDLE_THUMB_HIDDEN : HANDLE_THUMB}
                handleAccessibilityLabel="Resize the terminal"
              >
                <ResizablePanel minSize={160}>
                  <View class="min-h-0 flex-1">
                    <If when={!settingsVisible()}><Workspace {...props} /></If>
                    <If when={settingsVisible()}>
                      <SettingsPage
                        {...props}
                        page={settingsPage()}
                      />
                    </If>
                  </View>
                </ResizablePanel>
                <If when={terminalStarted()}>
                  <AnimatedResizablePanel
                    side="bottom"
                    open={terminalVisible() && !settingsVisible()}
                    size={terminalHeight()}
                    minSize={100}
                    animating={terminalAnimating}
                    onResize={(height) => terminalHeight.set(height)}
                  >
                    <TerminalPanel {...props} onClose={toggleTerminal} />
                  </AnimatedResizablePanel>
                </If>
              </ResizablePanelGroup>
            </View>
          </ResizablePanel>
          <AnimatedResizablePanel
            side="right"
            open={rightVisible() && !settingsVisible()}
            size={rightWidth()}
            minSize={280}
            animating={rightAnimating}
            onResize={(width) => rightWidth.set(width)}
          >
            <RightPanel {...props} onClose={toggleRight} />
          </AnimatedResizablePanel>
        </ResizablePanelGroup>
      </View>
      <If when={searchVisible()}>
        <SearchPopup {...props} onClose={() => searchVisible.set(false)} />
      </If>
    </View>
  );
}

export function mountApp(selftest = false): void {
  const themeStore = new ThemeStore();
  const store = new AgentStore(selftest);
  mount(() => <App store={store} themeStore={themeStore} />);
}
