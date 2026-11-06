import { colorScheme, computed, effect, signal, View, type JSX } from "nsh";
import { appLocale } from "../i18n";
import type { UiContext } from "../ui-context";
import type { PluginPage } from "../protocol";
import { pluginPages, type PluginPageEntry } from "./ui-coordinator";
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
      sessionId={props.store.current()}
      theme={props.themeStore.serialized()}
      locale={appLocale()}
      appearance={colorScheme()}
      mode={props.entry.pkg.component_entry ? "component" : "surface"}
      exportName={props.entry.page.surface}
      testID={`plugin-page-${props.entry.pkg.id}-${props.entry.page.id}`}
    />
  );
}

export function PluginPageHost(props: UiContext & {
  kind: PluginPage["kind"];
  active: string | null;
}): JSX.Element {
  const visited = signal<Set<string>>(new Set());
  effect(() => {
    const active = props.active;
    if (!active || visited().has(active)) return;
    visited.update((pages) => new Set(pages).add(active));
  });
  const visitedPages = () => pluginPages(
    props.store.pluginPackages(),
    props.kind,
    props.store.pluginRuntime(),
    props.store.pluginOrder(),
  ).filter((entry) => visited().has(pluginPageKey(entry)));
  return (
    <Each of={visitedPages()} key={(entry) => pluginPageKey(entry)}>
      {(entry) => {
        const key = pluginPageKey(entry());
        const active = () => props.active === key;
        return (
          <View
            class={active() ? "min-h-0 min-w-0 flex-1" : "hidden"}
            importantForAccessibility={active() ? "yes" : "no-hide-descendants"}
          >
            <PluginPageView {...props} entry={entry()} />
          </View>
        );
      }}
    </Each>
  );
}
