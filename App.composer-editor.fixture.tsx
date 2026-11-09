import { mount, signal, Text, View } from "nsh";
import { Button, RichEditor } from "nsh/base";
import { createComposerEditor, insertComposerMention, measureComposerEditorHeight, measureComposerEditorLineHeight, readComposerDocument } from "./app/composer-editor";

const editor = createComposerEditor(() => {}, () => {});
const report = signal("");
const block = signal("");
const editorHeight = signal(40);
const actualEditorHeight = signal(0);
let editorResizeFrame: number | undefined;
let editorWidth = 0;
let editorLineHeight = 0;
function scheduleEditorResize() {
  if (editorResizeFrame !== undefined) cancelAnimationFrame(editorResizeFrame);
  editorResizeFrame = requestAnimationFrame(() => {
    editorResizeFrame = undefined;
    const measured = measureComposerEditorHeight(editor);
    const lineHeight = measureComposerEditorLineHeight(editor);
    if (lineHeight !== null) editorLineHeight = lineHeight;
    const maxHeight = editorLineHeight > 0 ? 8 + editorLineHeight * 10 : 208;
    if (measured !== null) editorHeight.set(Math.max(40, Math.min(maxHeight, measured)));
  });
}
editor.on("revision", scheduleEditorResize);
function measure() {
  const selection = editor.selection;
  report.set(JSON.stringify({
    position: selection.head ?? selection.anchor ?? 0,
    selection: selection.type,
    types: (editor.doc.content?.[0].content ?? []).map((node) => node.type),
    text: editor.getText(),
    model: readComposerDocument(editor.doc, [{ id: "image-1" }]).text,
  }));
  block.set(JSON.stringify(editor.pageNodeRect(0)));
  editor.focus();
}

mount(() => (
  <View class="w-full bg-background p-4">
    <Button testID="insert" onPress={() => {
      insertComposerMention(editor, "image", "image-1", "图片 1");
      editor.focus();
    }}><Text>Insert</Text></Button>
    <Button testID="measure" onPress={measure}><Text>Measure</Text></Button>
    <RichEditor
      testID="rich"
      editor={editor}
      onLayout={({ bounds }) => {
        actualEditorHeight.set(bounds.size.height);
        if (editorWidth === bounds.size.width) return;
        editorWidth = bounds.size.width;
        scheduleEditorResize();
      }}
      style={{ height: editorHeight() }}
      classes={{
        content: "min-h-10 p-1 text-sm text-foreground",
        caret: "bg-foreground",
        nodes: { file: "h-[18px] align-text-bottom", image: "h-[18px] align-text-bottom", command: "h-[18px] align-text-bottom", space: "px-0.5" },
      }}
      class="min-h-10"
    />
    <Text testID="editor-height">{`editor-height:${actualEditorHeight()}`}</Text>
    <Text testID="report">{report()}</Text>
    <Text testID="block">{block()}</Text>
  </View>
));
