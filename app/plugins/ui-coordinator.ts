import type {
  PluginPackageInfo,
  PluginRuntimeState,
  PluginUiConflict,
  PluginUiContribution,
} from "../protocol";
import { BaseUiRegistry, protectedUiTarget } from "./base-ui-registry";
import { REMOTE_ACCESSIBILITY_KEYS, REMOTE_PROP_KEYS, safeHostStyle } from "./ui-protocol";

const HOST_EVENT_PROPS = new Set(["onPress", "onChange", "onFocus", "onBlur", "onKeyPress"]);

export type PluginRuntimeStates = Record<string, PluginRuntimeState>;

export interface PluginUiEntry {
  pkg: PluginPackageInfo;
  contribution: PluginUiContribution;
  instance: string;
}

export interface PluginUiPlan {
  entries: PluginUiEntry[];
  conflicts: PluginUiConflict[];
  rejected: Array<{ target: string; contribution: string; reason: string }>;
}

export function pluginInstance(packageInfo: PluginPackageInfo): string {
  if (packageInfo.source === "user") return `${packageInfo.id}:${packageInfo.digest}:user:default`;
  if (packageInfo.source === "built_in") return `${packageInfo.id}:${packageInfo.digest}:built-in:global`;
  return `${packageInfo.id}:${packageInfo.digest}:project:${packageInfo.source.project.workspace}`;
}

function permitted(pkg: PluginPackageInfo, contribution: PluginUiContribution): boolean {
  const addition = ["page", "panel", "route", "settings", "command", "overlay", "decoration"].includes(contribution.kind);
  const interaction = Object.keys(contribution.props ?? {}).some((key) => key.startsWith("on") || key === "disabled" || key === "focusable");
  const capability = addition ? "ui.add" : interaction ? "ui.interact" : contribution.kind === "patch" ? "ui.patch.style" : "ui.patch.structure";
  return pkg.capabilities.some((item) =>
    item.name === capability && (item.targets.includes("*") || item.targets.includes(contribution.target))
  );
}

function active(pkg: PluginPackageInfo, runtime: PluginRuntimeStates): boolean {
  const needsEngine = (pkg.contributions.ui ?? []).some((item) => item.export || item.kind === "command") ||
    ((pkg.contributions.tools ?? []).length > 0) ||
    ((pkg.contributions.commands ?? []).length > 0);
  return !needsEngine || runtime[pluginInstance(pkg)]?.state === "active";
}

function sourceRank(pkg: PluginPackageInfo): number {
  return pkg.source === "built_in" ? 0 : pkg.source === "user" ? 1 : 2;
}

function collect(pkg: PluginPackageInfo, dynamic: Record<string, PluginUiContribution[]>): PluginUiContribution[] {
  const instance = pluginInstance(pkg);
  const contributions = new Map<string, PluginUiContribution>();
  for (const contribution of pkg.contributions.ui ?? []) contributions.set(contribution.id, contribution);
  for (const contribution of dynamic[instance] ?? []) contributions.set(contribution.id, contribution);
  return [...contributions.values()];
}

