import { useEffect, useState } from "react";
import { Link, useNavigate, useSearchParams } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { ArrowDown, ArrowUp, ChevronLeft, ChevronRight, Info, Loader2, Search, TriangleAlert } from "lucide-react";
import { Card, Msg, Pills } from "../components/ui";
import { Avatar, Money } from "../components/pm";
import { formatPct, formatUsd } from "../lib/odds";
import {
  PM_CATEGORIES, PM_WINDOWS, cents, displayName, formatShares, pm, shortAddr, timeAgo, walletAge,
  type BoardRow, type Mover, type TradeCard, type WalletStats,
} from "../lib/pm";
import { CopyBots } from "./CopyBots";

// Top Traders: Polymarket wallets ranked by real results (Polymarket's own
// leaderboard), with the record behind each rank. Discover = quick scan;
// Leaderboard = the full sortable table; click any wallet for its profile.
export function TopTraders() {
  const [params, setParams] = useSearchParams();
  const tab = params.get("tab") ?? "discover";
  return (
    <div className="space-y-4">
      <div className="flex items-center gap-3 flex-wrap">
        <h1 className="text-lg font-semibold">Top Traders</h1>
        <span className="text-sm text-gray-500">Polymarket wallets ranked by real results</span>
        <div className="ml-auto">
          <Pills options={[{ key: "discover", label: "Discover" }, { key: "leaderboard", label: "Leaderboard" }, { key: "bots", label: "Bots" }]}
            value={tab} onChange={(t) => setParams({ tab: t })} />
        </div>
      </div>
      {tab === "leaderboard" ? <Leaderboard /> : tab === "bots" ? <CopyBots /> : <Discover />}
    </div>
  );
}

