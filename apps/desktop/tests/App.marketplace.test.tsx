import { expect, open, test } from "nsh/test";
import {
  localizedMarketplaceDescription,
  marketplaceAction,
} from "../app/components/settings/marketplace";
import type { PluginMarketplacePackage, PluginPackageInfo } from "../app/protocol";

const catalogPackage: PluginMarketplacePackage = {
  id: "com.example.moon",
  name: "Moon",
  version: "1.2.0",
  directory: "packages/com.example.moon",
  description: { en: "English description", "zh-CN": "中文描述" },
  digest: "sha256:aaaa",
  files: [],
};

function installed(digest: string, enabled = true): PluginPackageInfo {
  return {
    id: catalogPackage.id,
    name: catalogPackage.name,
    version: catalogPackage.version,
    digest,
    root: "/plugins/moon",
    source: "user",
    enabled,
    capabilities: [],
    contributions: {},
  };
}

test("distinguishes installed, updating and installing marketplace packages", () => {
  expect(marketplaceAction(installed("sha256:aaaa"), catalogPackage.digest, false)).toBe("installed");
  expect(marketplaceAction(installed("sha256:bbbb"), catalogPackage.digest, false)).toBe("update");
  expect(marketplaceAction(installed("sha256:bbbb"), catalogPackage.digest, true)).toBe("installing");
  expect(marketplaceAction(undefined, catalogPackage.digest, false)).toBe("install");
});

test("uses the localized marketplace description with English and fallbacks", () => {
  expect(localizedMarketplaceDescription(catalogPackage, "zh-CN")).toBe("中文描述");
  expect(localizedMarketplaceDescription(catalogPackage, "en")).toBe("English description");
  expect(localizedMarketplaceDescription({ ...catalogPackage, description: {} }, "zh-CN")).toBe(catalogPackage.id);
});

test("loads the built-in official marketplace without network in selftest", async () => {
  const app = await open("./App.selftest.tsx", {
    network: "live",
    platform: "macos",
    locale: "zh-CN",
    width: 1280,
    height: 800,
  });
  await app.press("id:title-theme-settings");
  await app.press("id:settings-marketplace");
  await app.waitFor("id:plugin-marketplace-page");
  await app.waitFor("id:marketplace-no-packages", { timeout: 10000 });
  expect(app.count("id:marketplace-source-official")).toBe(1);
  expect(app.count("id:marketplace-remove-official")).toBe(0);
  expect(app.texts()).toContain("pig-code-plugins");
  expect(app.texts()).toContain("sha256:488ba14b57449a35989a3a1d27a8fdf7d9596d6e9c427b46c9669639104df9c3");
  expect(app.count("id:marketplace-url-input")).toBe(1);
  expect(app.count("id:plugin-package-path")).toBe(0);

  await app.press("id:settings-plugins");
  await app.waitFor("id:plugin-manager-page");
  expect(app.count("id:plugin-package-path")).toBe(1);
  expect(app.count("id:plugin-root-path")).toBe(1);
  expect(app.count("id:market-refresh")).toBe(0);
});
