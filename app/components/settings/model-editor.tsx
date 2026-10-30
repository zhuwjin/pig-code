import { effect, signal, Text, View } from "nsh";
import { Button, Checkbox, Input } from "nsh/base";
import type { ModelConfig, ProviderConfig } from "../../protocol";
import type { UiContext } from "../../ui-context";

export function ModelEditor(props: UiContext & {
  provider: ProviderConfig;
  model?: ModelConfig;
  onClose: () => void;
}) {
  const id = signal("");
  const name = signal("");
  const contextWindow = signal("128000");
  const maxOutput = signal("8192");
  const reasoning = signal("");
  const defaultReasoning = signal("");
  const inputImage = signal(false);
  const inputPdf = signal(false);
  const enabled = signal(true);
  const error = signal("");
  effect(() => {
    id.set(props.model?.id ?? "");
    name.set(props.model?.name ?? "");
    contextWindow.set(String(props.model?.context_window ?? 128000));
    maxOutput.set(String(props.model?.max_output_tokens ?? 8192));
    reasoning.set((props.model?.reasoning_levels ?? []).join(", "));
    defaultReasoning.set(props.model?.default_reasoning_level ?? "");
    inputImage.set(props.model?.input_image ?? false);
    inputPdf.set(props.model?.input_pdf ?? false);
    enabled.set(props.model?.enabled ?? true);
    error.set("");
  });
  const save = () => {
    const modelId = id().trim();
    const context = Number(contextWindow());
    const output = Number(maxOutput());
    const levels = [...new Set(reasoning().split(/[,\s]+/).map((item) => item.trim()).filter(Boolean))];
    const defaultLevel = defaultReasoning().trim();
    if (!modelId) {
      error.set("请输入模型 ID。");
      return;
    }
    if (!props.model && props.provider.models.some((item) => item.id === modelId)) {
      error.set("这个模型 ID 在当前供应商下已经存在。");
      return;
    }
    if (!Number.isSafeInteger(context) || context <= 0 || !Number.isSafeInteger(output) || output <= 0) {
      error.set("上下文窗口和最大输出必须是正整数。");
      return;
    }
    if (defaultLevel && !levels.includes(defaultLevel)) {
      error.set("默认推理等级必须包含在推理等级列表中。");
      return;
    }
    const model: ModelConfig = {
      ...(props.model ?? {}),
      id: modelId,
      name: name().trim() || null,
      enabled: enabled(),
      context_window: context,
      max_output_tokens: output,
      input_image: inputImage(),
      input_pdf: inputPdf(),
      reasoning_levels: levels,
      default_reasoning_level: defaultLevel || null,
    };
    if (props.store.upsertModel(props.provider.id, model, props.model?.id)) props.onClose();
    else error.set(props.store.globalError() || "保存模型失败。");
  };
  return (
    <View testID="model-editor" class="gap-4 rounded-xl border border-border bg-muted/30 p-4">
      <View class="flex-row items-center">
        <Text class="flex-1 text-sm font-semibold text-foreground">{props.model ? "编辑模型" : "添加模型"}</Text>
        <Button accessibilityLabel="关闭模型编辑" class="rounded px-2 py-1 hover:bg-muted" onPress={props.onClose}>
          <Text class="text-xs text-muted-foreground">取消</Text>
        </Button>
      </View>
      <View class="flex-row gap-3">
        <View class="flex-1 gap-1.5">
          <Text class="text-xs text-muted-foreground">模型 ID</Text>
          <Input testID="model-id" value={id()} readonly={Boolean(props.model)} onChange={(text) => id.set(text)} placeholder="deepseek-flash" class="rounded-(--radius) border border-border bg-background px-3 py-2 font-mono text-sm text-foreground" />
        </View>
        <View class="flex-1 gap-1.5">
          <Text class="text-xs text-muted-foreground">显示名（可选）</Text>
          <Input testID="model-name" value={name()} onChange={(text) => name.set(text)} placeholder="DeepSeek Flash" class="rounded-(--radius) border border-border bg-background px-3 py-2 text-sm text-foreground" />
        </View>
      </View>
      <View class="flex-row gap-3">
        <View class="flex-1 gap-1.5">
          <Text class="text-xs text-muted-foreground">上下文窗口</Text>
          <Input testID="model-context" value={contextWindow()} onChange={(text) => contextWindow.set(text)} keyboardType="number-pad" class="rounded-(--radius) border border-border bg-background px-3 py-2 font-mono text-sm text-foreground" />
        </View>
        <View class="flex-1 gap-1.5">
          <Text class="text-xs text-muted-foreground">最大输出 tokens</Text>
          <Input testID="model-max-output" value={maxOutput()} onChange={(text) => maxOutput.set(text)} keyboardType="number-pad" class="rounded-(--radius) border border-border bg-background px-3 py-2 font-mono text-sm text-foreground" />
        </View>
      </View>
      <View class="flex-row gap-3">
        <View class="flex-1 gap-1.5">
          <Text class="text-xs text-muted-foreground">推理等级（逗号分隔）</Text>
          <Input testID="model-reasoning" value={reasoning()} onChange={(text) => reasoning.set(text)} placeholder="low, high, max" class="rounded-(--radius) border border-border bg-background px-3 py-2 font-mono text-xs text-foreground" />
        </View>
        <View class="flex-1 gap-1.5">
          <Text class="text-xs text-muted-foreground">默认推理等级</Text>
          <Input testID="model-default-reasoning" value={defaultReasoning()} onChange={(text) => defaultReasoning.set(text)} placeholder="可选" class="rounded-(--radius) border border-border bg-background px-3 py-2 font-mono text-xs text-foreground" />
        </View>
      </View>
      <View class="flex-row flex-wrap gap-4">
        <Checkbox testID="model-vision" checked={inputImage()} onChange={(checked) => inputImage.set(checked)} class="flex-row items-center gap-2" indicatorClass="h-4 w-4 rounded border border-border">
          <Text class="text-xs text-foreground">支持图片</Text>
        </Checkbox>
        <Checkbox testID="model-pdf" checked={inputPdf()} onChange={(checked) => inputPdf.set(checked)} class="flex-row items-center gap-2" indicatorClass="h-4 w-4 rounded border border-border">
          <Text class="text-xs text-foreground">支持 PDF</Text>
        </Checkbox>
        <Checkbox testID="model-enabled" checked={enabled()} onChange={(checked) => enabled.set(checked)} class="flex-row items-center gap-2" indicatorClass="h-4 w-4 rounded border border-border">
          <Text class="text-xs text-foreground">启用模型</Text>
        </Checkbox>
      </View>
      <If when={error()}><Text selectable class="text-xs leading-5 text-destructive">{error()}</Text></If>
      <View class="flex-row justify-end gap-2">
        <Button class="rounded-(--radius) border border-border px-3 py-1.5 hover:bg-muted" onPress={props.onClose}>
          <Text class="text-xs text-foreground">取消</Text>
        </Button>
        <Button testID="model-save" class="rounded-(--radius) bg-primary px-3 py-1.5" onPress={save}>
          <Text class="text-xs text-primary-foreground">保存模型</Text>
        </Button>
      </View>
    </View>
  );
}
