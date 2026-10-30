import { setColorScheme, signal } from "nsh";
import {
  BUILTIN_THEMES,
  DEFAULT_THEME_ID,
  PLUGIN_THEME_SLOTS,
  PluginThemeDefinition,
  ThemeBackground,
  ThemeDefinition,
  ThemeMode,
  serializedTheme,
} from "./themes";

export class ThemeStore {
  readonly catalog = signal<ThemeDefinition[]>([...BUILTIN_THEMES]);
  readonly activeId = signal(DEFAULT_THEME_ID);
  readonly mode = signal<ThemeMode>("system");
  private hydrated = false;
  private userModified = false;

  active(): ThemeDefinition {
    return (
      this.selectableThemes().find((theme) => theme.id === this.activeId()) ??
      this.selectableThemes()[0] ??
      BUILTIN_THEMES[0]
    );
  }

  selectableThemes(): ThemeDefinition[] {
    return this.catalog().filter((theme) => theme.kind !== "style");
  }

  activeStylePacks(): ThemeDefinition[] {
    const activeId = this.active().id;
    return this.catalog().filter(
      (theme) =>
        theme.kind === "style" &&
        (theme.targetThemes?.includes("*") || theme.targetThemes?.includes(activeId)),
    );
  }

  serialized(): string {
    return serializedTheme(this.active(), this.activeStylePacks());
  }

  classes(slot: string, base = ""): string {
    this.assertSlot(slot);
    return [
      base,
      ...this.activeLayers().map((theme) => theme.parts?.[slot]?.class ?? ""),
    ]
      .filter(Boolean)
      .join(" ");
  }

  styles(slot: string, base: Record<string, any> = {}): Record<string, any> {
    this.assertSlot(slot);
    return Object.assign(
      base,
      ...this.activeLayers().map((theme) => theme.parts?.[slot]?.style ?? {}),
    );
  }

  background(): ThemeBackground | null {
    let background: ThemeBackground | null = null;
    for (const theme of this.activeLayers()) {
      if (theme.background !== undefined) background = theme.background;
    }
    return background;
  }

  hydrate(themeId: string | null | undefined, mode: ThemeMode | null | undefined): void {
    if (this.hydrated || this.userModified) {
      this.hydrated = true;
      return;
    }
    this.hydrated = true;
    if (themeId) this.activeId.set(themeId);
    this.setMode(mode === "light" || mode === "dark" ? mode : "system");
  }

  select(themeId: string): void {
    if (this.selectableThemes().some((theme) => theme.id === themeId)) {
      this.userModified = true;
      this.activeId.set(themeId);
    }
  }

  setMode(mode: ThemeMode): void {
    this.userModified = true;
    this.mode.set(mode);
    setColorScheme(mode);
  }

  register(definitions: PluginThemeDefinition[]): void {
    this.assertDefinitions(definitions);
    this.catalog.update((current) => {
      const themes = new Map(current.map((theme) => [theme.id, theme]));
      for (const definition of definitions) {
        themes.set(definition.id, { ...definition, source: "plugin" });
      }
      return [...themes.values()];
    });
  }

  syncPlugins(definitions: PluginThemeDefinition[]): void {
    this.assertDefinitions(definitions);
    this.catalog.update((current) => [
      ...current.filter((theme) => theme.source === "builtin"),
      ...definitions.map((definition) => ({ ...definition, source: "plugin" as const })),
    ]);
  }

  private activeLayers(): ThemeDefinition[] {
    return [this.active(), ...this.activeStylePacks()];
  }

  private assertSlot(slot: string): void {
    if (!PLUGIN_THEME_SLOTS.has(slot)) throw new Error(`Unknown theme slot: ${slot}`);
  }

  private assertDefinitions(definitions: PluginThemeDefinition[]): void {
    for (const definition of definitions) {
      if (!definition.id.trim() || !definition.name.trim()) {
        throw new Error("Theme id and name must not be empty");
      }
      if (BUILTIN_THEMES.some((theme) => theme.id === definition.id)) {
        throw new Error(`Plugin theme id conflicts with a built-in theme: ${definition.id}`);
      }
      if (definition.kind === "style" && !definition.targetThemes?.length) {
        throw new Error(`Style pack ${definition.id} must declare targetThemes`);
      }
      if (definition.kind !== "style" && definition.targetThemes?.length) {
        throw new Error(`Selectable theme ${definition.id} cannot declare targetThemes`);
      }
      for (const slot of Object.keys(definition.parts ?? {})) this.assertSlot(slot);
      const background = definition.background;
      if (background?.opacity !== undefined && (background.opacity < 0 || background.opacity > 1)) {
        throw new Error(`Theme ${definition.id} background opacity must be between 0 and 1`);
      }
      if (background?.blur !== undefined && (background.blur < 0 || background.blur > 64)) {
        throw new Error(`Theme ${definition.id} background blur must be between 0 and 64`);
      }
    }
  }
}
