import { signal, Text, View } from "nsh";
import { Button, Input } from "nsh/base";
import { appLocale, t } from "../i18n";
import {
  errorMessage,
  type PluginConfigItem,
  type PluginCredentialDeclaration,
  type PluginPackageInfo,
} from "../protocol";

/**
 * The host-generated form for a plugin's declared configuration items. Values
 * are read and written through the ordinary host configuration service, so
 * the core validates them against the item's JSON Schema and a trusted
 * workspace can override them without a plugin-provided settings page.
 */
export function PluginConfigEditor(props: {
  plugin: () => PluginPackageInfo;
  read?: (plugin: PluginPackageInfo, key: string, sessionId?: string | null) => Promise<unknown>;
  write?: (
    plugin: PluginPackageInfo,
    key: string,
    value: unknown,
    sessionId?: string | null,
  ) => Promise<unknown>;
  sessionId?: () => string | null;
}) {
  const open = signal(false);
  const loading = signal(false);
  const values = signal<Record<string, unknown>>({});
  const status = signal("");
  const workspace = signal(false);
  const items = (): PluginConfigItem[] => props.plugin().contributions.config?.items ?? [];
  const scope = () => (workspace() ? props.sessionId?.() ?? null : null);
  const load = async () => {
    if (open()) {
      open.set(false);
      return;
    }
    open.set(true);
    loading.set(true);
    status.set("");
    try {
      const next: Record<string, unknown> = {};
      for (const item of items()) {
        next[item.key] = props.read
          ? await props.read(props.plugin(), item.key, scope())
          : item.default;
      }
      values.set(next);
    } catch (error) {
      status.set(t("settings.plugins.config.failed", { detail: errorMessage(error) }));
    } finally {
      loading.set(false);
    }
  };
  const changeScope = async (next: boolean) => {
    if (workspace() === next) return;
    if (next && !props.sessionId?.()) {
      status.set(t("settings.plugins.config.workspace_unavailable"));
      return;
    }
    workspace.set(next);
    if (open()) {
      open.set(false);
      await load();
    }
  };
  const save = async () => {
    status.set("");
    try {
      for (const item of items()) {
        await props.write?.(
          props.plugin(),
          item.key,
          values()[item.key] ?? item.default,
          scope(),
        );
      }
      status.set(t("settings.plugins.config.saved"));
    } catch (error) {
      status.set(t("settings.plugins.config.failed", { detail: errorMessage(error) }));
    }
  };
  const descriptionOf = (item: PluginConfigItem): string => {
    const locale = appLocale();
    return item.description?.[locale] ?? item.description?.en ?? item.key;
  };
  const optionsOf = (item: PluginConfigItem): string[] => {
    const schema = item.schema as { enum?: unknown[] } | null | undefined;
    return Array.isArray(schema?.enum) ? schema!.enum!.map((value) => String(value)) : [];
  };
  const kindOf = (item: PluginConfigItem): string => {
    const schema = item.schema as { type?: string } | null | undefined;
    return schema?.type ?? typeof item.default;
  };
  return (
    <View class="gap-2">
      <View class="flex-row flex-wrap items-center gap-2">
        <Button
          testID={`plugin-config-${props.plugin().id}`}
          class="rounded-(--radius) border border-border px-2 py-1 hover:bg-accent"
          onPress={() => void load()}
        >
          <Text class="text-xs text-foreground">
            {t(open() ? "common.cancel" : "settings.plugins.config.open")}
          </Text>
        </Button>
        <If when={open()}>
          <Button
            testID={`plugin-config-scope-user-${props.plugin().id}`}
            selected={!workspace()}
            class="rounded-full border border-border px-2 py-1 selected:border-primary/60 selected:bg-accent"
            onPress={() => void changeScope(false)}
          >
            <Text class="text-[10px] text-foreground">{t("settings.plugins.config.scope.user")}</Text>
          </Button>
          <Button
            testID={`plugin-config-scope-workspace-${props.plugin().id}`}
            selected={workspace()}
            class="rounded-full border border-border px-2 py-1 selected:border-primary/60 selected:bg-accent"
            onPress={() => void changeScope(true)}
          >
            <Text class="text-[10px] text-foreground">
              {t("settings.plugins.config.scope.workspace")}
            </Text>
          </Button>
        </If>
      </View>
      <If when={open()}>
        <View class="gap-2 rounded-(--radius) border border-border bg-accent/20 p-3">
          <If when={loading()}>
            <Text class="text-xs text-muted-foreground">{t("settings.plugins.config.loading")}</Text>
          </If>
          <Each of={items()} key={(item) => item.key}>
            {(item) => {
              const kind = kindOf(item());
              const options = optionsOf(item());
              const current = () => values()[item().key] ?? item().default;
              return (
                <View class="gap-1">
                  <Text class="text-xs font-medium text-foreground">{descriptionOf(item())}</Text>
                  <If when={kind === "boolean"}>
                    <Button
                      testID={`plugin-config-toggle-${props.plugin().id}-${item().key}`}
                      selected={Boolean(current())}
                      class="w-12 rounded-full border border-border px-2 py-0.5 selected:border-primary/60 selected:bg-accent"
                      onPress={() =>
                        values.update((all) => ({ ...all, [item().key]: !Boolean(current()) }))
                      }
                    >
                      <Text class="text-[10px] text-foreground">
                        {Boolean(current()) ? "on" : "off"}
                      </Text>
                    </Button>
                  </If>
                  <If when={kind !== "boolean" && options.length > 0}>
                    <View class="flex-row flex-wrap gap-1">
                      <Each of={options} key={(option) => option}>
                        {(option) => (
                          <Button
                            testID={`plugin-config-option-${props.plugin().id}-${item().key}-${option()}`}
                            selected={String(current()) === option()}
                            class="rounded-full border border-border px-2 py-1 selected:border-primary/60 selected:bg-accent"
                            onPress={() =>
                              values.update((all) => ({ ...all, [item().key]: option() }))
                            }
                          >
                            <Text class="text-[10px] text-foreground">{option()}</Text>
                          </Button>
                        )}
                      </Each>
                    </View>
                  </If>
                  <If when={kind !== "boolean" && options.length === 0}>
                    <Input
                      testID={`plugin-config-input-${props.plugin().id}-${item().key}`}
                      value={String(current())}
                      onChange={({ value: text }) =>
                        values.update((all) => ({
                          ...all,
                          [item().key]:
                            kind === "number" && text.trim() !== "" && Number.isFinite(Number(text))
                              ? Number(text)
                              : text,
                        }))
                      }
                      class="rounded-(--radius) border border-input bg-background px-3 py-2 text-xs text-foreground"
                    />
                  </If>
                  <If when={kind === "number" && options.length === 0}>
                    <Text class="text-[10px] text-muted-foreground">{kind}</Text>
                  </If>
                </View>
              );
            }}
          </Each>
          <View class="flex-row items-center gap-2">
            <Button
              testID={`plugin-config-save-${props.plugin().id}`}
              class="rounded-(--radius) bg-primary px-3 py-1"
              onPress={() => void save()}
            >
              <Text class="text-xs text-primary-foreground">
                {t("settings.plugins.config.save")}
              </Text>
            </Button>
            <If when={status()}>
              <Text class="text-[10px] text-muted-foreground">{status()}</Text>
            </If>
          </View>
        </View>
      </If>
    </View>
  );
}