function CategorySelect({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  return (
    <select value={value} onChange={(e) => onChange(e.target.value)}
      className="bg-surface border border-glass rounded-md px-2 py-1 text-xs text-gray-200 focus:outline-none">
      {PM_CATEGORIES.map((c) => <option key={c.key} value={c.key}>Category: {c.label}</option>)}
    </select>
  );
}

function WinRate({ s }: { s: WalletStats | null }) {
  if (!s || s.full_at == null) return <Loader2 size={12} className="inline animate-spin text-gray-600" />;
  if (s.win_rate == null) return <span className="text-gray-600">—</span>;
  return (
    <span title={`${s.wins} won · ${s.losses} lost of the last ${s.closed_counted}${s.closed_capped ? "+" : ""} resolved positions (lost bets left unredeemed count as losses)`}>
      <span className={clsx("font-mono", s.win_rate >= 0.55 ? "text-emerald-light" : s.win_rate < 0.45 ? "text-crimson-light" : "text-gray-200")}>
        {formatPct(s.win_rate, 0)}
      </span>
      <span className="text-gray-600 text-[11px] ml-1">{s.wins}–{s.losses}</span>
    </span>
  );
}

/** Warn when a wallet topping a short window is down all-time. */
function NetLoser({ s, window }: { s: WalletStats | null; window: string }) {
  if (window === "all" || !s || s.all_pnl == null || s.all_pnl >= 0) return null;
  return (
    <span title={`Down ${formatUsd(s.all_pnl)} all-time: this window is a hot streak, not a track record`}
      className="inline-flex items-center gap-0.5 text-[10px] px-1 rounded bg-crimson/15 text-crimson-light">
      <TriangleAlert size={10} /> all-time {formatUsd(s.all_pnl)}
    </span>
  );
}

// ---------------- Leaderboard ----------------

function Leaderboard() {
  const navigate = useNavigate();
  const [category, setCategory] = useState("overall");
  const [window, setWindow] = useState("month");
  const [order, setOrder] = useState("pnl");
  const [page, setPage] = useState(0);
  const [search, setSearch] = useState("");
  const [q, setQ] = useState("");
  useEffect(() => { const t = setTimeout(() => setQ(search.trim()), 400); return () => clearTimeout(t); }, [search]);
  useEffect(() => setPage(0), [category, window, order, q]);
  const PAGE = 25;
  const { data, isLoading, error, isFetching } = useQuery({
    queryKey: ["pm-board", category, window, order, page, q],
    queryFn: () => pm.leaderboard({ category, window, order, limit: PAGE, offset: page * PAGE, q: q || undefined }),
    // Win rates fill in over a minute or two the first time; poll until done.
    refetchInterval: (query) => ((query.state.data?.refreshing ?? 0) > 0 ? 8_000 : 60_000),
  });
  const [glossary, setGlossary] = useState(false);

  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2 flex-wrap">
        <div className="relative">
          <Search size={13} className="absolute left-2 top-1/2 -translate-y-1/2 text-gray-500" />
          <input value={search} onChange={(e) => setSearch(e.target.value)} placeholder="Search name or 0x wallet"
            className="bg-surface border border-glass rounded-md pl-7 pr-2 py-1 text-xs w-56 focus:outline-none focus:border-violet/50" />
        </div>
        <span className="text-xs text-gray-500 ml-2">Sort by</span>
        <Pills options={[{ key: "pnl", label: "P&L" }, { key: "vol", label: "Volume" }]} value={order} onChange={setOrder} />
        <Pills options={PM_WINDOWS} value={window} onChange={setWindow} />
        <CategorySelect value={category} onChange={setCategory} />
        <button onClick={() => setGlossary((g) => !g)} className="text-xs text-gray-400 hover:text-gray-200 inline-flex items-center gap-1">
          <Info size={12} /> Glossary
        </button>
        {isFetching && <Loader2 size={13} className="animate-spin text-gray-500" />}
        {data && data.refreshing > 0 && <span className="text-[11px] text-gray-500">computing records for {data.refreshing} wallets…</span>}
      </div>
      {glossary && (
        <Card className="text-xs text-gray-400 space-y-1">
          <div><b className="text-gray-200">P&L</b> — Polymarket's own profit figure for the window and category (realized, matches their leaderboard).</div>
          <div><b className="text-gray-200">Win rate</b> — resolved positions won ÷ won+lost, over the wallet's most recent 1,000 closed plus all settled-unredeemed positions. Lost bets are rarely redeemed, so counting only "closed" positions would hide most losses; we count them.</div>
          <div><b className="text-gray-200">Markets</b> — distinct markets ever traded. <b className="text-gray-200">Active</b> — open positions still trading. <b className="text-gray-200">Value</b> — current value of open positions.</div>
          <div><b className="text-gray-200">Won / Lost</b> — sum of P&L on winning and losing resolved positions in that sample.</div>
          <div><b className="text-gray-200">All-time warning</b> — shown when a wallet leads a short window but is down all-time.</div>
        </Card>
      )}
      {isLoading && <Msg>Loading leaderboard…</Msg>}
      {error && <Msg tone="error">{(error as Error).message}</Msg>}
      {data && data.rows.length === 0 && <Msg>No wallets match.</Msg>}
      {data && data.rows.length > 0 && (
        <div className="overflow-auto rounded-lg border border-glass">
          <table className="w-full text-sm">
            <thead className="bg-surface text-gray-400 text-[11px] uppercase tracking-wide">
              <tr>
                <th className="text-left px-3 py-2">Rank</th>
                <th className="text-left px-3 py-2">Trader</th>
                <th className="text-right px-3 py-2">{window === "all" ? "R. P&L" : "P&L"}</th>
                <th className="text-right px-3 py-2">Volume</th>
                <th className="text-right px-3 py-2">Win rate</th>
                <th className="text-right px-3 py-2">Markets</th>
                <th className="text-right px-3 py-2">Active</th>
                <th className="text-right px-3 py-2">Value</th>
                <th className="text-right px-3 py-2">Age</th>
                <th className="text-right px-3 py-2">Won</th>
                <th className="text-right px-3 py-2">Lost</th>
              </tr>
            </thead>
            <tbody>
              {data.rows.map((r) => <BoardLine key={r.wallet} r={r} window={window} onOpen={() => navigate(`/wallets/${r.wallet}`)} />)}
            </tbody>
          </table>
        </div>
      )}
      {data && !q && (
        <div className="flex items-center justify-end gap-2 text-xs text-gray-400">
          <button disabled={page === 0} onClick={() => setPage((p) => p - 1)} className="p-1 rounded border border-glass disabled:opacity-30"><ChevronLeft size={14} /></button>
          <span>ranks {page * PAGE + 1}–{page * PAGE + data.rows.length}</span>
          <button disabled={data.rows.length < PAGE} onClick={() => setPage((p) => p + 1)} className="p-1 rounded border border-glass disabled:opacity-30"><ChevronRight size={14} /></button>
        </div>
      )}
    </div>
  );
}

