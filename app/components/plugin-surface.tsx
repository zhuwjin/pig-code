import { host, nativeComponent } from "nsh";
import type { UiContext } from "../ui-context";

type PluginEvent = { name: string; data: any };

const PluginSurface = nativeComponent<
  {
    package: string;
    root: string;
    slot: string;
    theme: string;
    metrics: { tokensPerSecond: number; running: boolean };
  },
  { pluginError: { message: string }; pluginAction: PluginEvent }
>("pig/plugin-surface", {
  props: ["package", "root", "slot", "theme", "metrics"],
  events: ["pluginError", "pluginAction"],
});

export const executablePlugins = host.available() && host.params.executablePlugins === true;
const pluginPackage = typeof host.params.pluginPackage === "string" ? host.params.pluginPackage : "";
const pluginRoot = typeof host.params.pluginRoot === "string" ? host.params.pluginRoot : "";

export function ExecutablePlugin(props: UiContext & { slot: string; class?: string }) {
  const { store, themeStore } = props;
  const running = () => {
    const id = store.current();
    return id ? (store.running()[id] ?? false) : false;
  };
  const rate = () => {
    const id = store.current();
    return id ? (store.outputRate()[id] ?? 0) : 0;
  };
  return (
    <PluginSurface
      package={pluginPackage}
      root={pluginRoot}
      slot={props.slot}
      theme={themeStore.serialized()}
      metrics={{ tokensPerSecond: rate(), running: running() }}
      class={props.class}
      onPluginError={(event) => store.globalError.set(`插件错误：${event.message}`)}
      onPluginAction={(event) => {
        switch (event.name) {
          case "composer.submit":
            store.send();
            break;
          case "composer.interrupt":
            store.submitOrStop();
            break;
          case "composer.setDraft":
            store.draft.set(typeof event.data.text === "string" ? event.data.text : "");
            break;
        }
      }}
    />
  );
}
