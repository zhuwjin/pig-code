import { token } from "nsh";
import type { CanvasRenderingContext2D } from "nsh";
import { expect, open, test } from "nsh/test";
import { iconTint, paintIcon } from "./app/components/icon";
import { ICON_ELEMENTS } from "./app/components/icon-elements";
import { createParticles, scatterParticles, stepParticles } from "./app/hero-particles";
import en from "./app/locales/en.json";
import zhCN from "./app/locales/zh-CN.json";
import { ThemeStore } from "./app/theme";

function catalogKeys(value: Record<string, unknown>, prefix = ""): string[] {
  return Object.entries(value).flatMap(([key, child]) => {
    const path = prefix ? `${prefix}.${key}` : key;
    return typeof child === "object" && child !== null
      ? catalogKeys(child as Record<string, unknown>, path)
      : [path];
  }).sort();
}

function openApp() {
  return open("./App.selftest.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 1280,
    height: 800,
  });
}

async function realPlatform(app: Awaited<ReturnType<typeof openApp>>): Promise<boolean> {
  return (await app.screenshot().catch(() => null)) !== null;
}

async function installMoon(app: Awaited<ReturnType<typeof openApp>>) {
  await app.press("id:title-theme-settings");
  await app.waitFor("id:settings-plugins");
  await app.press("id:settings-plugins");
  await app.waitFor("id:plugin-package-path");
  await app.type("id:plugin-package-path", "target/test-tmp/moon-plugin/moon.nshpkg");
  await app.type("id:plugin-root-path", "target/test-tmp/moon-plugin/root.pub");
  expect(app.value("id:plugin-package-path")).toBe("target/test-tmp/moon-plugin/moon.nshpkg");
  expect(app.value("id:plugin-root-path")).toBe("target/test-tmp/moon-plugin/root.pub");
  expect(app.enabled("id:plugin-install")).toBe(true);
  await app.press("id:plugin-install");
  await app.waitFor("text:/com.pigcodets.plugins.moon/", { timeout: 10000 });
  await app.press("id:theme-package-com.pigcodets.plugins.moon");
  await app.press("id:settings-back");
}

test("uses theme foreground for controls and preserves the full-color brand", () => {
  expect(iconTint("settings")).toBe(token("foreground"));
  expect(iconTint("folder-open", token("muted-foreground"))).toBe(token("muted-foreground"));
  expect(iconTint("pig") === undefined).toBe(true);
});

test("draws original SVG geometry at the requested size and tint", () => {
  let scale = 0;
  let strokes = 0;
  const context = {
    scale(x: number, _y: number) { scale = x; },
    beginPath() {},
    arc() {},
    stroke() { strokes += 1; },
  } as unknown as CanvasRenderingContext2D;
  paintIcon(context, ICON_ELEMENTS.sun, "#f5f5f5ff", 14);
  expect(scale).toBe(14 / 24);
  expect(context.strokeStyle).toBe("#f5f5f5ff");
  expect(context.lineWidth).toBe(2);
  expect(context.lineCap).toBe("round");
  expect(context.lineJoin).toBe("round");
  expect(strokes).toBe(ICON_ELEMENTS.sun.length);
});

test("draws filled dots from the generated catalog without losing their strokes", () => {
  expect(Object.keys(ICON_ELEMENTS).length).toBe(1832);
  let fills = 0;
  let strokes = 0;
  const context = {
    scale() {},
    beginPath() {},
    arc() {},
    fill() { fills += 1; },
    stroke() { strokes += 1; },
  } as unknown as CanvasRenderingContext2D;
  const elements = ICON_ELEMENTS["chart-scatter"];
  paintIcon(context, elements, "#f5f5f5ff", 12);
  expect(elements.filter((element) => element.filled).length).toBe(5);
  expect(fills).toBe(5);
  expect(strokes).toBe(elements.length);
  expect(context.fillStyle).toBe("#f5f5f5ff");
});

test("keeps the macOS traffic-light zone clear and places the interactive wordmark", async () => {
  const app = await openApp();
  expect(app.count("id:sidebar-brand")).toBe(0);
  expect(app.count("id:brand-mark")).toBe(0);
  expect(app.count("id:hero-pig-code-wordmark")).toBe(1);
  expect(app.count("id:sidebar-toggle")).toBe(1);
  await app.hover("id:hero-pig-code-wordmark");
  await app.press("id:hero-pig-code-wordmark");
  await app.wait(50);
});

