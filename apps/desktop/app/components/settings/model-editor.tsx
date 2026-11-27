import { effect, Scroll, signal, Text, token, View } from "nsh";
import { Button, Checkbox, Collapsible, Dialog, Input, NumberInput, Radio, RadioGroup, Switch } from "nsh/base";
import { t } from "../../i18n";
import type { MessageKey } from "../../i18n";
import type { ModelConfig, ModelRegistryInfo, ProviderConfig } from "../../protocol";
import type { UiContext } from "../../ui-context";
import { Icon } from "../icon";
import { defaultReasoningParams } from "./provider-presets";

const COMMON_LABELS: Record<string, MessageKey> = {
  none: "settings.models.reasoning.none",
  disabled: "settings.models.reasoning.none",
  enabled: "settings.models.reasoning.enabled",
  minimal: "settings.models.reasoning.minimal",
  low: "settings.models.reasoning.low",
  medium: "settings.models.reasoning.medium",
  high: "settings.models.reasoning.high",
  xhigh: "settings.models.reasoning.xhigh",
  max: "settings.models.reasoning.max",
};

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function CapabilityCheckbox(props: {
  testID: string;
  checked: boolean;
  label: string;
  onChange: (checked: boolean) => void;
}) {
  return (
    <Checkbox
      testID={props.testID}
      checked={props.checked}
      onChange={({ checked }) => props.onChange(checked)}
      class="flex-row items-center gap-2"
      indicatorClass="h-4 w-4 items-center justify-center rounded border border-border checked:border-primary checked:bg-primary"
      indicator={props.checked
        ? <Icon testID={`${props.testID}-check`} name="check" size={12} color={token("primary-foreground")} />
        : null}
    >
      <Text class="text-xs text-foreground">{props.label}</Text>
    </Checkbox>
  );
}

