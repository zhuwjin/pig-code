import { mount, Text, View } from "nsh";
import { Editor } from "nsh/base";

const source = Array.from({ length: 160 }, (_, index) => {
  const line = index + 1;
  if (line % 10 === 1) return `# Plugin system ${line}`;
  if (line % 10 === 2 || line % 10 === 4 || line % 10 === 6) return "";
  if (line % 10 === 3) return `Pig Code installs signed NSH packages without rebuilding or restarting the app. ${"The horizontal viewport keeps its vertical scrollbar at the visible right edge. ".repeat(4)}`;
  if (line % 10 === 5) return `## Scope ${line}`;
  if (line % 10 === 7) return "- Agent tools, including model- and session-aware tool selection;";
  if (line % 10 === 8) return "- Commands invoked from the host's command search;";
  if (line % 10 === 9) return "- Lifecycle hooks (`SessionStart`, `PermissionRequest`, `PostToolUse`);";
  return "- HTTP origins and credential declarations;";
}).join("\n");

mount(() => (
  <View class="h-full w-full bg-background p-4">
    <Text class="pb-3 text-sm text-muted-foreground">
      Official Editor · 160 lines · read-only · soft wrap off
    </Text>
    <View class="min-h-0 w-full flex-1 overflow-hidden rounded-lg border border-border bg-card">
      <Editor
        testID="official-editor"
        accessibilityLabel="Official Editor comparison"
        defaultValue={source}
        language="markdown"
        readOnly
        lineNumbers
        softWrap={false}
        folding={false}
        activeLine={false}
        scrollBeyondLastLine={false}
        fontSize={12}
        lineHeight={19}
        class="flex-1"
      />
    </View>
  </View>
));
