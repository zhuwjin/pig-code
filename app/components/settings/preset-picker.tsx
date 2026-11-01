import { Scroll, signal, Text, View } from "nsh";
import { Button, Input } from "nsh/base";
import { Icon } from "../icon";
import { ProviderIcon } from "./provider-icon";
import { presetMatchesQuery, SORTED_PROVIDER_PRESETS, type PresetProvider } from "./provider-presets";

export function PresetPicker(props: {
  onCustom: () => void;
  onSelect: (preset: PresetProvider) => void;
  onClose: () => void;
}) {
  const query = signal("");
  const presets = () => SORTED_PROVIDER_PRESETS.filter((preset) => presetMatchesQuery(preset, query()));
  return (
    <View class="absolute inset-0 z-50 items-center justify-center">
      <Button accessibilityLabel="关闭供应商预设" class="absolute inset-0 bg-black/30" onPress={props.onClose} />
      <View testID="preset-picker" class="max-h-[640px] w-[560px] max-w-[90%] gap-3 rounded-xl border border-border bg-popover p-4 shadow-xl">
        <View class="flex-row items-center">
          <Text class="flex-1 text-lg font-semibold text-foreground">添加供应商</Text>
          <Button testID="preset-picker-close" accessibilityLabel="关闭供应商预设" class="rounded-(--radius) px-2 py-1 hover:bg-muted" onPress={props.onClose}>
            <Text class="text-sm text-muted-foreground">关闭</Text>
          </Button>
        </View>
        <Input
          testID="preset-search"
          value={query()}
          onChange={(text) => query.set(text)}
          autoFocus
          placeholder="搜索供应商、地址或模型"
          class="rounded-(--radius) border border-border bg-background px-3 py-2 selection:bg-foreground text-sm text-foreground placeholder:text-muted-foreground"
        />
        <Scroll class="max-h-[480px]">
          <View class="gap-4 pr-1">
            <Text class="text-sm text-muted-foreground">选择预设快速创建，或添加自定义供应商。</Text>
            <View class="flex-row flex-wrap gap-2">
              <Button
                testID="preset-custom"
                class="w-[250px] max-w-full rounded-(--radius) border border-dashed border-border px-3 py-2 hover:bg-accent/60"
                onPress={props.onCustom}
              >
                <View class="flex-row items-center gap-2">
                  <View class="h-8 w-8 items-center justify-center rounded-lg bg-accent">
                    <Icon name="plus" size={16} />
                  </View>
                  <View class="min-w-0 flex-1 gap-0.5">
                    <Text class="text-sm font-medium text-foreground">自定义供应商</Text>
                    <Text numberOfLines={1} class="text-xs text-muted-foreground">配置兼容的 API 地址</Text>
                  </View>
                </View>
              </Button>
              <Each of={presets()} key={(preset) => preset.name}>
                {(preset, index) => (
                  <Button
                    testID={`preset-card-${index()}`}
                    class="w-[250px] max-w-full rounded-(--radius) border border-border px-3 py-2 hover:bg-accent/60"
                    onPress={() => props.onSelect(preset())}
                  >
                    <View class="flex-row items-center gap-2">
                      <View class="h-8 w-8 items-center justify-center overflow-hidden rounded-lg">
                        <ProviderIcon icon={preset().icon} size={32} />
                      </View>
                      <View class="min-w-0 flex-1 gap-0.5">
                        <Text numberOfLines={1} class="text-sm font-medium text-foreground">{preset().name}</Text>
                        <Text numberOfLines={1} class="text-xs text-muted-foreground">{preset().base_url}</Text>
                      </View>
                      <Icon name="chevron-down" class="-rotate-90" size={14} />
                    </View>
                  </Button>
                )}
              </Each>
            </View>
            <If when={presets().length === 0}>
              <Text testID="preset-picker-empty" class="text-sm text-muted-foreground">没有匹配的供应商</Text>
            </If>
          </View>
        </Scroll>
        <View class="flex-row justify-end">
          <Button testID="cancel-preset-picker" class="rounded-(--radius) border border-border px-3 py-1.5 hover:bg-muted" onPress={props.onClose}>
            <Text class="text-xs text-foreground">取消</Text>
          </Button>
        </View>
      </View>
    </View>
  );
}
