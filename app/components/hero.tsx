import { Text, View } from "nsh";
import { Button } from "nsh/base";
import { t } from "../i18n";
import type { MessageKey } from "../i18n";
import type { UiContext } from "../ui-context";
import { Composer } from "./composer";
import { ParticleWordmark } from "./wordmark";

const SUGGESTIONS: { labelKey: MessageKey; promptKey: MessageKey }[] = [
  { labelKey: "hero.summarize_label", promptKey: "hero.summarize_prompt" },
  { labelKey: "hero.fix_error_label", promptKey: "hero.fix_error_prompt" },
  { labelKey: "hero.unit_tests_label", promptKey: "hero.unit_tests_prompt" },
];

export function EmptyWorkspace(props: UiContext) {
  return (
    <View class="flex-1 items-center justify-center gap-5 px-4 py-8">
      <ParticleWordmark />
      <View class="w-full max-w-[720px] px-4"><Composer {...props} hero /></View>
      <View class="flex-row flex-wrap items-center justify-center gap-2">
        <Each of={SUGGESTIONS} key={(item) => item.labelKey}>
          {(item, index) => (
            <Button
              testID={`empty-suggestion-${index()}`}
              class="rounded-full border border-border px-3 py-1 hover:bg-accent/60"
              onPress={() => props.store.draft.set(t(item().promptKey))}
            >
              <Text class="text-sm text-muted-foreground">{t(item().labelKey)}</Text>
            </Button>
          )}
        </Each>
      </View>
    </View>
  );
}
