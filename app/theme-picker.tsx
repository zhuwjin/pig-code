import { colorScheme, Scroll, Text, token, View, signal } from "nsh";
import { Button, Input } from "nsh/base";
import { PluginMarketplacePackage, PluginPackageInfo, PluginRuntimeState } from "./protocol";
import { Icon } from "./components/icon";
import { t } from "./i18n";
import type { MessageKey } from "./i18n";
import { ThemeStore } from "./theme";
import { DEFAULT_THEME_ID, ThemeDefinition, ThemeMode, themePreviewColor } from "./themes";
import { pluginInstance } from "./plugins/ui-coordinator";

const MODE_OPTIONS: { value: ThemeMode; label: MessageKey }[] = [
  { value: "light", label: "settings.appearance.mode.light" },
  { value: "dark", label: "settings.appearance.mode.dark" },
  { value: "system", label: "settings.appearance.mode.system" },
];

type ThemeFilter = "all" | "light" | "dark";

const THEME_FILTERS: { value: ThemeFilter; label: MessageKey }[] = [
  { value: "all", label: "settings.appearance.filter.all" },
  { value: "light", label: "settings.appearance.filter.light" },
  { value: "dark", label: "settings.appearance.filter.dark" },
];

function themeModeOf(theme: ThemeDefinition): "light" | "dark" {
  return theme.mode ?? (colorScheme() === "dark" ? "dark" : "light");
}

function MiniWindow(props: { dark: boolean }) {
  const wallpaper = props.dark ? "#26262b" : "#dfe4ec";
  const window = props.dark ? "#1b1b1e" : "#ffffff";
  const bar = props.dark ? "#3a3a42" : "#ccd2db";
  return (
    <View class="h-[60px] w-[100px] shrink-0 p-[7px]" style={{ backgroundColor: wallpaper }}>
      <View class="h-full w-full overflow-hidden rounded-[4px]" style={{ backgroundColor: window }}>
        <View class="h-[11px] w-full flex-row items-center gap-[2px] px-1" style={{ backgroundColor: bar }}>
          <View class="h-[3px] w-[3px] rounded-full" style={{ backgroundColor: "#ff5f57" }} />
          <View class="h-[3px] w-[3px] rounded-full" style={{ backgroundColor: "#febc2e" }} />
          <View class="h-[3px] w-[3px] rounded-full" style={{ backgroundColor: "#28c840" }} />
        </View>
        <View class="gap-[3px] p-[5px]">
          <View class="h-[3px] w-[30px] rounded-full" style={{ backgroundColor: "#3b82f6" }} />
          <View class="h-[3px] w-[54px] rounded-full" style={{ backgroundColor: bar }} />
          <View class="h-[3px] w-[40px] rounded-full" style={{ backgroundColor: bar }} />
        </View>
      </View>
    </View>
  );
}

function ModeCard(props: { option: { value: ThemeMode; label: MessageKey }; selected: boolean; onPress: () => void }) {
  const value = () => props.option.value;
  return (
    <Button testID={`theme-mode-${props.option.value}`} class="items-center gap-1.5" onPress={props.onPress}>
      <View class={`h-16 w-[104px] overflow-hidden rounded-(--radius) border-2 ${props.selected ? "border-primary" : "border-border"}`}>
        <If when={value() === "system"}>
          <View class="h-full w-full flex-row">
            <View class="h-full w-[50px] overflow-hidden"><MiniWindow dark={false} /></View>
            <View class="h-full w-[50px] flex-row justify-end overflow-hidden"><MiniWindow dark={true} /></View>
          </View>
        </If>
        <If when={value() === "light"}><MiniWindow dark={false} /></If>
        <If when={value() === "dark"}><MiniWindow dark={true} /></If>
      </View>
      <View class="flex-row items-center gap-1.5">
        <View class={`h-3.5 w-3.5 items-center justify-center rounded-full border-2 ${props.selected ? "border-primary" : "border-muted-foreground"}`}>
          <If when={props.selected}><View class="h-1.5 w-1.5 rounded-full bg-primary" /></If>
        </View>
        <Text class="text-sm text-foreground">{t(props.option.label)}</Text>
      </View>
    </Button>
  );
}

