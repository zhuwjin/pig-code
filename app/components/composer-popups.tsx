import { Scroll, signal, Text, token, View } from "nsh";
import { AlertDialog, Button, Checkbox, Input, Popover } from "nsh/base";
import { filterSlashCommands, modeForPermissions, permissionsForMode } from "../commands";
import type { SlashCommand } from "../commands";
import type { ExecMode } from "../protocol";
import type { UiContext } from "../ui-context";
import { Icon } from "./icon";

const EXEC_MODES: { id: ExecMode; label: string; description: string }[] = [
  { id: "read-only", label: "只读", description: "读取和分析，不修改工作区" },
  { id: "auto", label: "自动", description: "在工作区内执行，危险操作仍需批准" },
  { id: "full", label: "完全访问", description: "不使用沙箱，可修改任意文件" },
];

export function PermissionPicker(props: UiContext) {
  const open = signal(false);
  const configuration = () => props.store.configuration();
  const mode = () => modeForPermissions(configuration().permissions);
  const label = () => EXEC_MODES.find((item) => item.id === mode())?.label ?? "自定义";
  const select = async (value: ExecMode) => {
    if (value === "full") {
      const answer = await AlertDialog.show({
        title: "启用完全访问？",
        description: "Agent 将不受工作区沙箱限制，也不会进行常规操作审批。仅在你信任当前任务时启用。",
        actions: [
          { id: "cancel", label: "取消", role: "cancel" },
          { id: "full", label: "启用完全访问", role: "destructive" },
        ],
      });
      if (answer !== "full") return;
    }
    props.store.setPermissions(permissionsForMode(value));
    open.set(false);
  };
  return (
    <View class="flex-row items-center gap-1.5">
      <Popover
        open={open()}
        onOpenChange={(value) => open.set(value)}
        placement="top"
        align="start"
        popupClass="w-80 rounded-xl border border-border bg-popover p-2 shadow-xl"
        trigger={
          <View role="button" testID="exec-mode" class="flex-row items-center gap-1 rounded-full px-3 py-1 hover:bg-accent">
            <Icon name="shield-check" size={16} color={token(mode() === "full" ? "destructive" : mode() === "auto" ? "warning" : "muted-foreground")} />
            <Text class={mode() === "full" ? "text-sm text-destructive" : mode() === "auto" ? "text-sm text-warning" : "text-sm text-muted-foreground"}>{label()}</Text>
            <Icon name="chevron-down" size={12} color={token("muted-foreground")} />
          </View>
        }
      >
        <View class="gap-1">
          <Each of={EXEC_MODES} key={(item) => item.id}>
            {(item) => (
              <Button
                testID={`exec-mode-${item().id}`}
                selected={mode() === item().id}
                class="rounded-(--radius) px-3 py-2 hover:bg-accent selected:bg-accent/60"
                onPress={() => void select(item().id)}
              >
                <View class="flex-row items-center gap-2">
                  <View class="min-w-0 flex-1 gap-0.5">
                    <Text class="text-sm font-medium text-foreground">{item().label}</Text>
                    <Text class="text-xs leading-5 text-muted-foreground">{item().description}</Text>
                  </View>
                  <If when={mode() === item().id}><Text class="text-sm text-primary">✓</Text></If>
                </View>
              </Button>
            )}
          </Each>
          <View class="my-1 h-px bg-border" />
          <Checkbox
            testID="plan-mode"
            checked={configuration().planEnabled}
            onChange={(checked) => props.store.setPlanMode(checked)}
            class="flex-row items-center gap-3 rounded-(--radius) px-3 py-2 hover:bg-accent"
            indicatorClass="h-4 w-4 rounded border border-border"
          >
            <View class="flex-1 gap-0.5">
              <Text class="text-sm font-medium text-foreground">Plan 模式</Text>
              <Text class="text-xs leading-5 text-muted-foreground">先制定计划，批准后再修改文件</Text>
            </View>
          </Checkbox>
        </View>
      </Popover>
      <If when={configuration().planEnabled}>
        <Button
          testID="plan-chip"
          accessibilityLabel="关闭 Plan 模式"
          class="rounded-full bg-accent/60 px-2 py-1 hover:bg-accent"
          onPress={() => props.store.setPlanMode(false)}
        >
          <Text class="text-xs text-foreground">Plan ×</Text>
        </Button>
      </If>
    </View>
  );
}

