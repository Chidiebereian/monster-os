// Normal distribution helpers for the projections curve. The backend's
// simulation-engine uses a normal approximation of the posterior, so these
// match how it computes Over/Under probabilities.

export function normalPdf(x: number, mean: number, sd: number): number {
  const z = (x - mean) / sd;
  return Math.exp(-0.5 * z * z) / (sd * Math.sqrt(2 * Math.PI));
}

// Abramowitz & Stegun erf approximation.
function erf(x: number): number {
  const t = 1 / (1 + 0.3275911 * Math.abs(x));
  const y =
    1 -
    ((((1.061405429 * t - 1.453152027) * t + 1.421413741) * t - 0.284496736) * t + 0.254829592) *
      t *
      Math.exp(-x * x);
  return x >= 0 ? y : -y;
}

export function normalCdf(x: number, mean: number, sd: number): number {
  return 0.5 * (1 + erf((x - mean) / (sd * Math.SQRT2)));
}

/** P(value >= line) under Normal(mean, sd). */
export function probOver(line: number, mean: number, sd: number): number {
  return 1 - normalCdf(line, mean, sd);
}
