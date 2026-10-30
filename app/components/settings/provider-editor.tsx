import { effect, signal, Text, View } from "nsh";
import { Button, Checkbox, Input } from "nsh/base";
import type { ApiFormat, ProviderConfig } from "../../protocol";
import type { UiContext } from "../../ui-context";

const FORMATS: { id: ApiFormat; label: string }[] = [
  { id: "OpenAiChat", label: "OpenAI Chat" },
  { id: "OpenAiResponses", label: "OpenAI Responses" },
  { id: "AnthropicMessages", label: "Anthropic" },
];

function slugify(value: string): string {
  return value.trim().toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "");
}

export function ProviderEditor(props: UiContext & {
  provider?: ProviderConfig;
  onClose: () => void;
  onSaved: (providerId: string) => void;
}) {
  const name = signal("");
  const id = signal("");
  const baseUrl = signal("https://api.openai.com/v1");
  const format = signal<ApiFormat>("OpenAiChat");
  const requiresKey = signal(true);
  const apiKey = signal("");
  const error = signal("");
  let idDirty = false;
  effect(() => {
    idDirty = false;
    name.set(props.provider?.name ?? "");
    id.set(props.provider?.id ?? "");
    baseUrl.set(props.provider?.base_url ?? "https://api.openai.com/v1");
    format.set(props.provider?.api_format ?? "OpenAiChat");
    requiresKey.set(props.provider?.requires_api_key ?? true);
    apiKey.set("");
    error.set("");
  });
  effect(() => {
    if (!props.provider && !idDirty) id.set(slugify(name()));
  });
  const save = () => {
    const providerId = id().trim();
    if (!name().trim()) {
      error.set("请输入供应商名称。");
      return;
    }
    if (!/^[a-z0-9][a-z0-9._-]*$/.test(providerId)) {
      error.set("标识需以小写字母或数字开头，只能包含小写字母、数字、点、下划线和连字符。");
      return;
    }
    if (!props.provider && props.store.config()?.providers.some((item) => item.id === providerId)) {
      error.set("这个供应商标识已经存在。");
      return;
    }
    if (!/^https?:\/\/\S+$/.test(baseUrl().trim())) {
      error.set("请输入以 http:// 或 https:// 开头的连接地址。");
      return;
    }
    const provider: ProviderConfig = {
      ...(props.provider ?? {}),
      id: providerId,
      name: name().trim(),
      base_url: baseUrl().trim(),
      api_format: format(),
      requires_api_key: requiresKey(),
      enabled: props.provider?.enabled ?? true,
      models: props.provider?.models ?? [],
    };
    if (!props.store.upsertProvider(provider)) {
      error.set(props.store.globalError() || "保存供应商失败。");
      return;
    }
    if (apiKey().trim() && !props.store.setProviderApiKey(providerId, apiKey().trim())) {
      error.set(props.store.globalError() || "供应商已保存，但密钥写入失败。");
      return;
    }
    apiKey.set("");
    props.onSaved(providerId);
  };
  return (
    <View testID="provider-editor" class="gap-4 rounded-xl border border-border bg-card p-4">
      <View class="flex-row items-center">
        <Text class="flex-1 text-base font-semibold text-foreground">
          {props.provider ? "编辑供应商" : "添加供应商"}
        </Text>
        <Button accessibilityLabel="关闭供应商编辑" class="rounded px-2 py-1 hover:bg-muted" onPress={props.onClose}>
          <Text class="text-sm text-muted-foreground">取消</Text>
        </Button>
      </View>
      <View class="flex-row gap-3">
        <View class="flex-1 gap-1.5">
          <Text class="text-xs text-muted-foreground">名称</Text>
          <Input testID="provider-name" value={name()} onChange={(text) => name.set(text)} placeholder="例如 DeepSeek" class="rounded-(--radius) border border-border bg-background px-3 py-2 text-sm text-foreground" />
        </View>
        <View class="flex-1 gap-1.5">
          <Text class="text-xs text-muted-foreground">标识</Text>
          <Input testID="provider-id" value={id()} readonly={Boolean(props.provider)} onChange={(text) => { idDirty = true; id.set(text); }} placeholder="deepseek" class="rounded-(--radius) border border-border bg-background px-3 py-2 font-mono text-sm text-foreground" />
        </View>
      </View>
      <View class="gap-1.5">
        <Text class="text-xs text-muted-foreground">连接地址</Text>
        <Input testID="provider-base-url" value={baseUrl()} onChange={(text) => baseUrl.set(text)} placeholder="https://api.example.com/v1" class="rounded-(--radius) border border-border bg-background px-3 py-2 font-mono text-xs text-foreground" />
      </View>
      <View class="gap-2">
        <Text class="text-xs text-muted-foreground">API 协议</Text>
        <View class="flex-row flex-wrap gap-2">
          <Each of={FORMATS} key={(item) => item.id}>
            {(item) => (
              <Button
                testID={`provider-format-${item().id}`}
                selected={format() === item().id}
                class="rounded-full border border-border px-3 py-1.5 hover:bg-accent selected:border-primary selected:bg-accent"
                onPress={() => format.set(item().id)}
              >
                <Text class="text-xs text-foreground">{item().label}</Text>
              </Button>
            )}
          </Each>
        </View>
      </View>
      <View class="gap-2 rounded-(--radius) bg-muted/40 p-3">
        <Checkbox
          testID="provider-requires-key"
          checked={requiresKey()}
          onChange={(checked) => requiresKey.set(checked)}
          class="flex-row items-center gap-2"
          indicatorClass="h-4 w-4 rounded border border-border"
        >
          <Text class="text-sm text-foreground">需要 API Key</Text>
        </Checkbox>
        <If when={requiresKey()}>
          <Input
            testID="provider-api-key"
            value={apiKey()}
            onChange={(text) => apiKey.set(text)}
            masked
            placeholder={props.provider ? "留空以保留已保存的密钥" : "API Key（可稍后配置）"}
            class="rounded-(--radius) border border-border bg-background px-3 py-2 font-mono text-sm text-foreground placeholder:text-muted-foreground"
          />
        </If>
        <Text class="text-xs leading-5 text-muted-foreground">密钥由系统密钥库保存，不写入页面配置。</Text>
      </View>
      <If when={error()}><Text selectable class="text-xs leading-5 text-destructive">{error()}</Text></If>
      <View class="flex-row justify-end gap-2">
        <Button class="rounded-(--radius) border border-border px-4 py-2 hover:bg-muted" onPress={props.onClose}>
          <Text class="text-sm text-foreground">取消</Text>
        </Button>
        <Button testID="provider-save" class="rounded-(--radius) bg-primary px-4 py-2" onPress={save}>
          <Text class="text-sm text-primary-foreground">保存供应商</Text>
        </Button>
      </View>
    </View>
  );
}
