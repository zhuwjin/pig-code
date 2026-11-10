import { LayoutAnimation } from "nsh";
import type { AgentStore } from "./store";
import type { ThemeStore } from "./theme";

export interface UiContext {
  store: AgentStore;
  themeStore: ThemeStore;
}

export function toggleExpanded(context: UiContext, id: string): void {
  LayoutAnimation.configureNext(LayoutAnimation.Presets.easeInEaseOut);
  context.store.toggleExpanded(id);
}

export function toggleTurnExpanded(context: UiContext, id: string): void {
  LayoutAnimation.configureNext(LayoutAnimation.Presets.easeInEaseOut);
  context.store.toggleTurnExpanded(id);
}

export function workspaceName(path: string): string {
  const normalized = path.replace(/[\\/]+$/, "");
  return normalized.split(/[\\/]/).filter(Boolean).pop() || path;
}
