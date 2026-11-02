import { effect, openUrl, Scroll, signal, Text, token, View } from "nsh";
import { Button, Input, Popover, Switch } from "nsh/base";
import type { ApiFormat, ConnectionTestResult, ModelConfig, ProviderConfig } from "../../protocol";
import type { UiContext } from "../../ui-context";
import { Icon } from "../icon";
import { ProviderIconButton } from "./provider-icon";

const FORMATS: { id: ApiFormat; label: string; short: string }[] = [
  { id: "OpenAiChat", label: "OpenAI Chat Completions (/chat/completions)", short: "OpenAI Chat" },
  { id: "OpenAiResponses", label: "OpenAI Responses (/responses)", short: "OpenAI Responses" },
  { id: "AnthropicMessages", label: "Anthropic Messages (/v1/messages)", short: "Anthropic" },
];

function formatTokens(value: number): string {
  if (value >= 1_000_000) return `${Number((value / 1_000_000).toFixed(1))}M`;
  if (value >= 1_000) return `${Number((value / 1_000).toFixed(1))}K`;
  return String(value);
}

function formatLabel(format: ApiFormat): string {
  return FORMATS.find((item) => item.id === format)?.label ?? format;
}

function formatShort(format: ApiFormat): string {
  return FORMATS.find((item) => item.id === format)?.short ?? format;
}

function resultText(result: ConnectionTestResult): { text: string; ok: boolean } {
  switch (result.result) {
    case "connected": return { text: `连接成功 · HTTP ${result.status}`, ok: true };
    case "timeout": return { text: `连接超时 · ${result.secs} 秒`, ok: false };
    case "failed": return { text: `连接失败 · ${result.detail}`, ok: false };
  }
}

function ModelRow(props: UiContext & {
  provider: () => ProviderConfig;
  model: () => ModelConfig;
  separator: boolean;
  onEdit: () => void;
  onPickIcon: () => void;
}) {
  const { provider, model, store } = props;
  const renamed = model().name && model().name !== model().id;
  return (
    <View class={`flex-row items-center gap-2 px-3 py-2 ${props.separator ? "border-t border-border" : ""}`}>
      <ProviderIconButton
        testID={`model-icon-picker-${provider().id}-${model().id}`}
        accessibilityLabel={`选择模型图标 ${model().name || model().id}`}
        icon={() => model().icon}
        kind="model"
        size={20}
        onPress={props.onPickIcon}
      />
      <Text numberOfLines={1} class="shrink-0 text-sm text-foreground">{renamed ? model().name : model().id}</Text>
      <If when={renamed}>
        <Text numberOfLines={1} class="min-w-0 font-mono text-xs text-muted-foreground">{model().id}</Text>
      </If>
      <Text class="shrink-0 rounded bg-muted px-1.5 py-0.5 font-mono text-[10px] text-muted-foreground">{formatTokens(model().context_window)}</Text>
      <If when={model().input_image}><Text class="text-[10px] text-muted-foreground">视觉</Text></If>
      <View class="flex-1" />
      <Button
        testID={`model-edit-${provider().id}-${model().id}`}
        accessibilityLabel={`编辑模型 ${model().id}`}
        class="rounded px-2 py-1 hover:bg-accent"
        onPress={props.onEdit}
      >
        <Text class="text-[11px] text-muted-foreground">编辑</Text>
      </Button>
      <Button
        testID={`model-delete-${provider().id}-${model().id}`}
        accessibilityLabel={`删除模型 ${model().id}`}
        class="rounded px-2 py-1 hover:bg-destructive/10"
        onPress={() => store.deleteModel(provider().id, model().id)}
      >
        <Text class="text-[11px] text-destructive">删除</Text>
      </Button>
      <Switch
        testID={`model-toggle-${provider().id}-${model().id}`}
        accessibilityLabel={`${model().enabled ? "停用" : "启用"}模型 ${model().id}`}
        checked={model().enabled}
        onChange={(checked) => store.setModelEnabled(provider().id, model().id, checked)}
        trackClass="h-5 w-9 rounded-full bg-muted p-0.5 transition-colors checked:bg-primary"
        thumbClass="h-4 w-4 rounded-full bg-background shadow-sm transition-transform checked:translate-x-4"
      />
    </View>
  );
}

