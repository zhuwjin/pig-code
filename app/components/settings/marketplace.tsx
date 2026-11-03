import { effect, Scroll, signal, Text, token, View } from "nsh";
import { Button, Input } from "nsh/base";
import { appLocale, t, tCount } from "../../i18n";
import {
  errorMessage,
  type PluginMarketplacePackage,
  type PluginMarketplacePreview,
  type PluginMarketplaceSource,
  type PluginPackageInfo,
} from "../../protocol";
import type { UiContext } from "../../ui-context";
import { Icon } from "../icon";

export type MarketplaceAction = "installed" | "installing" | "update" | "install";

export function marketplaceAction(
  installed: PluginPackageInfo | undefined,
  digest: string,
  installing: boolean,
): MarketplaceAction {
  if (installed?.digest === digest) return "installed";
  if (installing) return "installing";
  return installed ? "update" : "install";
}

export function localizedMarketplaceDescription(
  plugin: PluginMarketplacePackage,
  locale = appLocale(),
): string {
  return plugin.description[locale]
    ?? plugin.description.en
    ?? Object.values(plugin.description)[0]
    ?? plugin.id;
}

function installedPackage(packages: PluginPackageInfo[], id: string): PluginPackageInfo | undefined {
  return packages.find((plugin) => plugin.id === id && plugin.enabled)
    ?? packages.find((plugin) => plugin.id === id);
}

