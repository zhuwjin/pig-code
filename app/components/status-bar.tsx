import { Text, View } from "nsh";
import type { UiContext } from "../ui-context";

export function StatusBar(props: UiContext) {
  const { store } = props;
  const running = () => {
    const id = store.current();
    return id ? (store.running()[id] ?? false) : false;
  };
  const rate = () => {
    const id = store.current();
    return id ? (store.outputRate()[id] ?? 0) : 0;
  };
  const usage = () => {
    const id = store.current();
    return id ? store.contextUsage()[id] : undefined;
  };
  return (
    <If when={running() || rate() > 0 || usage()}>
      <View class="mx-auto flex h-8 w-full max-w-3xl flex-row items-center gap-3 px-4">
        <If when={running()}>
          <View class="flex-row items-center gap-1.5">
            <View class="h-1.5 w-1.5 rounded-full bg-success" />
            <Text class="text-xs text-muted-foreground">生成中</Text>
          </View>
        </If>
        <If when={rate() > 0}>
          <Text class="text-xs text-muted-foreground">≈ {rate()} token/s</Text>
        </If>
        <If when={usage()}>
          <Text numberOfLines={1} class="text-xs text-muted-foreground">{usage()}</Text>
        </If>
      </View>
    </If>
  );
}
