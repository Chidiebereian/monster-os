import { create } from "zustand";
import { persist } from "zustand/middleware";
import type { SportKey } from "../types/monster-os";

// Sports MonsterOS covers (Odds-API key + Kalshi game series + label).
export const SPORTS: SportKey[] = [
  { key: "basketball_nba", series: "KXNBAGAME", label: "NBA" },
  { key: "baseball_mlb", series: "KXMLBGAME", label: "MLB" },
  { key: "icehockey_nhl", series: "KXNHLGAME", label: "NHL" },
  { key: "americanfootball_nfl", series: "KXNFLGAME", label: "NFL" },
];

interface SettingsState {
  sportKey: string;
  bankroll: number;
  setSport: (key: string) => void;
  setBankroll: (n: number) => void;
}

export const useSettings = create<SettingsState>()(
  persist(
    (set) => ({
      sportKey: "basketball_nba",
      bankroll: 100,
      setSport: (sportKey) => set({ sportKey }),
      setBankroll: (bankroll) => set({ bankroll }),
    }),
    { name: "monsteros-settings" },
  ),
);
