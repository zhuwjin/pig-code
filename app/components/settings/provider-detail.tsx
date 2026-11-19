import { effect, Scroll, signal, Text, token, View } from "nsh";
import { Button, Input, Link, Select, Switch } from "nsh/base";
import { t } from "../../i18n";
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
    case "connected": return { text: t("settings.provider.connected", { status: result.status }), ok: true };
    case "timeout": return { text: t("settings.provider.timeout", { secs: result.secs }), ok: false };
    case "failed": return { text: t("settings.provider.failed", { detail: result.detail }), ok: false };
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
        accessibilityLabel={t("settings.models.select_icon", { name: model().name || model().id })}
        icon={() => model().icon}
        kind="model"
        size={20}
        onPress={props.onPickIcon}
      />
      <Text style={{ numberOfLines: 1 }} class="shrink-0 text-sm text-foreground">{renamed ? model().name : model().id}</Text>
      <If when={renamed}>
        <Text style={{ numberOfLines: 1 }} class="min-w-0 font-mono text-xs text-muted-foreground">{model().id}</Text>
      </If>
      <Text class="shrink-0 rounded bg-muted px-1.5 py-0.5 font-mono text-[10px] text-muted-foreground">{formatTokens(model().context_window)}</Text>
      <If when={model().input_image}><Text class="text-[10px] text-muted-foreground">{t("settings.models.vision")}</Text></If>
      <View class="flex-1" />
      <Button
        testID={`model-edit-${provider().id}-${model().id}`}
        accessibilityLabel={t("settings.models.edit_model", { id: model().id })}
        class="rounded px-2 py-1 hover:bg-accent"
        onPress={props.onEdit}
      >
        <Text class="text-[11px] text-muted-foreground">{t("common.edit")}</Text>
      </Button>
      <Button
        testID={`model-delete-${provider().id}-${model().id}`}
        accessibilityLabel={t("settings.models.delete_model", { id: model().id })}
        class="rounded px-2 py-1 hover:bg-destructive/10"
        onPress={() => store.deleteModel(provider().id, model().id)}
      >
        <Text class="text-[11px] text-destructive">{t("common.delete")}</Text>
      </Button>
      <Switch
        testID={`model-toggle-${provider().id}-${model().id}`}
        accessibilityLabel={t(model().enabled ? "settings.models.disable_model" : "settings.models.enable_model", { id: model().id })}
        checked={model().enabled}
        onChange={({ checked }) => store.setModelEnabled(provider().id, model().id, checked)}
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
          accessibilityLabel={t("settings.provider.toggle_connection")}
          class="min-w-0 flex-1 flex-row items-center gap-2 rounded-(--radius)"
          onPress={() => collapsed.update((value) => !value)}
        >
          <Text class="text-sm text-muted-foreground">{t("settings.provider.connection")}</Text>
          <If when={collapsed()}>
            <Text class="rounded bg-muted px-2 py-0.5 text-[10px] text-muted-foreground">{formatShort(provider().api_format)}</Text>
            <Text style={{ numberOfLines: 1 }} class="min-w-0 max-w-80 font-mono text-xs text-muted-foreground">{provider().base_url}</Text>
            <If when={!hasKey()}><Text class="text-xs" style={{ color: token("warning") }}>{t("settings.models.missing_key")}</Text></If>
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
          <Text class="text-xs text-foreground">{store.testingProviders()[provider().id] ? t("settings.provider.testing") : t("settings.provider.test")}</Text>
        </Button>
        <View
          testID={`provider-chevron-${provider().id}`}
          role="button"
          accessibilityLabel={t("settings.provider.toggle_connection")}
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
              <Text class="text-sm text-muted-foreground">{t("settings.provider.name")}</Text>
              <Input
                testID="provider-name"
                value={name()}
                onChange={({ value: text }) => {
                  name.set(text);
                  scheduleSave();
                }}
                class="rounded-(--radius) border border-border bg-background px-3 py-2 h-9 selection:bg-foreground text-sm text-foreground"
              />
            </View>
            <View class="min-w-0 flex-1 gap-1">
              <Text class="text-sm text-muted-foreground">{t("settings.provider.base_url")}</Text>
              <Input
                testID="provider-base-url"
                value={baseUrl()}
                onChange={({ value: text }) => {
                  baseUrl.set(text);
                  scheduleSave();
                }}
                class="rounded-(--radius) border border-border bg-background px-3 py-2 h-9 selection:bg-foreground font-mono text-xs text-foreground"
              />
            </View>
          </View>
          <View class="flex-row items-start gap-3">
            <View class="min-w-0 flex-1 gap-1">
              <Text class="text-sm text-muted-foreground">{t("settings.provider.api_format")}</Text>
              <Select
                items={FORMATS.map((item) => ({ value: item.id, title: item.label }))}
                value={format()}
                onChange={({ value }) => {
                  if (value === null) return;
                  const next = value as ApiFormat;
                  format.set(next);
                  save({ api_format: next });
                }}
                accessibilityLabel={t("settings.provider.api_format")}
                placement="bottom"
                align="start"
                popupClass="w-96 rounded-xl border border-border bg-popover p-1 shadow-xl"
                itemClass="rounded-(--radius) px-3 py-2 hover:bg-accent selected:bg-accent/60"
                renderItem={(item) => (
                  <View testID={`provider-format-${item.value}`}>
                    <Text style={{ numberOfLines: 1 }} class="text-xs text-foreground">{item.title}</Text>
                  </View>
                )}
                trigger={
                  <View role="button" testID="provider-format" class="h-9 w-full cursor-pointer flex-row items-center rounded-(--radius) border border-border bg-background px-3 hover:bg-accent">
                    <Text style={{ numberOfLines: 1 }} class="min-w-0 flex-1 text-xs text-foreground">{formatLabel(format())}</Text>
                    <Icon name="chevron-down" size={12} />
                  </View>
                }
              />
            </View>
            <View class="min-w-0 flex-1 gap-1">
              <View class="flex-row items-center gap-2">
                <Text class="text-sm text-muted-foreground">API Key</Text>
                <Switch
                  testID="provider-requires-key"
                  accessibilityLabel={t("settings.provider.requires_key")}
                  checked={provider().requires_api_key ?? true}
                  onChange={({ checked }) => save({ requires_api_key: checked })}
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
                  <Text class="text-[11px] text-muted-foreground">{t("settings.provider.clear_key")}</Text>
                </Button>
                <If when={provider().key_url}>
                  {(url) => (
                    <Link testID={`provider-key-link-${provider().id}`} href={url()} class="flex-row items-center gap-0.5 rounded px-0">
                      <Text class="text-xs text-foreground underline">{t("settings.provider.get_key")}</Text>
                      <Icon name="external-link" size={12} color={token("foreground")} />
                    </Link>
                  )}
                </If>
              </View>
              <View class="flex-row items-center gap-1">
                <Input
                  testID="provider-api-key"
                  value={apiKey()}
                  onChange={({ value: text }) => {
                    apiKey.set(text);
                    scheduleSave();
                  }}
                  readonly={provider().requires_api_key === false}
                  masked={masked()}
                  placeholder={t(provider().requires_api_key === false ? "settings.provider.key_not_required" : hasKey() ? "settings.provider.key_saved_placeholder" : "settings.provider.key_placeholder")}
                  class={`min-w-0 flex-1 rounded-(--radius) border border-border bg-background px-3 py-2 h-9 selection:bg-foreground font-mono text-sm text-foreground placeholder:text-muted-foreground ${provider().requires_api_key === false ? "opacity-50" : ""}`}
                />
                <Button
                  testID="provider-toggle-mask"
                  accessibilityLabel={t(masked() ? "settings.provider.show_key" : "settings.provider.hide_key")}
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
              return <Text class="text-xs leading-5" style={{ userSelect: "text", color: token(display().ok ? "success" : "destructive") }}>{display().text}</Text>;
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
  let deleteProviderId = provider().id;
  effect(() => {
    const id = provider().id;
    if (id === deleteProviderId) return;
    deleteProviderId = id;
    deleteArmed.set(false);
  });
  return (
    <View class="min-w-0 gap-4">
      <View class="flex-row items-center gap-3 rounded-xl border border-border bg-card p-4">
        <ProviderIconButton
          testID={`provider-icon-picker-${provider().id}`}
          accessibilityLabel={t("settings.provider.select_icon", { name: provider().name })}
          icon={() => provider().icon}
          size={28}
          buttonClass="rounded-md p-1"
          onPress={props.onPickProviderIcon}
        />
        <Text style={{ numberOfLines: 1 }} class="min-w-0 flex-1 text-lg font-semibold text-foreground">{provider().name}</Text>
        <Switch
          testID={`provider-toggle-${provider().id}`}
          accessibilityLabel={t(provider().enabled ? "settings.provider.disable" : "settings.provider.enable", { name: provider().name })}
          checked={provider().enabled}
          onChange={({ checked }) => store.setProviderEnabled(provider().id, checked)}
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
          <Text class="text-xs text-destructive">{deleteArmed() ? t("common.confirm_delete") : t("common.delete")}</Text>
        </Button>
      </View>
      <ConnectionCard {...props} />
      <View class="gap-3 rounded-xl border border-border bg-card p-4">
        <View class="flex-row items-center gap-2">
          <Text class="text-sm font-medium text-foreground">{t("settings.models.section")}</Text>
          <Text class="text-xs text-muted-foreground">{provider().models.length}</Text>
          <View class="flex-1" />
          <Button testID={`model-add-${provider().id}`} class="rounded-(--radius) bg-muted px-3 py-1.5 hover:bg-accent" onPress={() => props.onEditModel()}>
            <Text class="text-xs text-foreground">{t("settings.models.add_model")}</Text>
          </Button>
        </View>
        <If when={provider().models.length === 0}>
          <View class="flex-row items-center gap-2 rounded-lg border border-dashed border-border px-4 py-3">
            <Text class="text-sm font-semibold text-muted-foreground">i</Text>
            <Text class="text-sm text-muted-foreground">{t("settings.models.empty")}</Text>
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
