import type { AgentStore } from "./store";
import type { ThemeStore } from "./theme";

export interface UiContext {
  store: AgentStore;
  themeStore: ThemeStore;
  onManualLayoutChange?: (collapsing: boolean) => void;
  onLayoutAnimation?: (active: boolean) => void;
  onLayoutHeightChange?: (delta: number) => void;
}

export function toggleExpanded(context: UiContext, id: string): void {
  context.onManualLayoutChange?.(context.store.isExpanded(id));
  context.store.toggleExpanded(id);
}

export function toggleTurnExpanded(context: UiContext, id: string): void {
  context.onManualLayoutChange?.(context.store.isTurnExpanded(id));
  context.store.toggleTurnExpanded(id);
}

export function workspaceName(path: string): string {
  const normalized = path.replace(/[\\/]+$/, "");
  return normalized.split(/[\\/]/).filter(Boolean).pop() || path;
}
