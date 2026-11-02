import themeCatalog from "./presets/themes.json";

export type ThemeMode = "system" | "light" | "dark";
export type ThemeKind = "theme" | "style";
export type ThemeTokenValue = string | number;

export interface ThemeCssVars {
  theme?: Record<string, ThemeTokenValue>;
  light?: Record<string, ThemeTokenValue>;
  dark?: Record<string, ThemeTokenValue>;
}

export interface ThemePart {
  class?: string;
  style?: Record<string, any>;
}

export interface ThemeBackground {
  asset: string;
  opacity?: number;
  blur?: number;
  overlay?: string;
}

export interface ThemeDefinition {
  id: string;
  name: string;
  description?: string;
  source: "builtin" | "plugin";
  kind?: ThemeKind;
  targetThemes?: string[];
  mode?: "light" | "dark";
  cssVars: ThemeCssVars;
  parts?: Record<string, ThemePart>;
  background?: ThemeBackground | null;
}

export type PluginThemeDefinition = Omit<ThemeDefinition, "source">;

export const PLUGIN_THEME_SLOTS = new Set([
  "app.root",
  "app.background",
  "app.content",
  "title-bar.root",
  "sidebar.root",
  "sidebar.session",
  "composer.root",
  "composer.input",
  "question.root",
  "approval.root",
  "queue.root",
  "settings.root",
  "settings.nav",
  "settings.models",
  "settings.provider-card",
  "settings.model-row",
  "chat.user",
  "chat.markdown",
  "markdown.code-block",
  "markdown.inline-code",
  "diff.root",
  "diff.gutter",
  "diff.line-added",
  "diff.line-deleted",
  "diff.word-added",
  "diff.word-deleted",
]);

export const DEFAULT_THEME_ID = "pig";

const PIG_CSS_VARS: ThemeCssVars = {
  theme: {
    radius: "0.625rem",
  },
  light: {
    background: "#fafafa",
    foreground: "#262626",
    card: "#ffffff",
    "card-foreground": "#262626",
    popover: "#ffffff",
    "popover-foreground": "#262626",
    primary: "#171717",
    "primary-foreground": "#fafafa",
    secondary: "#f0f0f0",
    "secondary-foreground": "#262626",
    muted: "#f0f0f0",
    "muted-foreground": "#737373",
    accent: "#e5e5e5",
    "accent-foreground": "#262626",
    destructive: "#dc2626",
    border: "#e5e5e5",
    input: "#d4d4d4",
    ring: "#a3a3a3",
    sidebar: "#f5f5f5",
    "sidebar-foreground": "#262626",
    "sidebar-primary": "#171717",
    "sidebar-primary-foreground": "#fafafa",
    "sidebar-accent": "#e5e5e5",
    "sidebar-accent-foreground": "#262626",
    "sidebar-border": "#e5e5e5",
    "sidebar-ring": "#a3a3a3",
    "title-bar": "#f0f0f0",
    composer: "#ffffff",
    "tool-surface": "#f5f5f5",
    selection: "#bfdbfe",
    success: "#16a34a",
    info: "#2563eb",
    warning: "#ca8a04",
    "diff-added": "#dcfce7",
    "diff-added-word": "#bbf7d0",
    "diff-deleted": "#fee2e2",
    "diff-deleted-word": "#fecaca",
  },
  dark: {
    background: "#0a0a0a",
    foreground: "#d4d4d4",
    card: "#0a0a0a",
    "card-foreground": "#f5f5f5",
    popover: "#0a0a0a",
    "popover-foreground": "#f5f5f5",
    primary: "#f5f5f5",
    "primary-foreground": "#0a0a0a",
    secondary: "#262626",
    "secondary-foreground": "#f5f5f5",
    muted: "#262626",
    "muted-foreground": "#a3a3a3",
    accent: "#262626",
    "accent-foreground": "#f5f5f5",
    destructive: "#f87171",
    border: "#262626",
    input: "#2f2f2f",
    ring: "#737373",
    sidebar: "#0a0a0a",
    "sidebar-foreground": "#f5f5f5",
    "sidebar-primary": "#f5f5f5",
    "sidebar-primary-foreground": "#0a0a0a",
    "sidebar-accent": "#262626",
    "sidebar-accent-foreground": "#f5f5f5",
    "sidebar-border": "#262626",
    "sidebar-ring": "#737373",
    "title-bar": "#171717",
    composer: "#1b1b1b",
    "tool-surface": "#171717",
    selection: "#1d4ed8",
    success: "#4ade80",
    info: "#60a5fa",
    warning: "#facc15",
    "diff-added": "#052e16",
    "diff-added-word": "#14532d",
    "diff-deleted": "#450a0a",
    "diff-deleted-word": "#7f1d1d",
  },
};

