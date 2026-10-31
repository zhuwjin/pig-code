import { Text, View } from "nsh";
import { Button } from "nsh/base";
import type { UiContext } from "../ui-context";
import { Composer } from "./composer";
import { ParticleWordmark } from "./wordmark";

const SUGGESTIONS = [
  { label: "总结这个工作区", prompt: "请阅读 README 并总结这个工作区的结构和主要模块。" },
  { label: "修复一个报错", prompt: "我遇到了一个报错： " },
  { label: "写单元测试", prompt: "请为主要模块写单元测试。" },
];

export function EmptyWorkspace(props: UiContext) {
  return (
    <View class="flex-1 items-center justify-center gap-5 px-4 py-8">
      <ParticleWordmark />
      <View class="w-full max-w-[720px] px-4"><Composer {...props} hero /></View>
      <View class="flex-row flex-wrap items-center justify-center gap-2">
        <Each of={SUGGESTIONS} key={(item) => item.label}>
          {(item, index) => (
            <Button
              testID={`empty-suggestion-${index()}`}
              class="rounded-full border border-border px-3 py-1 hover:bg-accent/60"
              onPress={() => props.store.draft.set(item().prompt)}
            >
              <Text class="text-sm text-muted-foreground">{item().label}</Text>
            </Button>
          )}
        </Each>
      </View>
    </View>
  );
}
