import { mount, View } from "nsh";
import { MessageList } from "./app/components/messages";
import { AgentStore } from "./app/store";
import { ThemeStore } from "./app/theme";
import type { ChatItem } from "./app/protocol";

const store = new AgentStore(true);
const themeStore = new ThemeStore();
const items: ChatItem[] = [
  { kind: "user", id: "user-0", text: "User message 0", files: [], imageNums: [], sentAtMs: 0 },
  { kind: "markdown", id: "markdown-1", text: "Message 1", streaming: false, turnId: "turn-0" },
  { kind: "user", id: "user-1", text: "User message 1", files: [], imageNums: [], sentAtMs: 2 },
  { kind: "markdown", id: "markdown-3", text: "Message 3", streaming: false, turnId: "turn-1" },
];
store.current.set("short-history");
store.items.set({ "short-history": items });
store.turns.set({ "short-history": [] });
mount(() => (
  <View class="h-[700px] w-[900px]">
    <MessageList store={store} themeStore={themeStore} items={items} />
  </View>
));
