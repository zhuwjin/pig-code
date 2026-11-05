import type {
  PluginPackageInfo,
  PluginPage,
  PluginRuntimeState,
  PluginUiContribution,
  PluginUiSlot,
} from "../protocol";

export type PluginRuntimeStates = Record<string, PluginRuntimeState>;

export interface PluginUiEntry {
  pkg: PluginPackageInfo;
  contribution: PluginUiContribution;
  instance: string;
}

export interface PluginPageEntry {
  pkg: PluginPackageInfo;
  page: PluginPage;
  instance: string;
}

export function pluginInstance(pkg: PluginPackageInfo): string {
  if (pkg.source === "user") return `${pkg.id}:${pkg.digest}:user:default`;
  if (pkg.source === "built_in") return `${pkg.id}:${pkg.digest}:built-in:global`;
  return `${pkg.id}:${pkg.digest}:project:${pkg.source.project.workspace}`;
}

function active(pkg: PluginPackageInfo, runtime: PluginRuntimeStates): boolean {
  return pkg.enabled && runtime[pluginInstance(pkg)]?.state === "active";
}

function compare(order: readonly string[], left: PluginPackageInfo, right: PluginPackageInfo): number {
  const leftOrder = order.indexOf(left.id);
  const rightOrder = order.indexOf(right.id);
  return (leftOrder < 0 ? order.length : leftOrder) - (rightOrder < 0 ? order.length : rightOrder) ||
    left.id.localeCompare(right.id);
}

export function uiContributions(
  packages: PluginPackageInfo[],
  slot: PluginUiSlot,
  runtime: PluginRuntimeStates = {},
  pluginOrder: readonly string[] = [],
): PluginUiEntry[] {
  return packages
    .filter((pkg) => active(pkg, runtime))
    .flatMap((pkg) => (pkg.contributions.ui ?? [])
      .filter((contribution) => contribution.slot === slot)
      .map((contribution) => ({ pkg, contribution, instance: pluginInstance(pkg) })))
    .sort((left, right) => compare(pluginOrder, left.pkg, right.pkg) ||
      left.contribution.order - right.contribution.order ||
      left.contribution.id.localeCompare(right.contribution.id));
}

export function pluginPages(
  packages: PluginPackageInfo[],
  kind: PluginPage["kind"],
  runtime: PluginRuntimeStates = {},
  pluginOrder: readonly string[] = [],
): PluginPageEntry[] {
  return packages
    .filter((pkg) => active(pkg, runtime))
    .flatMap((pkg) => (pkg.contributions.pages ?? [])
      .filter((page) => page.enabled && page.kind === kind)
      .map((page) => ({ pkg, page, instance: pluginInstance(pkg) })))
    .sort((left, right) => compare(pluginOrder, left.pkg, right.pkg) ||
      left.page.order - right.page.order || left.page.id.localeCompare(right.page.id));
}
