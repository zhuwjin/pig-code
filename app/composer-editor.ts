import {
  baseKeymap,
  command,
  createRichEditor,
  defineExtension,
  Document,
  history,
  insertContentAt,
  insertMention,
  keymap,
  Paragraph,
  RichEditorPlugin,
  RichEditorSession,
  RichNodeJSON,
  RichRange,
  richExtension,
  suggestion,
} from "nsh/base";
import { t } from "./i18n";

export type ComposerMentionKind = "file" | "image" | "command";

function spaceNode(): RichNodeJSON {
  return { type: "space", attrs: { id: "", label: "" } };
}

export function createComposerEditor(
  onSubmit: (opposite: boolean) => void,
  onSearch: () => void,
): RichEditorSession {
  const submit = new RichEditorPlugin("composer-submit").captureKeys(
    ["Enter", "Mod-Enter", "Ctrl-Enter", "Cmd-Enter"],
    (key) => onSubmit(key !== "Enter"),
  );
  const search = new RichEditorPlugin("composer-search").captureKeys(
    ["Mod-f", "Ctrl-f", "Cmd-f"],
    onSearch,
  );
  submit.capturing = true;
  search.capturing = true;
  const keys = defineExtension({
    name: "composerKeys",
    priority: 1000,
    addProseMirrorPlugins: () => [submit, search],
  });
  return createRichEditor({
    extensions: [Document, Paragraph, richExtension("text"), richExtension("composerChips"), keys],
    plugins: [
      keymap({ "Shift-Enter": command("splitBlock") }),
      suggestion({ key: "mention", char: "@" }),
      suggestion({ key: "slash", char: "/", startOfLine: true }),
      baseKeymap(),
      history(),
    ],
  });
}

function composerInlineSize(node: RichNodeJSON): number {
  if (node.type === "text") return node.text?.length ?? 0;
  if (!node.content) return 1;
  return 2 + node.content.reduce((size, child) => size + composerInlineSize(child), 0);
}

export function measureComposerEditorHeight(editor: RichEditorSession): number | null {
  let position = 0;
  let height = 8;
  for (const block of editor.doc.content ?? []) {
    const rect = editor.pageNodeRect(position) as { height: number } | null;
    if (!rect) return null;
    height += rect.height;
    position += 2 + (block.content ?? []).reduce((size, node) => size + composerInlineSize(node), 0);
  }
  return height;
}

export function measureComposerEditorLineHeight(editor: RichEditorSession): number | null {
  const selection = editor.selection;
  const rect = editor.pageCoordsAtPos(selection.head ?? selection.anchor ?? 0) as { height: number } | null;
  return rect?.height ?? null;
}

export function insertComposerMention(
  editor: RichEditorSession,
  kind: ComposerMentionKind,
  id: string,
  label: string,
  range?: RichRange,
): void {
  const content: RichNodeJSON[] = [
    { type: kind, attrs: { id, label } },
    spaceNode(),
  ];
  if (range) editor.run(insertMention(range, id, label, kind));
  else editor.insertContent(content);
}

export interface ComposerDocumentValue {
  text: string;
  files: string[];
  current: boolean;
}

export function readComposerDocument(
  doc: RichNodeJSON,
  images: readonly { id: string }[],
): ComposerDocumentValue {
  const files: string[] = [];
  let current = true;
  const readInline = (node: RichNodeJSON): string => {
    if (node.type === "space") return " ";
    if (node.type === "text") return node.text ?? "";
    const id = typeof node.attrs?.id === "string" ? node.attrs.id : "";
    if (node.type === "file") {
      if (!id) current = false;
      else {
        files.push(id);
        return `@${id}`;
      }
      return "";
    }
    if (node.type === "image") {
      const index = images.findIndex((image) => image.id === id);
      if (index < 0) {
        current = false;
        return "";
      }
      const label = t("messages.image_label", { index: index + 1 });
      if (node.attrs?.label !== label) current = false;
      return `Image ${index + 1}`;
    }
    if (node.type === "command") {
      if (!id) current = false;
      return id;
    }
    return (node.content ?? []).map(readInline).join("");
  };
  const text = (doc.content ?? []).map((block) => (block.content ?? []).map(readInline).join("")).join("\n");
  return { text, files, current };
}

interface ComposerToken {
  text: string;
  node: RichNodeJSON;
}

function inlineComposerNodes(text: string, tokens: ComposerToken[]): RichNodeJSON[] {
  const nodes: RichNodeJSON[] = [];
  let offset = 0;
  while (offset < text.length) {
    let found: { index: number; token: ComposerToken } | null = null;
    for (const token of tokens) {
      const index = text.indexOf(token.text, offset);
      if (index < 0) continue;
      if (!found || index < found.index || (index === found.index && token.text.length > found.token.text.length)) {
        found = { index, token };
      }
    }
    if (!found) {
      nodes.push({ type: "text", text: text.slice(offset) });
      break;
    }
    if (found.index > offset) nodes.push({ type: "text", text: text.slice(offset, found.index) });
    nodes.push({
      ...found.token.node,
      attrs: found.token.node.attrs ? { ...found.token.node.attrs } : undefined,
    });
    offset = found.index + found.token.text.length;
  }
  return nodes;
}

export function buildComposerDocument(
  text: string,
  files: readonly string[],
  images: readonly { id: string }[],
): RichNodeJSON {
  const tokens: ComposerToken[] = [
    ...files.map((path) => ({
      text: `@${path}`,
      node: {
        type: "file",
        attrs: { id: path, label: path.split(/[\\/]/).pop() ?? path },
      },
    })),
    ...images.map((image, index) => ({
      text: `Image ${index + 1}`,
      node: {
        type: "image",
        attrs: { id: image.id, label: t("messages.image_label", { index: index + 1 }) },
      },
    })),
    ...["/clear", "/compact", "/plan"].map((id) => ({
      text: id,
      node: { type: "command", attrs: { id, label: t(`commands.${id.slice(1)}_label` as "commands.clear_label" | "commands.compact_label" | "commands.plan_label") } },
    })),
  ];
  return {
    type: "doc",
    content: text.split("\n").map((line) => {
      const content = inlineComposerNodes(line, tokens);
      return { type: "paragraph", ...(content.length > 0 ? { content } : {}) };
    }),
  };
}
