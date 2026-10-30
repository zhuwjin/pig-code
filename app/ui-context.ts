import type { AgentStore } from "./store";
import type { ThemeStore } from "./theme";

export interface UiContext {
  store: AgentStore;
  themeStore: ThemeStore;
}

export function workspaceName(path: string): string {
  const normalized = path.replace(/[\\/]+$/, "");
  return normalized.split(/[\\/]/).filter(Boolean).pop() || path;
}
