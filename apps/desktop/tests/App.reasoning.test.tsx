import { expect, open, test } from "nsh/test";
import { ReasoningTicker, reasoningTickerLine, type ReasoningTickerLine } from "../app/components/reasoning-ticker";

function line(index: number, suffix = ""): ReasoningTickerLine {
  return { index, text: `Line ${index}${suffix}` };
}

test("finds the last non-empty reasoning line and normalizes whitespace", () => {
  expect(reasoningTickerLine("First\n\n  Second\tline \r\n\n")).toEqual({ index: 2, text: "Second line" });
  expect(reasoningTickerLine(" \n\t\n")).toBe(null);
});

test("refreshes a line in place and paces new lines with a bounded queue", () => {
  const ticker = new ReasoningTicker();
  expect(ticker.feed(line(0))).toBe(false);
  expect(ticker.feed(line(0, " updated"))).toBe(false);
  expect(ticker.displayed).toEqual(line(0, " updated"));
  expect(ticker.exiting).toBe(null);

  expect(ticker.feed(line(1))).toBe(true);
  const firstRoll = ticker.generation;
  expect(ticker.displayed).toEqual(line(1));
  expect(ticker.exiting).toEqual(line(0, " updated"));
  expect(ticker.feed(line(2))).toBe(false);
  expect(ticker.feed(line(3))).toBe(false);
  expect(ticker.feed(line(4))).toBe(false);
  expect(ticker.feed(line(4, " latest"))).toBe(false);
  expect(ticker.queue).toEqual([line(2), line(4, " latest")]);
  expect(ticker.fire(firstRoll + 1, Date.now())).toBe(false);

  expect(ticker.fire(firstRoll, Date.now() + 800)).toBe(true);
  expect(ticker.displayed).toEqual(line(2));
  expect(ticker.exiting).toEqual(line(1));
  const secondRoll = ticker.generation;
  expect(ticker.fire(secondRoll, Date.now() + 1600)).toBe(true);
  expect(ticker.displayed).toEqual(line(4, " latest"));
  expect(ticker.queue).toEqual([]);
  expect(ticker.fire(ticker.generation, Date.now() + 2400)).toBe(false);
  expect(ticker.exiting).toBe(null);
  expect(ticker.rolling).toBe(false);

  ticker.feed(line(5));
  ticker.reset(line(5));
  expect(ticker.queue).toEqual([]);
  expect(ticker.exiting).toBe(null);
  expect(ticker.rolling).toBe(false);
  expect(ticker.feed(line(5, " current"))).toBe(false);
});

test("skips an intermediate queued line after timer drift", () => {
  const ticker = new ReasoningTicker();
  ticker.feed(line(0));
  ticker.feed(line(1));
  const generation = ticker.generation;
  ticker.feed(line(2));
  ticker.feed(line(3));
  expect(ticker.fire(generation, Date.now() + 1200)).toBe(true);
  expect(ticker.displayed).toEqual(line(3));
  expect(ticker.queue).toEqual([]);
});

test("settles visible reasoning labels as tools, new reasoning and text arrive", async () => {
  const app = await open("./App.reasoning.fixture.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 1000,
    height: 500,
    colorScheme: "dark",
  });
  await app.press("id:next-tool");
  await app.press("id:activity-thinking");
  await app.waitFor("id:tool-tool-1");
  expect(app.shows("id:thinking-ticker-thinking")).toBe(false);
  expect(app.texts()).toContain("思考过程");
  expect(app.texts()).not.toContain("思考中");

  await app.press("id:late-thinking");
  await app.wait(60);
  expect(app.shows("id:thinking-ticker-thinking")).toBe(false);
  expect(app.texts()).not.toContain("思考中");

  await app.press("id:next-reasoning");
  await app.waitFor("id:thinking-ticker-thinking-next-1");
  expect(app.shows("id:thinking-ticker-thinking")).toBe(false);
  expect(app.count("label:思考中")).toBe(1);

  await app.press("id:next-text");
  await app.waitFor("Answer body");
  expect(app.shows("id:thinking-ticker-thinking-next-1")).toBe(false);
  expect(app.texts()).not.toContain("思考中");

  await app.press("id:next-reasoning");
  await app.waitFor("id:thinking-ticker-thinking-next-2");
  expect(app.count("label:思考中")).toBe(1);
  await app.press("id:next-tool");
  await app.press("id:activity-thinking-next-2");
  await app.waitFor("id:tool-tool-2");
  expect(app.shows("id:thinking-ticker-thinking-next-2")).toBe(false);
  expect(app.texts()).not.toContain("思考中");
  expect(app.texts().filter((text) => text === "思考过程")).toHaveLength(1);
  expect(app.texts().filter((text) => /^思考了 \d+ 秒$/.test(text))).toHaveLength(2);
}, 90000);

test("opens reasoning from its preview and restores the latest line on collapse", async () => {
  const app = await open("./App.reasoning.fixture.tsx", {
    platform: "macos",
    locale: "zh-CN",
    width: 1000,
    height: 500,
    colorScheme: "dark",
  });
  await app.press("id:append-thinking");
  await app.waitFor("text:/First rolling line with enough content.*longer suffix/");
  await app.press("id:thinking-ticker-thinking");
  await app.waitFor("id:thinking-thinking-content");
  expect(app.texts().some((text) => text.includes("First rolling line with enough content"))).toBe(true);

  await app.press("id:thinking-thinking");
  await app.waitFor("id:thinking-thinking-content", { state: "gone" });
  await app.press("id:next-thinking-line");
  await app.waitFor("Second rolling line");
  await app.press("id:thinking-ticker-thinking");
  await app.waitFor("id:thinking-thinking-content");
  expect(app.texts().some((text) => text.includes("Second rolling line"))).toBe(true);
});
