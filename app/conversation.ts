import { t } from "./i18n";
import type { ChatItem, EditDiff, MarkdownItem, TurnState, TurnUsageStats } from "./protocol";
import type { IconName } from "./components/icon";

export type ConversationRow =
  | { id: string; type: "item"; item: ChatItem }
  | { id: string; type: "work"; turn: TurnState; items: ChatItem[] }
  | { id: string; type: "changes"; turn: TurnState }
  | { id: string; type: "actions"; turn: TurnState; markdown: MarkdownItem[] };

export function buildConversationRows(
  items: ChatItem[],
  turns: TurnState[],
): ConversationRow[] {
  const rows: ConversationRow[] = [];
  let index = 0;
  while (index < items.length) {
    const item = items[index];
    const turnId = item.kind === "markdown" || item.kind === "reasoning" || item.kind === "tool"
      ? item.turnId
      : undefined;
    if (!turnId) {
      rows.push({ id: `item-${item.id}`, type: "item", item });
      index += 1;
      continue;
    }
    const group: ChatItem[] = [];
    while (index < items.length) {
      const next = items[index];
      const nextTurn = next.kind === "markdown" || next.kind === "reasoning" || next.kind === "tool"
        ? next.turnId
        : undefined;
      if (nextTurn !== turnId) break;
      group.push(next);
      index += 1;
    }
    const turn = turns.find((candidate) => candidate.id === turnId);
    const ended = turn?.status === "done" || turn?.status === "stopped";
    const hasWork = group.some((entry) => entry.kind === "reasoning" || entry.kind === "tool");
    const lastBody = group.map((entry) => entry.kind).lastIndexOf("markdown");
    const workItems = turn && ended && lastBody >= 0
      ? group.filter((entry, entryIndex) => {
        const foldable = entry.kind === "reasoning" || entry.kind === "tool" || entry.kind === "markdown";
        const isPlan = entry.kind === "tool" && entry.tool === "ExitPlanMode";
        return entryIndex < lastBody && foldable && !isPlan;
      })
      : [];
    if (turn && ended && hasWork) rows.push({ id: `work-${turnId}`, type: "work", turn, items: workItems });
    group.forEach((entry) => {
      if (!workItems.includes(entry)) rows.push({ id: `item-${entry.id}`, type: "item", item: entry });
    });
    if (turn?.changes.length) rows.push({ id: `changes-${turnId}`, type: "changes", turn });
    if (turn && ended) {
      rows.push({
        id: `actions-${turnId}`,
        type: "actions",
        turn,
        markdown: group.filter((entry): entry is MarkdownItem => entry.kind === "markdown"),
      });
    }
  }
  return rows;
}

function sameConversationItem(previous: ChatItem, next: ChatItem): boolean {
  if (previous.kind !== next.kind || previous.id !== next.id) return false;
  if (previous.kind === "markdown" || previous.kind === "reasoning") {
    return next.kind === previous.kind &&
      previous.turnId === next.turnId &&
      previous.streaming === next.streaming &&
      previous.sources === next.sources;
  }
  return previous === next;
}

function sameConversationRow(previous: ConversationRow, next: ConversationRow): boolean {
  if (previous.id !== next.id || previous.type !== next.type) return false;
  if (previous.type === "item" && next.type === "item") return sameConversationItem(previous.item, next.item);
  if (previous.type === "work" && next.type === "work") {
    return previous.turn === next.turn && previous.items.length === next.items.length &&
      previous.items.every((item, itemIndex) => sameConversationItem(item, next.items[itemIndex]));
  }
  if (previous.type === "changes" && next.type === "changes") return previous.turn === next.turn;
  if (previous.type === "actions" && next.type === "actions") {
    return previous.turn === next.turn && previous.markdown.length === next.markdown.length &&
      previous.markdown.every((item, itemIndex) => sameConversationItem(item, next.markdown[itemIndex]));
  }
  return false;
}

export function conversationRowsEqual(previous: ConversationRow[], next: ConversationRow[]): boolean {
  return previous.length === next.length && previous.every((row, index) => sameConversationRow(row, next[index]));
}

export function reuseConversationRows(previous: ConversationRow[], next: ConversationRow[]): ConversationRow[] {
  const cached = new Map(previous.map((row) => [row.id, row]));
  return next.map((row) => {
    const old = cached.get(row.id);
    return old && sameConversationRow(old, row) ? old : row;
  });
}

