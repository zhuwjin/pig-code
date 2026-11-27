import type { MessageKey } from "./i18n";
import type { ExecMode, MessageDelivery, PermissionSettings } from "./protocol";

export interface SlashCommand {
  name: "/clear" | "/compact" | "/plan";
  instruction?: string;
}

export const SLASH_COMMANDS: { name: SlashCommand["name"]; descriptionKey: MessageKey }[] = [
  { name: "/clear", descriptionKey: "commands.clear" },
  { name: "/compact", descriptionKey: "commands.compact" },
  { name: "/plan", descriptionKey: "commands.plan" },
];

export function parseSlashCommand(text: string): SlashCommand | null {
  const trimmed = text.trim();
  const match = /^(\/clear|\/compact|\/plan)(?:\s+([\s\S]*))?$/.exec(trimmed);
  if (!match) return null;
  return {
    name: match[1] as SlashCommand["name"],
    instruction: match[2]?.trim() || undefined,
  };
}

export function filterSlashCommands(query: string) {
  const normalized = query.toLowerCase();
  return SLASH_COMMANDS.filter((command) => command.name.slice(1).includes(normalized));
}

export function oppositeDelivery(delivery: MessageDelivery): MessageDelivery {
  return delivery === "queue" ? "steer" : "queue";
}

export function permissionsForMode(mode: ExecMode): PermissionSettings {
  switch (mode) {
    case "read-only":
      return { sandbox: { mode: "read_only" }, approval: "on_request", reviewer: "user" };
    case "auto":
      return { sandbox: { mode: "workspace_write" }, approval: "on_request", reviewer: "user" };
    case "full":
      return { sandbox: { mode: "danger_full_access" }, approval: "never", reviewer: "user" };
  }
}

export function modeForPermissions(permissions: PermissionSettings): ExecMode | null {
  if (permissions.reviewer !== "user") return null;
  if (permissions.sandbox.mode === "read_only" && permissions.approval === "on_request") {
    return "read-only";
  }
  if (permissions.sandbox.mode === "workspace_write" && permissions.approval === "on_request") {
    return "auto";
  }
  if (permissions.sandbox.mode === "danger_full_access" && permissions.approval === "never") {
    return "full";
  }
  return null;
}