test("samples the original glyph and returns scattered particles home", () => {
  const particles = createParticles();
  expect(particles.length).toBeGreaterThan(713);
  expect(particles.length <= 1800).toBe(true);
  expect(particles.every((particle) =>
    particle.home.x >= 0 && particle.home.x <= 420 &&
    particle.home.y >= 0 && particle.home.y <= 120 &&
    particle.radius >= 1.05 && particle.radius <= 1.75
  )).toBe(true);

  const home = { ...particles[0].home };
  for (let frame = 0; frame < 10; frame += 1) {
    stepParticles(particles, home, 1 / 60, false, false);
  }
  expect(Math.hypot(
    particles[0].position.x - home.x,
    particles[0].position.y - home.y,
  )).toBeGreaterThan(0);

  scatterParticles(particles, { value: 0x123456789abcdefn });
  expect(particles.every((particle) => Math.hypot(
    particle.velocity.x,
    particle.velocity.y,
  ) > 200)).toBe(true);
  for (let frame = 0; frame < 540; frame += 1) {
    stepParticles(particles, null, 1 / 60, true, false);
  }
  expect(Math.hypot(
    particles[0].position.x - home.x,
    particles[0].position.y - home.y,
  )).toBeLessThan(0.05);
});

test("creates, streams and replays a session", async () => {
  const app = await openApp();
  expect(app.texts()).toContain("Pig Code v0.1.0");
  await app.type("id:composer-input", "read the mock file");
  await app.press("id:send-button");
  await app.waitFor("Conclusion: MOCK_REPLY_OK", { timeout: 20000 });
  await app.waitFor("text:/工作了/");
  await app.press("text:/工作了/");
  await app.waitFor("读取");
  expect(app.value("id:composer-input")).toBe("");

  await app.press("id:new-task");
  await app.waitFor("auto-title selftest", { timeout: 10000 });
  await app.press("auto-title selftest");
  await app.waitFor("Conclusion: MOCK_REPLY_OK", { timeout: 20000 });
  expect(app.count("Conclusion: MOCK_REPLY_OK")).toBe(1);
});

test("restores the hero layout and fills a suggestion", async () => {
  const app = await openApp();
  expect(app.count("id:brand-mark")).toBe(0);
  expect(app.texts()).toContain("Pig Code v0.1.0");
  expect(app.texts()).toContain("总结这个工作区");
  expect(app.texts()).toContain("选择工作区");
  expect(app.count("text:按 Enter 加入队列 · 按 Ctrl+Enter 发送引导消息")).toBe(1);
  expect(app.texts()).not.toContain("暂无历史会话");
  expect(app.count("id:hero-cwd")).toBe(1);
  expect(app.texts()).not.toContain("Rust agent");

  await app.press("id:hero-cwd");
  await app.press("id:hero-cwd-option-0");
  expect(app.texts()).not.toContain("选择工作区");
  await app.press("id:hero-cwd");
  await app.press("id:hero-cwd-clear");
  expect(app.texts()).toContain("选择工作区");

  await app.press("id:empty-suggestion-1");
  expect(app.value("id:composer-input")).toBe("我遇到了一个报错： ");
});

test("searches workspaces and toggles search with the keyboard", async () => {
  const app = await openApp();
  await app.press("id:open-search");
  await app.type("id:global-search-input", "workspace");
  await app.press("id:search-workspace-0");
  expect(app.count("id:global-search-input")).toBe(0);
  expect(app.texts()).not.toContain("选择工作区");

  await app.keyboard.press("mod+k");
  expect(app.count("id:global-search-input")).toBe(1);
  await app.keyboard.press("escape");
  expect(app.count("id:global-search-input")).toBe(0);
});

test("opens and refreshes the git changes panel", async () => {
  const app = await openApp();
  await app.press("id:right-panel-toggle");
  await app.waitFor("id:right-panel");
  await app.waitFor("当前目录不是 Git 仓库", { timeout: 10000 });
  await app.press("id:right-refresh");
  await app.press("id:right-panel-close");
  await app.wait(300);
  await app.press("id:right-panel-toggle");
  await app.waitFor("当前目录不是 Git 仓库", { timeout: 10000 });
});

