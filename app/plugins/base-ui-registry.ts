export interface BaseUiNodeDescriptor {
  uiId: string;
  component: "button" | "input" | "view" | "text" | "list" | "navigation" | "message" | "panel";
  schemaVersion: number;
  protected?: boolean;
  allowedStyles?: readonly string[];
  preservedBehavior: readonly string[];
}

const descriptors = new Map<string, BaseUiNodeDescriptor>();
let baseRevision = 0;

function register(descriptor: BaseUiNodeDescriptor): void {
  if (descriptors.has(descriptor.uiId)) throw new Error(`duplicate base UI id: ${descriptor.uiId}`);
  descriptors.set(descriptor.uiId, Object.freeze(descriptor));
}

register({ uiId: "composer.sendButton", component: "button", schemaVersion: 1, preservedBehavior: ["onPress", "disabled", "accessibilityLabel", "testID"] });
register({ uiId: "composer.root", component: "view", schemaVersion: 1, preservedBehavior: ["children"] });
register({ uiId: "composer.input", component: "input", schemaVersion: 1, preservedBehavior: ["value", "onChange", "onSubmit", "onFocus", "onBlur", "placeholder"] });
register({ uiId: "composer.actions", component: "view", schemaVersion: 1, preservedBehavior: ["children"] });
register({ uiId: "sidebar.root", component: "panel", schemaVersion: 1, preservedBehavior: ["children"] });
register({ uiId: "sidebar.sessionList", component: "list", schemaVersion: 1, preservedBehavior: ["items", "onOpen"] });
register({ uiId: "message.item", component: "message", schemaVersion: 1, preservedBehavior: ["item", "behavior"] });
register({ uiId: "message.markdown", component: "message", schemaVersion: 1, preservedBehavior: ["markdown", "selectable"] });
register({ uiId: "message.turnStats", component: "view", schemaVersion: 1, preservedBehavior: ["children"] });
register({ uiId: "status.activity", component: "view", schemaVersion: 1, preservedBehavior: ["children"] });
register({ uiId: "status.usage", component: "view", schemaVersion: 1, preservedBehavior: ["children"] });
register({ uiId: "status.context", component: "view", schemaVersion: 1, preservedBehavior: ["children"] });
register({ uiId: "settings.navigation", component: "navigation", schemaVersion: 1, preservedBehavior: ["items", "onNavigate"] });
register({ uiId: "settings.content", component: "panel", schemaVersion: 1, preservedBehavior: ["children"] });
register({ uiId: "main.panelHost", component: "panel", schemaVersion: 1, preservedBehavior: ["children", "dismiss"] });
register({ uiId: "main.commandHost", component: "view", schemaVersion: 1, preservedBehavior: ["onCommand"] });
register({ uiId: "plugins.manager", component: "panel", schemaVersion: 1, protected: true, preservedBehavior: ["all"] });
register({ uiId: "approval.root", component: "panel", schemaVersion: 1, protected: true, preservedBehavior: ["all"] });
register({ uiId: "security.root", component: "panel", schemaVersion: 1, protected: true, preservedBehavior: ["all"] });
register({ uiId: "credentials.root", component: "panel", schemaVersion: 1, protected: true, preservedBehavior: ["all"] });

export const BaseUiRegistry = {
  get(uiId: string): BaseUiNodeDescriptor | null {
    return descriptors.get(uiId) ?? null;
  },

  require(uiId: string): BaseUiNodeDescriptor {
    const descriptor = this.get(uiId);
    if (!descriptor) throw new Error(`unregistered base UI id: ${uiId}`);
    return descriptor;
  },

  revision(): number {
    return baseRevision;
  },

  update(next: BaseUiNodeDescriptor): void {
    const previous = this.require(next.uiId);
    if (previous.component !== next.component || previous.protected !== next.protected) {
      throw new Error(`incompatible base UI replacement: ${next.uiId}`);
    }
    descriptors.set(next.uiId, Object.freeze(next));
    baseRevision += 1;
  },

  entries(): BaseUiNodeDescriptor[] {
    return [...descriptors.values()].sort((a, b) => a.uiId.localeCompare(b.uiId));
  },

  canPatch(uiId: string, interaction = false): boolean {
    const descriptor = this.get(uiId);
    return !!descriptor && !descriptor.protected && (!interaction || descriptor.component !== "message");
  },
};

export function protectedUiTarget(target: string): boolean {
  return ["approval.", "security.", "credentials.", "plugins.", "plugin.manager"].some((prefix) => target.startsWith(prefix)) || !!BaseUiRegistry.get(target)?.protected;
}
