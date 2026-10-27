import { useState, type ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { api, type MarketScreenRow } from "../lib/api";
import { useSettings } from "../store/settings";
import { formatAmerican, formatPct } from "../lib/odds";
import { gameMarkets, segmentOf, sportOf } from "../lib/markets";
import type { OddsScreenRow } from "../types/monster-os";

// Module 1 (Phase 2): the Odds Screen line-shopping board. Per outcome it shows
// the best available number + which book, each book's price, the no-vig fair,
// and the value edge of the best price. "Moneyline · live" is The Odds API via
// odds-service /screen (refreshed every 10 s); every other market — spreads,
// totals, team totals and the first segment — comes from the recorded multi-book
// lines (projection-service /market/{sport}/screen), with the model's probability
// and EV where the sport's champion engine has priced the game.

/** One board row, whichever source it came from. */
export type BoardRow = Omit<OddsScreenRow, "commence_time"> & { commence_time: string | null } & Partial<Omit<MarketScreenRow, keyof OddsScreenRow>>;

export const LIVE = "moneyline_live";

/** The board for a sport and market (shared with the +EV Finder). */
export function useBoard(sportKey: string, market: string) {
  const sport = sportOf(sportKey);
  const live = useQuery({
    queryKey: ["odds-screen", sportKey],
    queryFn: () => api.oddsScreen(sportKey),
    refetchInterval: 10_000, // odds refresh every 10s
    enabled: market === LIVE,
  });
  const recorded = useQuery({
    queryKey: ["market-screen", sport, market],
    queryFn: () => api.marketScreen(sport, market),
    refetchInterval: 60_000,
    enabled: market !== LIVE,
  });
  const q = market === LIVE ? live : recorded;
  const rows: BoardRow[] | undefined = market === LIVE ? live.data : recorded.data?.rows;
  return { rows, isLoading: q.isLoading, error: q.error, isFetching: q.isFetching, asOf: recorded.data?.as_of ?? null, live: market === LIVE };
}

export function MarketPicker({ sportKey, value, onChange }: { sportKey: string; value: string; onChange: (m: string) => void }) {
  const sport = sportOf(sportKey);
  const opts = [{ key: LIVE, label: "Moneyline · live" }, ...gameMarkets(sport)];
  return (
    <div className="flex flex-wrap gap-1">
      {opts.map((o) => (
        <button key={o.key} onClick={() => onChange(o.key)}
          className={clsx("px-2.5 py-1 rounded-md text-xs border", value === o.key ? "bg-violet/20 border-violet/40 text-violet-light" : "border-glass text-gray-400 hover:text-gray-200")}>
          {o.label}
        </button>
      ))}
    </div>
  );
}

export function Odds() {
  const sportKey = useSettings((s) => s.sportKey);
  const [market, setMarket] = useState(LIVE);
  const [mainOnly, setMainOnly] = useState(true);
  const { rows, isLoading, error, isFetching, asOf, live } = useBoard(sportKey, market);
  const shown = (rows ?? []).filter((r) => live || !mainOnly || r.main_line);
  const seg = segmentOf(sportOf(sportKey));

  return (
    <div className="space-y-4">
      <div className="flex items-baseline gap-3 flex-wrap">
        <h1 className="text-lg font-semibold">Odds Screen</h1>
        <span className="text-sm text-gray-500">
          Line-shopping board · best number per outcome vs no-vig fair
        </span>
        {isFetching && <span className="text-xs text-violet-light">updating…</span>}
      </div>
      <div className="flex items-center gap-3 flex-wrap">
        <MarketPicker sportKey={sportKey} value={market} onChange={setMarket} />
        {!live && (
          <label className="text-xs text-gray-400 flex items-center gap-1.5 ml-auto">
            <input type="checkbox" checked={mainOnly} onChange={(e) => setMainOnly(e.target.checked)} /> main lines only
          </label>
        )}
      </div>
      {!live && (
        <div className="text-[11px] text-gray-500">
          Recorded lines from every book we track{asOf ? `, newest ${new Date(asOf).toLocaleTimeString()}` : ""}. {market.endsWith("_1h") ? `${seg.tag} = ${seg.label}. ` : ""}
          Model = the champion engine's probability (pushes excluded) where it has priced the game.
        </div>
      )}

      {isLoading && <Panel>Loading odds…</Panel>}
      {error && (
        <Panel tone="error">
          Couldn’t load odds: {(error as Error).message}. {live ? "Is the gateway + odds-service running (ODDS_API_KEYS set)?" : "Is projection-service running?"}
        </Panel>
      )}
      {rows && shown.length === 0 && (
        <Panel>No {live ? "games" : "lines for this market"} on the board for this sport right now.{!live && market.includes("1h") ? " First-segment and team-total lines arrive with the SportsGameOdds pull (every 4 h)." : ""}</Panel>
      )}
      {shown.length > 0 && <Board rows={shown} />}
    </div>
  );
}

export function Board({ rows }: { rows: BoardRow[] }) {
  const books = Array.from(new Set(rows.flatMap((r) => Object.keys(r.books)))).sort();
  const hasModel = rows.some((r) => r.model_probability != null);
  return (
    <div className="overflow-auto rounded-lg border border-glass">
      <table className="w-full text-sm border-collapse">
        <thead className="bg-surface text-gray-400 sticky top-0">
          <tr>
            <Th>Game</Th>
            <Th>Outcome</Th>
            <Th>Best</Th>
            <Th>Book</Th>
            <Th className="text-right">Fair</Th>
            <Th className="text-right">Edge</Th>
            {hasModel && <Th className="text-right">Model</Th>}
            {hasModel && <Th className="text-right">Model EV</Th>}
            {books.map((b) => (
              <Th key={b} className="text-right">
                {b}
              </Th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((r, i) => (
            <tr key={i} className="border-t border-glass hover:bg-white/[0.03]">
              <Td className="text-gray-300 whitespace-nowrap">{r.game}</Td>
              <Td className="font-medium whitespace-nowrap">{r.outcome}</Td>
              <Td className="font-mono text-emerald-light">{formatAmerican(r.best_price)}</Td>
              <Td className="text-gray-400">{r.best_book}</Td>
              <Td className="text-right font-mono text-gray-400">{formatAmerican(r.fair_odds)}</Td>
              <Td className="text-right font-mono">
                <span className={clsx(r.edge == null ? "text-gray-500" : r.edge > 0 ? "text-emerald-light" : "text-crimson-light")}>
                  {r.edge == null ? "—" : `${r.edge > 0 ? "+" : ""}${formatPct(r.edge)}`}
                </span>
              </Td>
              {hasModel && <Td className="text-right font-mono text-gray-300">{r.model_probability == null ? "—" : formatPct(r.model_probability)}</Td>}
              {hasModel && (
                <Td className="text-right font-mono">
                  {r.model_ev == null ? <span className="text-gray-600">—</span> : (
                    <span className={r.model_ev > 0 ? "text-emerald-light" : "text-crimson-light"}>{r.model_ev > 0 ? "+" : ""}{(r.model_ev * 100).toFixed(1)}%</span>
                  )}
                </Td>
              )}
              {books.map((b) => {
                const price = r.books[b];
                const isBest = price != null && price === r.best_price && b === r.best_book;
                return (
                  <Td key={b} className="text-right font-mono">
                    {price == null ? (
                      <span className="text-gray-600">—</span>
                    ) : (
                      <span className={clsx("px-1.5 py-0.5 rounded", isBest && "border border-emerald/40 bg-emerald/10 text-emerald-light")}>
                        {formatAmerican(price)}
                      </span>
                    )}
                  </Td>
                );
              })}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function Th({ children, className }: { children: ReactNode; className?: string }) {
  return <th className={clsx("text-left font-medium px-3 py-2 text-xs uppercase tracking-wide", className)}>{children}</th>;
}
function Td({ children, className }: { children: ReactNode; className?: string }) {
  return <td className={clsx("px-3 py-2", className)}>{children}</td>;
}
function Panel({ children, tone }: { children: ReactNode; tone?: "error" }) {
  return (
    <div
      className={clsx(
        "rounded-lg border border-glass bg-surface/60 px-4 py-6 text-sm",
        tone === "error" ? "text-crimson-light" : "text-gray-400",
      )}
    >
      {children}
    </div>
  );
}
