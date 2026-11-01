import type {
  PluginMessageKind,
  PluginProtocolError,
  PluginUiRejection,
  PluginService,
  PluginStream,
  RemoteDocument,
  RemoteNode,
} from "../protocol";

export const PLUGIN_PROTOCOL_VERSION = 1;
export const PLUGIN_UI_PROTOCOL_VERSION = 1;
export const PLUGIN_SERVICES_PROTOCOL_VERSION = 1;

export const PluginFeatures = {
  uiAtomic: 1n << 0n,
  uiHostPatch: 1n << 1n,
  uiInput: 1n << 2n,
  uiIme: 1n << 3n,
  uiScroll: 1n << 4n,
  uiAccessibility: 1n << 5n,
  dynamicContributions: 1n << 6n,
  serviceConfig: 1n << 7n,
  serviceHttp: 1n << 8n,
  serviceCredentials: 1n << 9n,
  serviceSession: 1n << 10n,
  serviceWorkspace: 1n << 11n,
  serviceNavigation: 1n << 12n,
  resources: 1n << 13n,
  i18n: 1n << 14n,
  lifecycleHealth: 1n << 15n,
  backpressure: 1n << 16n,
} as const;

export const PLUGIN_HOST_FEATURES = Object.values(PluginFeatures).reduce((all, feature) => all | feature, 0n);

export interface PluginEnvelope<T = unknown> {
  protocol: number;
  kind: PluginMessageKind;
  instance: string;
  epoch: number;
  stream: PluginStream;
  sequence: number;
  requestId?: string | null;
  generation: number;
  body: T;
}

export function pluginEnvelope<T>(options: {
  kind: PluginMessageKind;
  instance: string;
  epoch: number;
  stream: PluginStream;
  sequence: number;
  generation: number;
  body: T;
  requestId?: string;
}): PluginEnvelope<T> {
  return {
    protocol: PLUGIN_PROTOCOL_VERSION,
    kind: options.kind,
    instance: options.instance,
    epoch: options.epoch,
    stream: options.stream,
    sequence: options.sequence,
    requestId: options.requestId ?? null,
    generation: options.generation,
    body: options.body,
  };
}

export function validatePluginEnvelope(value: unknown): value is PluginEnvelope {
  if (!value || typeof value !== "object") return false;
  const envelope = value as Partial<PluginEnvelope>;
  return envelope.protocol === PLUGIN_PROTOCOL_VERSION &&
    typeof envelope.kind === "string" &&
    typeof envelope.instance === "string" && envelope.instance.length > 0 &&
    typeof envelope.epoch === "number" && Number.isSafeInteger(envelope.epoch) &&
    typeof envelope.stream === "string" &&
    typeof envelope.sequence === "number" && Number.isSafeInteger(envelope.sequence) && envelope.sequence > 0 &&
    typeof envelope.generation === "number" && Number.isSafeInteger(envelope.generation) &&
    "body" in envelope;
}

export class PluginSequencer {
  private sequences = new Map<PluginStream, number>();

  next(stream: PluginStream): number {
    const sequence = (this.sequences.get(stream) ?? 0) + 1;
    this.sequences.set(stream, sequence);
    return sequence;
  }

  accept(stream: PluginStream, sequence: number): boolean {
    if (!Number.isSafeInteger(sequence) || sequence <= (this.sequences.get(stream) ?? 0)) return false;
    this.sequences.set(stream, sequence);
    return true;
  }

  reset(): void {
    this.sequences.clear();
  }
}

export interface ServiceResult {
  service: PluginService;
  result: { Ok?: unknown; Err?: PluginProtocolError } | unknown;
}

export function serviceResultValue(response: ServiceResult): unknown {
  const result = response.result as { Ok?: unknown; Err?: PluginProtocolError };
  if (result && typeof result === "object" && "Err" in result && result.Err) throw new Error(result.Err.message);
  return result && typeof result === "object" && "Ok" in result ? result.Ok : response.result;
}

export function documentNodeMap(document: RemoteDocument): Map<string, RemoteNode> {
  return new Map(document.nodes.map((node) => [node.id, node]));
}

export function documentIsCurrent(document: RemoteDocument, epoch: number, generation: number): boolean {
  return document.epoch === epoch && document.generation === generation;
}

export const REMOTE_STYLE_KEYS = new Set([
  "backgroundColor", "borderColor", "borderRadius", "borderWidth", "opacity", "display", "overflow",
  "color", "fontSize", "fontWeight", "fontFamily", "lineHeight", "letterSpacing", "textAlign",
  "width", "height", "minWidth", "minHeight", "maxWidth", "maxHeight", "flex", "flexGrow", "flexShrink",
  "flexDirection", "flexWrap", "alignItems", "alignContent", "alignSelf", "justifyContent", "gap",
  "padding", "paddingHorizontal", "paddingVertical", "paddingTop", "paddingRight", "paddingBottom", "paddingLeft",
  "margin", "marginHorizontal", "marginVertical", "marginTop", "marginRight", "marginBottom", "marginLeft",
  "position", "top", "right", "bottom", "left", "zIndex", "aspectRatio", "transform",
]);

export const REMOTE_PROP_KEYS = new Set([
  "placeholder", "multiline", "submitBehavior", "maxLength", "readonly", "autoFocus", "focusable", "disabled",
  "numberOfLines", "ellipsizeMode", "selectable", "horizontal", "scrollbar", "scrollable", "bounces",
  "resizeMode", "keyboardDismissMode", "scrollEventThrottle", "contentOffset", "contentInset", "selectedRange",
  "allowFontScaling", "maxFontSizeMultiplier", "autoCapitalize", "autoCorrect", "returnKeyType", "keyboardType",
]);