function RankBadge({ rank, change }: { rank: number; change: number | null }) {
  return (
    <span className="inline-flex items-center gap-1">
      <span className={clsx("px-1.5 py-0.5 rounded text-xs font-mono",
        rank === 1 ? "bg-amber/20 text-amber" : rank <= 3 ? "bg-violet/20 text-violet-light" : "bg-white/5 text-gray-400")}>#{rank}</span>
      {change != null && change !== 0 && (
        <span className={clsx("text-[10px] inline-flex items-center", change > 0 ? "text-emerald-light" : "text-crimson-light")}>
          {change > 0 ? <ArrowUp size={10} /> : <ArrowDown size={10} />}{Math.abs(change)}
        </span>
      )}
    </span>
  );
}

function BoardLine({ r, window, onOpen }: { r: BoardRow; window: string; onOpen: () => void }) {
  const s = r.stats;
  return (
    <tr onClick={onOpen} className="border-t border-glass hover:bg-white/[0.03] cursor-pointer">
      <td className="px-3 py-2"><RankBadge rank={r.rank} change={r.rank_change} /></td>
      <td className="px-3 py-2">
        <div className="flex items-center gap-2 min-w-0">
          <Avatar wallet={r.wallet} src={r.profile_image} size={28} />
          <div className="min-w-0">
            <div className="font-medium truncate max-w-[180px] flex items-center gap-1">
              {displayName(r.name, r.wallet)} <NetLoser s={s} window={window} />
            </div>
            <div className="text-[11px] text-gray-600 font-mono">{shortAddr(r.wallet)}</div>
          </div>
        </div>
      </td>
      <td className="px-3 py-2 text-right"><Money v={r.pnl} /></td>
      <td className="px-3 py-2 text-right font-mono text-gray-300">{r.vol != null ? formatUsd(r.vol) : formatShares(r.vol_shares)}</td>
      <td className="px-3 py-2 text-right whitespace-nowrap"><WinRate s={s} /></td>
      <td className="px-3 py-2 text-right font-mono text-gray-300">{s?.markets_traded?.toLocaleString() ?? "—"}</td>
      <td className="px-3 py-2 text-right font-mono text-gray-300">{s?.active_positions?.toLocaleString() ?? "—"}</td>
      <td className="px-3 py-2 text-right font-mono text-gray-300">{formatUsd(s?.positions_value)}</td>
      <td className="px-3 py-2 text-right text-gray-400">{walletAge(s?.age_days)}</td>
      <td className="px-3 py-2 text-right"><Money v={s?.win_amount} /></td>
      <td className="px-3 py-2 text-right"><Money v={s?.loss_amount} /></td>
    </tr>
  );
}

// ---------------- Discover ----------------

function Section({ title, sub, right, children }: { title: string; sub: string; right?: React.ReactNode; children: React.ReactNode }) {
  return (
    <section className="space-y-2">
      <div className="flex items-end gap-3 flex-wrap">
        <div>
          <h2 className="font-semibold">{title}</h2>
          <div className="text-xs text-gray-500">{sub}</div>
        </div>
        {right && <div className="ml-auto flex items-center gap-2">{right}</div>}
      </div>
      {children}
    </section>
  );
}

function Discover() {
  return (
    <div className="space-y-8">
      <TopCards />
      <Movers />
      <div className="grid gap-6 xl:grid-cols-2">
        <Whales />
        <Insiders />
      </div>
    </div>
  );
}

function StatLine({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex justify-between text-xs">
      <span className="text-gray-500">{label}</span>
      <span>{children}</span>
    </div>
  );
}

