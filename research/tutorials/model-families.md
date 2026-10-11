# MonsterOS model families

A short reference to the model families MonsterOS uses for sports and
prediction-market pricing. Each produces a win/outcome probability that the
pricing-engine turns into fair odds and the arbitrage-engine compares to the
market.

## Elo
Rates team or player strength from match results, updating after each game by
the surprise of the outcome. Fast, robust, and a strong baseline for
head-to-head sports (NBA, NFL, soccer, tennis). Weak at modelling margins or
scores directly.

## Bradley-Terry
A paired-comparison model: the probability team A beats team B is a logistic
function of their strength parameters. Good for ranking and round-robin
competitions; extends to ties and home advantage.

## Poisson and Negative-Binomial
Count models for goals, runs, or points. Poisson assumes mean equals variance;
the Negative-Binomial (used by the MLB run-bucket model) adds a dispersion
parameter for the overdispersion seen in real scoring, giving better tails for
totals and over/under markets.

## Bayesian hierarchical models
Pool information across teams and seasons with partial pooling, yielding full
posterior distributions (mean, sd, credible intervals) rather than point
estimates. The feature-store stores these posterior summaries; the
simulation-engine draws from them for Monte Carlo totals, and rejects
posteriors with R-hat above 1.01 as unconverged.

## Choosing a model
Use Elo or Bradley-Terry for match-winner markets, Poisson/Negative-Binomial
for totals, and a Bayesian hierarchical model when uncertainty quantification
matters for staking (Kelly) and edge calculation.