export function formatDuration(ms: number, prefix = ""): string {
  const seconds = Math.max(1, Math.round(ms / 1000));
  const value = seconds >= 60
    ? t("conversation.duration.minutes_seconds", { minutes: Math.floor(seconds / 60), seconds: seconds % 60 })
    : t("conversation.duration.seconds", { seconds });
  return `${prefix}${value}`;
}

export function formatCompactDuration(ms: number): string {
  const seconds = Math.max(0, ms) / 1000;
  return `${seconds.toFixed(1)}s`;
}

export function turnSpeed(stats: TurnUsageStats): number | null {
  const decodeMs = stats.api_ms - stats.ttft_ms;
  return stats.ttft_ms > 0 && decodeMs > 0 && stats.output > 0
    ? stats.output / (decodeMs / 1000)
    : null;
}

export function formatTokens(value: number): string {
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(1).replace(/\.0$/, "")}M`;
  if (value >= 1000) return `${(value / 1000).toFixed(1).replace(/\.0$/, "")}K`;
  return String(Math.round(value));
}

export function formatClock(ms: number, nowMs = Date.now()): string {
  if (!ms) return "";
  const date = new Date(ms);
  const now = new Date(nowMs);
  const pad = (value: number) => String(value).padStart(2, "0");
  const time = `${pad(date.getHours())}:${pad(date.getMinutes())}`;
  return date.toDateString() === now.toDateString()
    ? time
    : `${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${time}`;
}

export function isExecTool(tool: string): boolean {
  return tool === "ExecCommand" || tool === "Bash" || tool === "Shell";
}

export function toolIcon(tool: string): IconName {
  if (isExecTool(tool)) return "square-terminal";
  switch (tool) {
    case "Read": return "eye";
    case "Write": return "file-plus";
    case "Edit": return "file-pen";
    case "Glob": return "folder-search";
    case "Grep": return "text-search";
    case "TodoList": return "list-todo";
    case "FetchURL": return "globe";
    case "AskUserQuestion": return "message-circle-question-mark";
    case "Agent":
    case "AgentSwarm": return "bot";
    default: return "settings";
  }
}

export function toolLabel(tool: string, running: boolean): string {
  if (isExecTool(tool)) return running ? t("tools.running") : t("tools.terminal");
  switch (tool) {
    case "Read": return t("tools.read");
    case "Write": return running ? t("tools.writing") : t("tools.write");
    case "Edit": return running ? t("tools.editing") : t("tools.edit");
    case "Glob": return t("tools.glob");
    case "Grep": return t("tools.grep");
    case "TodoList": return t("tools.todo");
    case "FetchURL": return t("tools.fetch_url");
    case "TaskList":
    case "TaskOutput":
    case "TaskStop": return t("tools.background_task");
    case "AskUserQuestion": return t("tools.ask_user");
    case "ExitPlanMode": return t("tools.exit_plan");
    default: return tool;
  }
}

export function readLineCount(output: string): number {
  return output.split("\n").filter((line) => /^\d+\t/.test(line)).length;
}

export function readFirstLine(output: string): number | undefined {
  const match = /^(\d+)\t/.exec(output.split("\n")[0] ?? "");
  return match ? Number(match[1]) : undefined;
}

export type ReadCode = { lines: { number: number; text: string }[]; notes: string[] };

export function parseReadOutput(output: string): ReadCode | null {
  const lines: ReadCode["lines"] = [];
  const notes: string[] = [];
  for (const line of output.split("\n")) {
    const match = /^(\d+)\t([\s\S]*)$/.exec(line);
    if (match) lines.push({ number: Number(match[1]), text: match[2] });
    else if (line.trim()) notes.push(line);
  }
  return lines.length ? { lines, notes } : null;
}

export type SearchResult = { path: string; line?: number; content?: string };

export function parseSearchOutput(tool: string, output: string): { rows: SearchResult[]; notes: string[] } | null {
  const rows: SearchResult[] = [];
  const notes: string[] = [];
  const source = tool === "Glob" ? output.split("\n\n")[0] : output;
  for (const line of source.split("\n")) {
    if (!line.trim() || line === "--") continue;
    if ((line.startsWith("[") && line.endsWith("]")) || line.startsWith("(")) {
      notes.push(line);
      continue;
    }
    if (tool === "Grep") {
      const match = /^(.*?):(\d+): ([\s\S]*)$/.exec(line);
      if (match) {
        rows.push({ path: match[1], line: Number(match[2]), content: match[3] });
        continue;
      }
      const count = /^(.*):(\d+)$/.exec(line);
      if (count) {
        rows.push({ path: count[1], content: count[2] });
        continue;
      }
    }
    rows.push({ path: line });
  }
  return rows.length ? { rows, notes } : null;
}

export function pathParts(path: string): { directory: string; name: string } {
  const normalized = path.replace(/[\\/]+$/, "");
  const index = Math.max(normalized.lastIndexOf("/"), normalized.lastIndexOf("\\"));
  return index < 0
    ? { directory: "", name: normalized }
    : { directory: normalized.slice(0, index + 1), name: normalized.slice(index + 1) };
}

export function changeTotals(changes: EditDiff[]): { additions: number; deletions: number } {
  return changes.reduce((totals, change) => ({
    additions: totals.additions + change.additions,
    deletions: totals.deletions + change.deletions,
  }), { additions: 0, deletions: 0 });
}

export function userMessageIsLong(text: string): boolean {
  return text.split("\n").length > 5 || text.length > 320;
}

export type MentionSegment = { type: "text" | "file" | "image"; text: string; index?: number };

export function mentionSegments(text: string, files: string[], imageCount: number): MentionSegment[] {
  const pattern = /(@[^\s]+|Image \d+)/g;
  const segments: MentionSegment[] = [];
  let last = 0;
  for (const match of text.matchAll(pattern)) {
    const index = match.index ?? 0;
    if (index > last) segments.push({ type: "text", text: text.slice(last, index) });
    const value = match[0];
    if (value.startsWith("@") && files.includes(value.slice(1))) {
      segments.push({ type: "file", text: value.slice(1) });
    } else {
      const image = /^Image (\d+)$/.exec(value);
      const number = image ? Number(image[1]) : 0;
      if (number > 0 && number <= imageCount) segments.push({ type: "image", text: value, index: number - 1 });
      else segments.push({ type: "text", text: value });
    }
    last = index + value.length;
  }
  if (last < text.length) segments.push({ type: "text", text: text.slice(last) });
  return segments.length ? segments : [{ type: "text", text }];
}

export function lastNonEmptyLine(text: string): string {
  return text.split("\n").filter((line) => line.trim()).pop()?.trim() ?? "";
}

export function partialJsonField(text: string, key: string): string {
  const marker = `"${key}"`;
  const keyIndex = text.indexOf(marker);
  if (keyIndex < 0) return "";
  const colon = text.indexOf(":", keyIndex + marker.length);
  if (colon < 0) return "";
  let index = colon + 1;
  while (text[index] === " " || text[index] === "\n" || text[index] === "\r" || text[index] === "\t") index += 1;
  if (text[index] !== '"') return "";
  index += 1;
  let value = "";
  while (index < text.length) {
    const char = text[index];
    if (char === '"') return value;
    if (char !== "\\") {
      value += char;
      index += 1;
      continue;
    }
    index += 1;
    if (index >= text.length) break;
    const escaped = text[index];
    const simple: Record<string, string> = { n: "\n", r: "\r", t: "\t", b: "\b", f: "\f" };
    if (simple[escaped]) {
      value += simple[escaped];
      index += 1;
      continue;
    }
    if (escaped === "u") {
      const hex = text.slice(index + 1, index + 5);
      if (!/^[0-9a-fA-F]{4}$/.test(hex)) break;
      value += String.fromCharCode(Number.parseInt(hex, 16));
      index += 5;
      continue;
    }
    value += escaped;
    index += 1;
  }
  return value;
}

export type StreamingDiffRow = { kind: "added" | "deleted"; text: string };

export function streamingDiffRows(tool: string, detail: string): StreamingDiffRow[] {
  const lines = (value: string, kind: StreamingDiffRow["kind"]) => value
    ? value.split("\n").map((text) => ({ kind, text }))
    : [];
  if (tool === "Write") {
    return lines(partialJsonField(detail, "content"), "added");
  }
  if (tool === "Edit") {
    return [
      ...lines(partialJsonField(detail, "old_string"), "deleted"),
      ...lines(partialJsonField(detail, "new_string"), "added"),
    ];
  }
  return [];
}
