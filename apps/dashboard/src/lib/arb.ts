// Dutching / arbitrage stake allocation (client-side convenience math; the
// cross-venue scanner's edges come from the backend). Exact formula per the
// MonsterOS directive.
import { americanToDecimal, americanToImpliedProb } from "./odds";

export interface ArbResult {
  decA: number;
  decB: number;
  impliedSum: number; // < 1 means a guaranteed arb exists
  stakeA: number;
  stakeB: number;
  profitMin: number;
  profitMax: number;
  roiPct: number;
}

export function calculateArbitrage(americanA: number, americanB: number, bankroll = 100): ArbResult {
  const decA = americanToDecimal(americanA);
  const decB = americanToDecimal(americanB);
  const invA = 1 / decA;
  const invB = 1 / decB;
  const totalInv = invA + invB;

  const stakeA = Math.round(((bankroll * invA) / totalInv) * 100) / 100;
  const stakeB = Math.round((bankroll - stakeA) * 100) / 100;
  const profitA = stakeA * decA - bankroll;
  const profitB = stakeB * decB - bankroll;

  return {
    decA,
    decB,
    impliedSum: americanToImpliedProb(americanA) + americanToImpliedProb(americanB),
    stakeA,
    stakeB,
    profitMin: Math.min(profitA, profitB),
    profitMax: Math.max(profitA, profitB),
    roiPct: (Math.min(profitA, profitB) / bankroll) * 100,
  };
}
