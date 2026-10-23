// Pure odds conversions. The dashboard never fabricates probabilities — these
// only reformat numbers the backend already computed.

export function americanToDecimal(american: number): number {
  return american > 0 ? american / 100 + 1 : 100 / Math.abs(american) + 1;
}

export function americanToImpliedProb(american: number): number {
  return american > 0 ? 100 / (american + 100) : Math.abs(american) / (Math.abs(american) + 100);
}

/** Implied probability of an American price, or null when it is not a valid
 *  price (|odds| < 100, empty, mid-typing). -113 -> 0.531. */
export function impliedOrNull(american: number | string | null | undefined): number | null {
  const a = parseOdds(american);
  return a == null ? null : americanToImpliedProb(a);
}

/** Two-way no-vig probability of the first side (the bookmaker's margin removed). */
export function noVig(a: number | null | undefined, b: number | null | undefined): number | null {
  const x = impliedOrNull(a), y = impliedOrNull(b);
  return x == null || y == null ? null : x / (x + y);
}

/** Read odds typed in any common format, as American odds:
 *  American "-162.5" / "+150" / "150", decimal "1.62" / "2.5", fractional "8/13" / "11/10", or "evens".
 *  Null when it is not a price yet (|American| < 100, decimal <= 1, half-typed). */
export function parseOdds(text: string | number | null | undefined): number | null {
  if (text == null) return null;
  const t = String(text).trim().toLowerCase();
  if (t === "") return null;
  if (t === "evens" || t === "even" || t === "ev") return 100;
  const frac = t.match(/^(\d+(?:\.\d+)?)\s*[/-]\s*(\d+(?:\.\d+)?)$/);
  if (frac) {
    const [n, d] = [Number(frac[1]), Number(frac[2])];
    if (!(n > 0 && d > 0)) return null;
    return n >= d ? (100 * n) / d : (-100 * d) / n;
  }
  const x = Number(t);
  if (!Number.isFinite(x)) return null;
  if (Math.abs(x) >= 100) return x;                               // American
  if (x > 1 && x < 100 && !t.startsWith("+") && !t.startsWith("-")) // decimal
    return x >= 2 ? (x - 1) * 100 : -100 / (x - 1);
  return null;
}

/** Decimal odds of an American price (2 dp). */
export function decimalOf(american: number): string {
  return americanToDecimal(american).toFixed(2);
}

/** Fractional odds, exact and reduced ("8/13", "11/8", "evens"); null when the
 *  fraction is not a readable one (e.g. -113 = 100/113). */
export function fractionalOf(american: number): string | null {
  // work in halves so -162.5 and +137.5 stay exact
  let [n, d] = american > 0 ? [Math.round(american * 2), 200] : [200, Math.round(-american * 2)];
  const g = (a: number, b: number): number => (b ? g(b, a % b) : a);
  const k = g(n, d);
  n /= k; d /= k;
  if (n === d) return "evens";
  // bookmakers keep some fractions unreduced (4/6 not 2/3, 6/4 not 3/2)
  const book = BOOK_FRACTIONS.find(([bn, bd]) => bn * d === n * bd);
  if (book) return `${book[0]}/${book[1]}`;
  return d <= 50 && n <= 500 ? `${n}/${d}` : null;
}

/** The traditional fractional prices, as bookmakers write them. */
const BOOK_FRACTIONS: [number, number][] = [
  [1, 10], [1, 9], [1, 8], [2, 13], [1, 7], [1, 6], [2, 11], [1, 5], [2, 9], [1, 4], [2, 7], [3, 10], [1, 3], [4, 11], [2, 5], [4, 9], [1, 2],
  [8, 15], [4, 7], [8, 13], [4, 6], [8, 11], [4, 5], [5, 6], [10, 11], [21, 20], [11, 10], [6, 5], [5, 4], [11, 8], [7, 5], [6, 4], [8, 5],
  [13, 8], [7, 4], [9, 5], [15, 8], [2, 1], [85, 40], [11, 5], [9, 4], [12, 5], [5, 2], [13, 5], [11, 4], [3, 1], [10, 3], [7, 2], [4, 1],
  [9, 2], [5, 1], [11, 2], [6, 1], [13, 2], [7, 1], [15, 2], [8, 1], [17, 2], [9, 1], [10, 1],
];

/** One click / arrow key up or down in American odds, skipping the gap between
 *  -100 and +100 (-101 -> +100, +100 -> -101): there are no prices in between. */
export function stepAmerican(american: number, dir: 1 | -1): number {
  const a = Math.round(american);
  if (dir > 0) return a === -101 || a === -100 ? 100 : a + 1;
  return a === 100 || a === -100 ? -101 : a - 1;
}

/** Implied probability as a percent, trailing ".0" dropped ("60%", "61.9%"). */
export function impliedPctText(american: number): string {
  // to 0.1%, an exact half to the even digit (+220 = 31.25% -> 31.2%, as odds tables print it)
  const x = americanToImpliedProb(american) * 1000;
  const r = Math.abs(x - Math.floor(x) - 0.5) < 1e-9 ? (Math.floor(x) % 2 === 0 ? Math.floor(x) : Math.ceil(x)) : Math.round(x);
  const p = r / 10;
  return `${p % 1 === 0 ? p.toFixed(0) : p.toFixed(1)}%`;
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
