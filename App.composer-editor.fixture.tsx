import { mount, signal, Text, View } from "nsh";
import { Button, RichEditor } from "nsh/base";
import { createComposerEditor, insertComposerMention, readComposerDocument } from "./app/composer-editor";

const editor = createComposerEditor(() => {}, () => {});
const report = signal("");
const block = signal("");
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
      classes={{
        content: "min-h-10 p-1 text-sm text-foreground",
        caret: "bg-foreground",
        nodes: { file: "h-[18px] align-text-bottom", image: "h-[18px] align-text-bottom", command: "h-[18px] align-text-bottom", space: "px-0.5" },
      }}
      class="min-h-10"
    />
    <Text testID="report">{report()}</Text>
    <Text testID="block">{block()}</Text>
  </View>
));
