import { Text, token, View } from "nsh";
import { Button, Input, Switch } from "nsh/base";
import { t, tCount } from "../../i18n";
import type { ModelConfig, ProviderConfig } from "../../protocol";
import type { UiContext } from "../../ui-context";
import { ProviderIcon, ProviderIconButton } from "./provider-icon";

function formatTokens(value: number): string {
  if (value >= 1_000_000) return `${Number((value / 1_000_000).toFixed(1))}M`;
  if (value >= 1_000) return `${Number((value / 1_000).toFixed(1))}K`;
  return String(value);
}

function formatShort(provider: ProviderConfig): string {
  switch (provider.api_format) {
    case "OpenAiChat": return "Chat";
    case "OpenAiResponses": return "Responses";
    case "AnthropicMessages": return "Anthropic";
  }
}

function reasoningLabel(model: () => ModelConfig): string {
  const levels = model().reasoning_levels ?? [];
  return levels.length ? levels.map((level) => model().reasoning_labels?.[level] ?? level).join(t("common.list_separator")) : "—";
}

function FleetModelRow(props: UiContext & {
  provider: ProviderConfig;
  model: () => ModelConfig;
  onEdit: () => void;
  onPickIcon: () => void;
}) {
  const { provider, model, store } = props;
  const renamed = model().name && model().name !== model().id;
  return (
    <View class="flex-row items-center gap-2 border-t border-border px-3 py-2 hover:bg-accent/30">
      <ProviderIconButton
        testID={`model-icon-picker-${provider.id}-${model().id}`}
        accessibilityLabel={t("settings.models.select_icon", { name: model().name || model().id })}
        icon={() => model().icon}
        kind="model"
        size={20}
        onPress={props.onPickIcon}
      />
      <View class="min-w-0 flex-1 gap-0.5">
        <Text style={{ numberOfLines: 1 }} class="text-sm font-medium text-foreground">{renamed ? model().name : model().id}</Text>
        <If when={renamed}><Text style={{ numberOfLines: 1 }} class="font-mono text-xs text-muted-foreground">{model().id}</Text></If>
      </View>
      <Text class="w-14 shrink-0 text-sm text-foreground">{formatTokens(model().context_window)}</Text>
      <Text class="w-14 shrink-0 text-sm text-foreground">{formatTokens(model().max_output_tokens)}</Text>
      <View class="w-28 shrink-0 flex-row gap-1">
        <If when={model().input_image}><Text class="text-[10px] text-muted-foreground">{t("settings.models.vision")}</Text></If>
        <If when={model().input_pdf}><Text class="text-[10px] text-muted-foreground">PDF</Text></If>
        <If when={!model().input_image && !model().input_pdf}><Text class="text-xs text-muted-foreground">—</Text></If>
      </View>
      <Text style={{ numberOfLines: 1 }} class="w-36 shrink-0 text-xs text-muted-foreground">{reasoningLabel(model)}</Text>
      <View class="w-12 shrink-0">
        <Switch
          testID={`model-toggle-${provider.id}-${model().id}`}
          accessibilityLabel={t(model().enabled ? "settings.models.disable_model" : "settings.models.enable_model", { id: model().id })}
          checked={model().enabled}
          onChange={({ checked }) => store.setModelEnabled(provider.id, model().id, checked)}
          trackClass="h-5 w-9 rounded-full bg-muted p-0.5 transition-colors checked:bg-primary"
          thumbClass="h-4 w-4 rounded-full bg-background shadow-sm transition-transform checked:translate-x-4"
        />
      </View>
      <View class="w-20 shrink-0 flex-row justify-end gap-1">
        <Button
          testID={`model-edit-${provider.id}-${model().id}`}
          accessibilityLabel={t("settings.models.edit_model", { id: model().id })}
          class="rounded px-1.5 py-1 hover:bg-accent"
          onPress={props.onEdit}
        >
          <Text class="text-[11px] text-muted-foreground">{t("common.edit")}</Text>
        </Button>
        <Button
          testID={`model-delete-${provider.id}-${model().id}`}
          accessibilityLabel={t("settings.models.delete_model", { id: model().id })}
          class="rounded px-1.5 py-1 hover:bg-destructive/10"
          onPress={() => store.deleteModel(provider.id, model().id)}
        >
          <Text class="text-[11px] text-destructive">{t("common.delete")}</Text>
        </Button>
      </View>
    </View>
  );
}

