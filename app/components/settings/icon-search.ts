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

function aliases(): [string, string][] {
  return [...ALIASES, ...(zhAliases as [string, string][])];
}

function normalize(text: string): string {
  return text.toLowerCase().split(/[^\p{L}\p{N}]+/gu).filter(Boolean).join(" ");
}

function compact(text: string): string {
  return normalize(text).replaceAll(" ", "");
}

function keys(meta: IconMeta): string[] {
  return [
    meta.id,
    meta.title,
    ...aliases().filter(([, target]) => target === meta.id).map(([alias]) => alias),
  ];
}

export function searchIcons(query: string): IconMeta[] {
  const normalized = normalize(query);
  if (!normalized) return ICON_CATALOG;
  const compactQuery = compact(query);
  return ICON_CATALOG.filter((meta) => keys(meta).some((key) =>
    normalize(key).includes(normalized) || (compactQuery && compact(key).includes(compactQuery))
  ));
}
