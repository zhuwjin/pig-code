import zhAliases from "../../locales/icon-aliases.zh-CN.json";
import { ICON_CATALOG, type IconMeta } from "./icon-catalog";

const ALIASES: [string, string][] = [
  ["bigmodel", "zhipu"], ["glm", "zhipu"], ["chatglm", "zhipu"],
  ["z.ai", "zai"], ["z-ai", "zai"],
  ["dashscope", "bailian"],
  ["mimo", "xiaomimimo"],
  ["ernie", "wenxin"],
  ["gpt", "openai"], ["chatgpt", "openai"], ["o1", "openai"], ["o3", "openai"],
  ["o4", "openai"], ["x.ai", "xai"], ["claude", "anthropic"], ["sonnet", "claude"],
  ["opus", "claude"], ["haiku", "claude"], ["grok", "xai"], ["gemini", "google"],
  ["llama", "meta"], ["mixtral", "mistral"], ["phi", "microsoft"], ["command", "commanda"],
  ["step", "stepfun"], ["sensechat", "sensenova"], ["nemotron", "nvidia"],
  ["siliconflow", "siliconcloud"], ["lm studio", "lmstudio"], ["glm", "glmv"],
];

const ALL_ALIASES: [string, string][] = [...ALIASES, ...(zhAliases as [string, string][])];

function normalize(text: string): string {
  return text.toLowerCase().split(/[^\p{L}\p{N}]+/gu).filter(Boolean).join(" ");
}

function keys(meta: IconMeta): string[] {
  return [
    meta.id,
    meta.title,
    ...ALL_ALIASES.filter(([, target]) => target === meta.id).map(([alias]) => alias),
  ];
}

const SEARCH_INDEX = ICON_CATALOG.map((meta) => ({
  meta,
  keys: keys(meta).map((key) => {
    const normalized = normalize(key);
    return { normalized, compact: normalized.replaceAll(" ", "") };
  }),
}));

export function searchIcons(query: string): IconMeta[] {
  const normalized = normalize(query);
  if (!normalized) return ICON_CATALOG;
  const compactQuery = normalized.replaceAll(" ", "");
  return SEARCH_INDEX.filter(({ keys }) => keys.some((key) =>
    key.normalized.includes(normalized) || (compactQuery && key.compact.includes(compactQuery))
  )).map(({ meta }) => meta);
}