export function PluginMarketplace(props: UiContext) {
  const { store } = props;
  const url = signal("");
  const previewing = signal(false);
  const preview = signal<PluginMarketplacePreview | null>(null);
  const managementError = signal("");
  const removing = signal<PluginMarketplaceSource | null>(null);
  const attempted = new Set<string>();
  const activeId = () => store.activePluginMarketplace();
  const activeSource = () => store.pluginMarketplaces().find((source) => source.id === activeId());
  const packages = () => store.pluginCatalog()[activeId()] ?? [];
  const loaded = () => store.pluginCatalog()[activeId()] !== undefined;
  const loading = () => store.pluginMarketplaceLoading()[activeId()] ?? false;
  const catalogError = () => store.pluginMarketplaceErrors()[activeId()] ?? "";
  const installing = () => store.pluginMarketplaceInstalling();
  effect(() => {
    const sources = store.pluginMarketplaces();
    for (const id of attempted) {
      if (!sources.some((source) => source.id === id)) attempted.delete(id);
    }
    const id = activeId();
    const source = sources.find((item) => item.id === id);
    if (!source || attempted.has(id)) return;
    attempted.add(id);
    void store.refreshMarketplace(id);
  });
  const previewSource = async () => {
    const indexUrl = url().trim();
    if (!indexUrl || previewing()) return;
    previewing.set(true);
    managementError.set("");
    preview.set(null);
    try {
      const result = await store.previewPluginMarketplace(indexUrl);
      if (result.official) throw new Error(t("settings.marketplace.add.official_exists"));
      preview.set(result);
    } catch (error) {
      managementError.set(errorMessage(error));
    } finally {
      previewing.set(false);
    }
  };
  const confirmAdd = () => {
    const candidate = preview();
    if (!candidate) return;
    try {
      const id = store.addPluginMarketplace(candidate);
      store.selectPluginMarketplace(id);
      preview.set(null);
      url.set("");
      managementError.set("");
    } catch (error) {
      managementError.set(errorMessage(error));
    }
  };
  return (
    <View testID="plugin-marketplace-page" class="min-w-0 flex-1 bg-background">
      <Scroll class="flex-1">
        <View class="w-full max-w-[960px] gap-6 p-6">
          <View class="flex-row items-start gap-4 border-b border-border pb-4">
            <View class="min-w-0 flex-1 gap-2">
              <Text class="text-xl font-semibold text-foreground">{t("settings.marketplace.title")}</Text>
              <Text class="text-sm text-muted-foreground">{t("settings.marketplace.description")}</Text>
            </View>
            <Button
              testID="market-refresh"
              disabled={loading() || !activeSource()}
              class="flex-row items-center gap-1.5 rounded-(--radius) border border-border px-3 py-1.5 hover:bg-accent disabled:opacity-40"
              onPress={() => void store.refreshMarketplace()}
            >
              <Icon name="refresh-cw" size={13} class={loading() ? "animate-spin" : undefined} color={token("muted-foreground")} />
              <Text class="text-xs text-muted-foreground">
                {t(loading() ? "settings.marketplace.refreshing" : "settings.marketplace.refresh")}
              </Text>
            </Button>
          </View>

          <View class="gap-3">
            <Text class="text-sm font-medium text-foreground">{t("settings.marketplace.sources.title")}</Text>
            <View class="flex-row flex-wrap gap-2">
              <Each of={store.pluginMarketplaces()} key={(source) => source.id}>
                {(source) => (
                  <View class="flex-row items-center gap-1">
                    <Button
                      testID={`marketplace-source-${source().id}`}
                      selected={activeId() === source().id}
                      class="rounded-full border border-border px-3 py-1.5 hover:bg-accent/50 selected:border-primary/60 selected:bg-accent"
                      onPress={() => store.selectPluginMarketplace(source().id)}
                    >
                      <View class="flex-row items-center gap-1.5">
                        <Icon name={source().official ? "globe" : "package"} size={13} color={token("muted-foreground")} />
                        <Text class="text-xs text-foreground">{source().name}</Text>
                        <If when={source().official}>
                          <Text class="rounded bg-accent px-1 py-0.5 text-[10px] text-muted-foreground">
                            {t("settings.marketplace.official_badge")}
                          </Text>
                        </If>
                      </View>
                    </Button>
                    <If when={!source().official}>
                      <Button
                        testID={`marketplace-remove-${source().id}`}
                        accessibilityLabel={t("settings.marketplace.sources.remove_label", { name: source().name })}
                        class="h-6 w-6 items-center justify-center rounded-full hover:bg-destructive/10"
                        onPress={() => removing.set(source())}
                      >
                        <Icon name="x" size={12} color={token("muted-foreground")} />
                      </Button>
                    </If>
                  </View>
                )}
              </Each>
            </View>
            <If when={activeSource()}>
              {(source) => (
                <View class="gap-1">
                  <Text selectable class="text-xs text-muted-foreground">{source().index_url}</Text>
                  <Text selectable class="font-mono text-[10px] text-muted-foreground">{source().fingerprint}</Text>
                </View>
              )}
            </If>
            <If when={removing()}>
              {(source) => (
                <View class="gap-2 rounded-(--radius) border border-destructive/30 bg-destructive/10 p-3">
                  <Text class="text-xs text-foreground">{t("settings.marketplace.sources.remove_confirm", { name: source().name })}</Text>
                  <View class="flex-row gap-2">
                    <Button
                      testID="marketplace-remove-confirm"
                      class="rounded-(--radius) bg-destructive px-3 py-1"
                      onPress={() => {
                        try {
                          store.removePluginMarketplace(source().id);
                          removing.set(null);
                          managementError.set("");
                        } catch (error) {
                          managementError.set(errorMessage(error));
                        }
                      }}
                    >
                      <Text class="text-xs text-background">{t("settings.marketplace.sources.remove")}</Text>
                    </Button>
                    <Button testID="marketplace-remove-cancel" class="rounded-(--radius) px-3 py-1 hover:bg-accent" onPress={() => removing.set(null)}>
                      <Text class="text-xs text-foreground">{t("common.cancel")}</Text>
                    </Button>
                  </View>
                </View>
              )}
            </If>
          </View>

          <View class="gap-3 rounded-xl border border-border p-4">
            <View class="gap-1">
              <Text class="text-sm font-medium text-foreground">{t("settings.marketplace.add.title")}</Text>
              <Text class="text-xs leading-5 text-muted-foreground">{t("settings.marketplace.add.description")}</Text>
            </View>
            <View class="flex-row items-center gap-2">
              <Input
                testID="marketplace-url-input"
                value={url()}
                onChange={(value) => url.set(value)}
                onSubmit={() => void previewSource()}
                placeholder={t("settings.marketplace.add.url_placeholder")}
                class="min-w-0 flex-1 rounded-(--radius) border border-input bg-background px-3 py-2 text-sm text-foreground selection:bg-foreground placeholder:text-muted-foreground"
              />
              <Button
                testID="marketplace-preview"
                disabled={previewing() || !url().trim()}
                class="shrink-0 rounded-(--radius) border border-border px-3 py-2 hover:bg-accent disabled:opacity-40"
                onPress={() => void previewSource()}
              >
                <Text class="text-xs text-foreground">
                  {t(previewing() ? "settings.marketplace.add.previewing" : "settings.marketplace.add.preview")}
                </Text>
              </Button>
            </View>
            <If when={preview()}>
              {(candidate) => (
                <View class="gap-3 rounded-(--radius) bg-accent/40 p-3">
                  <Text class="text-sm font-medium text-foreground">{candidate().name}</Text>
                  <View class="gap-1">
                    <Text class="text-xs text-muted-foreground">{t("settings.marketplace.add.fingerprint")}</Text>
                    <Text selectable testID="marketplace-preview-fingerprint" class="font-mono text-xs text-foreground">{candidate().fingerprint}</Text>
                  </View>
                  <Text class="text-xs leading-5 text-muted-foreground">{t("settings.marketplace.add.trust_notice")}</Text>
                  <View class="flex-row gap-2">
                    <Button testID="marketplace-add-confirm" class="rounded-(--radius) bg-primary px-3 py-1.5" onPress={confirmAdd}>
                      <Text class="text-xs text-primary-foreground">{t("settings.marketplace.add.confirm")}</Text>
                    </Button>
                    <Button testID="marketplace-add-cancel" class="rounded-(--radius) px-3 py-1.5 hover:bg-accent" onPress={() => preview.set(null)}>
                      <Text class="text-xs text-foreground">{t("common.cancel")}</Text>
                    </Button>
                  </View>
                </View>
              )}
            </If>
            <If when={managementError()}>
              <Text selectable testID="marketplace-management-error" class="text-xs leading-5 text-destructive">{managementError()}</Text>
            </If>
          </View>

          <View class="gap-3">
            <View class="flex-row items-center gap-2">
              <Text class="flex-1 text-sm font-medium text-foreground">{activeSource()?.name ?? ""}</Text>
              <Text class="text-xs text-muted-foreground">{tCount("settings.marketplace.packages", packages().length)}</Text>
            </View>
            <If when={catalogError()}>
              <View class="rounded-(--radius) bg-destructive/10 px-3 py-2">
                <Text selectable class="text-xs leading-5 text-destructive">{catalogError()}</Text>
              </View>
            </If>
            <If when={loading() && !loaded()}>
              <View testID="marketplace-loading" class="flex-row items-center gap-2 py-4">
                <Icon name="loader-circle" size={14} class="animate-spin" color={token("muted-foreground")} />
                <Text class="text-sm text-muted-foreground">{t("settings.marketplace.loading")}</Text>
              </View>
            </If>
            <If when={!loaded() && !loading() && !catalogError()}>
              <Text class="py-4 text-sm text-muted-foreground">{t("settings.marketplace.empty")}</Text>
            </If>
            <If when={loaded() && packages().length === 0}>
              <Text testID="marketplace-no-packages" class="py-4 text-sm text-muted-foreground">{t("settings.marketplace.no_packages")}</Text>
            </If>
            <Each of={packages()} key={(plugin) => plugin.id}>
              {(plugin) => {
                const installed = () => installedPackage(store.pluginPackages(), plugin().id);
                const action = () => marketplaceAction(
                  installed(),
                  plugin().digest,
                  installing()?.sourceId === activeId() && installing()?.packageId === plugin().id,
                );
                return (
                  <View class="gap-2 rounded-(--radius) border border-border p-3">
                    <View class="flex-row items-center gap-3">
                      <View class="min-w-0 flex-1 gap-1">
                        <Text numberOfLines={1} class="text-sm font-semibold text-foreground">{plugin().name}</Text>
                        <Text numberOfLines={1} class="font-mono text-xs text-muted-foreground">{plugin().id} · {plugin().version}</Text>
                      </View>
                      <Button
                        testID={`market-install-${plugin().id}`}
                        disabled={action() === "installed" || Boolean(installing())}
                        class="shrink-0 rounded-(--radius) border border-border px-3 py-1.5 hover:bg-accent disabled:opacity-50"
                        onPress={() => void store.installMarketplacePlugin(plugin())}
                      >
                        <Text class="text-xs text-foreground">{t(`settings.marketplace.${action()}`)}</Text>
                      </Button>
                    </View>
                    <Text class="text-sm leading-5 text-muted-foreground">{localizedMarketplaceDescription(plugin())}</Text>
                  </View>
                );
              }}
            </Each>
          </View>
        </View>
      </Scroll>
    </View>
  );
}