function ConnectionCard(props: UiContext & { provider: () => ProviderConfig }) {
  const { provider, store } = props;
  const name = signal(provider().name);
  const baseUrl = signal(provider().base_url);
  const format = signal<ApiFormat>(provider().api_format);
  const apiKey = signal("");
  const masked = signal(true);
  const collapsed = signal(false);
  const formatOpen = signal(false);
  const credential = () => store.providerCredentials()[provider().id];
  const result = () => store.testResults()[provider().id];
  const hasKey = () => credential() === "configured" || credential() === "not_required" || provider().requires_api_key === false;
  let initialized = "";
  effect(() => {
    if (initialized === provider().id) return;
    initialized = provider().id;
    name.set(provider().name);
    baseUrl.set(provider().base_url);
    format.set(provider().api_format);
    apiKey.set("");
    collapsed.set(hasKey());
  });
  const save = (changes: Partial<ProviderConfig>) => store.upsertProvider({ ...provider(), ...changes });
  let saveTimer: ReturnType<typeof setTimeout> | undefined;
  const scheduleSave = () => {
    if (saveTimer !== undefined) clearTimeout(saveTimer);
    const providerId = provider().id;
    const nextName = name();
    const nextBaseUrl = baseUrl();
    const nextKey = apiKey();
    saveTimer = setTimeout(() => {
      saveTimer = undefined;
      const current = store.config()?.providers.find((item) => item.id === providerId);
      if (!current) return;
      store.upsertProvider({ ...current, name: nextName, base_url: nextBaseUrl });
      if (nextKey.trim()) {
        store.setProviderApiKey(providerId, nextKey.trim());
        apiKey.set("");
      }
    }, 500);
  };
  return (
    <View class="gap-3 rounded-xl border border-border bg-card p-4">
      <View class="flex-row items-center gap-2">
        <Button
          testID={`provider-edit-${provider().id}`}
          accessibilityLabel="展开或折叠连接配置"
          class="min-w-0 flex-1 flex-row items-center gap-2 rounded-(--radius)"
          onPress={() => collapsed.update((value) => !value)}
        >
          <Text class="text-sm text-muted-foreground">连接配置</Text>
          <If when={collapsed()}>
            <Text class="rounded bg-muted px-2 py-0.5 text-[10px] text-muted-foreground">{formatShort(provider().api_format)}</Text>
            <Text numberOfLines={1} class="min-w-0 max-w-80 font-mono text-xs text-muted-foreground">{provider().base_url}</Text>
            <If when={!hasKey()}><Text class="text-xs text-warning">缺少密钥</Text></If>
          </If>
          <View class="flex-1" />
        </Button>
        <Button
          testID={`provider-test-${provider().id}`}
          disabled={store.testingProviders()[provider().id]}
          class="shrink-0 flex-row items-center gap-1.5 rounded-(--radius) border border-border px-3 py-1.5 hover:bg-accent disabled:opacity-50"
          onPress={() => store.testProvider(provider().id)}
        >
          <Icon name="globe" size={14} color={token("foreground")} />
          <Text class="text-xs text-foreground">{store.testingProviders()[provider().id] ? "测试中…" : "测试连接"}</Text>
        </Button>
        <View
          testID={`provider-chevron-${provider().id}`}
          role="button"
          accessibilityLabel="展开或折叠连接配置"
          class="shrink-0 cursor-pointer"
          onPress={() => collapsed.update((value) => !value)}
        >
          <Icon name="chevron-right" class={collapsed() ? undefined : "rotate-90"} size={16} color={token("muted-foreground")} />
        </View>
      </View>
      <If when={!collapsed()}>
        <View class="gap-3">
          <View class="flex-row gap-3">
            <View class="min-w-0 flex-1 gap-1">
              <Text class="text-sm text-muted-foreground">名称</Text>
              <Input
                testID="provider-name"
                value={name()}
                onChange={(text) => {
                  name.set(text);
                  scheduleSave();
                }}
                class="rounded-(--radius) border border-border bg-background px-3 py-2 h-9 selection:bg-foreground text-sm text-foreground"
              />
            </View>
            <View class="min-w-0 flex-1 gap-1">
              <Text class="text-sm text-muted-foreground">连接地址</Text>
              <Input
                testID="provider-base-url"
                value={baseUrl()}
                onChange={(text) => {
                  baseUrl.set(text);
                  scheduleSave();
                }}
                class="rounded-(--radius) border border-border bg-background px-3 py-2 h-9 selection:bg-foreground font-mono text-xs text-foreground"
              />
            </View>
          </View>
          <View class="flex-row items-start gap-3">
            <View class="min-w-0 flex-1 gap-1">
              <Text class="text-sm text-muted-foreground">API 协议</Text>
              <Popover
                open={formatOpen()}
                onOpenChange={(open) => formatOpen.set(open)}
                placement="bottom"
                align="start"
                popupClass="w-96 rounded-xl border border-border bg-popover p-1 shadow-xl"
                trigger={
                  <View role="button" testID="provider-format" class="h-9 w-full cursor-pointer flex-row items-center rounded-(--radius) border border-border bg-background px-3 hover:bg-accent">
                    <Text numberOfLines={1} class="min-w-0 flex-1 text-xs text-foreground">{formatLabel(format())}</Text>
                    <Icon name="chevron-down" size={12} />
                  </View>
                }
              >
                <View class="gap-1">
                  <Each of={FORMATS} key={(item) => item.id}>
                    {(item) => (
                      <Button
                        testID={`provider-format-${item().id}`}
                        selected={format() === item().id}
                        class="rounded-(--radius) px-3 py-2 hover:bg-accent selected:bg-accent/60"
                        onPress={() => {
                          format.set(item().id);
                          save({ api_format: item().id });
                          formatOpen.set(false);
                        }}
                      >
                        <Text numberOfLines={1} class="text-xs text-foreground">{item().label}</Text>
                      </Button>
                    )}
                  </Each>
                </View>
              </Popover>
            </View>
            <View class="min-w-0 flex-1 gap-1">
              <View class="flex-row items-center gap-2">
                <Text class="text-sm text-muted-foreground">API Key</Text>
                <Switch
                  testID="provider-requires-key"
                  accessibilityLabel="需要 API Key"
                  checked={provider().requires_api_key ?? true}
                  onChange={(checked) => save({ requires_api_key: checked })}
                  trackClass="h-4 w-7 rounded-full bg-muted p-0.5 transition-colors checked:bg-primary"
                  thumbClass="h-3 w-3 rounded-full bg-background shadow-sm transition-transform checked:translate-x-3"
                />
                <View class="flex-1" />
                <Button
                  testID={`provider-clear-key-${provider().id}`}
                  disabled={!hasKey() || provider().requires_api_key === false}
                  class="rounded px-1 py-0.5 hover:bg-muted disabled:opacity-30"
                  onPress={() => store.clearProviderApiKey(provider().id)}
                >
                  <Text class="text-[11px] text-muted-foreground">清除密钥</Text>
                </Button>
                <If when={provider().key_url}>
                  {(url) => (
                    <Button testID={`provider-key-link-${provider().id}`} class="flex-row items-center gap-0.5 rounded px-0" onPress={() => void openUrl(url())}>
                      <Text class="text-xs text-foreground underline">获取密钥</Text>
                      <Icon name="external-link" size={12} color={token("foreground")} />
                    </Button>
                  )}
                </If>
              </View>
              <View class="flex-row items-center gap-1">
                <Input
                  testID="provider-api-key"
                  value={apiKey()}
                  onChange={(text) => {
                    apiKey.set(text);
                    scheduleSave();
                  }}
                  readonly={provider().requires_api_key === false}
                  masked={masked()}
                  placeholder={provider().requires_api_key === false ? "无需 API Key" : hasKey() ? "留空以保留已保存的密钥" : "输入 API Key"}
                  class={`min-w-0 flex-1 rounded-(--radius) border border-border bg-background px-3 py-2 h-9 selection:bg-foreground font-mono text-sm text-foreground placeholder:text-muted-foreground ${provider().requires_api_key === false ? "opacity-50" : ""}`}
                />
                <Button
                  testID="provider-toggle-mask"
                  accessibilityLabel={masked() ? "显示密钥" : "隐藏密钥"}
                  disabled={provider().requires_api_key === false}
                  class="h-9 items-center justify-center rounded-(--radius) px-2 hover:bg-accent disabled:opacity-30"
                  onPress={() => masked.update((value) => !value)}
                >
                  <If when={masked()}><Icon name="eye-off" size={14} /></If>
                  <If when={!masked()}><Icon name="eye" size={14} /></If>
                </Button>
              </View>
            </View>
          </View>
          <If when={result()}>
            {(value) => {
              const display = () => resultText(value());
              return <Text selectable class={display().ok ? "text-xs leading-5 text-success" : "text-xs leading-5 text-destructive"}>{display().text}</Text>;
            }}
          </If>
        </View>
      </If>
    </View>
  );
}

