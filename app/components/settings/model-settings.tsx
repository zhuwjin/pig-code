import { Scroll, signal, Text, View } from "nsh";
import { AlertDialog, Button, Input, Switch } from "nsh/base";
import type { ConnectionTestResult, ModelConfig, ProviderConfig } from "../../protocol";
import type { UiContext } from "../../ui-context";
import { Icon } from "../icon";
import { IconPicker } from "./icon-picker";
import { ModelEditor } from "./model-editor";
import { ProviderEditor } from "./provider-editor";
import { ProviderIcon } from "./provider-icon";

type EditorState =
  | { kind: "provider"; providerId?: string }
  | { kind: "model"; providerId: string; modelId?: string }
  | null;

interface IconTarget {
  kind: "provider" | "model";
  providerId: string;
  modelId?: string;
  targetName: string;
}

function formatTokens(value: number): string {
  if (value >= 1_000_000) return `${Number((value / 1_000_000).toFixed(1))}M`;
  if (value >= 1_000) return `${Number((value / 1_000).toFixed(1))}K`;
  return String(value);
}

function formatLabel(format: ProviderConfig["api_format"]): string {
  switch (format) {
    case "OpenAiChat": return "OpenAI Chat";
    case "OpenAiResponses": return "OpenAI Responses";
    case "AnthropicMessages": return "Anthropic";
  }
}

function resultText(result: ConnectionTestResult): { text: string; ok: boolean } {
  switch (result.result) {
    case "connected": return { text: `连接成功 · HTTP ${result.status}`, ok: true };
    case "timeout": return { text: `连接超时 · ${result.secs} 秒`, ok: false };
    case "failed": return { text: `连接失败 · ${result.detail}`, ok: false };
  }
}

function ModelRow(props: UiContext & {
  provider: ProviderConfig;
  model: ModelConfig;
  showProvider?: boolean;
  onEdit: () => void;
  onPickIcon: () => void;
}) {
  const { provider, model, store } = props;
  const isDefault = () => store.config()?.default_provider === provider.id && store.config()?.default_model === model.id;
  const remove = async () => {
    const answer = await AlertDialog.show({
      title: "删除模型？",
      description: `将从 ${provider.name} 中删除 ${model.id}，已有会话的历史消息不受影响。`,
      actions: [
        { id: "cancel", label: "取消", role: "cancel" },
        { id: "delete", label: "删除", role: "destructive" },
      ],
    });
    if (answer === "delete") store.deleteModel(provider.id, model.id);
  };
  return (
    <View class={props.themeStore.classes("settings.model-row", "flex-row items-center gap-2 rounded-(--radius) bg-muted/40 px-3 py-2")}>
      <Button
        testID={`model-icon-picker-${provider.id}-${model.id}`}
        accessibilityLabel={`选择模型图标 ${model.name || model.id}`}
        class="flex-row items-center gap-0.5 rounded-md p-0.5 hover:bg-accent"
        onPress={props.onPickIcon}
      >
        <ProviderIcon icon={model.icon} kind="model" size={18} />
        <Icon name="pencil" size={9} />
      </Button>
      <View class="min-w-0 flex-1 gap-0.5">
        <View class="flex-row items-center gap-1.5">
          <Text numberOfLines={1} class="text-sm font-medium text-foreground">{model.name || model.id}</Text>
          <Text class="rounded bg-muted px-1.5 py-0.5 font-mono text-[10px] text-muted-foreground">{formatTokens(model.context_window)}</Text>
          <If when={model.input_image}><Text class="text-[10px] text-muted-foreground">视觉</Text></If>
          <If when={model.input_pdf}><Text class="text-[10px] text-muted-foreground">PDF</Text></If>
        </View>
        <Text numberOfLines={1} class="font-mono text-[11px] text-muted-foreground">
          {props.showProvider ? `${provider.name} · ${model.id}` : model.id}
        </Text>
      </View>
      <If when={isDefault()}>
        <Text class="rounded-full bg-primary/10 px-2 py-1 text-[10px] text-primary">默认</Text>
      </If>
      <If when={!isDefault()}>
        <Button
          testID={`model-default-${provider.id}-${model.id}`}
          accessibilityLabel={`设为默认模型 ${model.id}`}
          disabled={!model.enabled || !provider.enabled}
          class="rounded px-2 py-1 hover:bg-accent disabled:opacity-30"
          onPress={() => store.setDefaultModel(provider.id, model.id)}
        >
          <Text class="text-[11px] text-muted-foreground">设为默认</Text>
        </Button>
      </If>
      <Button
        testID={`model-edit-${provider.id}-${model.id}`}
        accessibilityLabel={`编辑模型 ${model.id}`}
        class="rounded px-2 py-1 hover:bg-accent"
        onPress={props.onEdit}
      >
        <Text class="text-[11px] text-muted-foreground">编辑</Text>
      </Button>
      <Button
        testID={`model-delete-${provider.id}-${model.id}`}
        accessibilityLabel={`删除模型 ${model.id}`}
        class="rounded px-2 py-1 hover:bg-destructive/10"
        onPress={() => void remove()}
      >
        <Text class="text-[11px] text-destructive">删除</Text>
      </Button>
      <Switch
        testID={`model-toggle-${provider.id}-${model.id}`}
        accessibilityLabel={`${model.enabled ? "停用" : "启用"}模型 ${model.id}`}
        checked={model.enabled}
        onChange={(checked) => store.setModelEnabled(provider.id, model.id, checked)}
        trackClass="h-5 w-9 rounded-full bg-muted checked:bg-primary"
        thumbClass="h-4 w-4 rounded-full bg-background checked:translate-x-4"
      />
    </View>
  );
}