test("runs a real shell in the terminal panel", async () => {
  const app = await openApp();
  await app.press("id:terminal-toggle");
  await app.waitFor("id:terminal-input", { timeout: 10000 });
  await app.waitFor("id:terminal-output", { timeout: 10000 });
  expect(app.count("id:terminal-run")).toBe(0);
  expect(app.count("id:terminal-interrupt")).toBe(0);
  await app.type("id:terminal-input", "printf '\\033[1;33mPIG_TERMINAL_YELLOW\\033[0m\\n'");
  await app.keyboard.press("enter");
  await app.waitFor("text:/PIG_TERMINAL_YELLOW/");
  await app.type("id:terminal-input", "printf PIG_TERMINAL_OK");
  await app.keyboard.press("enter");
  await app.waitFor("text:/PIG_TERMINAL_OK/", { timeout: 10000 });
  await app.type("id:terminal-input", "printf PIG_TERMINAL_SECOND");
  await app.keyboard.press("enter");
  await app.waitFor("text:/PIG_TERMINAL_SECOND/", { timeout: 10000 });
  await app.press("id:terminal-new-tab");
  await app.waitFor("id:terminal-tab-2");
  await app.press("id:terminal-tab-close-2");
  await app.waitFor("id:terminal-tab-2", { state: "gone" });
  await app.type("id:terminal-input", "for i in $(seq 1 80); do echo LINE_$i; done");
  await app.keyboard.press("enter");
  await app.waitFor("text:/LINE_80/");
  await app.scroll("id:terminal-output", -240);
  await app.type("id:terminal-input", "echo PIG_AFTER_SCROLL");
  await app.keyboard.press("enter");
  await app.waitFor("text:/PIG_AFTER_SCROLL/");
  const dragging = app.drag("id:terminal-output", { x: -100, y: -36 });
  await app.wait(120);
  expect(app.count("id:terminal-selection")).toBeGreaterThan(0);
  await dragging;
  expect(app.count("id:terminal-selection")).toBeGreaterThan(0);
  await app.keyboard.press("mod+c");
  await app.drag("id:terminal-output", { x: 100, y: 100 });
  expect(app.count("id:terminal-selection")).toBeGreaterThan(0);
  await app.keyboard.press("mod+c");
  await app.drag("id:terminal-output", { x: 0, y: 0 });
  expect(app.count("id:terminal-selection")).toBe(0);
  await app.drag("label:Resize the terminal", { x: 0, y: -40 });
  await app.press("id:terminal-close");
  await app.waitFor("id:terminal-panel", { state: "gone" });
});

test("installs declarative controls and plugin pages and cleans them on disable", async () => {
  const app = await openApp();
  await app.type("id:composer-input", "MOON_SURFACE_SESSION");
  await app.press("id:send-button");
  await app.waitFor("Conclusion: MOCK_REPLY_OK", { timeout: 20000 });
  await installMoon(app);
  await app.waitFor("id:plugin-status-com.pigcodets.plugins.moon-moon-metric", { timeout: 10000 });
  await app.waitFor("text:Moon 0");

  await app.press("id:workspace-plugin-com.pigcodets.plugins.moon-moon-page");
  await app.waitFor("id:plugin-page-com.pigcodets.plugins.moon-moon-page", { timeout: 10000 });
  if (await realPlatform(app)) {
    await app.waitFor("text:This page is rendered in the host Engine.", { timeout: 10000 });
    await app.screenshot("target/test-tmp/moon-workspace-surface.png");
  }

  await app.press("id:new-task");
  await app.type("id:composer-input", "MOON_BUTTON_SUBMIT");
  await app.press("id:plugin-composer-com.pigcodets.plugins.moon-moon-action");
  await app.waitFor("text:/MOON_BUTTON_SUBMIT/", { timeout: 20000 });
  await app.waitFor("text:Moon 1");
  await app.press("id:plugin-status-com.pigcodets.plugins.moon-moon-metric");
  await app.waitFor("text:Submissions");
  if (await realPlatform(app)) {
    await app.screenshot("target/test-tmp/moon-host-popover.png");
  }
  await app.press("id:plugin-status-com.pigcodets.plugins.moon-moon-metric");
  await app.press("id:plugin-status-com.pigcodets.plugins.moon-moon-popover");
  const loading = "id:plugin-surface-com.pigcodets.plugins.moon-moon-popover-loading";
  await app.waitFor("id:plugin-surface-com.pigcodets.plugins.moon-moon-popover");
  if (await realPlatform(app)) {
    await app.waitFor(loading, { state: "gone", timeout: 10000 });
    await app.waitFor("text:This popover shares the host View tree.", { timeout: 10000 });
    await app.screenshot("target/test-tmp/moon-popover-open.png");
    await app.keyboard.press("escape");
  } else {
    await app.press("id:plugin-status-com.pigcodets.plugins.moon-moon-popover");
  }
  await app.waitFor("text:Closed 1");
  if (await realPlatform(app)) {
    await app.screenshot("target/test-tmp/moon-popover-closed.png");
  }

  await app.press("id:title-theme-settings");
  await app.press("id:settings-plugin-com.pigcodets.plugins.moon-moon-settings");
  await app.waitFor("id:plugin-page-com.pigcodets.plugins.moon-moon-settings", { timeout: 10000 });
  await app.press("id:settings-plugins");
  await app.press("id:theme-package-com.pigcodets.plugins.moon");
  await app.press("id:settings-back");
  await app.waitFor("id:plugin-status-com.pigcodets.plugins.moon-moon-metric", { state: "gone" });
  await app.waitFor("id:plugin-composer-com.pigcodets.plugins.moon-moon-action", { state: "gone" });
  await app.press("id:title-theme-settings");
  await app.press("id:settings-plugins");
  await app.press("id:plugin-uninstall-com.pigcodets.plugins.moon");
  await app.press("id:plugin-uninstall-confirm");
  await app.waitFor("id:theme-package-com.pigcodets.plugins.moon", { state: "gone" });
  await app.press("id:settings-back");
}, 60000);

