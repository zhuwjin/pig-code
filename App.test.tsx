import { token } from "nsh";
import type { CanvasRenderingContext2D } from "nsh";
import { expect, open, test } from "nsh/test";
import { iconTint, paintIcon } from "./app/components/icon";
import { ICON_ELEMENTS } from "./app/components/icon-elements";
import { createParticles, scatterParticles, stepParticles } from "./app/hero-particles";
import { ThemeStore } from "./app/theme";

function openApp() {
  return open("./App.selftest.tsx", {
    platform: "macos",
    width: 1280,
    height: 800,
  });
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
  expect(app.texts()).toContain("Read");
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

test("installs a signed UI plugin and restores the host on disable", async () => {
  const app = await openApp();
  await app.press("id:title-theme-settings");
  await app.type("id:plugin-package-path", "target/test-tmp/moon-plugin/moon.nshpkg");
  await app.type("id:plugin-root-path", "target/test-tmp/moon-plugin/root.pub");
  await app.press("id:plugin-install");
  await app.waitFor("text:/com.pigcodets.plugins.moon/", { timeout: 10000 });
  await app.press("id:theme-package-com.pigcodets.plugins.moon");
  await app.press("id:theme-picker-close");
  await app.waitFor("id:plugin-page-com.pigcodets.plugins.moon-moon-panel", { timeout: 10000 });
  await app.waitFor("id:plugin-send-icon");
  await app.waitFor("text:Moon engine is running", { timeout: 10000 });
  await app.waitFor("text:Dynamic moon note");
  await app.type("id:composer-input", "MOON_BUTTON_SUBMIT");
  await app.press("id:send-button");
  await app.waitFor("text:/MOON_BUTTON_SUBMIT/", { timeout: 10000 });
  await app.press("id:moon-submit");
  await app.type("id:composer-input", "MOON_REMOTE_SUBMIT");
  await app.press("id:moon-submit");
  await app.waitFor("text:/MOON_REMOTE_SUBMIT/", { timeout: 10000 });
  await app.press("id:title-theme-settings");
  expect(app.count("id:theme-option-moon")).toBe(1);
  await app.press("id:theme-package-com.pigcodets.plugins.moon");
  await app.press("id:theme-picker-close");
  await app.waitFor("id:plugin-send-icon", { state: "gone" });
  await app.waitFor("id:plugin-page-com.pigcodets.plugins.moon-moon-panel", { state: "gone" });
  await app.press("id:title-theme-settings");
  await app.press("id:plugin-uninstall-com.pigcodets.plugins.moon");
  await app.press("id:plugin-uninstall-confirm");
  await app.waitFor("id:theme-package-com.pigcodets.plugins.moon", { state: "gone" });
  await app.press("id:theme-picker-close");
});

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
  expect(app.spanStyle("✓").color).toBe("#3b82f6ff");

  await app.press("id:theme-option-pig");
  expect(app.spanStyle("id:title-bar >> text:Pig Code v0.1.0").color).toBe("#d4d4d4ff");
  await app.press("id:theme-mode-light");
  expect(app.spanStyle("id:title-bar >> text:Pig Code v0.1.0").color).toBe("#262626ff");
  await app.press("id:theme-picker-close");
  expect(app.count("id:theme-picker")).toBe(0);
});

test("loads, applies, disables and restores plugin themes", async () => {
  const app = await openApp();
  await app.press("id:title-theme-settings");
  await app.waitFor("id:theme-option-acme.midnight");
  await app.press("id:theme-mode-dark");
  await app.press("id:theme-option-acme.midnight");
  expect(app.spanStyle("id:title-bar >> text:Pig Code v0.1.0").color).toBe("#e2e8f0ff");
  expect(app.spanStyle("✓").color).toBe("#22d3eeff");
  expect(app.texts()).toContain("Glass Wallpaper");

  await app.press("id:theme-package-acme.visual");
  await app.waitFor("id:theme-option-acme.midnight", { state: "gone" });
  expect(app.spanStyle("id:title-bar >> text:Pig Code v0.1.0").color).toBe("#d4d4d4ff");
  await app.press("id:theme-package-acme.visual");
  await app.waitFor("id:theme-option-acme.midnight");
  expect(app.spanStyle("id:title-bar >> text:Pig Code v0.1.0").color).toBe("#e2e8f0ff");
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
  store.hydrate("acme.midnight", "system");
  expect(store.active().id).toBe("pig");
  store.register([
    {
      id: "acme.midnight",
      name: "Midnight",
      description: "Plugin theme",
      cssVars: { dark: { background: "#101820" } },
    },
  ]);
  expect(store.active().source).toBe("plugin");
  expect(store.catalog().filter((theme) => theme.id === "acme.midnight").length).toBe(1);
  expect(store.serialized()).toContain('"background":"#101820"');
  expect(store.serialized()).toContain('"tool-surface":"#171717"');
});

test("merges and removes style packs by their target themes", () => {
  const store = new ThemeStore();
  store.register([
    {
      id: "acme.glass",
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
