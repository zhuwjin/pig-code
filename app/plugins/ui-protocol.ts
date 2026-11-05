import type {
  PluginMessageKind,
  PluginProtocolError,
  PluginService,
  PluginStream,
} from "../protocol";

export const PLUGIN_PROTOCOL_VERSION = 1;
export const PLUGIN_UI_PROTOCOL_VERSION = 1;
export const PLUGIN_SERVICES_PROTOCOL_VERSION = 1;

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
