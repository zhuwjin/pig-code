import { Scroll, Text, View } from "nsh";
import { Switch } from "nsh/base";
import { t } from "../../i18n";
import type { UiContext } from "../../ui-context";

export function DeveloperSettings(props: UiContext) {
  const config = () => props.store.config();
  const showFps = () => config()?.developer?.show_fps ?? false;
  return (
    <View testID="developer-settings" class="relative min-w-0 flex-1 bg-background">
      <Scroll class="flex-1">
        <View class="w-full max-w-[960px] gap-6 p-6">
          <View class="gap-2 border-b border-border pb-4">
            <Text class="text-xl font-semibold text-foreground">{t("settings.developer.title")}</Text>
            <Text class="text-sm text-muted-foreground">{t("settings.developer.description")}</Text>
          </View>
          <View class="rounded-xl border border-border">
            <View class="flex-row items-center gap-4 p-4">
              <View class="min-w-0 flex-1 gap-0.5">
                <Text class="text-sm font-medium text-foreground">{t("settings.developer.show_fps.label")}</Text>
                <Text class="text-xs text-muted-foreground">{t("settings.developer.show_fps.description")}</Text>
              </View>
              <Switch
                testID="developer-show-fps"
                checked={showFps()}
                disabled={!config()}
                accessibilityLabel={t("settings.developer.show_fps.label")}
                onChange={({ checked }) => props.store.saveDeveloperOptions({ show_fps: checked })}
                trackClass="h-5 w-9 rounded-full bg-muted p-0.5 transition-colors checked:bg-primary disabled:opacity-40"
                thumbClass="h-4 w-4 rounded-full bg-background shadow-sm transition-transform checked:translate-x-4"
              />
            </View>
          </View>
        </View>
      </Scroll>
    </View>
  );
}