export function uiPlan(
  packages: PluginPackageInfo[],
  target: string,
  kinds: PluginUiContribution["kind"][] | undefined,
  runtime: PluginRuntimeStates = {},
  dynamic: Record<string, PluginUiContribution[]> = {},
  pluginOrder: readonly string[] = [],
): PluginUiPlan {
  const conflicts: PluginUiConflict[] = [];
  const rejected: PluginUiPlan["rejected"] = [];
  const restored = new Set(packages.flatMap((pkg) => collect(pkg, dynamic)
    .filter((contribution) => contribution.kind === "restore")
    .map((contribution) => `${pluginInstance(pkg)}:${contribution.target}`)));
  const candidates = packages
    .filter((pkg) => pkg.enabled && active(pkg, runtime))
    .flatMap((pkg) => collect(pkg, dynamic)
      .filter((contribution) => contribution.kind !== "restore")
      .filter((contribution) => contribution.target === target || (contribution.kind === "move" && contribution.moveTarget === target))
      .filter((contribution) => !restored.has(`${pluginInstance(pkg)}:${contribution.target}`))
      .filter((contribution) => !kinds || kinds.includes(contribution.kind))
      .filter((contribution) => {
        const reject = (reason: string) => {
          rejected.push({ target: contribution.target, contribution: contribution.id, reason });
          return false;
        };
        if (protectedUiTarget(contribution.target)) return reject("protected_target");
        if (!permitted(pkg, contribution)) return reject("permission_denied");
        if (contribution.baseRevision != null && contribution.baseRevision !== BaseUiRegistry.revision()) {
          return reject("stale_revision");
        }
        const addition = ["page", "panel", "route", "settings", "command", "overlay", "decoration"].includes(contribution.kind);
        const descriptor = BaseUiRegistry.get(contribution.target);
        if (!addition && !descriptor) return reject("unregistered_target");
        const interaction = Object.keys(contribution.props ?? {}).some((key) => key.startsWith("on") || key === "disabled" || key === "focusable");
        if (descriptor && !BaseUiRegistry.canPatch(contribution.target, interaction)) return reject("protected_target");
        if (descriptor && contribution.preserve.some((key) => !descriptor.preservedBehavior.includes(key))) {
          return reject("invalid_preserve");
        }
        for (const [key, value] of Object.entries(contribution.props ?? {})) {
          if (key.startsWith("on") && (!HOST_EVENT_PROPS.has(key) || value !== true)) {
            return reject("invalid_event_handler");
          }
          if (!key.startsWith("on") && descriptor && key !== "disabled" && key !== "focusable" &&
            (!REMOTE_PROP_KEYS.has(key) || !descriptor.preservedBehavior.includes(key))) {
            return reject("unsupported_property");
          }
        }
        if (descriptor && Object.keys(contribution.accessibility ?? {}).some((key) =>
          !REMOTE_ACCESSIBILITY_KEYS.has(key) || !descriptor.preservedBehavior.includes(key)
        )) return reject("unsupported_accessibility");
        if (descriptor && Object.keys(safeHostStyle(contribution.style, descriptor.allowedStyles)).length !== Object.keys(contribution.style ?? {}).length) {
          return reject("invalid_style");
        }
        if (contribution.kind === "move" && (!contribution.moveTarget || !BaseUiRegistry.get(contribution.moveTarget))) {
          return reject("invalid_move_target");
        }
        if (BaseUiRegistry.get(target)?.component === "input" && ["replaceChildren", "replace"].includes(contribution.kind)) {
          return reject("invalid_operation");
        }
        return true;
      })
      .map((contribution) => ({ pkg, contribution, instance: pluginInstance(pkg) })))
    .sort((a, b) => {
      const aOrder = pluginOrder.indexOf(a.pkg.id);
      const bOrder = pluginOrder.indexOf(b.pkg.id);
      const aUser = aOrder < 0 ? pluginOrder.length : aOrder;
      const bUser = bOrder < 0 ? pluginOrder.length : bOrder;
      return aUser - bUser || sourceRank(a.pkg) - sourceRank(b.pkg) ||
        (a.contribution.order ?? 0) - (b.contribution.order ?? 0) ||
        a.pkg.id.localeCompare(b.pkg.id) || a.contribution.id.localeCompare(b.contribution.id);
    });
  const structural = new Set(["replaceChildren", "replace", "wrap", "remove", "move"]);
  const winners = new Map<string, PluginUiEntry>();
  for (const entry of candidates) {
    if (structural.has(entry.contribution.kind)) winners.set(entry.contribution.target, entry);
  }
  const suppressed = new Map<string, string[]>();
  const entries = candidates.filter((entry) => {
    if (!structural.has(entry.contribution.kind)) return true;
    const winner = winners.get(entry.contribution.target);
    if (winner === entry) return true;
    if (winner) {
      const values = suppressed.get(winner.contribution.target) ?? [];
      values.push(`${entry.instance}:${entry.contribution.id}`);
      suppressed.set(winner.contribution.target, values);
    }
    return false;
  });
  for (const [target, winner] of winners) {
    const losing = suppressed.get(target) ?? [];
    if (losing.length > 0) {
      conflicts.push({
        target,
        winner: `${winner.instance}:${winner.contribution.id}`,
        suppressed: losing,
        reason: "mutually_exclusive_structure",
      });
    }
  }
  return { entries, conflicts, rejected };
}

export function uiContributions(
  packages: PluginPackageInfo[],
  target: string,
  kinds?: PluginUiContribution["kind"][],
  runtime: PluginRuntimeStates = {},
  dynamic: Record<string, PluginUiContribution[]> = {},
  pluginOrder: readonly string[] = [],
): PluginUiEntry[] {
  return uiPlan(packages, target, kinds, runtime, dynamic, pluginOrder).entries;
}

export function uiContribution(
  packages: PluginPackageInfo[],
  target: string,
  runtime: PluginRuntimeStates = {},
  dynamic: Record<string, PluginUiContribution[]> = {},
): PluginUiEntry | null {
  return uiContributions(packages, target, ["replaceChildren"], runtime, dynamic)[0] ?? null;
}
