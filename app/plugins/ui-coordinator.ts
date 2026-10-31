import type { PluginPackageInfo, PluginUiContribution } from "../protocol";

export type PluginRuntimeStates = Record<string, { state: string; epoch?: number; message?: string }>;

export function pluginInstance(packageInfo: PluginPackageInfo): string {
  return `${packageInfo.id}:${packageInfo.digest}`;
}

function permitted(pkg: PluginPackageInfo, contribution: PluginUiContribution): boolean {
  const page = contribution.kind === "page" || contribution.kind === "panel";
  const capability = page ? "ui.add" : "ui.patch.structure";
  return pkg.capabilities.some((item) =>
    item.name === capability && (item.targets.includes("*") || item.targets.includes(contribution.target))
  );
}

function active(pkg: PluginPackageInfo, runtime: PluginRuntimeStates): boolean {
  const needsEngine = (pkg.contributions.ui ?? []).some((item) => item.export) ||
    ((pkg.contributions.tools as unknown[] | undefined)?.length ?? 0) > 0;
  return !needsEngine || runtime[pluginInstance(pkg)]?.state === "active";
}

export function uiContributions(
  packages: PluginPackageInfo[],
  target: string,
  kinds?: PluginUiContribution["kind"][],
  runtime: PluginRuntimeStates = {},
): Array<{ pkg: PluginPackageInfo; contribution: PluginUiContribution }> {
  return packages
    .filter((pkg) => pkg.enabled && active(pkg, runtime))
    .flatMap((pkg) => (pkg.contributions.ui ?? [])
      .filter((contribution) => contribution.target === target && (!kinds || kinds.includes(contribution.kind)))
      .filter((contribution) => permitted(pkg, contribution))
      .map((contribution) => ({ pkg, contribution })))
    .sort((a, b) => `${a.pkg.id}:${a.contribution.id}`.localeCompare(`${b.pkg.id}:${b.contribution.id}`));
}

export function uiContribution(
  packages: PluginPackageInfo[],
  target: string,
  runtime: PluginRuntimeStates = {},
): { pkg: PluginPackageInfo; contribution: PluginUiContribution } | null {
  return uiContributions(packages, target, ["replaceChildren"], runtime)[0] ?? null;
}
