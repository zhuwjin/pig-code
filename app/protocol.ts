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
}

export interface EditDiff {
  path: string;
  unified_diff: string;
  additions: number;
  deletions: number;
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

export interface PendingApproval {
  requestId: string;
  tool: string;
  detail: string;
}

export interface ProviderConfig {
  id: string;
  name: string;
  enabled: boolean;
  models: { id: string; name?: string | null; enabled: boolean }[];
}

export interface AppConfig {
  default_provider?: string | null;
  default_model?: string | null;
  providers: ProviderConfig[];
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