function TopCards() {
  const [category, setCategory] = useState("overall");
  const [window, setWindow] = useState("week");
  const { data, isLoading, error } = useQuery({
    queryKey: ["pm-top", category, window],
    queryFn: () => pm.leaderboard({ category, window, limit: 8 }),
    refetchInterval: (q) => ((q.state.data?.refreshing ?? 0) > 0 ? 8_000 : 60_000),
  });
  return (
    <Section title="Top Traders" sub="Highest earners for the window"
      right={<><CategorySelect value={category} onChange={setCategory} /><Pills options={PM_WINDOWS} value={window} onChange={setWindow} /></>}>
      {isLoading && <Msg>Loading…</Msg>}
      {error && <Msg tone="error">{(error as Error).message}</Msg>}
      <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
        {data?.rows.map((r) => (
          <Link key={r.wallet} to={`/wallets/${r.wallet}`}>
            <Card className="space-y-2 hover:border-violet/40 transition-colors h-full">
              <div className="flex items-start gap-2">
                <Avatar wallet={r.wallet} src={r.profile_image} size={32} />
                <div className="min-w-0 flex-1">
                  <div className="font-medium truncate">{displayName(r.name, r.wallet)}</div>
                  <div className="text-[11px] text-gray-500">{r.stats?.all_rank ? `#${r.stats.all_rank.toLocaleString()} all-time overall` : shortAddr(r.wallet)}</div>
                </div>
                <div className="text-right">
                  <div className="font-bold font-mono">#{r.rank}</div>
                  <div className="text-[10px] text-gray-500">{PM_CATEGORIES.find((c) => c.key === category)?.label}</div>
                </div>
              </div>
              <StatLine label={`${PM_WINDOWS.find((w) => w.key === window)?.label} P&L`}><Money v={r.pnl} signed /></StatLine>
              <StatLine label="Win rate"><WinRate s={r.stats} /></StatLine>
              <StatLine label="Wallet age"><span className="text-gray-300">{walletAge(r.stats?.age_days)}</span></StatLine>
              <NetLoser s={r.stats} window={window} />
            </Card>
          </Link>
        ))}
      </div>
    </Section>
  );
}

function Movers() {
  const [category, setCategory] = useState("overall");
  const [window, setWindow] = useState("week");
  const { data, isLoading, error } = useQuery({
    queryKey: ["pm-movers", category, window],
    queryFn: () => pm.movers({ category, window, limit: 8 }),
    refetchInterval: 120_000,
  });
  return (
    <Section title="Recent Alpha Movers" sub="Climbing fastest: top of this window, far lower all-time — and still up all-time"
      right={<><CategorySelect value={category} onChange={setCategory} />
        <Pills options={PM_WINDOWS.filter((w) => w.key !== "all")} value={window} onChange={setWindow} /></>}>
      {isLoading && <Msg>Comparing ranks…</Msg>}
      {error && <Msg tone="error">{(error as Error).message}</Msg>}
      {data && data.rows.length === 0 && <Msg>No proven wallets climbing in this window.</Msg>}
      <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
        {data?.rows.map((m: Mover) => (
          <Link key={m.wallet} to={`/wallets/${m.wallet}`}>
            <Card className="space-y-2 hover:border-violet/40 transition-colors h-full">
              <div className="flex items-start gap-2">
                <Avatar wallet={m.wallet} src={m.profile_image} size={32} />
                <div className="min-w-0 flex-1">
                  <div className="font-medium truncate">{displayName(m.name, m.wallet)}</div>
                  <div className="text-[11px] text-gray-500">{m.all_rank ? `#${m.all_rank.toLocaleString()} all-time` : "—"}</div>
                </div>
                <div className="text-right">
                  <div className="font-bold font-mono">#{m.rank}</div>
                  {m.climb != null && m.climb > 0 && (
                    <span className="text-[10px] px-1 rounded bg-emerald/15 text-emerald-light inline-flex items-center"><ArrowUp size={10} />{m.climb.toLocaleString()}</span>
                  )}
                </div>
              </div>
              <StatLine label={`${PM_WINDOWS.find((w) => w.key === window)?.label} P&L`}><Money v={m.pnl} signed /></StatLine>
              <StatLine label="All-time P&L"><Money v={m.all_pnl} signed /></StatLine>
              <StatLine label="Win rate"><WinRate s={m.stats} /></StatLine>
              <StatLine label="Wallet age"><span className="text-gray-300">{walletAge(m.stats?.age_days)}</span></StatLine>
            </Card>
          </Link>
        ))}
      </div>
    </Section>
  );
}