test("renders signed Moon components in the host Engine", async () => {
  const app = await openApp();
  if (!await realPlatform(app)) return;
  await installMoon(app);
  await app.waitFor("id:plugin-composer-com.pigcodets.plugins.moon-moon-action", { timeout: 10000 });
  await app.press("id:workspace-plugin-com.pigcodets.plugins.moon-moon-page");
  await app.waitFor("id:plugin-page-com.pigcodets.plugins.moon-moon-page", { timeout: 10000 });
  await app.waitFor("text:This page is rendered in the host Engine.", { timeout: 10000 });
  expect(app.count("text:This page is rendered by the plugin Surface.")).toBe(0);
  await app.press("id:title-theme-settings");
  await app.press("id:settings-plugin-com.pigcodets.plugins.moon-moon-settings");
  await app.waitFor("text:This settings page is rendered in the host Engine.", { timeout: 10000 });
  await app.press("id:settings-back");
  await app.waitFor("text:This page is rendered in the host Engine.");
  await app.press("id:open-search");
  await app.waitFor("id:global-search-dialog");
  await app.waitFor("id:global-search-input");
  expect(app.texts()).toContain("This page is rendered in the host Engine.");
  await app.screenshot("target/test-tmp/moon-component-search.png");
  await app.keyboard.press("escape");
  await app.waitFor("id:global-search-dialog", { state: "gone" });
  await app.screenshot("target/test-tmp/moon-component-workspace.png");
}, 60000);

test("keeps plugin settings pages mounted across navigation", async () => {
  const app = await openApp();
  if (!await realPlatform(app)) return;
  await installMoon(app);
  const page = "id:plugin-page-com.pigcodets.plugins.moon-moon-settings";
  const loading = `${page}-loading`;
  await app.press("id:title-theme-settings");
  await app.waitFor("id:settings-plugin-com.pigcodets.plugins.moon-moon-settings");
  await app.press("id:settings-plugin-com.pigcodets.plugins.moon-moon-settings");
  await app.waitFor(page, { timeout: 10000 });
  await app.waitFor(loading, { state: "gone", timeout: 10000 });
  await app.waitFor("text:This settings page is rendered in the host Engine.", { timeout: 10000 });
  for (let index = 0; index < 3; index += 1) {
    await app.press("id:settings-plugins");
    await app.waitFor("id:plugin-manager-page");
    await app.waitFor(page, { state: "gone" });
    await app.press("id:settings-plugin-com.pigcodets.plugins.moon-moon-settings");
    await app.waitFor(page);
    expect(app.count(loading)).toBe(0);
    expect(app.count(page)).toBe(1);
  }
  await app.wait(500);
  expect(app.count(loading)).toBe(0);
  await app.screenshot("target/test-tmp/moon-settings-stable.png");
}, 60000);

