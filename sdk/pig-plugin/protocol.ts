// The `pig/plugin` wire contract: the same envelopes, streams, identities and
// error codes the Rust host speaks. Plugin authors use the typed helpers in
// `index.ts`; this file exists so the protocol is readable in one place.

export const PLUGIN_PROTOCOL_VERSION = 1;
export const PLUGIN_SERVICES_PROTOCOL_VERSION = 1;

export type PluginStream =
  | "lifecycle"
  | "events"
  | "contributions"
  | "tools"
  | "services"
  | "logs";

export type PluginMessageKind =
  | "lifecycle.hello"
  | "lifecycle.start"
  | "lifecycle.ready"
  | "lifecycle.health"
  | "lifecycle.stop"
  | "lifecycle.failed"
  | "contribution.register"
  | "contribution.remove"
  | "service.request"
  | "service.response"
  | "service.event"
  | "tools.call"
  | "tools.select"
  | "tools.result"
  | "tools.cancel"
  | "log.event"
  | "host.action"
  | "runtime";

export type PluginErrorCode =
  | "unsupported_protocol"
  | "unsupported_capability"
  | "invalid_envelope"
  | "stale_sequence"
  | "stale_epoch"
  | "stale_generation"
  | "stale_revision"
  | "permission_denied"
  | "invalid_contribution"
  | "invalid_operation"
  | "invalid_input"
  | "invalid_output"
  | "busy"
  | "protected_target"
  | "quota_exceeded"
  | "timeout"
  | "cancelled"
  | "not_active"
  | "not_found"
  | "transport"
  | "internal";

/** A structured failure, carried across the envelope as `{code, message, retryable}`. */
export class PluginError extends Error {
  readonly code: PluginErrorCode;
  readonly retryable: boolean;

  constructor(code: PluginErrorCode, message: string, retryable = false) {
    super(message);
    this.name = "PluginError";
    this.code = code;
    this.retryable = retryable;
  }

  toJSON(): { code: PluginErrorCode; message: string; retryable: boolean } {
    return { code: this.code, message: this.message, retryable: this.retryable };
  }
}

export function asPluginError(error: unknown): PluginError {
  if (error instanceof PluginError) return error;
  if (error instanceof Error) {
    if (error.name === "PluginError") {
      const structured = error as PluginError;
      return new PluginError(structured.code ?? "internal", error.message, Boolean(structured.retryable));
    }
    return new PluginError("internal", error.message);
  }
  if (typeof error === "string") return new PluginError("internal", error);
  try {
    return new PluginError("internal", JSON.stringify(error));
  } catch {
    return new PluginError("internal", String(error));
  }
}

export interface PluginProtocolErrorPayload {
  code: PluginErrorCode;
  message: string;
  retryable?: boolean;
}

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

/** The host's launch parameters (`host.params.plugin`). */
export interface PluginIdentity {
  instance: string;
  epoch: number;
  generation: number;
  protocol: number;
  servicesProtocol: number;
  locale: string;
  resourcePrefix?: string;
  package: { id: string; digest: string };
}

export function readPluginIdentity(params: unknown): PluginIdentity | undefined {
  if (!params || typeof params !== "object") return undefined;
  const plugin = (params as Record<string, unknown>).plugin;
  if (!plugin || typeof plugin !== "object") return undefined;
  const value = plugin as Record<string, unknown>;
  if (
    typeof value.instance !== "string" ||
    value.instance.length === 0 ||
    typeof value.epoch !== "number" ||
    typeof value.generation !== "number" ||
    typeof value.protocol !== "number"
  ) {
    return undefined;
  }
  const pkg = value.package;
  const identity: PluginIdentity = {
    instance: value.instance,
    epoch: value.epoch,
    generation: value.generation,
    protocol: value.protocol,
    servicesProtocol:
      typeof value.servicesProtocol === "number" ? value.servicesProtocol : PLUGIN_SERVICES_PROTOCOL_VERSION,
    locale: typeof value.locale === "string" ? value.locale : "en",
    resourcePrefix: typeof value.resourcePrefix === "string" ? value.resourcePrefix : undefined,
    // The instance is `<id>:<digest>:<scope>`; a plugin id never contains a
    // colon, so the head is a safe fallback for an old host.
    package: {
      id:
        pkg && typeof pkg === "object" && typeof (pkg as Record<string, unknown>).id === "string"
          ? ((pkg as Record<string, unknown>).id as string)
          : value.instance.split(":")[0],
      digest:
        pkg && typeof pkg === "object" && typeof (pkg as Record<string, unknown>).digest === "string"
          ? ((pkg as Record<string, unknown>).digest as string)
          : "",
    },
  };
  return identity;
}

export function validatePluginEnvelope(value: unknown): value is PluginEnvelope {
  if (!value || typeof value !== "object") return false;
  const envelope = value as Partial<PluginEnvelope>;
  return (
    envelope.protocol === PLUGIN_PROTOCOL_VERSION &&
    typeof envelope.kind === "string" &&
    typeof envelope.instance === "string" &&
    envelope.instance.length > 0 &&
    typeof envelope.epoch === "number" &&
    Number.isSafeInteger(envelope.epoch) &&
    typeof envelope.stream === "string" &&
    typeof envelope.sequence === "number" &&
    Number.isSafeInteger(envelope.sequence) &&
    envelope.sequence > 0 &&
    typeof envelope.generation === "number" &&
    Number.isSafeInteger(envelope.generation) &&
    "body" in envelope
  );
}

/** One counter per stream: the host rejects a sequence that does not grow. */
export class PluginSequencer {
  private sequences = new Map<PluginStream, number>();

  next(stream: PluginStream): number {
    const sequence = (this.sequences.get(stream) ?? 0) + 1;
    this.sequences.set(stream, sequence);
    return sequence;
  }

  accept(stream: PluginStream, sequence: number): boolean {
    if (!Number.isSafeInteger(sequence) || sequence <= (this.sequences.get(stream) ?? 0)) {
      return false;
    }
    this.sequences.set(stream, sequence);
    return true;
  }

  reset(): void {
    this.sequences.clear();
  }
}

/** An inbound envelope belongs to this instance only when all three parts match. */
export function isCurrent(envelope: PluginEnvelope, identity: PluginIdentity): boolean {
  return (
    envelope.instance === identity.instance &&
    envelope.epoch === identity.epoch &&
    envelope.generation === identity.generation
  );
}