function ProviderDetail(props: UiContext & {
  provider: ProviderConfig;
  onEditProvider: () => void;
  onEditModel: (modelId?: string) => void;
  onPickProviderIcon: () => void;
  onPickModelIcon: (modelId: string) => void;
}) {
  const { provider, store } = props;
  const credential = () => store.providerCredentials()[provider.id];
  const result = () => store.testResults()[provider.id];
  const remove = async () => {
    const answer = await AlertDialog.show({
      title: "删除供应商？",
      description: `将删除 ${provider.name} 及其模型配置，已有会话的历史消息不受影响。`,
      actions: [
        { id: "cancel", label: "取消", role: "cancel" },
        { id: "delete", label: "删除", role: "destructive" },
      ],
    });
    if (answer === "delete") store.deleteProvider(provider.id);
  };
  return (
    <View class="gap-3">
      <View class={props.themeStore.classes("settings.provider-card", "flex-row items-center gap-3 rounded-xl border border-border bg-card p-4")}>
        <Button
          testID={`provider-icon-picker-${provider.id}`}
          accessibilityLabel={`选择供应商图标 ${provider.name}`}
          class="flex-row items-center gap-1 rounded-md p-1 hover:bg-accent"
          onPress={props.onPickProviderIcon}
        >
          <ProviderIcon icon={provider.icon} size={28} />
          <Icon name="pencil" size={11} />
        </Button>
        <View class="min-w-0 flex-1 gap-0.5">
          <View class="flex-row items-center gap-2">
            <Text numberOfLines={1} class="text-lg font-semibold text-foreground">{provider.name}</Text>
            <If when={store.config()?.default_provider === provider.id}>
              <Text class="rounded-full bg-primary/10 px-2 py-0.5 text-[10px] text-primary">默认供应商</Text>
            </If>
          </View>
          <Text numberOfLines={1} class="font-mono text-xs text-muted-foreground">{provider.id}</Text>
        </View>
        <Button testID={`provider-edit-${provider.id}`} class="rounded-(--radius) border border-border px-3 py-1.5 hover:bg-accent" onPress={props.onEditProvider}>
          <Text class="text-xs text-foreground">编辑</Text>
        </Button>
        <Button testID={`provider-delete-${provider.id}`} class="rounded-(--radius) border border-destructive/30 px-3 py-1.5 hover:bg-destructive/10" onPress={() => void remove()}>
          <Text class="text-xs text-destructive">删除</Text>
        </Button>
        <Switch
          testID={`provider-toggle-${provider.id}`}
          accessibilityLabel={`${provider.enabled ? "停用" : "启用"}供应商 ${provider.name}`}
          checked={provider.enabled}
          onChange={(checked) => store.setProviderEnabled(provider.id, checked)}
          trackClass="h-6 w-10 rounded-full bg-muted checked:bg-primary"
          thumbClass="h-5 w-5 rounded-full bg-background checked:translate-x-4"
        />
      </View>

      <View class="gap-3 rounded-xl border border-border bg-card p-4">
        <View class="flex-row items-center gap-2">
          <Text class="text-sm font-medium text-foreground">连接配置</Text>
          <Text class="rounded bg-muted px-2 py-0.5 text-[10px] text-muted-foreground">{formatLabel(provider.api_format)}</Text>
          <View class="flex-1" />
          <Button
            testID={`provider-test-${provider.id}`}
            disabled={store.testingProviders()[provider.id]}
            class="rounded-(--radius) border border-border px-3 py-1.5 hover:bg-accent disabled:opacity-50"
            onPress={() => store.testProvider(provider.id)}
          >
            <Text class="text-xs text-foreground">{store.testingProviders()[provider.id] ? "测试中…" : "测试连接"}</Text>
          </Button>
        </View>
        <Text selectable class="font-mono text-xs leading-5 text-muted-foreground">{provider.base_url}</Text>
        <View class="flex-row items-center gap-2">
          <View class={credential() === "configured" || credential() === "not_required" ? "h-1.5 w-1.5 rounded-full bg-success" : "h-1.5 w-1.5 rounded-full bg-warning"} />
          <Text class="flex-1 text-xs text-muted-foreground">
            {credential() === "configured" ? "密钥已保存" : credential() === "not_required" || provider.requires_api_key === false ? "无需密钥" : credential() === "unavailable" ? "密钥库不可用" : "尚未配置密钥"}
          </Text>
          <If when={credential() === "configured"}>
            <Button
              testID={`provider-clear-key-${provider.id}`}
              class="rounded px-2 py-1 hover:bg-muted"
              onPress={() => store.clearProviderApiKey(provider.id)}
            >
              <Text class="text-[11px] text-muted-foreground">清除密钥</Text>
            </Button>
          </If>
        </View>
        <If when={result()}>
          {(value) => {
            const display = () => resultText(value());
            return <Text selectable class={display().ok ? "text-xs leading-5 text-success" : "text-xs leading-5 text-destructive"}>{display().text}</Text>;
          }}
        </If>
      </View>

      <View class="gap-3 rounded-xl border border-border bg-card p-4">
        <View class="flex-row items-center">
          <Text class="text-sm font-semibold text-foreground">模型</Text>
          <Text class="ml-1 text-xs text-muted-foreground">{provider.models.length}</Text>
          <View class="flex-1" />
          <Button testID={`model-add-${provider.id}`} class="rounded-(--radius) bg-muted px-3 py-1.5 hover:bg-accent" onPress={() => props.onEditModel()}>
            <Text class="text-xs text-foreground">＋ 添加模型</Text>
          </Button>
        </View>
        <View class="gap-2">
          <Each of={provider.models} key={(model) => model.id}>
            {(model) => <ModelRow {...props} model={model()} onEdit={() => props.onEditModel(model().id)} onPickIcon={() => props.onPickModelIcon(model().id)} />}
          </Each>
          <If when={provider.models.length === 0}>
            <View class="items-center rounded-(--radius) border border-dashed border-border px-4 py-8">
              <Text class="text-xs text-muted-foreground">还没有模型，点击“添加模型”创建。</Text>
            </View>
          </If>
        </View>
      </View>
    </View>
  );
}