interface CatalogTheme {
  id: string;
  name: string;
  description?: string;
  mode: "light" | "dark";
  cssVars: ThemeCssVars;
}

const ZED_THEMES: ThemeDefinition[] = (themeCatalog as CatalogTheme[]).map((theme) => ({
  ...theme,
  source: "builtin",
}));

export const BUILTIN_THEMES: readonly ThemeDefinition[] = [
  {
    id: DEFAULT_THEME_ID,
    name: "Pig",
    description: "Pig 中性色",
    source: "builtin",
    cssVars: PIG_CSS_VARS,
  },
  {
    id: "ocean",
    name: "Ocean",
    description: "清晰的蓝色界面",
    source: "builtin",
    cssVars: {
      theme: {
        radius: "0.5rem",
      },
      light: {
        background: "#f8fafc",
        foreground: "#0f172a",
        card: "#ffffff",
        "card-foreground": "#0f172a",
        popover: "#ffffff",
        "popover-foreground": "#0f172a",
        primary: "#2563eb",
        "primary-foreground": "#eff6ff",
        secondary: "#e2e8f0",
        "secondary-foreground": "#0f172a",
        muted: "#e2e8f0",
        "muted-foreground": "#64748b",
        accent: "#dbeafe",
        "accent-foreground": "#1e3a8a",
        border: "#cbd5e1",
        input: "#94a3b8",
        ring: "#3b82f6",
        sidebar: "#eff6ff",
        "sidebar-foreground": "#0f172a",
        "sidebar-primary": "#2563eb",
        "sidebar-primary-foreground": "#eff6ff",
        "sidebar-accent": "#dbeafe",
        "sidebar-accent-foreground": "#1e3a8a",
        "sidebar-border": "#cbd5e1",
        "sidebar-ring": "#3b82f6",
        "title-bar": "#e2e8f0",
        composer: "#ffffff",
        "tool-surface": "#f1f5f9",
        selection: "#bfdbfe",
        info: "#2563eb",
      },
      dark: {
        background: "#020617",
        foreground: "#cbd5e1",
        card: "#0f172a",
        "card-foreground": "#f1f5f9",
        popover: "#0f172a",
        "popover-foreground": "#f1f5f9",
        primary: "#3b82f6",
        "primary-foreground": "#eff6ff",
        secondary: "#1e293b",
        "secondary-foreground": "#f1f5f9",
        muted: "#1e293b",
        "muted-foreground": "#94a3b8",
        accent: "#1e3a8a",
        "accent-foreground": "#dbeafe",
        border: "#1e293b",
        input: "#334155",
        ring: "#60a5fa",
        sidebar: "#0f172a",
        "sidebar-foreground": "#f1f5f9",
        "sidebar-primary": "#3b82f6",
        "sidebar-primary-foreground": "#eff6ff",
        "sidebar-accent": "#1e293b",
        "sidebar-accent-foreground": "#dbeafe",
        "sidebar-border": "#1e293b",
        "sidebar-ring": "#60a5fa",
        "title-bar": "#0f172a",
        composer: "#111c31",
        "tool-surface": "#0f172a",
        selection: "#1d4ed8",
        info: "#60a5fa",
      },
    },
  },
  ...ZED_THEMES,
];

export function mergeCssVars(base: ThemeCssVars, overrides: ThemeCssVars): ThemeCssVars {
  return {
    theme: { ...base.theme, ...overrides.theme },
    light: { ...base.light, ...overrides.light },
    dark: { ...base.dark, ...overrides.dark },
  };
}

export function resolvedCssVars(definition: ThemeDefinition): ThemeCssVars {
  return mergeCssVars(PIG_CSS_VARS, definition.cssVars);
}

export function serializedTheme(
  definition: ThemeDefinition,
  stylePacks: readonly ThemeDefinition[] = [],
  fonts: { ui?: string | null; mono?: string | null } = {},
): string {
  const cssVars = stylePacks.reduce(
    (vars, pack) => mergeCssVars(vars, pack.cssVars),
    resolvedCssVars(definition),
  );
  const theme = { ...cssVars.theme };
  if (fonts.ui?.trim()) theme["font-sans"] = fonts.ui.trim();
  if (fonts.mono?.trim()) theme["font-mono"] = fonts.mono.trim();
  return JSON.stringify({ cssVars: { ...cssVars, theme } });
}

export function themePreviewColor(
  definition: ThemeDefinition,
  name: string,
  mode: "light" | "dark",
): string | undefined {
  const vars = resolvedCssVars(definition);
  const value = vars[mode]?.[name] ?? vars.light?.[name] ?? vars.theme?.[name];
  return typeof value === "string" ? value : undefined;
}
