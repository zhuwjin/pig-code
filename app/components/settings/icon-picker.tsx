import { Scroll, signal, Text, View } from "nsh";
import { Button, Input } from "nsh/base";
import { t } from "../../i18n";
import { resolveIcon } from "./icon-catalog";
import { searchIcons } from "./icon-search";
import { ProviderIcon } from "./provider-icon";

export function IconPicker(props: {
  kind: "provider" | "model";
  targetName: string;
  value?: string | null;
  onSelect: (icon: string | null) => void;
  onClose: () => void;
}) {
  const query = signal("");
  const options = () => searchIcons(query());
  const kind = () => props.kind;
  return (
    <View class="absolute inset-0 z-50 items-center justify-center">
      <Button accessibilityLabel={t("settings.models.icon_picker.close_label")} class="absolute inset-0 bg-black/30" onPress={props.onClose} />
      <View testID="icon-picker" class="w-[560px] max-w-[90%] gap-3 rounded-xl border border-border bg-popover p-4 shadow-xl">
        <View class="flex-row items-center gap-3">
          <View class="min-w-0 flex-1 gap-0.5">
            <Text class="text-lg font-semibold text-foreground">{t("settings.models.icon_picker.title")}</Text>
            <Text style={{ numberOfLines: 1 }} class="text-xs text-muted-foreground">{props.targetName}</Text>
          </View>
          <Button testID="icon-picker-close" accessibilityLabel={t("settings.models.icon_picker.cancel_label")} class="rounded-(--radius) px-2 py-1 hover:bg-muted" onPress={props.onClose}>
            <Text class="text-sm text-muted-foreground">{t("common.close")}</Text>
          </Button>
        </View>
        <Input
          testID="icon-search"
          value={query()}
          onChange={({ value: text }) => query.set(text)}
          autoFocus
          placeholder={t("settings.models.icon_picker.search")}
          class="rounded-(--radius) border border-border bg-background px-3 py-2 selection:bg-foreground text-sm text-foreground placeholder:text-muted-foreground"
        />
        <Scroll class="max-h-96">
          <View class="flex-row flex-wrap gap-2 pr-1">
            <Button
              testID="icon-option-default"
              selected={!resolveIcon(props.value)}
              class="w-20 items-center gap-1 rounded-(--radius) border border-border p-2 hover:bg-accent selected:border-primary/50 selected:bg-accent/60"
              onPress={() => props.onSelect(null)}
            >
              <ProviderIcon kind={kind()} size={24} />
              <Text style={{ numberOfLines: 1 }} class="w-full text-center text-xs text-foreground">
                {t(kind() === "model" ? "settings.models.icon_picker.default_brain" : "settings.models.icon_picker.default_server")}
              </Text>
            </Button>
            <Each of={options()} key={(option) => option.id}>
              {(option) => (
                <Button
                  testID={`icon-option-${option().id}`}
                  selected={resolveIcon(props.value)?.id === option().id}
                  class="w-20 items-center gap-1 rounded-(--radius) border border-border p-2 hover:bg-accent selected:border-primary/50 selected:bg-accent/60"
                  onPress={() => props.onSelect(option().id)}
                >
                  <ProviderIcon icon={option().id} size={24} />
                  <Text style={{ numberOfLines: 1 }} class="w-full text-center text-xs text-foreground">{option().title}</Text>
                </Button>
              )}
            </Each>
          </View>
          <If when={options().length === 0}>
            <Text class="p-4 text-sm text-muted-foreground">{t("settings.models.icon_picker.no_matches")}</Text>
          </If>
        </Scroll>
        <View class="flex-row justify-end">
          <Button testID="cancel-icon-picker" class="rounded-(--radius) border border-border px-3 py-1.5 hover:bg-muted" onPress={props.onClose}>
            <Text class="text-xs text-foreground">{t("common.cancel")}</Text>
          </Button>
        </View>
      </View>
    </View>
  );
}
