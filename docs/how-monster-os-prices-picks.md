# How Monster OS prices a pick (and when it has an edge)

Plain-language reference for MonsterGPT and users. Numbers are measured results from
Monster OS's own data (research-backlog.md R7-R9, validation.md); none are promises.

## 1. From a model to a probability

Every engine ends with a probability for one side of one line.

- **Simulators** (NBA possession-by-possession, NFL play-by-play, NHL event-by-event, MLB
  plate appearance) play the game thousands of times. The share of simulated games in which
  a side wins, covers or goes over is its probability, and the spread of simulated results is
  the uncertainty.
- **Team ratings** turn the gap between two teams' ratings, plus home advantage, into an
  expected margin. A rating that updates after every game (an Elo-style or least-squares
  rating) follows form better than one fitted once per season: on NFL spreads it scored
  better than the current team-points engine (Brier 0.2633 vs 0.2741 on 1,016 games).
- **Expected margin to probability.** A margin forecast is a distribution, not a point. The
  probability of covering a line is the share of that distribution beyond the line. Real
  results are lumpy: about 15% of NFL games end by exactly 3 points and 9% by exactly 7
  (field goals and touchdowns), and about 59% of NHL games end by 2 or more goals because
  trailing teams pull the goalie late. A model that ignores these shapes misprices lines that
  sit on them.

## 2. The market is the benchmark

A sportsbook's price, with its margin (vig) removed, is itself a probability, and the line
at kick-off (the closing line) is the hardest one to beat. Monster OS compares every model
with it:

- **Removing the margin.** The two sides' implied probabilities add up to more than 100%;
  the excess is the book's margin (about 6.5% on average in Monster OS's stored quotes).
  Monster OS removes it with the power method: each implied probability is raised to the
  same power so the two add up to 100%. That takes more of the margin from the longshot
  than dividing by the total does, and it fits the results better: on 16,319 settled
  selections, sides that the proportional method put at 0-20% won 10.9% of the time; the
  proportional method said 15.1%, the power method 11.9%. Books build more margin into
  longshots, so a proportional de-vig leaves fake value on the underdog.

- **Edge** = model probability minus the de-vigged market probability. It means something
  only if the model has beaten the market before, on games it had not seen.
- **Blend weight.** A pick's price is a blend: `final = market + w x (model - market)`. The
  weight `w` is fitted on settled real lines. If the model has not beaten the market, `w` is
  0 and the price is the market's, so there is no edge to publish. With fewer than 300
  settled lines the fitted weight is scaled down toward 0.
- **Closing-line value (CLV).** CLV is how much better the price taken was than the price at
  kick-off. The closing line has absorbed all the news and sharp money, so it is the best
  available estimate of a side's true probability, and beating it on average is the clearest
  sign a model knows something the market does not. A win-loss record over a few weeks is
  mostly noise.
- **Beating the close is not enough: the price must also clear the vig.** At -110 a bet needs
  52.4% to break even, so beating a fair 50% closing price by one point still loses. The
  test Monster OS uses is the expected value of the price actually taken at the closing fair
  (de-vigged) probability: positive means the bet was +EV, negative means the vig won.
  Measured on 2,916 settled picks (October-November 2026, quarantined feeds removed): 38.8%
  beat the close, but the average expected value at the closing fair price was -5.5% (power
  de-vig; -5.3% with the proportional method), and only 9.6% of picks were +EV at the close. Results that look better than that over a few
  weeks are mostly luck.
- **Where models can win.** Major markets (NFL sides, NBA totals) are very efficient. Edges
  are more likely in thinner markets (player props, smaller leagues) and in pricing a line
  before the market has adjusted to news, which needs fast, decision-time data.

What the research has found so far: no Monster OS model beats the closing line on NFL
sides, and the NHL simulator underprices the -1.5 favourite on the puck line (31.6% vs a
de-vigged market 36.2%, 89 games). Those markets are shown only as leans.

## 3. Tiers and publication

- **Evidence tier** (proven / baseline / unproven / losing) summarises how an engine did on
  a market in backtests and on real lines.
- **Publication state** (published / leans only / suspended / unpublished) is a separate,
  human decision. A market reaches the default AI Picks feed only when it is published and
  its tier qualifies. A new market starts unpublished. A safety rule suspends a published
  market whose tier falls; only an admin restores it.

## 4. Stake size

Stakes use a fraction of the Kelly criterion, which sizes a bet by edge and price so the
bankroll grows fastest in the long run. Full Kelly is very volatile and assumes the
probability is exact, so Monster OS uses a fraction: full fraction for proven markets, half
for baseline, nothing otherwise, capped at 2% of bankroll per pick.

## 5. Reading a track record

- Count only picks that were public at the time, at books that were actually quoting.
- Separate tiers and markets: one market can dominate a total. In October-November 2026 the
  settled record was -18.2 units on 3,382 picks once a quarantined feed was removed, and NHL
  player points alone were -85 units (that market is shown only as leans).
- Backtests are evidence about a method, not money won. Only settled real picks are a record.

## Further reading

Winston, Nestler and Pelechrinis, *Mathletics* (2nd ed., Princeton University Press, 2022):
chapters on rating teams, from point ratings to probabilities, Elo ratings, Kelly growth and
sports gambling basics.
