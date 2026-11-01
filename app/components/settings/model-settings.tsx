import { Scroll, signal, Text, View } from "nsh";
import { Button, Input } from "nsh/base";
import type { ModelConfig, ProviderConfig } from "../../protocol";
import type { UiContext } from "../../ui-context";
import { FleetView } from "./fleet-view";
import { IconPicker } from "./icon-picker";
import { ModelEditor } from "./model-editor";
import { nextProviderId, providerFromPreset, type PresetProvider } from "./provider-presets";
import { ProviderDetail } from "./provider-detail";
import { ProviderIcon } from "./provider-icon";
import { PresetPicker } from "./preset-picker";

interface IconTarget {
  kind: "provider" | "model";
  providerId: string;
  modelId?: string;
  targetName: string;
}

export function ModelSettings(props: UiContext) {
  const { store } = props;
  const view = signal<"providers" | "models">("providers");
  const providerQuery = signal("");
  const fleetQuery = signal("");
  const selectedId = signal<string | null>(null);
  const editor = signal<{ providerId: string; modelId?: string } | null>(null);
  const presetOpen = signal(false);
  const providers = () => store.config()?.providers ?? [];
  const filteredProviders = () => {
    const normalized = providerQuery().trim().toLowerCase();
    return providers().filter((provider) => !normalized || provider.name.toLowerCase().includes(normalized));
  };
  const enabledProviders = () => filteredProviders().filter((provider) => provider.enabled);
  const disabledProviders = () => filteredProviders().filter((provider) => !provider.enabled);
  const selected = () => providers().find((provider) => provider.id === selectedId()) ?? providers()[0];
  const modelProvider = () => {
    const current = editor();
    return current ? providers().find((provider) => provider.id === current.providerId) : undefined;
  };
  const openModel = (providerId: string, modelId?: string) => {
    selectedId.set(providerId);
    view.set("providers");
    editor.set({ providerId, modelId });
  };
  const selectProvider = (providerId: string) => {
    selectedId.set(providerId);
    view.set("providers");
    editor.set(null);
  };
  const addCustomProvider = () => {
    const id = nextProviderId(providers());
    const provider: ProviderConfig = {
      id,
      name: "自定义供应商",
      base_url: "https://",
      requires_api_key: true,
      api_format: "OpenAiChat",
      enabled: true,
      models: [],
      icon: null,
      key_url: null,
    };
    store.upsertProvider(provider);
    presetOpen.set(false);
    selectProvider(id);
  };
  const addPresetProvider = (preset: PresetProvider) => {
    const id = nextProviderId(providers());
    store.upsertProvider(providerFromPreset(preset, id));
    presetOpen.set(false);
    selectProvider(id);
  };
  const iconTarget = signal<IconTarget | null>(null);
  const pickProviderIcon = (provider: ProviderConfig) => iconTarget.set({
    kind: "provider",
    providerId: provider.id,
    targetName: provider.name,
  });
  const pickModelIcon = (provider: ProviderConfig, model: ModelConfig) => iconTarget.set({
    kind: "model",
    providerId: provider.id,
    modelId: model.id,
    targetName: model.name || model.id,
  });
  const currentIcon = () => {
    const target = iconTarget();
    const provider = providers().find((item) => item.id === target?.providerId);
    return target?.kind === "model"
      ? provider?.models.find((model) => model.id === target.modelId)?.icon
      : provider?.icon;
  };
  const selectIcon = (icon: string | null) => {
    const target = iconTarget();
    if (!target) return;
    if (target.kind === "provider") store.setProviderIcon(target.providerId, icon);
    else if (target.modelId) store.setModelIcon(target.providerId, target.modelId, icon);
    iconTarget.set(null);
  };
  const providerRow = (provider: ProviderConfig) => {
    const hasKey = store.providerCredentials()[provider.id] === "configured" ||
      store.providerCredentials()[provider.id] === "not_required" || provider.requires_api_key === false;
    return (
      <Button
        testID={`provider-select-${provider.id}`}
        selected={selected()?.id === provider.id}
        class="rounded-(--radius) border border-border px-3 py-2 hover:bg-accent/50 selected:border-foreground/30 selected:bg-accent/60"
        onPress={() => selectProvider(provider.id)}
      >
        <View class="flex-row items-center gap-2">
          <ProviderIcon icon={provider.icon} size={20} />
          <View class="min-w-0 flex-1 gap-0.5">
            <View class="flex-row items-center gap-1">
              <Text numberOfLines={1} class="min-w-0 flex-1 text-sm text-foreground">{provider.name}</Text>
              <If when={!hasKey}><Text class="text-xs text-warning">缺少密钥</Text></If>
            </View>
            <Text numberOfLines={1} class="text-xs text-muted-foreground">{provider.models.length} 个模型</Text>
          </View>
          <View class={provider.enabled ? "h-2 w-2 rounded-full bg-success" : "h-2 w-2 rounded-full bg-muted-foreground"} />
        </View>
      </Button>
    );
  };
  return (
    <View class={props.themeStore.classes("settings.models", "relative min-w-0 flex-1")}>
      <View class="gap-2 border-b border-border px-6 py-5">
        <Text class="text-xl font-semibold text-foreground">模型设置</Text>
        <Text class="text-sm text-muted-foreground">管理模型供应商、连接方式与可用模型。</Text>
        <View class="mt-2 flex-row items-center gap-3">
          <View class="flex-row rounded-full bg-muted p-1">
            <Button testID="providers-tab" selected={view() === "providers"} class="rounded-full px-3 py-1.5 selected:bg-background" onPress={() => { view.set("providers"); editor.set(null); }}>
              <Text class="text-xs text-foreground">供应商</Text>
            </Button>
            <Button testID="models-tab" selected={view() === "models"} class="rounded-full px-3 py-1.5 selected:bg-background" onPress={() => { view.set("models"); editor.set(null); }}>
              <Text class="text-xs text-foreground">全部模型</Text>
            </Button>
          </View>
          <Text class="text-xs text-muted-foreground">{view() === "providers" ? "按供应商管理连接与模型" : "查看并维护所有供应商的模型"}</Text>
        </View>
      </View>
      <If when={store.globalError()}>
        <View class="mx-6 mt-4 rounded-(--radius) bg-destructive/10 px-3 py-2">
          <Text selectable class="text-xs leading-5 text-destructive">{store.globalError()}</Text>
        </View>
      </If>
      <Scroll class="flex-1">
        <View class="gap-4 p-6">
          <If when={view() === "models"}>
            <FleetView
              {...props}
              providers={providers()}
              query={fleetQuery()}
              onQuery={(query) => fleetQuery.set(query)}
              onAddProvider={() => presetOpen.set(true)}
              onSelectProvider={selectProvider}
              onEditModel={openModel}
              onPickModelIcon={pickModelIcon}
            />
          </If>
          <If when={view() === "providers"}>
            <View class="flex-row items-start gap-4">
              <View class="w-56 shrink-0 gap-2">
                <Input
                  testID="provider-search"
                  value={providerQuery()}
                  onChange={(text) => providerQuery.set(text)}
                  placeholder="搜索供应商"
                  class="rounded-(--radius) border border-border bg-card px-3 py-2 selection:bg-foreground text-sm text-foreground placeholder:text-muted-foreground"
                />
                <Button testID="provider-add" class="rounded-(--radius) border border-border bg-card px-3 py-2 hover:bg-accent" onPress={() => presetOpen.set(true)}>
                  <Text class="text-sm text-foreground">＋ 添加供应商 …</Text>
                </Button>
                <Scroll class="max-h-[520px]">
                  <View class="gap-2 pr-1">
                    <Each of={enabledProviders()} key={(provider) => provider.id}>
                      {(provider) => providerRow(provider())}
                    </Each>
                    <If when={enabledProviders().length > 0 && disabledProviders().length > 0}>
                      <View class="flex-row items-center gap-2 px-1 pt-2">
                        <Text class="text-xs text-muted-foreground">已停用</Text>
                        <View class="h-px flex-1 bg-border" />
                      </View>
                    </If>
                    <Each of={disabledProviders()} key={(provider) => provider.id}>
                      {(provider) => providerRow(provider())}
                    </Each>
                    <If when={filteredProviders().length === 0 && providerQuery().trim()}>
                      <Text class="px-1 py-3 text-center text-xs text-muted-foreground">没有匹配的供应商</Text>
                    </If>
                  </View>
                </Scroll>
              </View>
              <View class="min-w-0 flex-1">
                <Each of={selected() ? [selected()!] : []} key={(provider) => provider.id}>
                  {(provider) => (
                    <ProviderDetail
                      {...props}
                      provider={provider}
                      onEditModel={(modelId) => openModel(provider().id, modelId)}
                      onPickProviderIcon={() => pickProviderIcon(provider())}
                      onPickModelIcon={(modelId) => {
                        const model = provider().models.find((item) => item.id === modelId);
                        if (model) pickModelIcon(provider(), model);
                      }}
                    />
                  )}
                </Each>
                <If when={!selected()}>
                  <View class="h-full min-h-64 items-center justify-center gap-2 text-muted-foreground">
                    <Text class="text-sm text-muted-foreground">选择或添加一个供应商开始配置。</Text>
                  </View>
                </If>
              </View>
            </View>
          </If>
        </View>
      </Scroll>
      <Each of={modelProvider() ? [modelProvider()!] : []}>
        {(provider) => {
          const modelId = editor()?.modelId;
          return <ModelEditor {...props} provider={provider()} model={provider().models.find((model) => model.id === modelId)} onClose={() => editor.set(null)} />;
        }}
      </Each>
      <If when={presetOpen()}>
        <PresetPicker onCustom={addCustomProvider} onSelect={addPresetProvider} onClose={() => presetOpen.set(false)} />
      </If>
      <If when={iconTarget()}>
        {(target) => (
          <IconPicker
            kind={target().kind}
            targetName={target().targetName}
            value={currentIcon()}
            onSelect={selectIcon}
            onClose={() => iconTarget.set(null)}
          />
        )}
      </If>
    </View>
  );
}