export function ModelPicker(props: UiContext) {
  const open = signal(false);
  const query = signal("");
  const models = () => {
    const normalized = query().trim().toLowerCase();
    return props.store.modelOptions().filter(({ provider, model }) =>
      !normalized || `${provider.name} ${model.name ?? model.id} ${model.id}`.toLowerCase().includes(normalized)
    );
  };
  const selected = () => props.store.selectedModel();
  const reasoning = () => props.store.configuration().reasoningLevel;
  const reasoningLabel = () => props.store.reasoningOptions().find((option) => option.id === reasoning())?.label;
  return (
    <Popover
      open={open()}
      onOpenChange={(value) => {
        open.set(value);
        if (!value) query.set("");
      }}
      placement="top"
      align="end"
      popupClass="w-80 rounded-xl border border-border bg-popover p-2 shadow-xl"
      trigger={
        <View role="button" testID="model-picker" class="max-w-56 flex-row items-center gap-1 rounded-full px-3 py-1 hover:bg-accent">
          <Text numberOfLines={1} class="text-sm text-foreground">
            {props.store.currentModel() || "选择模型"}
          </Text>
          <If when={props.store.currentModel() && reasoningLabel()}>
            <Text numberOfLines={1} class="text-sm text-muted-foreground">{reasoningLabel()}</Text>
          </If>
          <Icon name="chevron-down" size={12} color={token("muted-foreground")} />
        </View>
      }
    >
      <View class="gap-2">
        <Input
          testID="model-search"
          value={query()}
          onChange={(text) => query.set(text)}
          placeholder="搜索模型或提供商"
          class="rounded-(--radius) border border-border bg-background px-3 py-2 selection:bg-foreground text-sm text-foreground placeholder:text-muted-foreground"
        />
        <Scroll class="max-h-64">
          <View class="gap-1">
            <Each of={models()} key={({ provider, model }) => `${provider.id}/${model.id}`}>
              {(option) => {
                const value = () => option();
                const active = () => selected()?.provider.id === value().provider.id && selected()?.model.id === value().model.id;
                return (
                  <Button
                    testID={`model-option-${value().provider.id}-${value().model.id}`}
                    selected={active()}
                    class="rounded-(--radius) px-3 py-2 hover:bg-accent selected:bg-accent/60"
                    onPress={() => {
                      props.store.selectModel(value().provider.id, value().model.id);
                      open.set(false);
                    }}
                  >
                    <View class="flex-row items-center gap-2">
                      <View class="min-w-0 flex-1 gap-0.5">
                        <Text numberOfLines={1} class="text-sm font-medium text-foreground">{value().model.name || value().model.id}</Text>
                        <Text numberOfLines={1} class="text-xs text-muted-foreground">{value().provider.name} · {value().model.id}</Text>
                      </View>
                      <If when={active()}><Text class="text-sm text-primary">✓</Text></If>
                    </View>
                  </Button>
                );
              }}
            </Each>
            <If when={models().length === 0}>
              <Text class="px-3 py-4 text-center text-xs text-muted-foreground">没有匹配的可用模型</Text>
            </If>
          </View>
        </Scroll>
        <If when={props.store.reasoningOptions().length > 0}>
          <View class="gap-1 border-t border-border pt-2">
            <Text class="px-2 text-xs font-medium text-muted-foreground">推理等级</Text>
            <View class="flex-row flex-wrap gap-1">
              <Button
                testID="reasoning-off"
                selected={!reasoning()}
                class="rounded-full px-3 py-1 hover:bg-accent selected:bg-accent"
                onPress={() => props.store.setReasoning(null)}
              >
                <Text class="text-xs text-foreground">关闭</Text>
              </Button>
              <Each of={props.store.reasoningOptions()} key={(option) => option.id}>
                {(option) => (
                  <Button
                    testID={`reasoning-${option().id}`}
                    selected={reasoning() === option().id}
                    class="rounded-full px-3 py-1 hover:bg-accent selected:bg-accent"
                    onPress={() => props.store.setReasoning(option().id)}
                  >
                    <Text class="text-xs text-foreground">{option().label}</Text>
                  </Button>
                )}
              </Each>
            </View>
          </View>
        </If>
      </View>
    </Popover>
  );
}

export function SlashCandidates(props: {
  query: string;
  selected: number;
  onSelect: (command: SlashCommand["name"]) => void;
}) {
  const commands = () => filterSlashCommands(props.query);
  return (
    <View testID="slash-candidates" class="absolute bottom-full left-0 right-0 z-40 mb-2 gap-1 rounded-xl border border-border bg-popover p-2 shadow-xl">
      <Each of={commands()} key={(command) => command.name}>
        {(command, index) => (
          <Button
            testID={`slash-${command().name}`}
            selected={index() === props.selected}
            class="flex-row items-center gap-3 rounded-(--radius) px-3 py-2 hover:bg-accent selected:bg-accent"
            onPress={() => props.onSelect(command().name)}
          >
            <Text class="w-20 font-mono text-xs text-foreground">{command().name}</Text>
            <Text class="min-w-0 flex-1 text-xs text-muted-foreground">{command().description}</Text>
          </Button>
        )}
      </Each>
    </View>
  );
}