export const REMOTE_ACCESSIBILITY_KEYS = new Set([
  "accessible", "accessibilityLabel", "accessibilityHint", "role", "accessibilityState", "accessibilityValue",
  "accessibilityElementsHidden", "importantForAccessibility", "accessibilityActions",
]);

export function remoteStyle(node: RemoteNode): Record<string, unknown> {
  return Object.fromEntries(Object.entries(node.style ?? {}).filter(([key]) => REMOTE_STYLE_KEYS.has(key)));
}

export function remoteProps(node: RemoteNode): Record<string, unknown> {
  return Object.fromEntries(Object.entries(node.props ?? {}).filter(([key]) => REMOTE_PROP_KEYS.has(key)));
}

export function remoteAccessibility(node: RemoteNode): Record<string, unknown> {
  return Object.fromEntries(Object.entries(node.accessibility ?? {}).filter(([key]) => REMOTE_ACCESSIBILITY_KEYS.has(key)));
}

export function wants(node: RemoteNode, event: string): boolean {
  return node.press && event === "press" || (node.events ?? []).includes(event);
}

export function remoteDocumentRejection(
  document: RemoteDocument,
  envelope: PluginEnvelope,
  current?: RemoteDocument,
): PluginUiRejection | null {
  const reject = (code: string, message: string): PluginUiRejection => ({
    code,
    target: "remote.document",
    contribution: null,
    message,
  });
  if (document.instance !== envelope.instance || document.epoch !== envelope.epoch) {
    return reject("stale_epoch", "document instance or epoch does not match its envelope");
  }
  if (document.generation > envelope.generation) {
    return reject("stale_generation", "document generation is newer than the host snapshot");
  }
  if (!Number.isSafeInteger(document.sequence) || document.sequence <= 0) {
    return reject("invalid_operation", "document revision must be a positive safe integer");
  }
  if (!Array.isArray(document.nodes) || !Array.isArray(document.roots) || !Array.isArray(document.warnings)) {
    return reject("invalid_operation", "document nodes, roots and warnings must be arrays");
  }
  const ids = new Set<string>();
  for (const node of document.nodes) {
    if (!node || typeof node.id !== "string" || !node.id || ids.has(node.id) ||
      !["view", "text", "scroll", "input", "image"].includes(node.kind) ||
      !Array.isArray(node.children) || node.children.some((child) => typeof child !== "string")) {
      return reject("invalid_operation", "document contains a duplicate or invalid node");
    }
    ids.add(node.id);
  }
  if (document.roots.some((id) => !ids.has(id)) ||
    document.nodes.some((node) => node.children.some((child) => !ids.has(child)))) {
    return reject("invalid_operation", "document contains a missing child or root");
  }
  if (current && current.epoch > document.epoch) {
    return reject("stale_epoch", "document belongs to an older engine epoch");
  }
  if (current && current.epoch === document.epoch && current.sequence > document.sequence) {
    return reject("stale_revision", "document belongs to an older host revision");
  }
  return null;
}

const HOST_SAFE_STYLE_KEYS = new Set([
  "backgroundColor", "borderColor", "borderRadius", "borderWidth", "opacity", "display", "overflow",
  "color", "fontSize", "fontWeight", "fontFamily", "lineHeight", "letterSpacing", "textAlign",
  "width", "height", "minWidth", "minHeight", "maxWidth", "maxHeight", "flex", "flexGrow", "flexShrink",
  "flexDirection", "flexWrap", "alignItems", "alignContent", "alignSelf", "justifyContent", "gap",
  "padding", "paddingHorizontal", "paddingVertical", "paddingTop", "paddingRight", "paddingBottom", "paddingLeft",
  "margin", "marginHorizontal", "marginVertical", "marginTop", "marginRight", "marginBottom", "marginLeft",
  "aspectRatio",
]);

function safeHostStyleValue(key: string, value: unknown): boolean {
  if (value === null) return true;
  if (typeof value === "number") {
    if (!Number.isFinite(value)) return false;
    if (key === "opacity") return value >= 0 && value <= 1;
    return Math.abs(value) <= 10000;
  }
  if (typeof value !== "string" || value.length > 128) return false;
  if (["color", "backgroundColor", "borderColor"].includes(key)) {
    return /^#[0-9a-fA-F]{6}([0-9a-fA-F]{2})?$/.test(value);
  }
  if (key === "fontFamily") return /^[\w .-]+$/.test(value);
  if (["display", "flexDirection", "flexWrap", "alignItems", "alignContent", "alignSelf", "justifyContent", "overflow", "fontWeight", "textAlign"].includes(key)) {
    return /^[a-zA-Z][a-zA-Z0-9-]{0,31}$/.test(value);
  }
  return /^-?\d+(\.\d+)?(px|%)?$/.test(value) || value === "auto";
}

export function safeHostStyle(
  style: Record<string, unknown> | undefined,
  allowed?: readonly string[],
): Record<string, unknown> {
  return Object.fromEntries(Object.entries(style ?? {}).filter(([key, value]) =>
    HOST_SAFE_STYLE_KEYS.has(key) && REMOTE_STYLE_KEYS.has(key) &&
    (!allowed || allowed.includes(key)) && safeHostStyleValue(key, value)
  ));
}