test("answers dangerous command approvals", async () => {
  const app = await openApp();
  await app.type("id:composer-input", "DANGER_SCENARIO");
  await app.press("id:send-button");
  await app.waitFor("需要批准：ExecCommand", { timeout: 20000 });
  await app.keyboard.press("enter");
  await app.waitFor("需要批准：ExecCommand", { timeout: 20000 });
  await app.press("id:approval-allow");
  await app.waitFor("MOCK_DANGER_DONE", { timeout: 20000 });
  expect(app.texts()).toContain("MOCK_DANGER_DONE");
});

test("switches appearance, list view and folds the sidebar", async () => {
  const app = await openApp();
  await app.drag("label:Resize the sidebar", { x: 60, y: 0 });
  await app.press("id:workspace-row");
  expect(app.count("展开当前工作区")).toBe(1);
  await app.press("id:workspace-row");
  expect(app.count("折叠当前工作区")).toBe(1);

  await app.press("id:list-manage");
  await app.press("id:list-view-flat");
  expect(app.count("id:workspace-row")).toBe(0);
  await app.press("id:list-manage");
  await app.press("id:list-view-workspace");
  expect(app.count("id:workspace-row")).toBe(1);

  const initial = app.count("id:appearance-dark") === 1 ? "dark" : "light";
  await app.press(`id:appearance-${initial}`);
  const next = initial === "dark" ? "light" : "dark";
  expect(app.count(`id:appearance-${next}`)).toBe(1);
  await app.press("id:sidebar-toggle");
  await app.wait(300);
  await app.press("id:sidebar-toggle >> nth=0");
  await app.wait(300);
  expect(app.count("id:new-task")).toBe(1);
  expect(app.count("role:adjustable")).toBe(1);
});

test("applies named themes and appearance modes to native styles", async () => {
  const app = await openApp();
  await app.press("id:title-theme-settings");
  expect(app.count("id:theme-picker")).toBe(1);
  await app.press("id:theme-mode-dark");
  await app.press("id:theme-option-ocean");
  expect(app.spanStyle("id:title-bar >> text:Pig Code v0.1.0").color).toBe("#cbd5e1ff");

  await app.press("id:theme-option-pig");
  expect(app.spanStyle("id:title-bar >> text:Pig Code v0.1.0").color).toBe("#d4d4d4ff");
  await app.press("id:theme-mode-light");
  expect(app.spanStyle("id:title-bar >> text:Pig Code v0.1.0").color).toBe("#262626ff");
  await app.press("id:settings-back");
  expect(app.count("id:theme-picker")).toBe(0);
});

test("filters themes and switches appearance for single-mode palettes", async () => {
  const app = await openApp();
  await app.press("id:title-theme-settings");

  await app.press("id:theme-filter-dark");
  expect(app.count("id:theme-option-aurora-light")).toBe(0);
  expect(app.count("id:theme-option-adventure")).toBe(1);

  await app.press("id:theme-filter-light");
  await app.press("id:theme-option-aurora-light");
  expect(app.spanStyle("id:title-bar >> text:Pig Code v0.1.0").color).toBe("#020617ff");

  await app.press("id:theme-mode-dark");
  expect(app.spanStyle("id:title-bar >> text:Pig Code v0.1.0").color).toBe("#d4d4d4ff");

  await app.press("id:theme-filter-all");
  await app.type("id:theme-search", "ayu");
  expect(app.count("id:theme-option-ayu-light")).toBe(1);
  expect(app.count("id:theme-option-ayu-dark")).toBe(1);
  expect(app.count("id:theme-option-aurora-light")).toBe(0);
});

test("locale catalogs keep the same keys", () => {
  expect(catalogKeys(en)).toEqual(catalogKeys(zhCN));
});

test("switches language and applies fonts from appearance settings", async () => {
  const app = await openApp();
  await app.press("id:title-theme-settings");
  await app.press("id:language-select");
  await app.press("id:language-option-en");
  expect(app.texts()).toContain("Theme");
  expect(app.texts()).toContain("Language & fonts");

  await app.press("id:ui-font-select");
  await app.type("id:ui-font-select-search", "Menlo");
  await app.press("id:ui-font-select-option-0");
  expect(app.texts()).toContain("Menlo");

  await app.press("id:language-select");
  await app.press("id:language-option-zh-CN");
  expect(app.texts()).toContain("主题");
  expect(app.texts()).toContain("语言与字体");
});

