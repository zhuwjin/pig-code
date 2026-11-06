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

export interface WorkspaceMeta {
  path: string;
  added_at: number;
  alias?: string | null;
  hidden: boolean;
}

export interface SessionContentHit {
  session_id: string;
  before: string;
  hit: string;
  after: string;
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

export interface PendingImage {
  bytes: number[];
  mime: string;
}

export interface ComposerImage extends PendingImage {
  id: string;
  preview: string;
}

export interface Source {
  url: string;
  title: string;
  snippet?: string | null;
  start_index?: number | null;
  end_index?: number | null;
}

export interface TurnUsageStats {
  input: number;
  cache_read: number;
  output: number;
  duration_ms: number;
  api_ms: number;
  ttft_ms: number;
  ttft_steps: number;
  api_steps: number;
  tool_ms: number;
  ended_at_ms: number;
}

export interface HookRun {
  id: string;
  hook_event: string;
  label: string;
  source?: unknown;
  duration_ms: number;
  state: "running" | "finished" | "failed" | "blocked";
  reason?: string | null;
}

export interface AgentCard {
  agent_id: string;
  profile: string;
  description: string;
  model: string;
  background: boolean;
  finished: boolean;
  live_note?: string | null;
}

export interface UserItem {
  kind: "user";
  id: string;
  text: string;
  files: string[];
  imageNums: number[];
  sentAtMs: number;
}

export interface MarkdownItem {
  kind: "markdown" | "reasoning";
  id: string;
  text: string;
  streaming: boolean;
  turnId?: string;
  sources?: Source[];
}

export interface ToolItem {
  kind: "tool";
  id: string;
  tool: string;
  summary: string;
  detail: string;
  output: string;
  status: "input" | "running" | "done" | "error" | "stopped";
  turnId?: string;
  edit?: EditDiff | null;
  liveNote?: string | null;
  agents?: AgentCard[];
  approvalPending?: boolean;
}

export type RightPanelRequest =
  | { kind: "changes"; key: string }
  | { kind: "file"; key: string; path: string; cwd: string; line?: number }
  | { kind: "subagent"; key: string; sessionId: string; agentId: string }
  | { kind: "task"; key: string; task: TaskSummary }
  | { kind: "compact"; key: string; text: string };

export interface NoteItem {
  kind: "note";
  id: string;
  text: string;
  error: boolean;
  noteKind?: "plain" | "compacting" | "compacted";
  usedBefore?: number | null;
  usedAfter?: number | null;
  summary?: string | null;
}

export interface TaskNotification {
  id: string;
  task_id: string;
  kind: "shell" | "subagent";
  status: TaskStatus;
  exit_code?: number | null;
  severity: "info" | "warning";
  description: string;
  title: string;
  body: string;
  duration_ms?: number | null;
  turns?: number | null;
  model?: string | null;
  agent_id?: string | null;
  record_path?: string | null;
  output?: { type: "file"; path: string; bytes: number } | {
    type: "preview";
    text: string;
    total_bytes: number;
    truncated: boolean;
  } | null;
}

export interface NotificationItem {
  kind: "notification";
  id: string;
  notifications: TaskNotification[];
  sentAtMs: number;
}

export type ChatItem = UserItem | MarkdownItem | ToolItem | NoteItem | NotificationItem;

export interface TurnState {
  id: string;
  status: "running" | "done" | "stopped";
  startedAt: number;
  durationMs?: number;
  stats?: TurnUsageStats | null;
  finishReason?: "stop" | "tool_calls" | "length" | "content_filter" | "pause" | null;
  changes: EditDiff[];
  hooks: HookRun[];
}

export type TodoStatus = "Pending" | "InProgress" | "Done";
export interface TodoItem {
  content: string;
  status: TodoStatus;
}

export type TaskStatus = "queued" | "running" | "completed" | "failed" | "timed_out" | "killed" | "lost";
export interface TaskSummary {
  id: string;
  kind: "shell" | "subagent";
  command: string;
  status: TaskStatus;
  exit_code?: number | null;
  reason?: string | null;
  started_at_ms: number;
  ended_at_ms?: number | null;
  output_tail: string;
  agent_id?: string | null;
  spill_path?: string | null;
}

export interface ContextBreakdown {
  system: number;
  tools: number;
  messages: number;
}

export interface ContextUsage {
  used: number;
  total: number;
  cacheRead: number;
  input: number;
}

export interface PluginContextUsage {
  used: number;
  total: number;
  cacheReadTotal: number;
  inputTotal: number;
}

export interface PluginSessionStats {
  running: boolean;
  outputRate: number;
  turns: number;
  steps: number;
  output: number;
  decodeMs: number;
  apiMs: number;
  toolMs: number;
  ttftMs: number;
  ttftSteps: number;
  usage?: PluginContextUsage | null;
  breakdown?: ContextBreakdown | null;
}

export interface PluginTurnStats {
  id?: string | null;
  index: number;
  durationMs: number;
  stats?: TurnUsageStats | null;
}

export interface RetryStatus {
  attempt: number;
  max_attempts: number;
  delay_ms: number;
  reason: Payload;
}

export interface ProjectHooksNotice {
  path: string;
  count: number;
}

export interface SubagentItem {
  role: string;
  text: string;
  tool?: string | null;
  output?: string | null;
  is_error: boolean;
  stopped: boolean;
}

export interface SubagentHistory {
  sessionId: string;
  agentId: string;
  title: string;
  subtitle: string;
  items: SubagentItem[];
  running: boolean;
}

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
  permissions?: PermissionSettings;
  images?: PendingImage[];
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

export interface ModelRegistryInfo {
  full_id: string;
  context?: number | null;
  input?: number | null;
  output?: number | null;
  reasoning: boolean;
  reasoning_levels: string[];
  input_modalities: string[];
  structured_output?: boolean | null;
  strict_tools?: boolean | null;
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

export interface PluginMarketplaceSource {
  id: string;
  name: string;
  index_url: string;
  official: boolean;
  fingerprint: string;
}

export interface PluginMarketplacePreview extends PluginMarketplaceSource {
  public_key: string;
}

export interface PluginConfigItem {
  key: string;
  description?: Record<string, string>;
  default: unknown;
  schema?: Record<string, unknown> | null;
}

export interface PluginCredentialDeclaration {
  kind: "owned" | "provider";
  id: string;
  label?: string;
  description?: string | null;
  provider_id?: string;
}

export interface PluginModelContribution {
  id: string;
  name?: string | null;
  context_window: number;
  max_output_tokens: number;
  input_image?: boolean;
  input_pdf?: boolean;
  cap_structured?: boolean;
  cap_strict_tools?: boolean;
  cap_web_search?: boolean;
  reasoning_levels?: string[];
  default_reasoning_level?: string | null;
  reasoning_labels?: Record<string, string>;
  reasoning_params?: Record<string, unknown>;
  parallel_tool_calls?: boolean;
}

/** A model provider a package declares. The host runs it through one of its
 * own protocol adapters; the plugin supplies declarations, not request code. */
export interface PluginProviderContribution {
  id: string;
  name: string;
  icon?: string | null;
  base_url: string;
  api_format: ApiFormat;
  credential?: string | null;
  models: PluginModelContribution[];
}

/** One package's runtime-registered contributions, keyed by
 * `"<package>:<digest>"` in the snapshot. */
export interface PluginRuntimeContributions {
  [packageDigest: string]: {
    themes?: { id: string; entry: string }[];
    commands?: PluginCommand[];
    providers?: PluginProviderContribution[];
  };
}

export interface PluginContributionConflict {
  kind: string;
  name: string;
  winner: string;
  losers: string[];
}

export interface PluginPackageInfo {
  id: string;
  name: string;
  version: string;
  digest: string;
  root: string;
  source: "built_in" | "user" | { project: { workspace: string } };
  enabled: boolean;
  /** The manifest needs a newer host: files and the enable choice are kept,
   * the backend and contributions are not activated. */
  incompatible?: string | null;
  backend?: string | null;
  capabilities: PluginCapabilityRequest[];
  contributions: {
    themes?: { id: string; entry: string }[];
    tools?: unknown[];
    commands?: PluginCommand[];
    providers?: PluginProviderContribution[];
    credentials?: PluginCredentialDeclaration[];
    config?: { items?: PluginConfigItem[] };
    [key: string]: unknown;
  };
}

export interface PluginCommand {
  id: string;
  title: Record<string, string>;
  export?: string | null;
  order: number;
  enabled: boolean;
}

export type PluginMessageKind = "lifecycle.hello" | "lifecycle.start" | "lifecycle.ready" | "lifecycle.health" | "lifecycle.stop" | "lifecycle.failed" | "contribution.register" | "contribution.remove" | "service.request" | "service.response" | "service.event" | "tools.call" | "tools.select" | "tools.result" | "log.event" | "host.action" | "runtime";
export type PluginStream = "lifecycle" | "events" | "contributions" | "tools" | "services" | "logs";
export type PluginService = "storage_read" | "storage_write" | "config_read" | "config_write" | "http_request" | "credential_set" | "credential_clear" | "session_read" | "workspace_read" | "navigation_open" | "composer_submit" | "resource_read" | "locale_read";
export type PluginLifecycleState = "discovered" | "verified" | "installed" | "enabled" | "preparing" | "starting" | "retrying" | "active" | "stopping" | "stopped" | "failed";

export interface PluginProtocolError {
  code: string;
  message: string;
  retryable: boolean;
}

export interface PluginRuntimeMetrics {
  pendingRequests: number;
  timedOutRequests: number;
  cancelledRequests: number;
  queueDepth: number;
  droppedEvents: number;
  networkBytes: number;
  storageBytes: number;
}

export interface PluginRuntimeState {
  state: PluginLifecycleState;
  epoch: number;
  generation: number;
  attempt?: number;
  retryAfterMs?: number | null;
  message?: string | null;
  metrics?: PluginRuntimeMetrics;
}

export interface AppConfig {
  default_provider?: string | null;
  default_model?: string | null;
  default_delivery?: MessageDelivery;
  providers: ProviderConfig[];
  ui_font?: string | null;
  mono_font?: string | null;
  language?: "system" | "zh-CN" | "en" | null;
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
