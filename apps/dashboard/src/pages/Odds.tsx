import type { ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { api } from "../lib/api";
import { useSettings } from "../store/settings";
import { formatAmerican, formatPct } from "../lib/odds";
import type { OddsScreenRow } from "../types/monster-os";

// Module 1 (Phase 2): the Odds Screen line-shopping board. Per outcome it shows
// the best available number + which book, each book's price, the no-vig fair,
// and the value edge of the best price â€” straight from odds-service /screen.
export function Odds() {
  const sportKey = useSettings((s) => s.sportKey);
  const { data, isLoading, error, isFetching } = useQuery({
    queryKey: ["odds-screen", sportKey],
    queryFn: () => api.oddsScreen(sportKey),
    refetchInterval: 10_000, // odds refresh every 10s
  });

  return (
    <div className="space-y-4">
      <div className="flex items-baseline gap-3">
        <h1 className="text-lg font-semibold">Odds Screen</h1>
        <span className="text-sm text-gray-500">
          Line-shopping board Â· best number per outcome vs no-vig fair
        </span>
        {isFetching && <span className="text-xs text-violet-light">updatingâ€¦</span>}
      </div>

      {isLoading && <Panel>Loading oddsâ€¦</Panel>}
      {error && (
        <Panel tone="error">
          Couldnâ€™t load odds: {(error as Error).message}. Is the gateway + odds-service running
          (ODDS_API_KEYS set)?
        </Panel>
      )}
      {data && data.length === 0 && (
        <Panel>No games on the board for this sport right now.</Panel>
      )}
      {data && data.length > 0 && <Board rows={data} />}
    </div>
  );
}

function Board({ rows }: { rows: OddsScreenRow[] }) {
  const books = Array.from(new Set(rows.flatMap((r) => Object.keys(r.books)))).sort();
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
              <Td className="font-medium">{r.outcome}</Td>
              <Td className="font-mono text-emerald-light">{formatAmerican(r.best_price)}</Td>
              <Td className="text-gray-400">{r.best_book}</Td>
              <Td className="text-right font-mono text-gray-400">{formatAmerican(r.fair_odds)}</Td>
              <Td className="text-right font-mono">
                <span
                  className={clsx(
                    r.edge == null ? "text-gray-500" : r.edge > 0 ? "text-emerald-light" : "text-crimson-light",
                  )}
                >
                  {r.edge == null ? "â€”" : `${r.edge > 0 ? "+" : ""}${formatPct(r.edge)}`}
                </span>
              </Td>
              {books.map((b) => {
                const price = r.books[b];
                const isBest = price != null && price === r.best_price && b === r.best_book;
                return (
                  <Td key={b} className="text-right font-mono">
                    {price == null ? (
                      <span className="text-gray-600">â€”</span>
                    ) : (
                      <span
                        className={clsx(
                          "px-1.5 py-0.5 rounded",
                          isBest && "border border-emerald/40 bg-emerald/10 text-emerald-light",
                        )}
                      >
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

