export type ClockTimer = unknown;

/** The timer/time source used by extension services. */
export interface Clock {
  now(): number;
  setTimeout(callback: () => void, delayMs: number): ClockTimer;
  clearTimeout(timer: ClockTimer): void;
  setInterval(callback: () => void, intervalMs: number): ClockTimer;
  clearInterval(timer: ClockTimer): void;
  unref?(timer: ClockTimer): void;
  sleep(delayMs: number): Promise<void>;
}

export const realClock: Clock = {
  now: () => Date.now(),
  setTimeout: (callback, delayMs) => globalThis.setTimeout(callback, delayMs),
  clearTimeout: (timer) => globalThis.clearTimeout(timer as ReturnType<typeof setTimeout>),
  setInterval: (callback, intervalMs) => globalThis.setInterval(callback, intervalMs),
  clearInterval: (timer) => globalThis.clearInterval(timer as ReturnType<typeof setInterval>),
  unref: (timer) => (timer as { unref?: () => void }).unref?.(),
  sleep: (delayMs) => new Promise((resolve) => globalThis.setTimeout(resolve, delayMs)),
};

type ScheduledTimer = {
  readonly id: number;
  readonly order: number;
  due: number;
  readonly interval: number | null;
  readonly callback: () => void;
};

/** Deterministic scheduler. Advancing runs every due callback before returning. */
export class ManualClock implements Clock {
  private currentTime: number;
  private nextId = 1;
  private readonly timers = new Map<number, ScheduledTimer>();

  constructor(startAt = 0) {
    this.currentTime = startAt;
  }

  now(): number {
    return this.currentTime;
  }

  /** Timers still scheduled: lets tests assert that settle left nothing armed. */
  get pendingTimers(): number {
    return this.timers.size;
  }

  setTimeout(callback: () => void, delayMs: number): ClockTimer {
    return this.schedule(callback, delayMs, null);
  }

  clearTimeout(timer: ClockTimer): void {
    this.cancel(timer);
  }

  setInterval(callback: () => void, intervalMs: number): ClockTimer {
    return this.schedule(callback, Math.max(1, intervalMs), Math.max(1, intervalMs));
  }

  clearInterval(timer: ClockTimer): void {
    this.cancel(timer);
  }

  sleep(delayMs: number): Promise<void> {
    return new Promise((resolve) => this.setTimeout(resolve, delayMs));
  }

  advanceBy(deltaMs: number): void {
    if (!Number.isFinite(deltaMs) || deltaMs < 0) {
      throw new RangeError("ManualClock.advanceBy requires a finite non-negative duration");
    }
    this.advanceTo(this.currentTime + deltaMs);
  }

  advanceTo(targetTime: number): void {
    if (!Number.isFinite(targetTime) || targetTime < this.currentTime) {
      throw new RangeError("ManualClock.advanceTo cannot move backwards or to a non-finite time");
    }

    while (true) {
      const next = [...this.timers.values()]
        .filter((timer) => timer.due <= targetTime)
        .sort((a, b) => a.due - b.due || a.order - b.order)[0];
      if (!next) break;

      // It may have been cancelled by an earlier callback since selection.
      if (!this.timers.has(next.id)) continue;
      this.currentTime = next.due;
      if (next.interval === null) this.timers.delete(next.id);
      else next.due += next.interval;
      next.callback();
    }

    this.currentTime = targetTime;
  }

  private schedule(callback: () => void, delayMs: number, interval: number | null): ClockTimer {
    const delay = Number.isFinite(delayMs) ? Math.max(0, delayMs) : 0;
    const id = this.nextId++;
    this.timers.set(id, {
      id,
      order: id,
      due: this.currentTime + delay,
      interval,
      callback,
    });
    return id;
  }

  private cancel(timer: ClockTimer): void {
    if (typeof timer === "number") this.timers.delete(timer);
  }
}

/** Owns timer handles for one lifecycle scope and cancels them together. */
export class TimerScope {
  private readonly owned = new Map<ClockTimer, () => void>();
  private disposed = false;

  constructor(private readonly clock: Clock) {}

  get size(): number {
    return this.owned.size;
  }

  setTimeout(callback: () => void, delayMs: number): ClockTimer {
    this.assertActive();
    let timer: ClockTimer;
    timer = this.clock.setTimeout(() => {
      this.owned.delete(timer);
      if (!this.disposed) callback();
    }, delayMs);
    this.owned.set(timer, () => this.clock.clearTimeout(timer));
    return timer;
  }

  clearTimeout(timer: ClockTimer): void {
    this.clear(timer);
  }

  setInterval(callback: () => void, intervalMs: number): ClockTimer {
    this.assertActive();
    const timer = this.clock.setInterval(() => {
      if (!this.disposed) callback();
    }, intervalMs);
    this.owned.set(timer, () => this.clock.clearInterval(timer));
    return timer;
  }

  clearInterval(timer: ClockTimer): void {
    this.clear(timer);
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    for (const clear of this.owned.values()) clear();
    this.owned.clear();
  }

  private clear(timer: ClockTimer): void {
    const cancel = this.owned.get(timer);
    if (!cancel) return;
    cancel();
    this.owned.delete(timer);
  }

  private assertActive(): void {
    if (this.disposed) throw new Error("TimerScope is disposed");
  }
}
