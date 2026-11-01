import { colorScheme, Scroll, Text, View, signal } from "nsh";
import { Button, Input } from "nsh/base";
import { PluginMarketplacePackage, PluginPackageInfo, PluginRuntimeState } from "./protocol";
import { Icon } from "./components/icon";
import { ThemeStore } from "./theme";
import { ThemeMode, themePreviewColor } from "./themes";
import { pluginInstance } from "./plugins/ui-coordinator";

const MODE_OPTIONS: { value: ThemeMode; label: string }[] = [
  { value: "light", label: "浅色" },
  { value: "dark", label: "深色" },
  { value: "system", label: "跟随系统" },
];

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
        accessibilityLabel={`${plugin().enabled ? "停用" : "启用"}插件 ${plugin().name}`}
        class="flex-1 rounded-(--radius) border border-border px-3 py-2 hover:bg-accent/50 selected:border-primary/60 selected:bg-accent/30"
        onPress={() => props.onToggle(plugin())}
      >
        <View class="flex-row items-center gap-2">
          <View class="min-w-0 flex-1 gap-0.5">
            <Text numberOfLines={1} class="text-sm font-medium text-foreground">
              {plugin().name}
            </Text>
            <Text numberOfLines={1} class="text-xs text-muted-foreground">
              {plugin().id} · {plugin().version}
            </Text>
          </View>
          <View class="items-end">
            <Text class="text-xs text-muted-foreground">{runtime()?.state ?? (plugin().enabled ? "已启用" : "已停用")}</Text>
            <If when={runtime()}>
              <Text class="text-[10px] text-muted-foreground">
                epoch {runtime()?.epoch} · 节点 {runtime()?.metrics?.nodes ?? 0} · 操作 {runtime()?.metrics?.operations ?? 0} · 请求 {runtime()?.metrics?.pendingRequests ?? 0} · 队列 {runtime()?.metrics?.queueDepth ?? 0}
              </Text>
              <Text class="text-[10px] text-muted-foreground">
                网络 {runtime()?.metrics?.networkBytes ?? 0} B · 存储 {runtime()?.metrics?.storageBytes ?? 0} B · 超时 {runtime()?.metrics?.timedOutRequests ?? 0} · 取消 {runtime()?.metrics?.cancelledRequests ?? 0} · 丢弃 {runtime()?.metrics?.droppedEvents ?? 0}
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
          <Text class="text-xs">重试</Text>
        </Button>
      </If>
      <Button testID={`plugin-uninstall-${plugin().id}`} accessibilityLabel={`卸载插件 ${plugin().name}`} class="rounded-(--radius) px-2 py-1 hover:bg-destructive/10" onPress={props.onRemove}>
        <Text class="text-xs text-destructive">卸载</Text>
      </Button>
    </View>
  );
}

