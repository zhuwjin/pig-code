import { Text, View } from "nsh";
import { Button } from "nsh/base";
import type { UiContext } from "../ui-context";
import { Composer } from "./composer";
import { ParticleWordmark } from "./wordmark";

const SUGGESTIONS = ["总结这个工作区", "修复一个报错", "写单元测试"];

export function EmptyWorkspace(props: UiContext) {
  return (
    <View class="flex-1 items-center justify-center gap-5 px-4 py-8">
      <ParticleWordmark />
      <View class="w-full max-w-3xl"><Composer {...props} hero /></View>
      <View class="flex-row flex-wrap items-center justify-center gap-2">
        <Each of={SUGGESTIONS} key={(text) => text}>
          {(text, index) => (
            <Button
              testID={`empty-suggestion-${index()}`}
              class="rounded-full border border-border px-3 py-1 hover:bg-accent/60"
              onPress={() => props.store.draft.set(text())}
            >
              <Text class="text-sm text-muted-foreground">{text()}</Text>
            </Button>
          )}
        </Each>
      </View>
    </View>
  );
}
