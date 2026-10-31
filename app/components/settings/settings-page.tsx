import { Text, View } from "nsh";
import { Button } from "nsh/base";
import type { UiContext } from "../../ui-context";
import { Icon } from "../icon";
import { ModelSettings } from "./model-settings";
import { PluginPages } from "../../plugins/remote-renderer";

export function SettingsPage(props: UiContext & {
  onBack: () => void;
  onOpenThemes: () => void;
}) {
  return (
    <View testID="settings-page" class={props.themeStore.classes("settings.root", "min-w-0 flex-1 flex-row bg-background")}>
      <View class={props.themeStore.classes("settings.nav", "w-56 shrink-0 border-r border-border bg-sidebar p-3")}>
        <Button testID="settings-back" class="mb-4 flex-row items-center gap-2 rounded-(--radius) px-2 py-2 hover:bg-sidebar-accent" onPress={props.onBack}>
          <Text class="text-sm text-sidebar-foreground">← 返回工作区</Text>
        </Button>
        <Text class="px-2 pb-3 text-xs font-medium text-muted-foreground">设置</Text>
        <View class="gap-1">
          <Button testID="settings-models" selected class="flex-row items-center gap-2 rounded-(--radius) px-3 py-2 selected:bg-sidebar-accent">
            <Icon name="settings" size={15} />
            <Text class="text-sm text-sidebar-foreground">模型设置</Text>
          </Button>
          <Button testID="settings-appearance" class="flex-row items-center gap-2 rounded-(--radius) px-3 py-2 hover:bg-sidebar-accent" onPress={props.onOpenThemes}>
            <Icon name="sun" size={15} />
            <Text class="text-sm text-sidebar-foreground">外观</Text>
          </Button>
        </View>
        <PluginPages
          target="settings"
          packages={props.store.pluginPackages()}
          documents={props.store.pluginDocuments()}
          runtime={props.store.pluginRuntime()}
          onEvent={(instance, node, kind, text) => props.store.dispatchPluginEvent(instance, node, kind, text)}
        />
      </View>
      <ModelSettings {...props} />
    </View>
  );
}