function TradeList({ rows }: { rows: TradeCard[] }) {
  return (
    <div className="space-y-2">
      {rows.map((t, i) => (
        <Card key={`${t.wallet}-${i}`} className="!p-3 space-y-2">
          <Link to={`/wallets/${t.wallet}`} className="flex items-center gap-2 hover:opacity-90">
            <Avatar wallet={t.wallet} src={t.stats?.profile_image} size={26} />
            <div className="min-w-0 flex-1">
              <div className="text-sm font-medium truncate">{displayName(t.name ?? t.stats?.name, t.wallet)}</div>
              <div className="text-[10px] text-gray-500">{t.stats?.all_rank ? `#${t.stats.all_rank.toLocaleString()} all-time` : shortAddr(t.wallet)}</div>
            </div>
            <div className="grid grid-cols-4 gap-3 text-right text-[11px]">
              <div><div className="text-gray-600">R. P&L</div><Money v={t.stats?.all_pnl} signed /></div>
              <div><div className="text-gray-600">Volume</div><span className="font-mono text-gray-300">{formatUsd(t.stats?.all_vol)}</span></div>
              <div><div className="text-gray-600">Balance</div><span className="font-mono text-gray-300">{formatUsd(t.stats?.positions_value)}</span></div>
              <div><div className="text-gray-600">Age</div><span className="text-gray-300">{walletAge(t.stats?.age_days)}</span></div>
            </div>
          </Link>
          <div className="flex items-center gap-2 text-xs bg-obsidian/40 rounded px-2 py-1.5">
            <span className={clsx("px-1.5 py-0.5 rounded text-[10px] font-semibold", t.side === "BUY" ? "bg-emerald/15 text-emerald-light" : "bg-crimson/15 text-crimson-light")}>{t.side}</span>
            {t.icon && <img src={t.icon} alt="" className="w-5 h-5 rounded" />}
            <a href={t.slug ? `https://polymarket.com/event/${t.slug}` : undefined} target="_blank" rel="noreferrer"
              className="truncate flex-1 text-gray-200 hover:underline" title={t.title ?? ""}>{t.title}</a>
            <span className="text-gray-400 shrink-0">{t.outcome}</span>
            <span className="font-mono shrink-0">{formatUsd(t.usdc)}</span>
            <span className="font-mono text-gray-400 shrink-0">@ {cents(t.price)}</span>
          </div>
          <div className="text-[10px] text-gray-600">{timeAgo(t.ts)}</div>
        </Card>
      ))}
    </div>
  );
}

function Whales() {
  const [minValue, setMinValue] = useState(100_000);
  const { data, isLoading, error } = useQuery({
    queryKey: ["pm-whales", minValue],
    queryFn: () => pm.whales({ min_value: minValue, limit: 10 }),
    refetchInterval: 60_000,
  });
  return (
    <Section title="Whale Movements" sub={`Trades ≥ $10K from wallets holding ≥ ${formatUsd(minValue)} in positions`}
      right={<Pills options={[{ key: "100000", label: "$100K+" }, { key: "500000", label: "$500K+" }, { key: "1000000", label: "$1M+" }]}
        value={String(minValue)} onChange={(v) => setMinValue(Number(v))} />}>
      {isLoading && <Msg>Scanning large trades…</Msg>}
      {error && <Msg tone="error">{(error as Error).message}</Msg>}
      {data && data.rows.length === 0 && <Msg>No whale trades in the latest window.</Msg>}
      {data && <TradeList rows={data.rows} />}
    </Section>
  );
}

function Insiders() {
  const [age, setAge] = useState(7);
  const { data, isLoading, error } = useQuery({
    queryKey: ["pm-insiders", age],
    queryFn: () => pm.insiders({ max_age_days: age, limit: 10 }),
    refetchInterval: 60_000,
  });
  return (
    <Section title="New-Wallet Spotlight" sub={`Trades ≥ $10K from wallets created in the last ${age} days`}
      right={<Pills options={[{ key: "1", label: "1d" }, { key: "7", label: "7d" }, { key: "30", label: "30d" }]}
        value={String(age)} onChange={(v) => setAge(Number(v))} />}>
      {isLoading && <Msg>Scanning large trades…</Msg>}
      {error && <Msg tone="error">{(error as Error).message}</Msg>}
      {data && data.rows.length === 0 && <Msg>No large trades from new wallets right now.</Msg>}
      {data && <TradeList rows={data.rows} />}
      <div className="text-[11px] text-gray-600">A fresh wallet sizing up can mean information — or just a trader's new account. Check its history before reading anything into it.</div>
    </Section>
  );
}