export function FleetView(props: UiContext & {
  providers: ProviderConfig[];
  query: string;
  onQuery: (query: string) => void;
  onAddProvider: () => void;
  onSelectProvider: (providerId: string) => void;
  onEditModel: (providerId: string, modelId?: string) => void;
  onPickModelIcon: (provider: ProviderConfig, model: ModelConfig) => void;
}) {
  const { store } = props;
  const groups = () => {
    const normalized = props.query.trim().toLowerCase();
    return props.providers.map((provider) => {
      const providerHit = normalized.length > 0 && provider.name.toLowerCase().includes(normalized);
      const models = !normalized || providerHit
        ? provider.models
        : provider.models.filter((model) => `${model.name ?? ""} ${model.id}`.toLowerCase().includes(normalized));
      return { provider, models, providerHit };
    }).filter((group) => !normalized || group.providerHit || group.models.length > 0);
  };
  return (
    <View class="gap-4">
      <View class="flex-row items-center gap-2">
        <View class="w-60">
          <Input
            testID="all-model-search"
            value={props.query}
            onChange={({ value }) => props.onQuery(value)}
            placeholder={t("settings.models.search_all")}
            class="rounded-(--radius) border border-border bg-card px-3 py-2 selection:bg-foreground text-sm text-foreground placeholder:text-muted-foreground"
          />
        </View>
        <View class="flex-1" />
        <Button testID="fleet-provider-add" class="rounded-(--radius) border border-border px-3 py-2 hover:bg-accent" onPress={props.onAddProvider}>
          <Text class="text-sm text-foreground">{t("settings.models.add_provider")}</Text>
        </Button>
      </View>
      <View class="rounded-lg border border-border bg-card">
        <View class="flex-row items-center gap-2 px-3 py-2">
          <Text class="flex-1 text-xs text-muted-foreground">{t("settings.models.table.model")}</Text>
          <Text class="w-14 text-xs text-muted-foreground">{t("settings.models.table.context")}</Text>
          <Text class="w-14 text-xs text-muted-foreground">{t("settings.models.table.output")}</Text>
          <Text class="w-28 text-xs text-muted-foreground">{t("settings.models.table.capabilities")}</Text>
          <Text class="w-36 text-xs text-muted-foreground">{t("settings.models.table.reasoning")}</Text>
          <Text class="w-12 text-xs text-muted-foreground">{t("settings.models.table.enabled")}</Text>
          <Text class="w-20 text-right text-xs text-muted-foreground">{t("settings.models.table.actions")}</Text>
        </View>
        <Each of={groups()} key={(group) => group.provider.id}>
          {(group) => {
            const provider = () => group().provider;
            const hasKey = () => store.providerCredentials()[provider().id] === "configured" ||
              store.providerCredentials()[provider().id] === "not_required" || provider().requires_api_key === false;
            return (
              <>
                <View class="flex-row items-center gap-2 border-t border-border bg-muted/40 px-3 py-2">
                  <ProviderIcon icon={() => provider().icon} size={18} />
                  <Text style={{ numberOfLines: 1 }} class="text-sm font-semibold text-foreground">{provider().name}</Text>
                  <Text class="rounded bg-muted px-1.5 py-0.5 text-[10px] text-muted-foreground">{formatShort(provider())}</Text>
                  <Text class="text-xs text-muted-foreground">{tCount("settings.models.model_count", provider().models.length)}</Text>
                  <If when={!hasKey()}><Text class="text-xs" style={{ color: token("warning") }}>{t("settings.models.missing_key")}</Text></If>
                  <View class="flex-1" />
                  <View class="w-12">
                    <Switch
                      testID={`provider-toggle-${provider().id}`}
                      checked={provider().enabled}
                      onChange={({ checked }) => store.setProviderEnabled(provider().id, checked)}
                      trackClass="h-5 w-9 rounded-full bg-muted p-0.5 transition-colors checked:bg-primary"
                      thumbClass="h-4 w-4 rounded-full bg-background shadow-sm transition-transform checked:translate-x-4"
                    />
                  </View>
                  <View class="w-20 flex-row justify-end">
                    <Button
                      testID={`fleet-provider-settings-${provider().id}`}
                      class="rounded px-1.5 py-1 hover:bg-accent"
                      onPress={() => props.onSelectProvider(provider().id)}
                    >
                      <Text class="text-[11px] text-muted-foreground">{t("settings.models.provider_settings")}</Text>
                    </Button>
                  </View>
                </View>
                <Each of={group().models} key={(model) => model.id}>
                  {(model) => (
                    <FleetModelRow
                      {...props}
                      provider={provider()}
                      model={model}
                      onEdit={() => props.onEditModel(provider().id, model().id)}
                      onPickIcon={() => props.onPickModelIcon(provider(), model())}
                    />
                  )}
                </Each>
                <If when={group().models.length === 0}>
                  <View class="border-t border-border p-2">
                    <Button
                      testID={`fleet-model-add-${provider().id}`}
                      class="w-full rounded-(--radius) border border-dashed border-border px-3 py-2 hover:bg-accent"
                      onPress={() => props.onEditModel(provider().id)}
                    >
                      <Text class="text-xs text-muted-foreground">{t("settings.models.add_model")}</Text>
                    </Button>
                  </View>
                </If>
              </>
            );
          }}
        </Each>
        <If when={groups().length === 0}>
          <View class="items-center border-t border-border py-8">
            <Text class="text-sm text-muted-foreground">{t(props.query.trim() ? "settings.models.no_matching_models" : "settings.models.no_providers")}</Text>
          </View>
        </If>
      </View>
    </View>
  );
}