export function ModelEditor(props: UiContext & {
  provider: ProviderConfig;
  model?: ModelConfig;
  onClose: () => void;
}) {
  const id = signal("");
  const name = signal("");
  const contextWindow = signal(128000);
  const maxOutput = signal(8192);
  const enabled = signal(true);
  const advanced = signal(false);
  const inputImage = signal(false);
  const inputPdf = signal(false);
  const capStructured = signal(false);
  const capStrictTools = signal(false);
  const capWebSearch = signal(false);
  const webSearchJson = signal("");
  const levels = signal<string[]>([]);
  const labels = signal<Record<string, string>>({});
  const defaultLevel = signal("");
  const paramsJson = signal("{}");
  const newLevel = signal("");
  const newLabel = signal("");
  const error = signal("");
  const lookupState = signal<"idle" | "pending" | "notfound">("idle");
  let lookedUpId = "";
  let lookupOverwrite = false;
  const initialize = () => {
    const model = props.model;
    id.set(model?.id ?? "");
    name.set(model?.name ?? "");
    contextWindow.set(model?.context_window ?? 128000);
    maxOutput.set(model?.max_output_tokens ?? 8192);
    enabled.set(model?.enabled ?? true);
    advanced.set(false);
    inputImage.set(model?.input_image ?? false);
    inputPdf.set(model?.input_pdf ?? false);
    capStructured.set(Boolean(model?.cap_structured));
    capStrictTools.set(Boolean(model?.cap_strict_tools));
    capWebSearch.set(Boolean(model?.cap_web_search));
    webSearchJson.set(model?.web_search_tool ? JSON.stringify(model.web_search_tool, null, 2) : "");
    levels.set([...(model?.reasoning_levels ?? [])]);
    labels.set({ ...(model?.reasoning_labels ?? {}) });
    defaultLevel.set(model?.default_reasoning_level ?? "");
    paramsJson.set(JSON.stringify(model?.reasoning_params ?? {}, null, 2));
    newLevel.set("");
    newLabel.set("");
    error.set("");
    lookupState.set("idle");
    lookedUpId = "";
    lookupOverwrite = false;
  };
  effect(initialize);
  const lookup = (overwrite = false) => {
    const modelId = id().trim();
    if (!modelId || (!overwrite && lookedUpId === modelId)) return;
    lookedUpId = modelId;
    lookupOverwrite = overwrite;
    lookupState.set("pending");
    props.store.lookupModel(modelId);
  };
  effect(() => {
    if (lookupState() !== "pending" || !lookedUpId || id().trim() !== lookedUpId) return;
    const info = props.store.modelInfo()[lookedUpId];
    if (info === undefined) return;
    if (info === null) {
      lookedUpId = "";
      lookupState.set("notfound");
      return;
    }
    applyInfo(info, lookupOverwrite);
    lookupState.set("idle");
  });
  const applyInfo = (info: ModelRegistryInfo, overwrite: boolean) => {
    const context = info.context ?? info.input;
    if (overwrite || context != null) contextWindow.set(context ?? 128000);
    if (overwrite || info.output != null) maxOutput.set(info.output ?? 8192);
    if (overwrite || info.reasoning_levels.length > 0) {
      const nextLevels = [...info.reasoning_levels];
      const nextLabels = Object.fromEntries(Object.entries(labels()).filter(([level]) => nextLevels.includes(level)));
      for (const level of nextLevels) {
        if (COMMON_LABELS[level]) nextLabels[level] = t(COMMON_LABELS[level]);
      }
      labels.set(nextLabels);
      levels.set(nextLevels);
      if (defaultLevel() && !nextLevels.includes(defaultLevel())) defaultLevel.set("");
    }
    let untouched = true;
    try {
      const parsed: unknown = paramsJson().trim() ? JSON.parse(paramsJson()) : {};
      untouched = isObject(parsed) && Object.keys(parsed).length === 0;
    } catch {
      untouched = true;
    }
    if (overwrite || untouched) {
      paramsJson.set(JSON.stringify(defaultReasoningParams(info.reasoning_levels, props.provider.api_format), null, 2));
      if (info.reasoning_levels.length > 0) advanced.set(true);
    }
    if (overwrite || info.input_modalities.length > 0) {
      inputImage.set(info.input_modalities.includes("image"));
      inputPdf.set(info.input_modalities.includes("pdf"));
    }
    if (overwrite || info.structured_output != null) capStructured.set(info.structured_output ?? false);
    if (overwrite || info.strict_tools != null) capStrictTools.set(info.strict_tools ?? false);
  };
  const editLevel = (level: string) => {
    newLevel.set(level);
    newLabel.set(labels()[level] ?? "");
    levels.update((items) => items.filter((item) => item !== level));
    labels.update((items) => {
      const next = { ...items };
      delete next[level];
      return next;
    });
    if (defaultLevel() === level) defaultLevel.set("");
  };
  const addLevel = () => {
    const level = newLevel().trim();
    const label = newLabel().trim();
    if (!level) return;
    if (!levels().includes(level)) levels.update((items) => [...items, level]);
    labels.update((items) => {
      const next = { ...items };
      if (label) next[level] = label;
      else delete next[level];
      return next;
    });
    try {
      const parsed: unknown = paramsJson().trim() ? JSON.parse(paramsJson()) : {};
      if (isObject(parsed) && parsed[level] === undefined) {
        parsed[level] = defaultReasoningParams([level], props.provider.api_format)[level];
        paramsJson.set(JSON.stringify(parsed, null, 2));
      }
    } catch {
      // Keep the existing JSON visible so the validation error can be fixed.
    }
    newLevel.set("");
    newLabel.set("");
  };
  const save = () => {
    const modelId = id().trim();
    const context = Number(contextWindow());
    const output = Number(maxOutput());
    if (!modelId) {
      error.set(t("settings.models.error.id_required"));
      return;
    }
    if (props.model?.id !== modelId && props.provider.models.some((item) => item.id === modelId)) {
      error.set(t("settings.models.error.id_exists"));
      return;
    }
    if (!Number.isSafeInteger(context) || context <= 0 || !Number.isSafeInteger(output) || output <= 0) {
      error.set(t("settings.models.error.positive_integers"));
      return;
    }
    if (defaultLevel() && !levels().includes(defaultLevel())) {
      error.set(t("settings.models.error.default_level"));
      return;
    }
    let reasoningParams: Record<string, unknown>;
    try {
      const parsed: unknown = paramsJson().trim() ? JSON.parse(paramsJson()) : {};
      if (!isObject(parsed)) throw new Error("not object");
      reasoningParams = parsed;
    } catch {
      error.set(t("settings.models.error.params_json"));
      return;
    }
    let webSearchTool: unknown = props.model?.web_search_tool ?? null;
    if (props.provider.api_format !== "OpenAiChat") {
      const raw = webSearchJson().trim();
      if (raw) {
        try {
          webSearchTool = JSON.parse(raw);
        } catch {
          error.set(t("settings.models.error.web_search_json"));
          return;
        }
      } else {
        webSearchTool = null;
      }
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
      cap_structured: capStructured(),
      cap_strict_tools: capStrictTools(),
      cap_web_search: capWebSearch(),
      web_search_tool: webSearchTool,
      reasoning_levels: [...levels()],
      default_reasoning_level: defaultLevel() || null,
      reasoning_labels: { ...labels() },
      reasoning_params: reasoningParams,
      parallel_tool_calls: props.model?.parallel_tool_calls ?? true,
    };
    if (props.store.upsertModel(props.provider.id, model, props.model?.id)) props.onClose();
    else error.set(props.store.globalError() || t("settings.models.error.save_failed"));
  };
  return (
    <Dialog
      open
      animation="fade"
      backdropClass="bg-black/30"
      onOpenChange={({ open }) => { if (!open) props.onClose(); }}
      testID="model-editor"
      class="m-auto max-h-[640px] w-[560px] max-w-[90%] gap-3 rounded-xl border border-border bg-popover p-4 shadow-xl"
    >
        <View class="flex-row items-center">
          <Text class="flex-1 text-lg font-semibold text-foreground">{t(props.model ? "settings.models.dialog.edit_title" : "settings.models.dialog.add_title")}</Text>
          <Button testID="model-dialog-close" accessibilityLabel={t("settings.models.dialog.close_label")} class="rounded-(--radius) px-2 py-1 hover:bg-muted" onPress={props.onClose}>
            <Text class="text-sm text-muted-foreground">{t("common.close")}</Text>
          </Button>
        </View>
        <Scroll class="-mr-4 max-h-[520px] pr-4">
          <View class="gap-3 pr-2">
            <View class="gap-1">
              <View class="flex-row items-center gap-2">
                <Text class="text-xs text-muted-foreground">{t("settings.models.field.id")}</Text>
                <If when={lookupState() === "pending"}><Text class="text-xs text-muted-foreground">{t("settings.models.lookup.pending")}</Text></If>
                <If when={lookupState() === "notfound"}><Text class="text-xs" style={{ color: token("warning") }}>{t("settings.models.lookup.not_found")}</Text></If>
                <If when={lookupState() === "idle"}><Text class="text-[10px] text-muted-foreground">{t("settings.models.lookup.hint")}</Text></If>
              </View>
              <Input
                testID="model-id"
                value={id()}
                onChange={({ value: text }) => id.set(text)}
                onSubmit={() => lookup()}
                onBlur={() => lookup()}
                placeholder="deepseek-flash"
                class="rounded-(--radius) border border-border bg-background px-3 py-2 selection:bg-foreground font-mono text-sm text-foreground"
              />
            </View>
            <View class="gap-1">
              <Text class="text-xs text-muted-foreground">{t("settings.models.field.display_name")}</Text>
              <Input testID="model-name" value={name()} onChange={({ value: text }) => name.set(text)} placeholder="DeepSeek Flash" class="rounded-(--radius) border border-border bg-background px-3 py-2 selection:bg-foreground text-sm text-foreground" />
            </View>
            <View class="flex-row gap-3">
              <View class="flex-1 gap-1">
                <Text class="text-xs text-muted-foreground">{t("settings.models.field.context_window")}</Text>
                <NumberInput
                  testID="model-context"
                  value={contextWindow()}
                  min={1}
                  onChange={({ value }) => { if (value !== null) contextWindow.set(value); }}
                  accessibilityLabel={t("settings.models.field.context_window")}
                  class="flex-row rounded-(--radius) border border-border bg-background px-2 py-1"
                  inputClass="flex-1 font-mono text-sm text-foreground"
                  decrementButton={<Text class="px-2 text-sm text-muted-foreground">−</Text>}
                  incrementButton={<Text class="px-2 text-sm text-muted-foreground">+</Text>}
                />
              </View>
              <View class="flex-1 gap-1">
                <Text class="text-xs text-muted-foreground">{t("settings.models.field.max_output")}</Text>
                <NumberInput
                  testID="model-max-output"
                  value={maxOutput()}
                  min={1}
                  onChange={({ value }) => { if (value !== null) maxOutput.set(value); }}
                  accessibilityLabel={t("settings.models.field.max_output")}
                  class="flex-row rounded-(--radius) border border-border bg-background px-2 py-1"
                  inputClass="flex-1 font-mono text-sm text-foreground"
                  decrementButton={<Text class="px-2 text-sm text-muted-foreground">−</Text>}
                  incrementButton={<Text class="px-2 text-sm text-muted-foreground">+</Text>}
                />
              </View>
            </View>
            <View class="flex-row items-center gap-2">
              <Text class="flex-1 text-sm text-foreground">{t("settings.models.field.enabled")}</Text>
              <Switch
                testID="model-enabled"
                checked={enabled()}
                onChange={({ checked }) => enabled.set(checked)}
                trackClass="h-5 w-9 rounded-full bg-muted p-0.5 transition-colors checked:bg-primary"
                thumbClass="h-4 w-4 rounded-full bg-background shadow-sm transition-transform checked:translate-x-4"
              />
            </View>
            <Collapsible open={advanced()} reveal content={
              <View class="gap-3">
                <View class="flex-row flex-wrap gap-3">
                  <CapabilityCheckbox testID="model-vision" checked={inputImage()} onChange={(checked) => inputImage.set(checked)} label={t("settings.models.field.input_image")} />
                  <CapabilityCheckbox testID="model-pdf" checked={inputPdf()} onChange={(checked) => inputPdf.set(checked)} label={t("settings.models.field.input_pdf")} />
                </View>
                <View class="flex-row flex-wrap gap-3">
                  <CapabilityCheckbox testID="cap-structured" checked={capStructured()} onChange={(checked) => capStructured.set(checked)} label={t("settings.models.field.structured_output")} />
                  <CapabilityCheckbox testID="cap-strict-tools" checked={capStrictTools()} onChange={(checked) => capStrictTools.set(checked)} label={t("settings.models.field.strict_tools")} />
                </View>
                <If when={props.provider.api_format !== "OpenAiChat"}>
                  <CapabilityCheckbox testID="cap-web-search" checked={capWebSearch()} onChange={(checked) => capWebSearch.set(checked)} label={t("settings.models.field.web_search")} />
                  <View class="gap-1">
                    <Text class="text-xs text-muted-foreground">{t("settings.models.field.web_search_definition")}</Text>
                    <Input testID="web-search-json" multiline value={webSearchJson()} onChange={({ value: text }) => webSearchJson.set(text)} class="min-h-20 rounded-(--radius) border border-border bg-background px-3 py-2 selection:bg-foreground font-mono text-xs text-foreground" />
                  </View>
                </If>
                <If when={props.provider.api_format === "OpenAiChat"}>
                  <Text testID="web-search-local-hint" class="text-xs text-muted-foreground">{t("settings.models.field.web_search_local_hint")}</Text>
                </If>
                <View class="gap-1">
                  <Text class="text-xs text-muted-foreground">{t("settings.models.field.reasoning_levels")}</Text>
                  <View class="flex-row flex-wrap gap-1">
                    <Each of={levels()} key={(level) => level}>
                      {(level) => (
                        <View class="flex-row items-center gap-1 rounded-full bg-accent px-2 py-0.5">
                          <Button testID={`edit-level-${level()}`} onPress={() => editLevel(level())}>
                            <Text class="text-xs text-foreground">{level()}{labels()[level()] ? ` · ${labels()[level()]}` : ""}</Text>
                          </Button>
                          <Button testID={`del-level-${level()}`} accessibilityLabel={t("settings.models.delete_level", { level: level() })} onPress={() => editLevel(level())}>
                            <Icon name="x" size={11} />
                          </Button>
                        </View>
                      )}
                    </Each>
                    <View class="w-24">
                      <Input testID="model-reasoning" value={newLevel()} onChange={({ value: text }) => newLevel.set(text)} placeholder={t("settings.models.field.level_placeholder")} class="rounded-(--radius) border border-border bg-background px-2 py-1 selection:bg-foreground font-mono text-xs text-foreground" />
                    </View>
                    <View class="w-28">
                      <Input testID="model-reasoning-label" value={newLabel()} onChange={({ value: text }) => newLabel.set(text)} placeholder={t("settings.models.field.label_placeholder")} class="rounded-(--radius) border border-border bg-background px-2 py-1 selection:bg-foreground text-xs text-foreground" />
                    </View>
                    <Button testID="add-level" class="rounded-(--radius) px-2 py-1 hover:bg-accent" onPress={addLevel}>
                      <Icon name="plus" size={13} />
                    </Button>
                  </View>
                </View>
                <View class="gap-1">
                  <Text class="text-xs text-muted-foreground">{t("settings.models.field.default_level")}</Text>
                  <RadioGroup class="flex-row flex-wrap gap-1">
                    <Radio testID="default-level-unset" checked={!defaultLevel()} onChange={() => defaultLevel.set("")} class="rounded-full px-2 py-0.5 hover:bg-accent selected:bg-accent">
                      <Text class="text-xs text-foreground">{t("settings.models.field.default_level_unset")}</Text>
                    </Radio>
                    <Each of={levels()} key={(level) => level}>
                      {(level) => (
                        <Radio testID={`default-level-${level()}`} checked={defaultLevel() === level()} onChange={() => defaultLevel.set(level())} class="rounded-full px-2 py-0.5 hover:bg-accent selected:bg-accent">
                          <Text class="text-xs text-foreground">{labels()[level()] || level()}</Text>
                        </Radio>
                      )}
                    </Each>
                  </RadioGroup>
                </View>
                <View class="gap-1">
                  <Text class="text-xs text-muted-foreground">{t("settings.models.field.params_json")}</Text>
                  <Input testID="model-params-json" multiline value={paramsJson()} onChange={({ value: text }) => paramsJson.set(text)} class="min-h-24 rounded-(--radius) border border-border bg-background px-3 py-2 selection:bg-foreground font-mono text-xs text-foreground" />
                </View>
              </View>
            }>
              <Button testID="advanced-toggle" class="flex-row items-center gap-2 rounded-(--radius) py-1 hover:bg-muted" onPress={() => advanced.update((open) => !open)}>
                <Icon name="chevron-down" class={advanced() ? undefined : "-rotate-90"} size={14} />
                <Text class="text-sm text-foreground">{t("settings.models.field.advanced")}</Text>
              </Button>
            </Collapsible>
            <If when={error()}><Text style={{ userSelect: "text" }} class="text-xs leading-5 text-destructive">{error()}</Text></If>
          </View>
        </Scroll>
        <View class="flex-row items-center gap-2">
          <Button testID="model-reset" disabled={!props.model} class="rounded-(--radius) px-2 py-1.5 hover:bg-muted disabled:opacity-30" onPress={() => { initialize(); lookup(true); }}>
            <Text class="text-xs text-muted-foreground">{t("settings.models.action.reset")}</Text>
          </Button>
          <View class="flex-1" />
          <Button testID="model-cancel" class="rounded-(--radius) border border-border px-3 py-1.5 hover:bg-muted" onPress={props.onClose}>
            <Text class="text-xs text-foreground">{t("common.cancel")}</Text>
          </Button>
          <Button testID="model-save" class="rounded-(--radius) bg-primary px-3 py-1.5" onPress={save}>
            <Text class="text-xs text-primary-foreground">{t("settings.models.action.save")}</Text>
          </Button>
        </View>
    </Dialog>
  );
}
