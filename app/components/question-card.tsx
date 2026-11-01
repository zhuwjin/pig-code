import { effect, keymap, signal, Text, View } from "nsh";
import { Button, Input } from "nsh/base";
import type { PendingQuestion, QuestionItem } from "../protocol";
import type { UiContext } from "../ui-context";

function answered(selected: string[], other: string): boolean {
  return selected.length > 0 || other.trim().length > 0;
}

export function QuestionCard(props: UiContext & { question: PendingQuestion }) {
  const page = signal(0);
  const selections = signal<string[][]>([]);
  const others = signal<string[]>([]);
  effect(() => {
    props.question.requestId;
    page.set(0);
    selections.set([]);
    others.set([]);
  });
  const current = () => props.question.questions[
    Math.min(page(), Math.max(0, props.question.questions.length - 1))
  ];
  const selected = () => selections()[page()] ?? [];
  const other = () => others()[page()] ?? "";
  const canContinue = () => current() ? answered(selected(), other()) : false;
  const toggle = (label: string) => {
    const index = page();
    const question = current();
    if (!question) return;
    const previous = selections()[index] ?? [];
    const next = question.multi_select
      ? previous.includes(label)
        ? previous.filter((item) => item !== label)
        : [...previous, label]
      : [label];
    selections.update((all) => {
      const copy = [...all];
      copy[index] = next;
      return copy;
    });
  };
  const submit = () => {
    const answers = props.question.questions.map((_, index) => {
      const labels = [...(selections()[index] ?? [])];
      const custom = (others()[index] ?? "").trim();
      if (custom) labels.push(custom);
      return labels;
    });
    props.store.answerQuestion(answers);
  };
  effect(() => keymap.bind("escape", () => props.store.answerQuestion(null), { when: "always" }));
  return (
    <View
      testID="question-card"
      class={props.themeStore.classes("question.root", "gap-3 rounded-xl border border-info/40 bg-info/10 p-4")}
    >
      <If when={current()}>
        {(value) => {
          const question = () => value();
          const options = () => {
            const seen = new Set<string>();
            return question().options.filter((option) => {
              if (seen.has(option.label)) return false;
              seen.add(option.label);
              return true;
            });
          };
          return (
            <View class="gap-3">
              <View class="gap-1">
                <Text class="text-xs text-muted-foreground">
                  问题 {page() + 1} / {props.question.questions.length}
                  {question().header ? ` · ${question().header}` : ""}
                </Text>
                <Text class="text-sm font-semibold text-foreground">{question().question}</Text>
                <If when={question().multi_select}>
                  <Text class="text-xs text-muted-foreground">可选择多项，也可以填写其他回答。</Text>
                </If>
              </View>
              <View class="gap-2">
                <Each of={options()} key={(option) => option.label}>
                  {(option, index) => (
                    <Button
                      testID={`question-option-${page()}-${index()}`}
                      selected={selected().includes(option().label)}
                      class="flex-row items-center gap-3 rounded-(--radius) border border-border bg-background/60 px-3 py-2 hover:bg-accent/60 selected:border-primary selected:bg-primary/10"
                      onPress={() => toggle(option().label)}
                    >
                      <View class="min-w-0 flex-1 gap-0.5">
                        <Text class="text-sm text-foreground">{option().label}</Text>
                        <If when={option().description}>
                          <Text class="text-xs leading-5 text-muted-foreground">{option().description}</Text>
                        </If>
                      </View>
                      <If when={selected().includes(option().label)}>
                        <Text class="text-sm text-primary">✓</Text>
                      </If>
                    </Button>
                  )}
                </Each>
              </View>
              <Input
                testID={`question-other-${page()}`}
                value={other()}
                placeholder="其他回答（可选）"
                onChange={(text) => {
                  const index = page();
                  others.update((all) => {
                    const copy = [...all];
                    copy[index] = text;
                    return copy;
                  });
                }}
                class="rounded-(--radius) border border-border bg-background px-3 py-2 selection:bg-foreground text-sm text-foreground placeholder:text-muted-foreground"
              />
            </View>
          );
        }}
      </If>
      <View class="flex-row items-center gap-2">
        <Button
          testID="question-skip"
          class="rounded-(--radius) px-3 py-2 hover:bg-muted"
          onPress={() => props.store.answerQuestion(null)}
        >
          <Text class="text-sm text-muted-foreground">跳过 Esc</Text>
        </Button>
        <View class="flex-1" />
        <If when={page() > 0}>
          <Button class="rounded-(--radius) border border-border px-3 py-2" onPress={() => page.update((value) => value - 1)}>
            <Text class="text-sm text-foreground">上一题</Text>
          </Button>
        </If>
        <Button
          testID="question-next"
          disabled={!canContinue()}
          class="rounded-(--radius) bg-primary px-4 py-2 disabled:opacity-40"
          onPress={() => {
            if (page() + 1 < props.question.questions.length) page.update((value) => value + 1);
            else submit();
          }}
        >
          <Text class="text-sm text-primary-foreground">
            {page() + 1 < props.question.questions.length ? "下一题" : "提交"}
          </Text>
        </Button>
      </View>
    </View>
  );
}
