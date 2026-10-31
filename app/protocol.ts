export type Payload = Record<string, any>;

export interface SessionMeta {
  id: string;
  title: string;
  cwd: string;
  created_at: number;
  updated_at: number;
  pinned: boolean;
  archived: boolean;
  provider_id?: string | null;
  model_id?: string | null;
  reasoning_level?: string | null;
  permissions?: PermissionSettings;
}

export interface EditDiff {
  path: string;
  unified_diff: string;
  additions: number;
  deletions: number;
}

export interface GitFileChange {
  path: string;
  additions: number;
  deletions: number;
  status: string;
}

export interface GitStatusSnapshot {
  cwd: string;
  is_git: boolean;
  unstaged: GitFileChange[];
  staged: GitFileChange[];
}

export interface GitDiffSnapshot {
  cwd: string;
  path: string;
  staged: boolean;
  diff: string;
  note?: "Truncated" | "TooLarge" | "Binary" | null;
}

export interface UserItem {
  kind: "user";
  id: string;
  text: string;
}

export interface MarkdownItem {
  kind: "markdown" | "reasoning";
  id: string;
  text: string;
  streaming: boolean;
}

export interface ToolItem {
  kind: "tool";
  id: string;
  tool: string;
  summary: string;
  detail: string;
  output: string;
  status: "input" | "running" | "done" | "error" | "stopped";
  edit?: EditDiff | null;
}

export interface NoteItem {
  kind: "note";
  id: string;
  text: string;
  error: boolean;
}

export type ChatItem = UserItem | MarkdownItem | ToolItem | NoteItem;

export type ApprovalDecision = "Allow" | "ApprovedForSession" | "approved_for_project" | "Reject";

export interface PendingApproval {
  requestId: string;
  tool: string;
  detail: string;
  dangerKey?: string | null;
  requestedWriteRoots: string[];
  availableDecisions: ApprovalDecision[];
}

export interface QuestionOption {
  label: string;
  description?: string | null;
}

export interface QuestionItem {
  question: string;
  header?: string | null;
  multi_select: boolean;
  options: QuestionOption[];
}

export interface PendingQuestion {
  requestId: string;
  questions: QuestionItem[];
}

export interface QueueItem {
  id: string;
  text: string;
  images?: unknown[];
}

export type QueuePauseReason = "stopped" | "error" | "restarted";
export type HeldDisposition = "clear_queue_and_send" | "keep_queue_and_send" | "cancel";

export interface QueueState {
  pending: QueueItem[];
  steer: QueueItem[];
  paused: QueuePauseReason | null;
  held: string | null;
}

export type SandboxMode = "read_only" | "workspace_write" | "danger_full_access";
export type ApprovalPolicy = "on_request" | "never";
export type ExecMode = "read-only" | "auto" | "full";
export type MessageDelivery = "queue" | "steer";

export interface PermissionSettings {
  sandbox: { mode: SandboxMode };
  approval: ApprovalPolicy;
  reviewer: "user";
}

export interface SessionConfiguration {
  providerId: string | null;
  modelId: string | null;
  reasoningLevel: string | null;
  permissions: PermissionSettings;
  planEnabled: boolean;
}

export type ApiFormat = "OpenAiChat" | "AnthropicMessages" | "OpenAiResponses";
export type ProviderCredentialStatus = "not_required" | "configured" | "missing" | "unavailable";
export type ConnectionTestResult =
  | { result: "connected"; status: number }
  | { result: "timeout"; secs: number }
  | { result: "failed"; detail: string };

export interface ModelConfig {
  id: string;
  name?: string | null;
  icon?: string | null;
  enabled: boolean;
  context_window: number;
  max_output_tokens: number;
  input_image?: boolean;
  input_pdf?: boolean;
  reasoning_levels?: string[];
  default_reasoning_level?: string | null;
  reasoning_labels?: Record<string, string>;
  [key: string]: unknown;
}

export interface ProviderConfig {
  id: string;
  name: string;
  base_url: string;
  requires_api_key?: boolean;
  api_format: ApiFormat;
  enabled: boolean;
  models: ModelConfig[];
  icon?: string | null;
  key_url?: string | null;
  [key: string]: unknown;
}

export interface PluginUiContribution {
  id: string;
  kind: "replaceChildren" | "insertAfter" | "insertBefore" | "wrap" | "remove" | "patch" | "replace" | "page" | "panel";
  target: string;
  export?: string;
  icon?: string;
  style?: Record<string, any>;
  preserve: string[];
}

export interface PluginCapabilityRequest {
  name: string;
  targets: string[];
}

export interface PluginMarketplaceFile {
  path: string;
  size: number;
  sha256: string;
}

export interface PluginMarketplacePackage {
  id: string;
  name: string;
  version: string;
  directory: string;
  description: Record<string, string>;
  digest: string;
  files: PluginMarketplaceFile[];
}

export interface PluginMarketplaceCatalog {
  api_version: number;
  packages: PluginMarketplacePackage[];
}

export interface PluginPackageInfo {
  id: string;
  name: string;
  version: string;
  digest: string;
  root: string;
  source: "builtIn" | "user" | { project: { workspace: string } };
  enabled: boolean;
  capabilities: PluginCapabilityRequest[];
  contributions: {
    themes?: { id: string; entry: string }[];
    ui?: PluginUiContribution[];
    [key: string]: unknown;
  };
}

export interface RemoteNode {
  id: string;
  kind: string;
  text: string | null;
  exportName: string | null;
  testId: string | null;
  press: boolean;
  children: string[];
  style: Record<string, any>;
}

export interface RemoteDocument {
  instance: string;
  epoch: number;
  sequence: number;
  nodes: RemoteNode[];
  roots: string[];
  warnings: string[];
}

export interface AppConfig {
  default_provider?: string | null;
  default_model?: string | null;
  default_delivery?: MessageDelivery;
  providers: ProviderConfig[];
  ui_font?: string | null;
  mono_font?: string | null;
  theme?: string | null;
  theme_mode?: "system" | "light" | "dark" | null;
  [key: string]: unknown;
}

export function eventKind(event: Payload): string {
  return Object.keys(event)[0] ?? "";
}

export function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  const value = error as Payload;
  if (value && typeof value.detail === "string") return value.detail;
  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
}
