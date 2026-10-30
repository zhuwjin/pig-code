import { Image, View, effect, mount, signal } from "nsh";
import { Window } from "nsh/desktop";
import { AgentStore } from "./store";
import { ThemeStore } from "./theme";
import { ThemePicker } from "./theme-picker";
import type { UiContext } from "./ui-context";
import { Sidebar } from "./components/sidebar";
import { TitleBar } from "./components/title-bar";
import { Workspace } from "./components/workspace";
import { WindowGuard } from "./components/window-guard";

Window.setTitle("Pig Code");
Window.setTitleBar("hidden", 45);

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

function App(props: UiContext) {
  const { store, themeStore } = props;
  const sidebarVisible = signal(true);
  const themePickerVisible = signal(false);
  const saveTheme = () => store.saveThemePreference(themeStore.activeId(), themeStore.mode());
  effect(() => {
    store.start();
  });
  effect(() => {
    const config = store.config();
    if (config) themeStore.hydrate(config.theme, config.theme_mode);
  });
  effect(() => {
    themeStore.syncPlugins(store.pluginThemes());
  });
  return (
    <View
      // @ts-expect-error NSH's reactive uiTheme prop is not in its public TS declarations yet.
      uiTheme={themeStore.serialized()}
      class={themeStore.classes(
        "app.root",
        "relative flex-1 flex-row bg-background text-foreground selection:bg-selection pt-[env(safe-area-inset-top)] pr-[env(safe-area-inset-right)] pb-[env(safe-area-inset-bottom)] pl-[env(safe-area-inset-left)]",
      )}
      onLayout={(bounds) => store.reportLayout(bounds.size.width, bounds.size.height)}
    >
      <WindowGuard />
      <ThemeBackground {...props} />
      <If when={sidebarVisible()}>
        <Sidebar
          {...props}
          onToggle={() => sidebarVisible.set(false)}
          onOpenThemes={() => themePickerVisible.set(true)}
        />
      </If>
      <View class={themeStore.classes("app.content", "min-w-0 flex-1")}>
        <TitleBar
          {...props}
          sidebarVisible={sidebarVisible()}
          onToggleSidebar={() => sidebarVisible.set(true)}
          onOpenThemes={() => themePickerVisible.set(true)}
          onSaveTheme={saveTheme}
        />
        <Workspace {...props} />
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
    </View>
  );
}

export function mountApp(selftest = false): void {
  const themeStore = new ThemeStore();
  const store = new AgentStore(selftest);
  mount(() => <App store={store} themeStore={themeStore} />);
}