function ThemePackageRow(props: {
  item: () => PluginPackageInfo;
  runtime?: PluginRuntimeState;
  onToggle: (plugin: PluginPackageInfo) => void;
  onRetry?: () => void;
  onRemove: () => void;
}) {
  const plugin = () => props.item();
  const runtime = () => props.runtime;
  return (
    <View class="flex-row items-center gap-1">
      <Button
        testID={`theme-package-${plugin().id}`}
        selected={plugin().enabled}
        accessibilityLabel={t(plugin().enabled ? "settings.plugins.toggle.disable" : "settings.plugins.toggle.enable", { name: plugin().name })}
        class="flex-1 rounded-(--radius) border border-border px-3 py-2 hover:bg-accent/50 selected:border-primary/60 selected:bg-accent/30"
        onPress={() => props.onToggle(plugin())}
      >
        <View class="flex-row items-center gap-2">
          <View class="min-w-0 flex-1 gap-0.5">
            <Text numberOfLines={1} class="text-sm font-medium text-foreground">{plugin().name}</Text>
            <Text numberOfLines={1} class="text-xs text-muted-foreground">{plugin().id} · {plugin().version}</Text>
          </View>
          <View class="items-end">
            <Text class="text-xs text-muted-foreground">{runtime()?.state ?? t(plugin().enabled ? "settings.plugins.state.enabled" : "settings.plugins.state.disabled")}</Text>
            <If when={runtime()}>
              <Text class="text-[10px] text-muted-foreground">
                {t("settings.plugins.metrics.resources", {
                  epoch: runtime()?.epoch ?? 0,
                  nodes: runtime()?.metrics?.nodes ?? 0,
                  operations: runtime()?.metrics?.operations ?? 0,
                  requests: runtime()?.metrics?.pendingRequests ?? 0,
                  queue: runtime()?.metrics?.queueDepth ?? 0,
                })}
              </Text>
              <Text class="text-[10px] text-muted-foreground">
                {t("settings.plugins.metrics.network", {
                  network: runtime()?.metrics?.networkBytes ?? 0,
                  storage: runtime()?.metrics?.storageBytes ?? 0,
                  timeout: runtime()?.metrics?.timedOutRequests ?? 0,
                  cancelled: runtime()?.metrics?.cancelledRequests ?? 0,
                  dropped: runtime()?.metrics?.droppedEvents ?? 0,
                })}
              </Text>
            </If>
            <If when={runtime()?.message}>
              <Text numberOfLines={2} class="max-w-36 text-[10px] text-destructive">{runtime()?.message}</Text>
            </If>
          </View>
        </View>
      </Button>
      <If when={runtime()?.state === "failed"}>
        <Button testID={`plugin-retry-${plugin().id}`} class="rounded-(--radius) px-2 py-1 hover:bg-muted" onPress={props.onRetry}>
          <Text class="text-xs">{t("settings.plugins.retry")}</Text>
        </Button>
      </If>
      <Button testID={`plugin-uninstall-${plugin().id}`} accessibilityLabel={t("settings.plugins.uninstall.label", { name: plugin().name })} class="rounded-(--radius) px-2 py-1 hover:bg-destructive/10" onPress={props.onRemove}>
        <Text class="text-xs text-destructive">{t("settings.plugins.uninstall.action")}</Text>
      </Button>
    </View>
  );
}

interface ThemePickerProps {
  section: "appearance" | "plugins";
  store: ThemeStore;
  packages: () => PluginPackageInfo[];
  catalog: () => PluginMarketplacePackage[];
  runtime?: () => Record<string, PluginRuntimeState>;
  diagnostics?: () => Record<string, string[]>;
  logs?: () => Record<string, unknown[]>;
  onRetryPackage?: (plugin: PluginPackageInfo) => void;
  error?: () => string | undefined;
  onChange: () => void;
  onTogglePackage: (plugin: PluginPackageInfo) => void;
  onRefreshPackages: () => void;
  onInstallPlugin: (packagePath: string, rootPath: string) => Promise<void>;
  onRefreshMarketplace: () => Promise<void>;
  onInstallMarketplace: (plugin: PluginMarketplacePackage, rootPath: string) => Promise<void>;
  onUninstallPlugin: (plugin: PluginPackageInfo) => void;
}

