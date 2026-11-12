import { mount, signal, Text, View } from "nsh";
import { Button } from "nsh/base";
import { MessageList } from "../app/components/messages";
import { AgentStore } from "../app/store";
import { ThemeStore } from "../app/theme";
import type { ChatItem, RetryStatus } from "../app/protocol";

const store = new AgentStore(true);
const themeStore = new ThemeStore();
const session = "retry-status";
const retry = signal(true);
const status: RetryStatus = {
  attempt: 2,
  max_attempts: 10,
  delay_ms: 2000,
  reason: { Network: null },
};
const items: ChatItem[] = [
  { kind: "user", id: "user-1", text: "Retry this turn", files: [], imageNums: [], sentAtMs: 0 },
];

store.current.set(session);
store.items.set({ [session]: items });
store.running.set({ [session]: true });
store.turns.set({
  [session]: [{ id: "turn-1", status: "running", startedAt: Date.now() - 11_000, changes: [], hooks: [] }],
});
store.retrying.set({ [session]: status });

mount(() => (
  <View class="h-[420px] w-[760px]">
    <Button
      testID="toggle-retry"
      class="self-start px-2 py-1"
      onPress={() => {
        retry.update((value) => !value);
        store.retrying.set({ [session]: retry() ? status : null });
      }}
    >
      <Text>toggle</Text>
    </Button>
    <MessageList store={store} themeStore={themeStore} items={items} />
  </View>
));
