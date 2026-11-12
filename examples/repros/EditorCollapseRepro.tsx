import { LayoutAnimation, mount, signal, Text, token, View } from "nsh";
import { Button, Editor } from "nsh/base";
import { Icon } from "../../app/components/icon";

const open = signal(false);
const lineCount = signal(160);

function toggle(): void {
  LayoutAnimation.configureNext(LayoutAnimation.Presets.easeInEaseOut);
  open.update((value) => !value);
}

function chooseLines(count: number): void {
  open.set(false);
  lineCount.set(count);
}

function code(): string {
  return Array.from({ length: lineCount() }, (_, index) => {
    const line = index + 1;
    if (line % 10 === 1) return `# Plugin system ${line}`;
    if (line % 10 === 2 || line % 10 === 4 || line % 10 === 6) return "";
    if (line % 10 === 3) return "Pig Code installs signed NSH packages without rebuilding or restarting the app.";
    if (line % 10 === 5) return `## Scope ${line}`;
    if (line % 10 === 7) return "- Agent tools, including model- and session-aware tool selection;";
    if (line % 10 === 8) return "- Commands invoked from the host's command search;";
    if (line % 10 === 9) return "- Lifecycle hooks (`SessionStart`, `PermissionRequest`, `PostToolUse`);";
    return "- HTTP origins and credential declarations;";
  }).join("\n");
}

mount(() => (
  <View class="h-full w-full items-center bg-background p-6">
    <View class="w-full max-w-[900px] gap-4">
      <View class="flex-row items-center gap-2">
        <Button testID="lines-30" class="rounded-full border border-border px-3 py-1" onPress={() => chooseLines(30)}>
          <Text class="text-xs text-foreground">30 lines</Text>
        </Button>
        <Button testID="lines-160" class="rounded-full border border-border px-3 py-1" onPress={() => chooseLines(160)}>
          <Text class="text-xs text-foreground">160 lines</Text>
        </Button>
        <Text class="text-xs text-muted-foreground">Standalone Editor · no message List</Text>
      </View>
      <View class="w-full">
        <View
          role="button"
          testID="editor-toggle"
          accessibilityState={{ expanded: open() }}
          class="flex-row items-center gap-2 py-1"
          onPress={toggle}
        >
          <Icon name="eye" size={16} color={token("muted-foreground")} />
          <Text class="text-sm font-medium text-muted-foreground">Read</Text>
          <Text class="min-w-0 flex-1 text-sm text-muted-foreground">PLUGIN_SYSTEM.md</Text>
          <Text class="text-xs text-muted-foreground">{lineCount()} lines</Text>
          <Icon name={open() ? "chevron-down" : "chevron-right"} size={14} color={token("muted-foreground")} />
        </View>
        <If when={open()}>
          <View testID="editor-panel" class="mt-2 h-80 overflow-hidden rounded-xl border border-border bg-tool-surface">
            <Editor
              testID="editor-code"
              value={code()}
              language="PLUGIN_SYSTEM.md"
              readOnly
              softWrap={false}
              scrollBeyondLastLine={false}
              lineNumbers
              class="flex-1 text-foreground"
            />
          </View>
        </If>
      </View>
    </View>
  </View>
));