export function ThemePicker(props: ThemePickerProps) {
  const store = props.store;
  const pendingUninstall = signal<PluginPackageInfo | null>(null);
  const packagePath = signal("");
  const rootPath = signal("");
  const installing = signal(false);
  const themeFilter = signal<ThemeFilter>("all");
  const themeQuery = signal("");
  const filteredThemes = () => {
    const query = themeQuery().trim().toLowerCase();
    const filter = themeFilter();
    return store.selectableThemes().filter((theme) => {
      if (filter !== "all" && themeModeOf(theme) !== filter) return false;
      return !query || theme.name.toLowerCase().includes(query) ||
        (theme.description ?? "").toLowerCase().includes(query);
    });
  };
  const chooseMode = (choice: ThemeMode) => {
    const active = store.active();
    if (choice === "system") {
      if (active.mode) store.select(DEFAULT_THEME_ID);
      store.setMode(choice);
    } else if (active.mode && active.mode !== choice) {
      const base = active.name.replace(/ (Light|Dark)$/i, "");
      const match = store.selectableThemes().find((theme) =>
        theme.mode === choice && theme.name.replace(/ (Light|Dark)$/i, "") === base
      );
      if (match) store.select(match.id);
      else {
        store.select(DEFAULT_THEME_ID);
        store.setMode(choice);
      }
    } else {
      store.setMode(choice);
    }
    props.onChange();
  };
  const activeStylePacks = () => store.activeStylePacks();
  return (
    <View
      testID={props.section === "appearance" ? "theme-picker" : "plugin-manager-page"}
      class="relative min-w-0 flex-1 bg-background"
    >
      <Scroll class="flex-1">
        <View class="w-full max-w-[960px] gap-6 p-6">
          <View class="gap-2 border-b border-border pb-4">
            <Text class="text-xl font-semibold text-foreground">
              {t(props.section === "appearance" ? "settings.appearance.title" : "settings.plugins.title")}
            </Text>
            <Text class="text-sm text-muted-foreground">
              {t(props.section === "appearance" ? "settings.appearance.description" : "settings.plugins.description")}
            </Text>
          </View>
          <If when={props.section === "appearance"}>
            <View class="gap-6">
              <View class="gap-3">
                <View class="flex-row items-center justify-between gap-3">
                  <View class="flex-row flex-wrap gap-2">
                    <Each of={THEME_FILTERS} key={(option) => option.value}>
                      {(option) => (
                        <Button
                          testID={`theme-filter-${option().value}`}
                          selected={themeFilter() === option().value}
                          class="rounded-full border border-border px-3 py-1 text-muted-foreground selected:border-foreground selected:text-foreground"
                          onPress={() => themeFilter.set(option().value)}
                        >
                          <Text class="text-xs">{t(option().label)}</Text>
                        </Button>
                      )}
                    </Each>
                  </View>
                  <View class="relative w-[200px]">
                    <Input
                      testID="theme-search"
                      value={themeQuery()}
                      onChange={(text) => themeQuery.set(text)}
                      placeholder={t("settings.appearance.search")}
                      class="w-full rounded-(--radius) border border-border bg-background pl-8 pr-3 py-2 selection:bg-foreground text-xs text-foreground placeholder:text-muted-foreground"
                    />
                    <Icon name="search" size={13} color={token("muted-foreground")} class="absolute left-2 top-1/2 -translate-y-1/2" />
                  </View>
                </View>
                <View class="flex-row flex-wrap gap-2">
                  <Each of={filteredThemes()} key={(theme) => theme.id}>
                    {(theme) => (
                      <Button
                        testID={`theme-option-${theme().id}`}
                        selected={store.activeId() === theme().id}
                        accessibilityLabel={t("settings.appearance.select", { name: theme().name })}
                        class="w-[132px] shrink-0 flex-col items-stretch gap-2 rounded-xl border border-border bg-card p-2 hover:bg-accent/30"
                        style={{ borderColor: store.activeId() === theme().id ? themePreviewColor(theme(), "primary", themeModeOf(theme())) : undefined }}
                        onPress={() => {
                          store.select(theme().id);
                          props.onChange();
                        }}
                      >
                        <View class="h-[54px] w-full gap-1 overflow-hidden rounded-md p-[7px]" style={{ backgroundColor: themePreviewColor(theme(), "background", themeModeOf(theme())) }}>
                          <View class="h-1 w-10 rounded-full" style={{ backgroundColor: themePreviewColor(theme(), "primary", themeModeOf(theme())) }} />
                          <View class="h-1 w-[66px] rounded-full opacity-70" style={{ backgroundColor: themePreviewColor(theme(), "foreground", themeModeOf(theme())) }} />
                          <View class="h-1 w-[34px] rounded-full opacity-40" style={{ backgroundColor: themePreviewColor(theme(), "foreground", themeModeOf(theme())) }} />
                        </View>
                        <View class="flex-row items-center justify-between gap-1">
                          <Text numberOfLines={1} class="min-w-0 flex-1 text-xs text-foreground">{theme().name}</Text>
                          <Text class="shrink-0 text-xs text-muted-foreground">{t(themeModeOf(theme()) === "dark" ? "settings.appearance.badge.dark" : "settings.appearance.badge.light")}</Text>
                        </View>
                      </Button>
                    )}
                  </Each>
                  <If when={filteredThemes().length === 0}>
                    <Text class="py-3 text-sm text-muted-foreground">{t("settings.appearance.no_matches")}</Text>
                  </If>
                </View>
              </View>
              <View class="gap-3">
                <View class="gap-1">
                  <Text class="text-lg font-semibold text-foreground">{t("settings.appearance.mode.title")}</Text>
                  <Text class="text-sm text-muted-foreground">{t("settings.appearance.mode.description")}</Text>
                </View>
                <View class="flex-row flex-wrap gap-4">
                  <Each of={MODE_OPTIONS} key={(option) => option.value}>
                    {(option) => (
                      <ModeCard
                        option={option()}
                        selected={store.mode() === option().value}
                        onPress={() => chooseMode(option().value)}
                      />
                    )}
                  </Each>
                </View>
              </View>
              <If when={activeStylePacks().length > 0}>
                <View class="gap-1">
                  <Text class="text-xs font-medium text-muted-foreground">{t("settings.appearance.style_packs.active")}</Text>
                  <Text selectable class="text-xs leading-5 text-muted-foreground">
                    {activeStylePacks().map((theme) => theme.name).join("、")}
                  </Text>
                </View>
              </If>
              <If when={props.error?.()}>
                <Text selectable class="text-xs leading-5 text-destructive">{props.error?.()}</Text>
              </If>
              <Text class="text-xs leading-5 text-muted-foreground">
                {t("settings.appearance.style_packs.note")}
              </Text>
            </View>
          </If>
          <If when={props.section === "plugins"}>
            <View class="gap-6">
              <If when={props.error?.()}>
                <View class="rounded-(--radius) bg-destructive/10 px-3 py-2">
                  <Text selectable class="text-xs leading-5 text-destructive">{props.error?.()}</Text>
                </View>
              </If>
              <View class="gap-2">
                <View class="flex-row items-center">
                  <Text class="flex-1 text-xs font-medium text-muted-foreground">{t("settings.plugins.market.title")}</Text>
                  <Button testID="market-refresh" class="rounded-(--radius) px-2 py-1 hover:bg-muted" onPress={props.onRefreshMarketplace}>
                    <Text class="text-xs text-muted-foreground">{t("settings.plugins.market.refresh")}</Text>
                  </Button>
                </View>
                <If when={props.catalog().length === 0}>
                  <Text class="text-xs text-muted-foreground">{t("settings.plugins.market.empty")}</Text>
                </If>
                <Each of={props.catalog()} key={(plugin) => plugin.id}>
                  {(plugin) => (
                    <View class="gap-1 rounded-(--radius) border border-border p-3">
                      <Text class="text-sm font-medium text-foreground">{plugin().name} · {plugin().version}</Text>
                      <Text class="text-xs text-muted-foreground">{plugin().id}</Text>
                      <Button
                        testID={`market-install-${plugin().id}`}
                        disabled={!rootPath().trim()}
                        class="rounded-(--radius) bg-primary px-3 py-1.5 disabled:opacity-40"
                        onPress={() => props.onInstallMarketplace(plugin(), rootPath().trim())}
                      >
                        <Text class="text-xs text-primary-foreground">{t("settings.plugins.market.install")}</Text>
                      </Button>
                    </View>
                  )}
                </Each>
              </View>
              <View class="gap-2">
                <Text class="text-xs font-medium text-muted-foreground">{t("settings.plugins.signed.title")}</Text>
                <Input
                  testID="plugin-package-path"
                  value={packagePath()}
                  onChange={(text) => packagePath.set(text)}
                  placeholder={t("settings.plugins.signed.package_path")}
                  class="rounded-(--radius) border border-input px-3 py-2 selection:bg-foreground text-sm text-foreground placeholder:text-muted-foreground"
                />
                <Input
                  testID="plugin-root-path"
                  value={rootPath()}
                  onChange={(text) => rootPath.set(text)}
                  placeholder={t("settings.plugins.signed.root_path")}
                  class="rounded-(--radius) border border-input px-3 py-2 selection:bg-foreground text-sm text-foreground placeholder:text-muted-foreground"
                />
                <Button
                  testID="plugin-install"
                  disabled={installing() || !packagePath().trim() || !rootPath().trim()}
                  class="rounded-(--radius) bg-primary px-3 py-2 disabled:opacity-40"
                  onPress={async () => {
                    installing.set(true);
                    try {
                      await props.onInstallPlugin(packagePath().trim(), rootPath().trim());
                    } finally {
                      installing.set(false);
                    }
                  }}
                >
                  <Text class="text-xs text-primary-foreground">{installing() ? t("settings.plugins.signed.verifying") : t("settings.plugins.signed.verify")}</Text>
                </Button>
              </View>
              <View class="gap-2">
                <View class="flex-row items-center">
                  <Text class="flex-1 text-xs font-medium text-muted-foreground">{t("settings.plugins.installed.title")}</Text>
                  <Button
                    testID="theme-plugins-refresh"
                    accessibilityLabel={t("settings.plugins.installed.refresh_label")}
                    class="rounded-(--radius) px-2 py-1 hover:bg-muted"
                    onPress={props.onRefreshPackages}
                  >
                    <View class="flex-row items-center gap-1">
                      <Icon name="refresh-cw" size={12} />
                      <Text class="text-xs text-muted-foreground">{t("settings.plugins.installed.refresh")}</Text>
                    </View>
                  </Button>
                </View>
                <If when={props.packages().length === 0}>
                  <View class="items-center rounded-xl border border-dashed border-border px-6 py-10">
                    <Text class="text-sm text-muted-foreground">{t("settings.plugins.installed.empty")}</Text>
                  </View>
                </If>
                <Each of={props.packages()} key={(plugin) => `${plugin.id}:${plugin.digest}`}>
                  {(plugin) => (
                    <ThemePackageRow
                      item={plugin}
                      runtime={props.runtime ? props.runtime()[pluginInstance(plugin())] : undefined}
                      onToggle={(value) => props.onTogglePackage(value)}
                      onRetry={() => props.onRetryPackage?.(plugin())}
                      onRemove={() => pendingUninstall.set(plugin())}
                    />
                  )}
                </Each>
              </View>
              <If when={pendingUninstall()}>
                {(plugin) => (
                  <View class="gap-2 rounded-(--radius) border border-destructive/30 bg-destructive/10 p-3">
                    <Text class="text-xs text-foreground">{t("settings.plugins.uninstall.confirm", { name: plugin().name })}</Text>
                    <View class="flex-row gap-2">
                      <Button
                        testID="plugin-uninstall-confirm"
                        class="rounded-(--radius) bg-destructive px-3 py-1"
                        onPress={() => {
                          props.onUninstallPlugin(plugin());
                          pendingUninstall.set(null);
                        }}
                      >
                        <Text class="text-xs text-background">{t("settings.plugins.uninstall.confirm_action")}</Text>
                      </Button>
                      <Button testID="plugin-uninstall-cancel" class="rounded-(--radius) px-3 py-1 hover:bg-muted" onPress={() => pendingUninstall.set(null)}>
                        <Text class="text-xs">{t("common.cancel")}</Text>
                      </Button>
                    </View>
                  </View>
                )}
              </If>
              <If when={Object.values(props.diagnostics?.() ?? {}).flat().length > 0}>
                <View testID="plugin-ui-diagnostics" class="gap-1 rounded-(--radius) border border-border p-3">
                  <Text class="text-xs font-medium text-foreground">{t("settings.plugins.diagnostics.ui")}</Text>
                  <Each of={Object.values(props.diagnostics?.() ?? {}).flat().slice(0, 20)} key={(item) => item}>
                    {(item) => <Text selectable class="text-xs leading-4 text-muted-foreground">{item()}</Text>}
                  </Each>
                </View>
              </If>
              <If when={Object.values(props.logs?.() ?? {}).some((logs) => logs.length > 0)}>
                <View class="gap-1 rounded-(--radius) border border-border p-3">
                  <Text class="text-xs font-medium text-foreground">{t("settings.plugins.diagnostics.logs")}</Text>
                  <Each of={Object.values(props.logs?.() ?? {}).flatMap((logs) => logs.slice(-1))} key={(item) => JSON.stringify(item)}>
                    {(item) => <Text selectable numberOfLines={3} class="font-mono text-[10px] text-muted-foreground">{JSON.stringify(item()).slice(0, 500)}</Text>}
                  </Each>
                </View>
              </If>
            </View>
          </If>
        </View>
      </Scroll>
    </View>
  );
}