test("renders the main shell in English for en-US", async () => {
  const app = await open("./App.selftest.tsx", {
    platform: "macos",
    locale: "en-US",
    width: 1280,
    height: 800,
  });
  const texts = app.texts();
  expect(texts).toContain("New task");
  expect(texts).toContain("Search");
  expect(texts).toContain("Summarize this workspace");
  expect(texts).toContain("Write unit tests");

  await app.press("id:settings-open");
  const settings = app.texts();
  expect(settings).toContain("Models");
  expect(settings).toContain("Appearance");
  expect(settings).toContain("Plugins");
});

test("renders the theme page in English for en-US", async () => {
  const app = await open("./App.selftest.tsx", {
    platform: "macos",
    locale: "en-US",
    width: 1280,
    height: 800,
  });
  await app.press("id:title-theme-settings");
  expect(app.texts()).toContain("Theme");
  expect(app.texts()).toContain("Theme mode");
  expect(app.texts()).toContain("Follow the system, or pin light or dark.");
});

test("loads, applies, disables and restores plugin themes", async () => {
  const app = await openApp();
  await installMoon(app);
  await app.press("id:title-theme-settings");
  await app.waitFor("id:theme-option-moon");
  await app.press("id:theme-mode-dark");
  await app.press("id:theme-option-moon");
  expect(app.texts()).toContain("Moon Glass");

  await app.press("id:settings-plugins");
  await app.press("id:theme-package-com.pigcodets.plugins.moon");
  await app.waitFor("id:theme-option-moon", { state: "gone" });
  expect(app.spanStyle("id:title-bar >> text:Pig Code v0.1.0").color).toBe("#d4d4d4ff");
  await app.press("id:theme-package-com.pigcodets.plugins.moon");
  await app.press("id:settings-appearance");
  await app.waitFor("id:theme-option-moon");
});

test("applies configured interface and monospace fonts through theme tokens", () => {
  const store = new ThemeStore();
  store.hydrateFonts("Menlo", "JetBrains Mono");
  expect(store.serialized()).toContain('"font-sans":"Menlo"');
  expect(store.serialized()).toContain('"font-mono":"JetBrains Mono"');
  store.hydrateFonts(null, null);
  expect(store.serialized()).not.toContain('"font-sans"');
  expect(store.serialized()).not.toContain('"font-mono"');
});

test("registers partial plugin themes without losing fallback tokens", () => {
  const store = new ThemeStore();
  store.hydrate("moon", "system");
  expect(store.active().id).toBe("pig");
  store.register([
    {
      id: "moon",
      name: "Midnight",
      description: "Plugin theme",
      cssVars: { dark: { background: "#101820" } },
    },
  ]);
  expect(store.active().source).toBe("plugin");
  expect(store.catalog().filter((theme) => theme.id === "moon").length).toBe(1);
  expect(store.serialized()).toContain('"background":"#101820"');
  expect(store.serialized()).toContain('"tool-surface":"#171717"');
});

test("merges and removes style packs by their target themes", () => {
  const store = new ThemeStore();
  store.register([
    {
      id: "moon-glass",
      name: "Glass",
      kind: "style",
      targetThemes: ["pig"],
      cssVars: { dark: { primary: "#22d3ee" } },
      parts: {
        "composer.root": {
          class: "rounded-3xl border-primary/50",
          style: { borderWidth: 2 },
        },
      },
      background: { asset: "file:///background.svg", opacity: 0.5 },
    },
  ]);
  expect(store.classes("composer.root", "rounded-2xl")).toBe(
    "rounded-2xl rounded-3xl border-primary/50",
  );
  expect(store.styles("composer.root")).toEqual({ borderWidth: 2 });
  expect(store.serialized()).toContain('"primary":"#22d3ee"');
  expect(store.background()?.asset).toBe("file:///background.svg");
  store.select("ocean");
  expect(store.classes("composer.root", "rounded-2xl")).toBe("rounded-2xl");
  expect(store.background()).toBeNull();
  store.select("pig");
  store.syncPlugins([]);
  expect(store.classes("composer.root", "rounded-2xl")).toBe("rounded-2xl");
  expect(store.background()).toBeNull();
});

test("keeps user appearance ahead of late configuration", () => {
  const store = new ThemeStore();
  store.setMode("dark");
  store.hydrate("ocean", "light");
  expect(store.mode()).toBe("dark");
  expect(store.activeId()).toBe("pig");
});
