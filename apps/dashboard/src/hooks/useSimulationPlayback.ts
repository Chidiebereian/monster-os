import { useCallback, useEffect, useRef, useState } from "react";

// Playback over an authoritative event log. Pacing only: play / pause / step /
// scrub change WHICH event is shown, never what happened. `seq` 0 = pregame.

const BASE_MS: Record<string, number> = {
  pass_complete: 2600, pass_incomplete: 2100, rush: 2300, scramble: 2300, sack: 2100, interception: 2800, fumble_lost: 2800,
  punt: 2600, kickoff: 2300, field_goal: 2500, extra_point: 1500, two_point: 1800, penalty: 1700, kneel: 1400,
  turnover_on_downs: 1500, two_minute_warning: 1500, end_period: 1900, safety: 2400, game_end: 1000,
  // MLB
  pa: 2300, pitching_change: 1800, inning_end: 1700,
};

/** Any authoritative event log entry (NFL or MLB). */
export interface PlaybackEvent { seq: number; kind: string; points?: number; runs?: number }

export const SPEEDS = [0.5, 1, 2, 4, 10] as const;

export function useSimulationPlayback<E extends PlaybackEvent>(events: E[]) {
  const [seq, setSeq] = useState(0);
  const [playing, setPlaying] = useState(false);
  const [speed, setSpeed] = useState<number>(1);
  const timer = useRef<number | null>(null);
  const total = events.length;

  // new log -> back to pregame
  useEffect(() => {
    setSeq(0);
    setPlaying(false);
  }, [events]);

  useEffect(() => {
    if (timer.current) window.clearTimeout(timer.current);
    if (!playing) return;
    if (seq >= total) {
      setPlaying(false);
      return;
    }
    const nextKind = events[seq]?.kind ?? "rush";
    const current = seq > 0 ? events[seq - 1] : undefined;
    // hold a touchdown / score a little longer before the next snap
    const hold = current && ((current.points ?? 0) >= 6 || (current.runs ?? 0) > 0) ? 1.6 : 1;
    const delay = ((BASE_MS[nextKind] ?? 2000) * hold) / speed;
    timer.current = window.setTimeout(() => setSeq((s) => Math.min(total, s + 1)), seq === 0 ? 400 : delay);
    return () => {
      if (timer.current) window.clearTimeout(timer.current);
    };
  }, [playing, seq, speed, total, events]);

  const step = useCallback((d: number) => {
    setPlaying(false);
    setSeq((s) => Math.max(0, Math.min(total, s + d)));
  }, [total]);
  const jump = useCallback((s: number) => {
    setSeq(Math.max(0, Math.min(total, s)));
  }, [total]);
  const toggle = useCallback(() => {
    setPlaying((p) => {
      if (!p && seq >= total) setSeq(0);
      return !p;
    });
  }, [seq, total]);
  const restart = useCallback(() => {
    setSeq(0);
    setPlaying(true);
  }, []);

  return {
    seq,
    total,
    playing,
    speed,
    setSpeed,
    toggle,
    step,
    jump,
    restart,
    pause: () => setPlaying(false),
    current: seq > 0 ? events[seq - 1] : undefined,
    done: total > 0 && seq >= total,
  };
}
