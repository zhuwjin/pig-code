import { mount, View } from "nsh";
import { MessageList } from "./app/components/messages";
import { AgentStore } from "./app/store";
import { ThemeStore } from "./app/theme";
import type { ChatItem } from "./app/protocol";

const store = new AgentStore(true);
const themeStore = new ThemeStore();

export function seed(count: number): void {
  const items: ChatItem[] = Array.from({ length: Math.ceil(count / 2) }, (_, index) => [
    {
      kind: "user" as const,
      id: `user-${index}`,
      text: `User message ${index}`,
      files: [],
      imageNums: [],
      sentAtMs: index * 2,
    },
    {
      kind: "markdown" as const,
      id: `markdown-${index}`,
      text: `Message ${index * 2 + 1}`,
      streaming: false,
      turnId: `turn-${index}`,
    },
  ]).flat().slice(0, count);
  store.current.set("long-history");
  store.items.set({ "long-history": items });
  store.turns.set({ "long-history": [] });
}

export function appendStreamingMessage(text: string): void {
  store.items.update((current) => {
    const items = current["long-history"] ?? [];
    return {
      ...current,
      "long-history": [
        ...items,
        { kind: "markdown" as const, id: `streaming-${items.length}`, text, streaming: true, turnId: "streaming" },
      ],
    };
  });
}

seed(250);
setTimeout(() => appendStreamingMessage("Message 250"), 1500);
setTimeout(() => appendStreamingMessage("Message 251"), 3000);
mount(() => (
  <View class="h-[700px] w-[900px]">
    <MessageList store={store} themeStore={themeStore} items={store.items()["long-history"] ?? []} />
  </View>
));
