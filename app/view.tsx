import { Animated, Easing, Image, LayoutAnimation, View, createAnimatedValue, effect, keymap, mount, reduceMotion, signal, untracked } from "nsh";
import type { JSX, Signal } from "nsh";
import { ResizablePanel, ResizablePanelGroup } from "nsh/base";
import { Window } from "nsh/desktop";
import { AgentStore } from "./store";
import { ThemeStore } from "./theme";
import { ThemePicker } from "./theme-picker";
import type { UiContext } from "./ui-context";
import { Sidebar } from "./components/sidebar";
import { TitleBar } from "./components/title-bar";
import { Workspace } from "./components/workspace";
import { RightPanel } from "./components/right-panel";
import { TerminalPanel } from "./components/terminal-panel";
import { SearchPopup } from "./components/search-popup";
import { SettingsPage } from "./components/settings/settings-page";

Window.setTitle("Pig Code");
Window.setTitleBar("hidden", 44);

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
              resizeMode="cover"
              class={props.themeStore.classes(
                "app.background",
                `absolute inset-0 ${background.blur ? `blur-[${background.blur}px]` : ""}`,
              )}
              style={props.themeStore.styles("app.background", { opacity: background.opacity ?? 1 })}
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
  side: "left" | "right";
  open: boolean;
  width: number;
  minSize: number;
  animating: Signal<boolean>;
  onResize: (width: number) => void;
  children: JSX.Element;
}) {
  const direction = props.side === "left" ? -1 : 1;
  const presented = signal(props.open);
  const animating = props.animating;
  const slide = createAnimatedValue(props.open ? 0 : direction * props.width);
  let shown = props.open;
  let generation = 0;
  effect(() => {
    const open = props.open;
    if (shown === open) return;
    shown = open;
    const width = untracked(() => props.width);
    generation += 1;
    const current = generation;
    if (reduceMotion()) {
      slide.set(open ? 0 : direction * width);
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
      toValue: open ? 0 : direction * width,
      duration: 220,
      easing: Easing.bezier(0, 0, 0.58, 1),
    }).start();
    if (open) presented.set(true);
    setTimeout(finish, 220);
  });
  return (
    <ResizablePanel
      size={props.open ? props.width : 0}
      minSize={props.open ? props.minSize : 0}
      fixed
      visible={presented()}
      onResize={props.onResize}
    >
      <View
        class="relative h-full w-full overflow-hidden"
        importantForAccessibility={props.open ? "yes" : "no-hide-descendants"}
      >
        <View
          class="absolute bottom-0 top-0"
          style={props.side === "left"
            ? { left: 0, width: animating() ? props.width : "100%", transform: [{ translateX: slide }] }
            : { right: 0, width: animating() ? props.width : "100%", transform: [{ translateX: slide }] }}
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
  const settingsVisible = signal(false);
  const themePickerVisible = signal(false);
  const searchVisible = signal(false);
  const panelsAnimating = () => sidebarAnimating() || rightAnimating();
  const saveTheme = () => store.saveThemePreference(themeStore.activeId(), themeStore.mode());
  const toggleSidebar = (visible: boolean) => {
    sidebarVisible.set(visible);
  };
  const toggleRight = () => {
    rightVisible.update((visible) => !visible);
  };
  const toggleTerminal = () => {
    LayoutAnimation.configureNext(LayoutAnimation.create(200));
    terminalVisible.update((visible) => !visible);
  };
  effect(() => {
    store.start();
  });
  effect(() => {
    const config = store.config();
    if (config) {
      themeStore.hydrate(config.theme, config.theme_mode);
      themeStore.hydrateFonts(config.ui_font, config.mono_font);
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
            open={sidebarVisible() && !settingsVisible()}
            width={sidebarWidth()}
            minSize={200}
            animating={sidebarAnimating}
            onResize={(width) => sidebarWidth.set(width)}
          >
            <Sidebar
              {...props}
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
                rightVisible={rightVisible()}
                terminalVisible={terminalVisible()}
                onToggleSidebar={() => toggleSidebar(true)}
                onToggleRight={toggleRight}
                onToggleTerminal={toggleTerminal}
                onOpenThemes={() => themePickerVisible.set(true)}
                onSaveTheme={saveTheme}
              />
              <If when={!settingsVisible()}><Workspace {...props} /></If>
              <If when={terminalVisible() && !settingsVisible()}>
                <TerminalPanel {...props} onClose={toggleTerminal} />
              </If>
              <If when={settingsVisible()}>
                <SettingsPage
                  {...props}
                  onBack={() => settingsVisible.set(false)}
                  onOpenThemes={() => themePickerVisible.set(true)}
                />
              </If>
            </View>
          </ResizablePanel>
          <AnimatedResizablePanel
            side="right"
            open={rightVisible() && !settingsVisible()}
            width={rightWidth()}
            minSize={280}
            animating={rightAnimating}
            onResize={(width) => rightWidth.set(width)}
          >
            <RightPanel {...props} onClose={toggleRight} />
          </AnimatedResizablePanel>
        </ResizablePanelGroup>
      </View>
      <If when={themePickerVisible()}>
        <ThemePicker
          store={themeStore}
          packages={store.pluginPackages()}
          error={store.themeError()}
          onClose={() => themePickerVisible.set(false)}
          onChange={saveTheme}
          onTogglePackage={(plugin) => {
            const current = store.pluginPackages().find((item) => item.id === plugin.id) ?? plugin;
            store.setPluginEnabled(current, !current.enabled);
          }}
          onRefreshPackages={() => store.syncPlugins()}
        />
      </If>
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