/**
 * Host-generated entry for a package's `owned` credentials. The secret goes to
 * the OS credential store under the package's digest-scoped key; the plugin
 * can use it through provider/HTTP declarations but never reads it back.
 */
export function PluginCredentialEditor(props: {
  plugin: () => PluginPackageInfo;
  write?: (plugin: PluginPackageInfo, credential: string, secret: string) => Promise<unknown>;
  clear?: (plugin: PluginPackageInfo, credential: string) => Promise<unknown>;
}) {
  const values = signal<Record<string, string>>({});
  const status = signal("");
  const busy = signal("");
  const owned = (): PluginCredentialDeclaration[] =>
    (props.plugin().contributions.credentials ?? []).filter((item) => item.kind === "owned");
  const save = async (credential: PluginCredentialDeclaration) => {
    const secret = (values()[credential.id] ?? "").trim();
    if (!secret) return;
    busy.set(credential.id);
    status.set("");
    try {
      await props.write?.(props.plugin(), credential.id, secret);
      values.update((all) => ({ ...all, [credential.id]: "" }));
      status.set(t("settings.plugins.credentials.saved", { name: credential.label ?? credential.id }));
    } catch (error) {
      status.set(t("settings.plugins.credentials.failed", { detail: errorMessage(error) }));
    } finally {
      busy.set("");
    }
  };
  const clear = async (credential: PluginCredentialDeclaration) => {
    busy.set(credential.id);
    status.set("");
    try {
      await props.clear?.(props.plugin(), credential.id);
      values.update((all) => ({ ...all, [credential.id]: "" }));
      status.set(t("settings.plugins.credentials.cleared", { name: credential.label ?? credential.id }));
    } catch (error) {
      status.set(t("settings.plugins.credentials.failed", { detail: errorMessage(error) }));
    } finally {
      busy.set("");
    }
  };
  return (
    <If when={owned().length > 0}>
      <View class="gap-2 rounded-(--radius) border border-border bg-accent/20 p-3">
        <Text class="text-xs font-medium text-foreground">
          {t("settings.plugins.credentials.title")}
        </Text>
        <Each of={owned()} key={(credential) => credential.id}>
          {(credential) => (
            <View class="gap-1">
              <Text class="text-xs text-foreground">
                {credential().label ?? credential().id}
              </Text>
              <If when={credential().description}>
                <Text class="text-[10px] text-muted-foreground">{credential().description}</Text>
              </If>
              <View class="flex-row items-center gap-2">
                <Input
                  testID={`plugin-credential-input-${props.plugin().id}-${credential().id}`}
                  value={values()[credential().id] ?? ""}
                  masked={true}
                  placeholder={t("settings.plugins.credentials.placeholder")}
                  onChange={({ value: text }) =>
                    values.update((all) => ({ ...all, [credential().id]: text }))
                  }
                  class="min-w-0 flex-1 rounded-(--radius) border border-border bg-background px-3 py-2 text-xs text-foreground"
                />
                <Button
                  testID={`plugin-credential-save-${props.plugin().id}-${credential().id}`}
                  disabled={busy() === credential().id || !(values()[credential().id] ?? "").trim()}
                  class="rounded-(--radius) bg-primary px-3 py-1 disabled:opacity-40"
                  onPress={() => void save(credential())}
                >
                  <Text class="text-[10px] text-primary-foreground">
                    {t("settings.plugins.credentials.save")}
                  </Text>
                </Button>
                <Button
                  testID={`plugin-credential-clear-${props.plugin().id}-${credential().id}`}
                  disabled={busy() === credential().id}
                  class="rounded-(--radius) px-2 py-1 hover:bg-destructive/10 disabled:opacity-40"
                  onPress={() => void clear(credential())}
                >
                  <Text class="text-[10px] text-destructive">
                    {t("settings.plugins.credentials.clear")}
                  </Text>
                </Button>
              </View>
            </View>
          )}
        </Each>
        <If when={status()}>
          <Text class="text-[10px] text-muted-foreground">{status()}</Text>
        </If>
      </View>
    </If>
  );
}
