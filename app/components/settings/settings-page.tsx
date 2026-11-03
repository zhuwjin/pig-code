import { View } from "nsh";
import type { MessageKey } from "../../i18n";
import type { UiContext } from "../../ui-context";
import type { IconName } from "../icon";
import { ModelSettings } from "./model-settings";
import { PluginMarketplace } from "./marketplace";
import { ThemePicker } from "../../theme-picker";
import { hostPatchProps, UiNodeFactory } from "../../plugins/ui-node-factory";

export type SettingsPageId = "models" | "appearance" | "plugins" | "marketplace";

export const SETTINGS_PAGES: { id: SettingsPageId; titleKey: MessageKey; icon: IconName }[] = [
  { id: "models", titleKey: "settings.nav.models", icon: "settings" },
  { id: "appearance", titleKey: "settings.nav.appearance", icon: "sun" },
  { id: "plugins", titleKey: "settings.nav.plugins", icon: "sliders-horizontal" },
  { id: "marketplace", titleKey: "settings.nav.marketplace", icon: "globe" },
];

export function SettingsPage(props: UiContext & { page: SettingsPageId }) {
  const pickerProps = (section: "appearance" | "plugins") => ({
    section,
    store: props.themeStore,
    packages: () => props.store.pluginPackages(),
    fontFamilies: () => props.store.fontFamilies(),
    runtime: () => props.store.pluginRuntime(),
    diagnostics: () => props.store.pluginUiDiagnostics(),
    logs: () => props.store.pluginLogs(),
    onRetryPackage: (plugin: Parameters<UiContext["store"]["retryPlugin"]>[0]) => props.store.retryPlugin(plugin),
    error: () => section === "plugins"
      ? props.store.globalError() || props.store.themeError()
      : props.store.themeError(),
    onChange: () => props.store.saveThemePreference(props.themeStore.activeId(), props.themeStore.mode()),
    onLanguageChange: (language: Parameters<UiContext["store"]["saveLanguage"]>[0]) => props.store.saveLanguage(language),
    onFontsChange: (ui: string, mono: string) => props.store.saveFonts(ui, mono),
    onTogglePackage: (plugin: Parameters<UiContext["store"]["setPluginEnabled"]>[0]) => {
      const current = props.store.pluginPackages().find((item) => item.id === plugin.id) ?? plugin;
      props.store.setPluginEnabled(current, !current.enabled);
    },
    onRefreshPackages: () => props.store.syncPlugins(),
    onInstallPlugin: (packagePath: string, rootPath: string) => props.store.installPlugin(packagePath, rootPath),
    onUninstallPlugin: (plugin: Parameters<UiContext["store"]["uninstallPlugin"]>[0]) => props.store.uninstallPlugin(plugin),
  });
  return (
    <View testID="settings-page" class={props.themeStore.classes("settings.root", "min-w-0 flex-1 bg-background")}>
      <UiNodeFactory
        uiId="settings.content"
        store={props.store}
        render={(patch, children) => (
          <View class="min-w-0 flex-1" style={patch.style} {...hostPatchProps(patch)}>
            {children ?? (
              <>
                <If when={props.page === "models"}><ModelSettings {...props} /></If>
                <If when={props.page === "appearance"}><ThemePicker {...pickerProps("appearance")} /></If>
                <If when={props.page === "plugins"}><ThemePicker {...pickerProps("plugins")} /></If>
                <If when={props.page === "marketplace"}><PluginMarketplace {...props} /></If>
              </>
            )}
          </View>
        )}
      />
    </View>
  );
}
