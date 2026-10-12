// Pure odds conversions. The dashboard never fabricates probabilities — these
// only reformat numbers the backend already computed.

export function americanToDecimal(american: number): number {
  return american > 0 ? american / 100 + 1 : 100 / Math.abs(american) + 1;
}

export function americanToImpliedProb(american: number): number {
  return american > 0 ? 100 / (american + 100) : Math.abs(american) / (Math.abs(american) + 100);
}

/** Format American odds with an explicit sign, e.g. +145 / -160. */
export function formatAmerican(american: number | null | undefined): string {
  if (american == null) return "—";
  return american > 0 ? `+${american}` : `${american}`;
}

export function formatPct(p: number | null | undefined, digits = 1): string {
  if (p == null) return "—";
  return `${(p * 100).toFixed(digits)}%`;
}

/** Fractional (default quarter) Kelly stake fraction for a model probability
 *  at American odds. Never negative. */
export function kellyFraction(modelProb: number, american: number, fraction = 0.25): number {
  const b = americanToDecimal(american) - 1;
  if (b <= 0) return 0;
  const f = (b * modelProb - (1 - modelProb)) / b;
  return Math.max(0, f * fraction);
}

export function formatUsd(n: number | null | undefined): string {
  if (n == null) return "—";
  const abs = Math.abs(n);
  const sign = n < 0 ? "-" : "";
  if (abs >= 1e9) return `${sign}$${(abs / 1e9).toFixed(2)}B`;
  if (abs >= 1e6) return `${sign}$${(abs / 1e6).toFixed(2)}M`;
  if (abs >= 1e3) return `${sign}$${(abs / 1e3).toFixed(1)}K`;
  return `${sign}$${abs.toFixed(0)}`;
}
