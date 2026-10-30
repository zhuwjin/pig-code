import { ICON_CATALOG, type IconMeta } from "./icon-catalog";

const ALIASES: [string, string][] = [
  ["bigmodel", "zhipu"], ["glm", "zhipu"], ["chatglm", "zhipu"], ["智谱", "zhipu"],
  ["z.ai", "zai"], ["z-ai", "zai"], ["月之暗面", "kimi"], ["深度求索", "deepseek"],
  ["稀宇", "minimax"], ["百炼", "bailian"], ["dashscope", "bailian"], ["通义", "qwen"],
  ["千问", "qwen"], ["小米", "xiaomimimo"], ["mimo", "xiaomimimo"], ["混元", "hunyuan"],
  ["文心", "wenxin"], ["ernie", "wenxin"], ["豆包", "doubao"], ["火山", "volcengine"],
  ["阶跃", "stepfun"], ["商汤", "sensenova"], ["零一万物", "zeroone"], ["讯飞", "iflytekcloud"],
  ["gpt", "openai"], ["chatgpt", "openai"], ["o1", "openai"], ["o3", "openai"],
  ["o4", "openai"], ["x.ai", "xai"], ["claude", "anthropic"], ["sonnet", "claude"],
  ["opus", "claude"], ["haiku", "claude"], ["grok", "xai"], ["gemini", "google"],
  ["llama", "meta"], ["mixtral", "mistral"], ["phi", "microsoft"], ["command", "commanda"],
  ["step", "stepfun"], ["sensechat", "sensenova"], ["nemotron", "nvidia"],
  ["siliconflow", "siliconcloud"], ["lm studio", "lmstudio"], ["谷歌", "google"],
  ["微软", "microsoft"], ["亚马逊", "aws"], ["英伟达", "nvidia"], ["glm", "glmv"],
];

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
    ...ALIASES.filter(([, target]) => target === meta.id).map(([alias]) => alias),
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
