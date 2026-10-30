import { colorScheme, Scroll, Text, View } from "nsh";
import { Button } from "nsh/base";
import { PluginPackageInfo } from "./protocol";
import { Icon } from "./components/icon";
import { ThemeStore } from "./theme";
import { ThemeMode, themePreviewColor } from "./themes";

const MODE_OPTIONS: { value: ThemeMode; label: string }[] = [
  { value: "light", label: "浅色" },
  { value: "dark", label: "深色" },
  { value: "system", label: "跟随系统" },
];

function ThemePackageRow(props: {
  item: () => PluginPackageInfo;
  onToggle: (plugin: PluginPackageInfo) => void;
}) {
  const plugin = () => props.item();
  return (
    <Button
      testID={`theme-package-${plugin().id}`}
      selected={plugin().enabled}
      accessibilityLabel={`${plugin().enabled ? "停用" : "启用"}插件 ${plugin().name}`}
      class="rounded-(--radius) border border-border px-3 py-2 hover:bg-accent/50 selected:border-primary/60 selected:bg-accent/30"
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
        <Text class="text-xs text-muted-foreground">{plugin().enabled ? "已启用" : "已停用"}</Text>
      </View>
    </Button>
  );
}

export function ThemePicker(props: {
  store: ThemeStore;
  packages: PluginPackageInfo[];
  error?: string;
  onClose: () => void;
  onChange: () => void;
  onTogglePackage: (plugin: PluginPackageInfo) => void;
  onRefreshPackages: () => void;
}) {
  const store = props.store;
  const themePackages = () =>
    props.packages.filter((plugin) => (plugin.contributions.themes?.length ?? 0) > 0);
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
                <Text class="flex-1 text-xs font-medium text-muted-foreground">主题插件</Text>
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
              <Each of={themePackages()} key={(plugin) => plugin.id}>
                {(plugin) => (
                  <ThemePackageRow
                    item={plugin}
                    onToggle={(value) => props.onTogglePackage(value)}
                  />
                )}
              </Each>
            </View>
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
