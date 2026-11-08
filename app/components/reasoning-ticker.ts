export type ReasoningTickerLine = { index: number; text: string };

export const REASONING_TICKER_ROLL_MS = 300;
export const REASONING_TICKER_INTERVAL_MS = 800;
const DRIFT_SKIP_MS = 250;

export function reasoningTickerLine(text: string): ReasoningTickerLine | null {
  const lines = text.split("\n");
  for (let index = lines.length - 1; index >= 0; index -= 1) {
    const value = lines[index].trim().split(/\s+/).join(" ");
    if (value) return { index, text: value };
  }
  return null;
}

export class ReasoningTicker {
  displayed: ReasoningTickerLine | null = null;
  exiting: ReasoningTickerLine | null = null;
  queue: ReasoningTickerLine[] = [];
  rolling = false;
  generation = 0;
  private promotedAt: number | null = null;

  feed(target: ReasoningTickerLine): boolean {
    if (!this.displayed) {
      this.displayed = target;
      return false;
    }
    if (this.displayed.index === target.index) {
      this.displayed = target;
      return false;
    }
    if (this.rolling) {
      const existing = this.queue.findIndex((line) => line.index === target.index);
      if (existing >= 0) this.queue[existing] = target;
      else if (this.queue.length < 2) this.queue.push(target);
      else this.queue[1] = target;
      return false;
    }
    this.promote(target);
    return true;
  }

  fire(generation: number, now: number): boolean {
    if (generation !== this.generation) return false;
    this.exiting = null;
    this.rolling = false;
    if (this.promotedAt !== null && now - this.promotedAt > REASONING_TICKER_INTERVAL_MS + DRIFT_SKIP_MS && this.queue.length > 1) {
      this.queue = this.queue.slice(-1);
    }
    const next = this.queue.shift();
    if (!next) return false;
    this.promote(next);
    return true;
  }

  reset(target: ReasoningTickerLine | null): void {
    this.displayed = target;
    this.exiting = null;
    this.queue = [];
    this.rolling = false;
    this.promotedAt = null;
    this.generation += 1;
  }

  private promote(target: ReasoningTickerLine): void {
    this.exiting = this.displayed;
    this.displayed = target;
    this.rolling = true;
    this.promotedAt = Date.now();
    this.generation += 1;
  }
}
