import { computed, type JSX } from "nsh";
import { appLocale } from "../i18n";
import type { UiContext } from "../ui-context";
import type { PluginPageEntry } from "./ui-coordinator";
import { PluginSurface } from "./surface";

export function pluginPageKey(entry: PluginPageEntry): string {
  return `${entry.instance}:${entry.page.id}`;
}

export function pluginPageTitle(entry: PluginPageEntry): string {
  return entry.page.title[appLocale()] ?? entry.page.title.en ?? entry.page.id;
}

export function PluginPageView(props: UiContext & { entry: PluginPageEntry }): JSX.Element {
  const revision = computed(() => props.store.pluginRuntime()[props.entry.instance]?.generation ?? 1);
  return (
    <PluginSurface
      agent={props.store.agent}
      instance={props.entry.instance}
      contribution={props.entry.page.id}
      revision={revision()}
      context={{ sessionId: props.store.current() }}
      theme={props.themeStore.serialized()}
      locale={appLocale()}
      appearance={props.themeStore.mode()}
      testID={`plugin-page-${props.entry.pkg.id}-${props.entry.page.id}`}
    />
  );
}
