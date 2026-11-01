import type { PluginUiEvent, RemoteNode } from "../protocol";

export type PluginEventDispatcher = (event: PluginUiEvent) => void;

let nextEventId = 0;
const pendingCoalesced = new Map<string, ReturnType<typeof setTimeout>>();
const latestCoalesced = new Map<string, PluginUiEvent>();
const delivered = new Set<string>();
const deliveredOrder: string[] = [];

export function emitPluginEvent(
  dispatch: PluginEventDispatcher,
  input: {
    instance: string;
    contribution?: string;
    node: RemoteNode;
    kind: string;
    payload?: unknown;
    surfaceRevision: number;
  },
): void {
  const event: PluginUiEvent = {
    contribution: input.contribution ?? input.node.exportName ?? input.node.id,
    node: input.node.id,
    eventId: `${input.instance}:${++nextEventId}`,
    kind: input.kind,
    payload: (input.payload ?? {}) as Record<string, unknown>,
    surfaceRevision: input.surfaceRevision,
  };
  const coalesced = input.kind === "mouseMove" || input.kind === "hoverMove" || input.kind === "scroll";
  if (!coalesced) {
    dispatch(event);
    return;
  }
  const key = `${input.instance}:${input.node.id}:${input.kind}`;
  latestCoalesced.set(key, event);
  if (pendingCoalesced.has(key)) return;
  const timer = setTimeout(() => {
    pendingCoalesced.delete(key);
    const latest = latestCoalesced.get(key);
    latestCoalesced.delete(key);
    if (latest) dispatch(latest);
  }, 16);
  pendingCoalesced.set(key, timer);
}

const editCounts = new Map<string, number>();

export function nextEditCount(instance: string, node: string): number {
  const key = `${instance}:${node}`;
  const count = (editCounts.get(key) ?? 0) + 1;
  editCounts.set(key, count);
  return count;
}

export function createPluginHostEvent(input: {
  instance: string;
  contribution: string;
  node: string;
  kind: string;
  payload: Record<string, unknown>;
  surfaceRevision: number;
}): PluginUiEvent {
  return {
    contribution: input.contribution,
    node: input.node,
    eventId: `${input.instance}:host:${++nextEventId}`,
    kind: input.kind,
    payload: input.payload,
    surfaceRevision: input.surfaceRevision,
  };
}

export function acceptPluginEvent(event: PluginUiEvent, epoch: number, generation: number, document: {
  epoch: number;
  generation: number;
  sequence: number;
  nodes: RemoteNode[];
}): boolean {
  if (event.surfaceRevision !== document.sequence || document.epoch !== epoch || document.generation !== generation) return false;
  if (!document.nodes.some((node) => node.id === event.node)) return false;
  if (delivered.has(event.eventId)) return false;
  delivered.add(event.eventId);
  deliveredOrder.push(event.eventId);
  if (deliveredOrder.length > 4096) {
    const expired = deliveredOrder.shift();
    if (expired) delivered.delete(expired);
  }
  return true;
}

export function clearPluginEvents(instance?: string): void {
  const owned = (key: string) => !instance || key.startsWith(`${instance}:`);
  for (const [key, timer] of pendingCoalesced) {
    if (owned(key)) {
      clearTimeout(timer);
      pendingCoalesced.delete(key);
      latestCoalesced.delete(key);
    }
  }
  for (const key of editCounts.keys()) {
    if (owned(key)) editCounts.delete(key);
  }
  for (let index = deliveredOrder.length - 1; index >= 0; index -= 1) {
    const id = deliveredOrder[index];
    if (owned(id)) {
      delivered.delete(id);
      deliveredOrder.splice(index, 1);
    }
  }
  if (!instance) {
    latestCoalesced.clear();
    delivered.clear();
    deliveredOrder.length = 0;
  }
}