export function ModelSettings(props: UiContext) {
  const { store } = props;
  const view = signal<"providers" | "models">("providers");
  const query = signal("");
  const selectedId = signal<string | null>(null);
  const editor = signal<EditorState>(null);
  const providers = () => store.config()?.providers ?? [];
  const filteredProviders = () => {
    const normalized = query().trim().toLowerCase();
    return providers().filter((provider) => !normalized || `${provider.name} ${provider.id}`.toLowerCase().includes(normalized));
  };
  const selected = () => providers().find((provider) => provider.id === selectedId()) ?? providers()[0];
  const modelProvider = () => {
    const current = editor();
    return current?.kind === "model" ? providers().find((provider) => provider.id === current.providerId) : undefined;
  };
  const openProvider = (providerId?: string) => {
    if (providerId) selectedId.set(providerId);
    view.set("providers");
    editor.set({ kind: "provider", providerId });
  };
  const openModel = (providerId: string, modelId?: string) => {
    selectedId.set(providerId);
    view.set("providers");
    editor.set({ kind: "model", providerId, modelId });
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
  const allModels = () => providers().flatMap((provider) => provider.models.map((model) => ({ provider, model })));
  return (
    <View class={props.themeStore.classes("settings.models", "relative min-w-0 flex-1")}>
      <View class="gap-2 border-b border-border px-6 py-5">
        <Text class="text-xl font-semibold text-foreground">模型设置</Text>
        <Text class="text-sm text-muted-foreground">管理模型供应商、连接方式与可用模型。</Text>
        <View class="mt-2 flex-row items-center gap-1">
          <Button testID="providers-tab" selected={view() === "providers"} class="rounded-full px-3 py-1.5 hover:bg-accent selected:bg-accent" onPress={() => { view.set("providers"); editor.set(null); }}>
            <Text class="text-xs text-foreground">供应商</Text>
          </Button>
          <Button testID="models-tab" selected={view() === "models"} class="rounded-full px-3 py-1.5 hover:bg-accent selected:bg-accent" onPress={() => { view.set("models"); editor.set(null); }}>
            <Text class="text-xs text-foreground">全部模型</Text>
          </Button>
          <Text class="ml-2 text-xs text-muted-foreground">按供应商管理连接与模型</Text>
        </View>
      </View>
      <If when={store.globalError()}>
        <View class="mx-6 mt-4 rounded-(--radius) bg-destructive/10 px-3 py-2">
          <Text selectable class="text-xs leading-5 text-destructive">{store.globalError()}</Text>
        </View>
      </If>
      <Scroll class="flex-1">
        <View class="gap-4 p-6">
          <If when={editor()?.kind === "provider"}>
            <ProviderEditor
              {...props}
              provider={editor()?.kind === "provider" ? providers().find((provider) => provider.id === editor()?.providerId) : undefined}
              onClose={() => editor.set(null)}
              onSaved={(providerId) => {
                selectedId.set(providerId);
                editor.set(null);
              }}
            />
          </If>
          <If when={view() === "models" && !editor()}>
            <View class="gap-3">
              <Input testID="all-model-search" value={query()} onChange={(text) => query.set(text)} placeholder="搜索模型或供应商" class="rounded-(--radius) border border-border bg-card px-3 py-2 text-sm text-foreground placeholder:text-muted-foreground" />
              <Each of={allModels()}>
                {(option) => {
                  const visible = () => {
                    const normalized = query().trim().toLowerCase();
                    const { provider, model } = option();
                    return !normalized || `${provider.name} ${model.name ?? ""} ${model.id}`.toLowerCase().includes(normalized);
                  };
                  return (
                    <If when={visible()}>
                      <ModelRow {...props} provider={option().provider} model={option().model} showProvider onEdit={() => openModel(option().provider.id, option().model.id)} onPickIcon={() => pickModelIcon(option().provider, option().model)} />
                    </If>
                  );
                }}
              </Each>
            </View>
          </If>
          <If when={view() === "providers" && editor()?.kind !== "provider"}>
            <View class="flex-row items-start gap-4">
              <View class="w-64 shrink-0 gap-2">
                <Input testID="provider-search" value={query()} onChange={(text) => query.set(text)} placeholder="搜索供应商" class="rounded-(--radius) border border-border bg-card px-3 py-2 text-sm text-foreground placeholder:text-muted-foreground" />
                <Button testID="provider-add" class="rounded-(--radius) border border-border bg-card px-3 py-2 hover:bg-accent" onPress={() => openProvider()}>
                  <Text class="text-sm text-foreground">＋ 添加供应商</Text>
                </Button>
                <Scroll class="max-h-[520px]">
                  <View class="gap-2 pr-1">
                    <Each of={filteredProviders()}>
                      {(provider) => (
                        <Button
                          testID={`provider-select-${provider().id}`}
                          selected={selected()?.id === provider().id}
                          class="rounded-(--radius) border border-border bg-card px-3 py-2.5 hover:bg-accent/50 selected:border-primary/50 selected:bg-accent/50"
                          onPress={() => { selectedId.set(provider().id); editor.set(null); }}
                        >
                          <View class="flex-row items-center gap-2.5">
                            <ProviderIcon icon={provider().icon} size={20} />
                            <View class="min-w-0 flex-1 gap-0.5">
                              <Text numberOfLines={1} class="text-sm font-medium text-foreground">{provider().name}</Text>
                              <Text class="text-xs text-muted-foreground">{provider().models.length} 个模型</Text>
                            </View>
                            <View class={provider().enabled ? "h-2 w-2 rounded-full bg-success" : "h-2 w-2 rounded-full bg-muted-foreground"} />
                          </View>
                        </Button>
                      )}
                    </Each>
                  </View>
                </Scroll>
              </View>
              <View class="min-w-0 flex-1 gap-3">
                <Each of={selected() ? [selected()!] : []}>
                  {(provider) => (
                    <ProviderDetail
                      {...props}
                      provider={provider()}
                      onEditProvider={() => openProvider(provider().id)}
                      onEditModel={(modelId) => openModel(provider().id, modelId)}
                      onPickProviderIcon={() => pickProviderIcon(provider())}
                      onPickModelIcon={(modelId) => {
                        const model = provider().models.find((item) => item.id === modelId);
                        if (model) pickModelIcon(provider(), model);
                      }}
                    />
                  )}
                </Each>
                <Each of={modelProvider() ? [modelProvider()!] : []}>
                  {(provider) => {
                    const current = editor();
                    const modelId = current?.kind === "model" ? current.modelId : undefined;
                    return <ModelEditor {...props} provider={provider()} model={provider().models.find((model) => model.id === modelId)} onClose={() => editor.set(null)} />;
                  }}
                </Each>
                <If when={!selected() && !modelProvider()}>
                  <View class="items-center rounded-xl border border-dashed border-border px-6 py-16">
                    <Text class="text-sm text-muted-foreground">选择或添加一个供应商开始配置。</Text>
                  </View>
                </If>
              </View>
            </View>
          </If>
        </View>
      </Scroll>
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
