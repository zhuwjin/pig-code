import { expect, open, test } from "nsh/test";

interface EditorReport {
  position: number;
  selection: string;
  types: string[];
  text: string;
  model: string;
}

test("keeps chip spacing without rewriting subsequent input", async () => {
  const app = await open("./App.composer-editor.fixture.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 500,
    height: 220,
  });
  const report = (): EditorReport => {
    const value = app.texts().find((text) => text.startsWith("{") && text.includes("\"model\""));
    expect(value).toBeDefined();
    return JSON.parse(value!) as EditorReport;
  };

  await app.press("id:insert");
  await app.press("id:measure");
  expect(report()).toEqual({ position: 3, selection: "text", types: ["image", "space"], text: "图片 1", model: "Image 1 " });
  expect(app.count("id:composer-chip-icon-image")).toBe(1);

  await app.keyboard.press("space");
  await app.press("id:measure");
  expect(report()).toEqual({ position: 4, selection: "text", types: ["image", "space", "text"], text: "图片 1 ", model: "Image 1  " });

  await app.keyboard.press("x");
  await app.press("id:measure");
  expect(report()).toEqual({ position: 5, selection: "text", types: ["image", "space", "text"], text: "图片 1 x", model: "Image 1  x" });
});

test("keeps every character through repeated chip and space input", async () => {
  const app = await open("./App.composer-editor.fixture.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 500,
    height: 220,
  });
  for (let index = 0; index < 20; index++) {
    await app.press("id:insert");
    await app.keyboard.press("space");
    await app.keyboard.press("x");
  }
  await app.press("id:measure");
  const value = app.texts().find((text) => text.startsWith("{") && text.includes("\"model\""));
  expect(value).toBeDefined();
  expect((JSON.parse(value!) as EditorReport).model).toBe("Image 1  x".repeat(20));
});

test("does not grow the paragraph height around chips", async () => {
  const app = await open("./App.composer-editor.fixture.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 500,
    height: 260,
  });
  const blockHeight = () => {
    const value = app.texts().find((text) => text.startsWith("{\"height\""));
    expect(value).toBeDefined();
    return (JSON.parse(value!) as { height: number }).height;
  };

  await app.press("id:measure");
  const before = blockHeight();
  await app.press("id:insert");
  await app.press("id:measure");
  expect(blockHeight()).toBe(before);
});

test("deletes a chip as one inline atom", async () => {
  const app = await open("./App.composer-editor.fixture.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 500,
    height: 260,
  });
  const report = (): EditorReport => {
    const value = app.texts().find((text) => text.startsWith("{") && text.includes("\"model\""));
    expect(value).toBeDefined();
    return JSON.parse(value!) as EditorReport;
  };

  await app.type("id:rich", "before");
  await app.press("id:insert");
  await app.keyboard.press("backspace");
  expect(app.count("id:composer-chip-image")).toBe(1);
  await app.keyboard.press("backspace");
  expect(app.count("id:composer-chip-image")).toBe(0);
  await app.press("id:measure");
  expect(report().text).toBe("before");
});

test("grows with content up to ten lines and then keeps a scrolling viewport", async () => {
  const app = await open("./App.composer-editor.fixture.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 500,
    height: 500,
  });
  const height = () => {
    const value = app.texts().find((text) => text.startsWith("editor-height:"));
    expect(value).toBeDefined();
    return Number(value!.slice("editor-height:".length));
  };

  await app.waitUntil(() => height() === 40);
  await app.type("id:rich", "1");
  await app.keyboard.press("shift+enter");
  await app.type("id:rich", "2");
  await app.waitUntil(() => height() > 40);
  for (let line = 3; line <= 9; line++) {
    await app.keyboard.press("shift+enter");
    await app.type("id:rich", `${line}`);
  }
  const nineLineHeight = height();
  await app.keyboard.press("shift+enter");
  await app.type("id:rich", "10");
  await app.waitUntil(() => height() > nineLineHeight);
  const tenLineHeight = height();
  await app.keyboard.press("shift+enter");
  await app.type("id:rich", "11");
  await app.wait(100);
  expect(height()).toBe(tenLineHeight);
  await app.scroll("id:rich", -100);
});