export function ProviderDetail(props: UiContext & {
  provider: () => ProviderConfig;
  onEditModel: (modelId?: string) => void;
  onPickProviderIcon: () => void;
  onPickModelIcon: (modelId: string) => void;
}) {
  const { provider, store } = props;
  const deleteArmed = signal(false);
  effect(() => {
    provider().id;
    deleteArmed.set(false);
  });
  return (
    <View class="min-w-0 gap-4">
      <View class="flex-row items-center gap-3 rounded-xl border border-border bg-card p-4">
        <ProviderIconButton
          testID={`provider-icon-picker-${provider().id}`}
          accessibilityLabel={`选择供应商图标 ${provider().name}`}
          icon={() => provider().icon}
          size={28}
          buttonClass="rounded-md p-1"
          onPress={props.onPickProviderIcon}
        />
        <Text numberOfLines={1} class="min-w-0 flex-1 text-lg font-semibold text-foreground">{provider().name}</Text>
        <Switch
          testID={`provider-toggle-${provider().id}`}
          accessibilityLabel={`${provider().enabled ? "停用" : "启用"}供应商 ${provider().name}`}
          checked={provider().enabled}
          onChange={(checked) => store.setProviderEnabled(provider().id, checked)}
          trackClass="h-6 w-10 rounded-full bg-muted p-0.5 transition-colors checked:bg-primary"
          thumbClass="h-5 w-5 rounded-full bg-background shadow-sm transition-transform checked:translate-x-4"
        />
        <Button
          testID={`provider-delete-${provider().id}`}
          class="rounded-(--radius) border border-destructive/30 px-3 py-1.5 hover:bg-destructive/10"
          onPress={() => {
            if (deleteArmed()) store.deleteProvider(provider().id);
            else deleteArmed.set(true);
          }}
        >
          <Text class="text-xs text-destructive">{deleteArmed() ? "确认删除" : "删除"}</Text>
        </Button>
      </View>
      <ConnectionCard {...props} />
      <View class="gap-3 rounded-xl border border-border bg-card p-4">
        <View class="flex-row items-center gap-2">
          <Text class="text-sm font-medium text-foreground">模型</Text>
          <Text class="text-xs text-muted-foreground">{provider().models.length}</Text>
          <View class="flex-1" />
          <Button testID={`model-add-${provider().id}`} class="rounded-(--radius) bg-muted px-3 py-1.5 hover:bg-accent" onPress={() => props.onEditModel()}>
            <Text class="text-xs text-foreground">＋ 添加模型 …</Text>
          </Button>
        </View>
        <If when={provider().models.length === 0}>
          <View class="flex-row items-center gap-2 rounded-lg border border-dashed border-border px-4 py-3">
            <Text class="text-sm font-semibold text-muted-foreground">i</Text>
            <Text class="text-sm text-muted-foreground">还没有模型，点击“添加模型”创建。</Text>
          </View>
        </If>
        <If when={provider().models.length > 0}>
          <Scroll class="max-h-[480px] rounded-lg border border-border bg-background">
            <View class="py-1">
              <Each of={provider().models} key={(model) => model.id}>
                {(model, index) => (
                  <ModelRow
                    {...props}
                    model={model}
                    separator={index() > 0}
                    onEdit={() => props.onEditModel(model().id)}
                    onPickIcon={() => props.onPickModelIcon(model().id)}
                  />
                )}
              </Each>
            </View>
          </Scroll>
        </If>
      </View>
    </View>
  );
}
