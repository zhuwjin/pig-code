import { effect, keymap, signal, Text, View } from "nsh";
import { Button, Checkbox, Input, Radio, RadioGroup } from "nsh/base";
import { t } from "../i18n";
import type { PendingQuestion, QuestionOption } from "../protocol";
import type { UiContext } from "../ui-context";
import { Kbd } from "./kbd";

function answered(selected: string[], other: string): boolean {
  return selected.length > 0 || other.trim().length > 0;
}

function QuestionOptionContent(props: { option: () => QuestionOption; index: () => number }) {
  return (
    <>
      <View class="min-w-0 flex-1 gap-0.5">
        <Text class="text-sm text-foreground">{props.option().label}</Text>
        <If when={props.option().description}>
          <Text class="text-xs leading-5 text-muted-foreground">{props.option().description}</Text>
        </If>
      </View>
      <Kbd>{props.index() + 1}</Kbd>
    </>
  );
}

const optionRowClass = "group flex-row items-center gap-3 rounded-(--radius) border border-border bg-background/60 px-3 py-2 hover:bg-accent/60 checked:border-primary checked:bg-primary/10";

export function QuestionCard(props: UiContext & { question: PendingQuestion }) {
  const page = signal(0);
  const selections = signal<string[][]>([]);
  const others = signal<string[]>([]);
  const otherHeights = signal<number[]>([]);
  effect(() => {
    props.question.requestId;
    page.set(0);
    selections.set([]);
    others.set([]);
    otherHeights.set([]);
  });
  const current = () => props.question.questions[
    Math.min(page(), Math.max(0, props.question.questions.length - 1))
  ];
  const options = () => {
    const question = current();
    if (!question) return [];
    const seen = new Set<string>();
    return question.options.filter((option) => {
      if (seen.has(option.label)) return false;
      seen.add(option.label);
      return true;
    });
  };
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
  const toggleAt = (index: number) => {
    const option = options()[index];
    if (option) toggle(option.label);
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
  const advance = () => {
    if (!canContinue()) return;
    if (page() + 1 < props.question.questions.length) page.update((value) => value + 1);
    else submit();
  };
  effect(() => keymap.bind("escape", () => props.store.answerQuestion(null), { when: "always" }));
  effect(() => keymap.bind({
    "1": () => toggleAt(0),
    "2": () => toggleAt(1),
    "3": () => toggleAt(2),
    "4": () => toggleAt(3),
    enter: advance,
  }, { when: "notEditing" }));
  return (
    <View
      testID="question-card"
      class={props.themeStore.classes("question.root", "gap-3 p-4")}
    >
      <If when={current()}>
        {(value) => {
          const question = () => value();
          return (
            <View class="gap-3">
              <View class="gap-1">
                <Text class="text-xs text-muted-foreground">
                  {t("question.progress", { current: page() + 1, total: props.question.questions.length })}
                  {question().header ? ` · ${question().header}` : ""}
                </Text>
                <Text class="text-sm font-semibold text-foreground">{question().question}</Text>
                <If when={question().multi_select}>
                  <Text class="text-xs text-muted-foreground">{t("question.multi_hint")}</Text>
                </If>
              </View>
              <If when={question().multi_select}>
                <View class="gap-2">
                  <Each of={options()} key={(option) => option.label}>
                    {(option, index) => (
                      <Checkbox
                        testID={`question-option-${page()}-${index()}`}
                        checked={selected().includes(option().label)}
                        class={optionRowClass}
                        indicatorClass="group size-4 shrink-0 items-center justify-center rounded border border-muted-foreground checked:border-primary checked:bg-primary"
                        indicator={<Text class="hidden text-[10px] leading-3 text-primary-foreground group-checked:flex">✓</Text>}
                        onChange={() => toggle(option().label)}
                      >
                        <QuestionOptionContent option={option} index={index} />
                      </Checkbox>
                    )}
                  </Each>
                </View>
              </If>
              <If when={!question().multi_select}>
                <RadioGroup class="gap-2">
                  <Each of={options()} key={(option) => option.label}>
                    {(option, index) => (
                      <Radio
                        testID={`question-option-${page()}-${index()}`}
                        checked={selected().includes(option().label)}
                        class={optionRowClass}
                        onChange={() => toggle(option().label)}
                      >
                        <View
                          class="shrink-0 border-muted-foreground group-checked:border-primary"
                          style={{
                            width: 14,
                            height: 14,
                            borderRadius: 7,
                            borderWidth: 1,
                            alignItems: "center",
                            justifyContent: "center",
                          }}
                        >
                          <View
                            class="opacity-0 scale-50 transition duration-150 ease-out motion-reduce:transition-none group-checked:opacity-100 group-checked:scale-100 group-checked:bg-primary"
                            style={{ width: 6, height: 6, borderRadius: 3 }}
                          />
                        </View>
                        <QuestionOptionContent option={option} index={index} />
                      </Radio>
                    )}
                  </Each>
                </RadioGroup>
              </If>
              <Input
                testID={`question-other-${page()}`}
                multiline
                value={other()}
                placeholder={t("question.other_placeholder")}
                onChange={({ value: text }) => {
                  const index = page();
                  others.update((all) => {
                    const copy = [...all];
                    copy[index] = text;
                    return copy;
                  });
                }}
                onContentSizeChange={({ size }) => {
                  const index = page();
                  const height = Math.min(76, Math.max(40, size.height + 16));
                  otherHeights.update((all) => {
                    const copy = [...all];
                    copy[index] = height;
                    return copy;
                  });
                }}
                class="rounded-(--radius) border border-border bg-background px-3 py-2 selection:bg-foreground text-sm leading-5 text-foreground placeholder:text-muted-foreground"
                style={{ height: otherHeights()[page()] ?? 40 }}
              />
            </View>
          );
        }}
      </If>
      <View class="flex-row items-center gap-2">
        <Button
          testID="question-skip"
          class="flex-row items-center gap-2 rounded-(--radius) border border-border px-3 py-2 hover:bg-muted"
          onPress={() => props.store.answerQuestion(null)}
        >
          <Text class="text-sm text-muted-foreground">{t("question.skip")}</Text>
          <Kbd>Esc</Kbd>
        </Button>
        <View class="flex-1" />
        <If when={page() > 0}>
          <Button class="rounded-(--radius) border border-border px-3 py-2" onPress={() => page.update((value) => value - 1)}>
            <Text class="text-sm text-foreground">{t("question.previous")}</Text>
          </Button>
        </If>
        <Button
          testID="question-next"
          disabled={!canContinue()}
          class="flex-row items-center gap-2 rounded-(--radius) bg-primary px-4 py-2 disabled:opacity-40"
          onPress={advance}
        >
          <Text class="text-sm text-primary-foreground">
            {t(page() + 1 < props.question.questions.length ? "question.next" : "question.submit")}
          </Text>
          <Kbd
            variant="raised"
            class="border-primary-foreground/30 bg-primary-foreground/10 text-primary-foreground"
            accessibilityLabel="Enter"
          >
            ↵
          </Kbd>
        </Button>
      </View>
    </View>
  );
}
