import { locale, signal } from "nsh";
import en from "./locales/en.json";
import zhCN from "./locales/zh-CN.json";

export type AppLocale = "en" | "zh-CN";
export type LanguagePreference = "system" | AppLocale;

export const languagePreference = signal<LanguagePreference>("system");

export function setLanguagePreference(value: LanguagePreference): void {
  languagePreference.set(value);
}

type LeafKeys<T, Prefix extends string = ""> = {
  [K in keyof T & string]: T[K] extends string
    ? `${Prefix}${K}`
    : LeafKeys<T[K], `${Prefix}${K}.`>;
}[keyof T & string];

export type MessageKey = LeafKeys<typeof en>;

function lookup(messages: unknown, key: string): string | undefined {
  let current = messages;
  for (const part of key.split(".")) {
    if (typeof current !== "object" || current === null) return undefined;
    current = (current as Record<string, unknown>)[part];
  }
  return typeof current === "string" ? current : undefined;
}

export function appLocale(): AppLocale {
  const preference = languagePreference();
  if (preference !== "system") return preference;
  return locale().toLowerCase().startsWith("zh") ? "zh-CN" : "en";
}

export function t(key: MessageKey, params?: Record<string, string | number>): string {
  const text = lookup(appLocale() === "zh-CN" ? zhCN : en, key) ?? lookup(en, key) ?? key;
  if (!params) return text;
  return text.replace(/\{(\w+)\}/g, (_, name: string) =>
    params[name] === undefined ? `{${name}}` : String(params[name])
  );
}

export function tCount(base: string, count: number, params?: Record<string, string | number>): string {
  const form = appLocale() === "en" && count === 1 ? "one" : "other";
  return t(`${base}.${form}` as MessageKey, { count, ...params });
}