export function ThemePicker(props: {
  store: ThemeStore;
  packages: PluginPackageInfo[];
  catalog: PluginMarketplacePackage[];
  runtime?: Record<string, PluginRuntimeState>;
  diagnostics?: Record<string, string[]>;
  logs?: Record<string, unknown[]>;
  onRetryPackage?: (plugin: PluginPackageInfo) => void;
  error?: string;
  onClose: () => void;
  onChange: () => void;
  onTogglePackage: (plugin: PluginPackageInfo) => void;
  onRefreshPackages: () => void;
  onInstallPlugin: (packagePath: string, rootPath: string) => Promise<void>;
  onRefreshMarketplace: () => Promise<void>;
  onInstallMarketplace: (plugin: PluginMarketplacePackage, rootPath: string) => Promise<void>;
  onUninstallPlugin: (plugin: PluginPackageInfo) => void;
}) {
  const store = props.store;
  const pendingUninstall = signal<PluginPackageInfo | null>(null);
  const packagePath = signal("");
  const rootPath = signal("");
  const installing = signal(false);
  const themePackages = () => props.packages;
  const activeStylePacks = () => store.activeStylePacks();
  return (
    <View class="absolute inset-0 z-50">
      <Button
        accessibilityLabel="关闭主题选择"
        class="absolute inset-0 bg-black/30"
        onPress={props.onClose}
      />
      <View
        testID="theme-picker"
        class="absolute right-4 top-12 w-80 gap-4 rounded-xl border border-border bg-popover p-4 shadow-xl"
      >
        <View class="flex-row items-center">
          <View class="flex-1 gap-1">
            <Text class="text-base font-semibold text-popover-foreground">外观与主题</Text>
            <Text class="text-xs text-muted-foreground">选择后立即生效</Text>
          </View>
          <Button
            testID="theme-picker-close"
            accessibilityLabel="关闭"
            class="h-8 w-8 items-center justify-center rounded-(--radius) hover:bg-muted"
            onPress={props.onClose}
          >
            <Icon name="x" size={18} />
          </Button>
        </View>

        <Scroll class="max-h-[480px]">
          <View class="gap-4 pr-1">
            <View class="gap-2">
              <Text class="text-xs font-medium text-muted-foreground">外观</Text>
              <View class="flex-row gap-1 rounded-(--radius) bg-muted p-1">
                <Each of={MODE_OPTIONS} key={(option) => option.value}>
                  {(option) => (
                    <Button
                      testID={`theme-mode-${option().value}`}
                      selected={store.mode() === option().value}
                      class="flex-1 items-center rounded-(--radius) px-2 py-1.5 selected:bg-background"
                      onPress={() => {
                        store.setMode(option().value);
                        props.onChange();
                      }}
                    >
                      <Text class="text-xs text-foreground">{option().label}</Text>
                    </Button>
                  )}
                </Each>
              </View>
            </View>

            <View class="gap-2">
              <Text class="text-xs font-medium text-muted-foreground">主题</Text>
              <View class="gap-2">
                <Each of={store.selectableThemes()} key={(theme) => theme.id}>
                  {(theme) => (
                    <Button
                      testID={`theme-option-${theme().id}`}
                      selected={store.activeId() === theme().id}
                      accessibilityLabel={`选择主题 ${theme().name}`}
                      class="rounded-(--radius) border border-border p-3 hover:bg-accent/50 selected:border-primary selected:bg-accent/40"
                      onPress={() => {
                        store.select(theme().id);
                        props.onChange();
                      }}
                    >
                      <View class="flex-row items-center gap-3">
                        <View
                          class="h-10 w-12 overflow-hidden rounded-md border border-border"
                          style={{
                            backgroundColor: themePreviewColor(theme(), "background", colorScheme()),
                          }}
                        >
                          <View
                            class="h-3 w-full"
                            style={{
                              backgroundColor: themePreviewColor(theme(), "title-bar", colorScheme()),
                            }}
                          />
                          <View class="flex-1 justify-center px-2">
                            <View
                              class="h-2 w-7 rounded-full"
                              style={{
                                backgroundColor: themePreviewColor(theme(), "primary", colorScheme()),
                              }}
                            />
                          </View>
                        </View>
                        <View class="min-w-0 flex-1 gap-0.5">
                          <View class="flex-row items-center gap-2">
                            <Text class="flex-1 text-sm font-medium text-foreground">
                              {theme().name}
                            </Text>
                            <If when={theme().source === "plugin"}>
                              <Text class="text-xs text-muted-foreground">插件</Text>
                            </If>
                          </View>
                          <Text numberOfLines={1} class="text-xs text-muted-foreground">
                            {theme().description ?? "自定义主题"}
                          </Text>
                        </View>
                        <If when={store.activeId() === theme().id}>
                          <Text class="text-sm text-primary">✓</Text>
                        </If>
                      </View>
                    </Button>
                  )}
                </Each>
              </View>
            </View>

            <If when={activeStylePacks().length > 0}>
              <View class="gap-1">
                <Text class="text-xs font-medium text-muted-foreground">正在叠加的样式包</Text>
                <Text selectable class="text-xs leading-5 text-muted-foreground">
                  {activeStylePacks().map((theme) => theme.name).join("、")}
                </Text>
              </View>
            </If>

            <View class="gap-2">
              <View class="flex-row items-center">
                <Text class="flex-1 text-xs font-medium text-muted-foreground">插件市场</Text>
                <Button testID="market-refresh" class="rounded-(--radius) px-2 py-1 hover:bg-muted" onPress={props.onRefreshMarketplace}>
                  <Text class="text-xs text-muted-foreground">刷新目录</Text>
                </Button>
              </View>
              <If when={props.catalog.length === 0}>
                <Text class="text-xs text-muted-foreground">刷新目录以查看可用插件。</Text>
              </If>
              <Each of={props.catalog} key={(plugin) => plugin.id}>
                {(plugin) => (
                  <View class="gap-1 rounded-(--radius) border border-border p-2">
                    <Text class="text-xs font-medium text-foreground">{plugin().name} · {plugin().version}</Text>
                    <Text class="text-xs text-muted-foreground">{plugin().id}</Text>
                    <Button
                      testID={`market-install-${plugin().id}`}
                      disabled={!rootPath().trim()}
                      class="rounded-(--radius) bg-primary px-2 py-1 disabled:opacity-40"
                      onPress={() => props.onInstallMarketplace(plugin(), rootPath().trim())}
                    >
                      <Text class="text-xs text-primary-foreground">下载并安装</Text>
                    </Button>
                  </View>
                )}
              </Each>
            </View>
            <View class="gap-2">
              <Text class="text-xs font-medium text-muted-foreground">安装签名插件</Text>
              <Input testID="plugin-package-path" value={packagePath()} onChange={(text) => packagePath.set(text)} placeholder=".nshpkg 路径" class="rounded-(--radius) border border-input px-2 py-1 text-xs" />
              <Input testID="plugin-root-path" value={rootPath()} onChange={(text) => rootPath.set(text)} placeholder="根公钥 root.pub 路径" class="rounded-(--radius) border border-input px-2 py-1 text-xs" />
              <Button
                testID="plugin-install"
                disabled={installing() || !packagePath().trim() || !rootPath().trim()}
                class="rounded-(--radius) bg-primary px-3 py-1.5 disabled:opacity-40"
                onPress={async () => {
                  installing.set(true);
                  try {
                    await props.onInstallPlugin(packagePath().trim(), rootPath().trim());
                  } finally {
                    installing.set(false);
                  }
                }}
              >
                <Text class="text-xs text-primary-foreground">{installing() ? "正在验证…" : "验证并安装"}</Text>
              </Button>
            </View>
            <View class="gap-2">
              <View class="flex-row items-center">
                <Text class="flex-1 text-xs font-medium text-muted-foreground">插件</Text>
                <Button
                  testID="theme-plugins-refresh"
                  accessibilityLabel="刷新主题插件"
                  class="rounded-(--radius) px-2 py-1 hover:bg-muted"
                  onPress={props.onRefreshPackages}
                >
                  <View class="flex-row items-center gap-1">
                    <Icon name="refresh-cw" size={12} />
                    <Text class="text-xs text-muted-foreground">刷新</Text>
                  </View>
                </Button>
              </View>
              <If when={themePackages().length === 0}>
                <Text class="text-xs leading-5 text-muted-foreground">
                  将主题包放入 ~/.pigcodets/plugins 后点击刷新。
                </Text>
              </If>
              <Each of={themePackages()} key={(plugin) => `${plugin.id}:${plugin.digest}`}>
                {(plugin) => (
                  <ThemePackageRow
                    item={plugin}
                    runtime={props.runtime?.[pluginInstance(plugin())]}
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
                  <Text class="text-xs text-foreground">确认卸载 {plugin().name}？插件文件将被删除。</Text>
                  <View class="flex-row gap-2">
                    <Button
                      testID="plugin-uninstall-confirm"
                      class="rounded-(--radius) bg-destructive px-3 py-1"
                      onPress={() => {
                        props.onUninstallPlugin(plugin());
                        pendingUninstall.set(null);
                      }}
                    >
                      <Text class="text-xs text-background">确认卸载</Text>
                    </Button>
                    <Button testID="plugin-uninstall-cancel" class="rounded-(--radius) px-3 py-1 hover:bg-muted" onPress={() => pendingUninstall.set(null)}>
                      <Text class="text-xs">取消</Text>
                    </Button>
                  </View>
                </View>
              )}
            </If>
            <If when={Object.values(props.diagnostics ?? {}).flat().length > 0}>
              <View testID="plugin-ui-diagnostics" class="gap-1 rounded-(--radius) border border-border p-2">
                <Text class="text-xs font-medium text-foreground">UI 合成诊断</Text>
                <Each of={Object.values(props.diagnostics ?? {}).flat().slice(0, 20)} key={(item) => item}>
                  {(item) => <Text selectable class="text-xs leading-4 text-muted-foreground">{item()}</Text>}
                </Each>
              </View>
            </If>
            <If when={Object.values(props.logs ?? {}).some((logs) => logs.length > 0)}>
              <View class="gap-1 rounded-(--radius) border border-border p-2">
                <Text class="text-xs font-medium text-foreground">最近插件日志（已脱敏）</Text>
                <Each of={Object.values(props.logs ?? {}).flatMap((logs) => logs.slice(-1))} key={(item) => JSON.stringify(item)}>
                  {(item) => <Text selectable numberOfLines={3} class="font-mono text-[10px] text-muted-foreground">{JSON.stringify(item()).slice(0, 500)}</Text>}
                </Each>
              </View>
            </If>
            <If when={props.error}>
              <Text selectable class="text-xs leading-5 text-destructive">
                {props.error}
              </Text>
            </If>
          </View>
        </Scroll>
        <Text class="text-xs leading-5 text-muted-foreground">
          样式包可修改当前主题；更新插件后需要重新启用。
        </Text>
      </View>
    </View>
  );
}
