import { useEffect, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { ArrowLeft, Copy, ExternalLink, Loader2, RefreshCw, Search } from "lucide-react";
import { Card, Msg, Pills } from "../components/ui";
import { Avatar, CAT_COLORS, CategoryDonut, Money, PnlChart, catLabel } from "../components/pm";
import { formatPct, formatUsd } from "../lib/odds";
import { cents, displayName, pm, shortAddr, timeAgo, walletAge, type Position, type WalletStats } from "../lib/pm";

// Wallet profile: who the wallet is, how it has actually done (record, P&L
// history, categories), and what it holds now.
export function WalletProfile() {
  const { addr = "" } = useParams();
  const wallet = addr.toLowerCase();
  const [tab, setTab] = useState("overview");
  const stats = useQuery({ queryKey: ["pm-wallet", wallet], queryFn: () => pm.wallet(wallet), staleTime: 60_000 });
  const latest = useQuery({ queryKey: ["pm-pnl", wallet, "1d"], queryFn: () => pm.pnl(wallet, "1d"), staleTime: 60_000 });
  const s = stats.data;

  return (
    <div className="space-y-4">
      <Link to="/copy?tab=leaderboard" className="inline-flex items-center gap-1 text-xs text-gray-400 hover:text-gray-200 border border-glass rounded-md px-2 py-1">
        <ArrowLeft size={12} /> Leaderboard
      </Link>
      {stats.isLoading && <Msg><Loader2 size={14} className="inline animate-spin mr-2" />Reading this wallet's positions and record from Polymarket…</Msg>}
      {stats.error && <Msg tone="error">{(stats.error as Error).message}</Msg>}
      {s && <Header s={s} trades={latest.data?.latest.trade_count ?? null} onRefresh={() => stats.refetch()} refreshing={stats.isFetching} />}
      {s && (
        <>
          <Pills value={tab} onChange={setTab} options={[
            { key: "overview", label: "Overview" },
            { key: "active", label: `Active (${s.active_positions ?? "…"})` },
            { key: "resolved", label: "Settled, unredeemed" },
            { key: "closed", label: "Closed" },
            { key: "categories", label: "Categories" },
            { key: "activity", label: "Activity" },
          ]} />
          {tab === "overview" && <Overview s={s} />}
          {tab === "active" && <Positions wallet={wallet} status="active" />}
          {tab === "resolved" && <Positions wallet={wallet} status="resolved" />}
          {tab === "closed" && <Positions wallet={wallet} status="closed" />}
          {tab === "categories" && <Categories s={s} />}
          {tab === "activity" && <ActivityTab wallet={wallet} />}
        </>
      )}
    </div>
  );
}

function Header({ s, trades, onRefresh, refreshing }: { s: WalletStats; trades: number | null; onRefresh: () => void; refreshing: boolean }) {
  const [copied, setCopied] = useState(false);
  const copy = () => {
    navigator.clipboard?.writeText(s.wallet).then(() => { setCopied(true); setTimeout(() => setCopied(false), 1200); });
  };
  const n = (s.wins ?? 0) + (s.losses ?? 0);
  return (
    <Card className="grid gap-4 lg:grid-cols-[minmax(0,1fr)_minmax(0,2fr)]">
      <div className="flex items-start gap-3">
        <Avatar wallet={s.wallet} src={s.profile_image} size={64} />
        <div className="min-w-0 space-y-1">
          <div className="text-xl font-semibold truncate">{displayName(s.name, s.wallet)}</div>
          <button onClick={copy} className="text-xs font-mono text-violet-light inline-flex items-center gap-1 hover:underline">
            {shortAddr(s.wallet)} <Copy size={11} /> {copied && <span className="text-gray-400">copied</span>}
          </button>
          <div className="text-[11px] text-gray-500">
            {s.joined_at ? `Joined ${new Date(s.joined_at).toLocaleDateString(undefined, { month: "long", year: "numeric" })}` : "Join date unknown"}
            {s.all_rank ? ` · #${s.all_rank.toLocaleString()} all-time` : ""}
          </div>
          <div className="flex gap-2 pt-1">
            <a href={`https://polymarket.com/profile/${s.wallet}`} target="_blank" rel="noreferrer"
              className="text-[11px] inline-flex items-center gap-1 border border-glass rounded px-2 py-0.5 text-gray-300 hover:bg-white/5">
              Polymarket <ExternalLink size={10} />
            </a>
            <button onClick={onRefresh} className="text-[11px] inline-flex items-center gap-1 border border-glass rounded px-2 py-0.5 text-gray-300 hover:bg-white/5">
              <RefreshCw size={10} className={clsx(refreshing && "animate-spin")} /> Refresh
            </button>
          </div>
        </div>
      </div>
      <div className="grid grid-cols-2 sm:grid-cols-4 gap-x-6 gap-y-3">
        <Big label="Realized P&L"><Money v={s.all_pnl} signed /></Big>
        <Big label="Volume"><span className="font-mono">{formatUsd(s.all_vol)}</span></Big>
        <Big label="Positions value"><span className="font-mono">{formatUsd(s.positions_value)}</span></Big>
        <Big label="Wallet age"><span>{walletAge(s.age_days)}</span></Big>
        <Big label="Win rate" hint={`${s.wins}–${s.losses} over ${n.toLocaleString()} resolved${s.closed_capped ? " (latest 1,000 closed + all settled)" : ""}`}>
          <span className="font-mono">{formatPct(s.win_rate, 1)}</span>
        </Big>
        <Big label="Markets traded"><span className="font-mono">{s.markets_traded?.toLocaleString() ?? "—"}</span></Big>
        <Big label="Trades"><span className="font-mono">{trades != null ? trades.toLocaleString() : "—"}</span></Big>
        <Big label="Avg trade"><span className="font-mono">{trades && s.all_vol ? formatUsd(s.all_vol / trades) : "—"}</span></Big>
      </div>
    </Card>
  );
}

function Big({ label, hint, children }: { label: string; hint?: string; children: React.ReactNode }) {
  return (
    <div title={hint}>
      <div className="text-[10px] uppercase tracking-wide text-gray-500">{label}</div>
      <div className="text-lg">{children}</div>
      {hint && <div className="text-[10px] text-gray-600 truncate">{hint}</div>}
    </div>
  );
}

const RANGES = [
  { key: "1d", label: "1D" }, { key: "1w", label: "1W" }, { key: "1m", label: "1M" },
  { key: "1y", label: "1Y" }, { key: "ytd", label: "YTD" }, { key: "all", label: "ALL" },
];

function Overview({ s }: { s: WalletStats }) {
  const [range, setRange] = useState("all");
  const series = useQuery({ queryKey: ["pm-pnl", s.wallet, range], queryFn: () => pm.pnl(s.wallet, range) });
  const best = useQuery({ queryKey: ["pm-best", s.wallet], queryFn: () => pm.positions(s.wallet, { status: "closed", sort: "pnl", dir: "desc", limit: 10 }) });
  const worstClosed = useQuery({ queryKey: ["pm-worst-c", s.wallet], queryFn: () => pm.positions(s.wallet, { status: "closed", sort: "pnl", dir: "asc", limit: 10 }) });
  const worstSettled = useQuery({ queryKey: ["pm-worst-s", s.wallet], queryFn: () => pm.positions(s.wallet, { status: "resolved", sort: "pnl", dir: "asc", limit: 10 }) });
  const [side, setSide] = useState("wins");
  const wins = (best.data?.rows ?? []).map((p) => ({ title: p.title, pnl: p.realized_pnl ?? 0 })).filter((p) => p.pnl > 0);
  const losses = [
    ...(worstClosed.data?.rows ?? []).map((p) => ({ title: p.title, pnl: p.realized_pnl ?? 0 })),
    ...(worstSettled.data?.rows ?? []).map((p) => ({ title: p.title, pnl: p.cash_pnl ?? 0 })),
  ].filter((p) => p.pnl < 0).sort((a, b) => a.pnl - b.pnl).slice(0, 10);
  const list = side === "wins" ? wins : losses;
  const maxAbs = Math.max(1, ...list.map((p) => Math.abs(p.pnl)));

  return (
    <div className="space-y-4">
      <div className="grid gap-4 lg:grid-cols-[minmax(0,1fr)_minmax(0,2fr)]">
        <Card className="space-y-4">
          <div className="text-xs uppercase tracking-wide text-gray-500">Profit by category</div>
          <CategoryDonut cats={s.categories ?? []} />
          <div className="grid grid-cols-2 gap-3">
            <Mini label="Won" v={<Money v={s.win_amount} />} />
            <Mini label="Lost" v={<Money v={s.loss_amount} />} />
            <Mini label="Biggest win" v={<Money v={s.biggest_win} />} />
            <Mini label="Biggest loss" v={<Money v={s.biggest_loss} />} />
          </div>
        </Card>
        <Card className="space-y-2">
          <div className="flex items-center gap-2 flex-wrap">
            <div>
              <div className="text-xs uppercase tracking-wide text-gray-500">Profit / loss · {RANGES.find((r) => r.key === range)?.label}</div>
              <div className="text-2xl"><Money v={series.data?.change} signed /></div>
            </div>
            <div className="ml-auto"><Pills options={RANGES} value={range} onChange={setRange} /></div>
          </div>
          {series.isLoading ? <div className="h-[200px] flex items-center justify-center"><Loader2 className="animate-spin text-gray-600" /></div>
            : <PnlChart points={series.data?.points ?? []} />}
          <div className="text-[10px] text-gray-600">Realized P&L (Polymarket's figure, the same one its leaderboard ranks by).</div>
        </Card>
      </div>
      <div className="grid gap-4 lg:grid-cols-2">
        <Card className="space-y-3">
          <div className="flex items-center">
            <div className="text-xs uppercase tracking-wide text-gray-500">Wins and losses by market</div>
            <div className="ml-auto"><Pills options={[{ key: "wins", label: "Best wins" }, { key: "losses", label: "Biggest losses" }]} value={side} onChange={setSide} /></div>
          </div>
          {list.length === 0 && <div className="text-xs text-gray-500">None yet.</div>}
          {list.map((p, i) => (
            <div key={i} className="space-y-1">
              <div className="flex justify-between gap-3 text-xs">
                <span className="truncate text-gray-300"><span className="text-gray-600 mr-1">#{i + 1}</span>{p.title}</span>
                <Money v={p.pnl} />
              </div>
              <div className="h-1 rounded bg-white/5">
                <div className={clsx("h-1 rounded", p.pnl > 0 ? "bg-emerald" : "bg-crimson")} style={{ width: `${(Math.abs(p.pnl) / maxAbs) * 100}%` }} />
              </div>
            </div>
          ))}
        </Card>
        <CategoryBars s={s} />
      </div>
    </div>
  );
}

function Mini({ label, v }: { label: string; v: React.ReactNode }) {
  return (
    <div className="rounded-md bg-obsidian/40 px-3 py-2">
      <div className="text-[10px] uppercase text-gray-600">{label}</div>
      <div className="text-sm">{v}</div>
    </div>
  );
}

function CategoryBars({ s }: { s: WalletStats }) {
  const [side, setSide] = useState("best");
  const cats = [...(s.categories ?? [])].sort((a, b) => (side === "best" ? b.pnl - a.pnl : a.pnl - b.pnl))
    .filter((c) => (side === "best" ? c.pnl > 0 : c.pnl < 0));
  const maxAbs = Math.max(1, ...cats.map((c) => Math.abs(c.pnl)));
  return (
    <Card className="space-y-3">
      <div className="flex items-center">
        <div className="text-xs uppercase tracking-wide text-gray-500">Performance by category</div>
        <div className="ml-auto"><Pills options={[{ key: "best", label: "Best" }, { key: "worst", label: "Worst" }]} value={side} onChange={setSide} /></div>
      </div>
      {cats.length === 0 && <div className="text-xs text-gray-500">{side === "best" ? "No profitable category." : "No losing category."}</div>}
      {cats.map((c, i) => (
        <div key={c.category} className="space-y-1">
          <div className="flex justify-between text-xs">
            <span className="text-gray-300"><span className="text-gray-600 mr-1">#{i + 1}</span>{catLabel(c.category)}
              <span className="text-gray-600 ml-2">rank #{c.rank.toLocaleString()}</span></span>
            <Money v={c.pnl} />
          </div>
          <div className="h-1 rounded bg-white/5">
            <div className="h-1 rounded" style={{ width: `${(Math.abs(c.pnl) / maxAbs) * 100}%`, background: CAT_COLORS[c.category] ?? "#888" }} />
          </div>
        </div>
      ))}
    </Card>
  );
}

function Categories({ s }: { s: WalletStats }) {
  const cats = [...(s.categories ?? [])].sort((a, b) => b.pnl - a.pnl);
  if (cats.length === 0) return <Msg>No category history.</Msg>;
  return (
    <div className="overflow-auto rounded-lg border border-glass">
      <table className="w-full text-sm">
        <thead className="bg-surface text-[11px] uppercase text-gray-400">
          <tr><th className="text-left px-3 py-2">Category</th><th className="text-right px-3 py-2">P&L</th><th className="text-right px-3 py-2">Volume</th>
            <th className="text-right px-3 py-2">Return on volume</th><th className="text-right px-3 py-2">Category rank</th></tr>
        </thead>
        <tbody>
          {cats.map((c) => (
            <tr key={c.category} className="border-t border-glass">
              <td className="px-3 py-2"><span className="inline-block w-2.5 h-2.5 rounded-sm mr-2" style={{ background: CAT_COLORS[c.category] }} />{catLabel(c.category)}</td>
              <td className="px-3 py-2 text-right"><Money v={c.pnl} signed /></td>
              <td className="px-3 py-2 text-right font-mono text-gray-300">{formatUsd(c.vol)}</td>
              <td className="px-3 py-2 text-right font-mono text-gray-300">{c.vol > 0 ? formatPct(c.pnl / c.vol, 2) : "—"}</td>
              <td className="px-3 py-2 text-right font-mono text-gray-400">#{c.rank.toLocaleString()}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <div className="px-3 py-2 text-[11px] text-gray-600">From Polymarket's per-category leaderboards (all-time). Good in one category doesn't mean good in every category.</div>
    </div>
  );
}

const SORTS: Record<string, { key: string; label: string }[]> = {
  active: [{ key: "value", label: "Value" }, { key: "pnl", label: "P&L" }, { key: "pnl_pct", label: "P&L %" }, { key: "avg", label: "Avg" }, { key: "price", label: "Current" }],
  resolved: [{ key: "pnl", label: "P&L" }, { key: "value", label: "Value" }],
  closed: [{ key: "recent", label: "Recent" }, { key: "pnl", label: "P&L" }, { key: "avg", label: "Avg" }],
};

function Positions({ wallet, status }: { wallet: string; status: "active" | "resolved" | "closed" }) {
  const [sort, setSort] = useState(SORTS[status][0].key);
  const [dir, setDir] = useState(status === "resolved" ? "asc" : "desc");
  const [page, setPage] = useState(0);
  const [search, setSearch] = useState("");
  const [q, setQ] = useState("");
  useEffect(() => { const t = setTimeout(() => setQ(search.trim()), 400); return () => clearTimeout(t); }, [search]);
  useEffect(() => setPage(0), [sort, dir, q]);
  const PAGE = 50;
  const { data, isLoading, error } = useQuery({
    queryKey: ["pm-pos", wallet, status, sort, dir, page, q],
    queryFn: () => pm.positions(wallet, { status, sort: sort === "value" || sort === "recent" ? undefined : sort, dir, limit: PAGE, offset: page * PAGE, q: q || undefined }),
  });
  const closed = status === "closed";
  return (
    <div className="space-y-2">
      <div className="flex items-center gap-2 flex-wrap">
        <div className="relative flex-1 min-w-[200px]">
          <Search size={13} className="absolute left-2 top-1/2 -translate-y-1/2 text-gray-500" />
          <input value={search} onChange={(e) => setSearch(e.target.value)} placeholder="Search positions"
            className="w-full bg-surface border border-glass rounded-md pl-7 pr-2 py-1 text-xs focus:outline-none focus:border-violet/50" />
        </div>
        <Pills options={SORTS[status]} value={sort} onChange={setSort} />
        <Pills options={[{ key: "desc", label: "↓" }, { key: "asc", label: "↑" }]} value={dir} onChange={setDir} />
      </div>
      {status === "resolved" && <div className="text-[11px] text-gray-500">Markets that have settled but the wallet hasn't redeemed. These are mostly lost bets (redeeming a loser pays nothing), so Polymarket never lists them as closed. They count toward the win rate here.</div>}
      {isLoading && <Msg>Loading positions…</Msg>}
      {error && <Msg tone="error">{(error as Error).message}</Msg>}
      {data && data.rows.length === 0 && <Msg>No positions.</Msg>}
      {data && data.rows.length > 0 && (
        <div className="overflow-auto rounded-lg border border-glass">
          <table className="w-full text-sm">
            <thead className="bg-surface text-[11px] uppercase text-gray-400">
              <tr>
                <th className="text-left px-3 py-2">Market</th><th className="text-left px-3 py-2">Outcome</th>
                <th className="text-right px-3 py-2">Avg</th><th className="text-right px-3 py-2">{closed ? "Settle" : "Current"}</th>
                <th className="text-right px-3 py-2">{closed ? "Bought" : "Value"}</th><th className="text-right px-3 py-2">P&L</th>
              </tr>
            </thead>
            <tbody>{data.rows.map((p, i) => <PosRow key={i} p={p} closed={closed} />)}</tbody>
          </table>
        </div>
      )}
      {data && (
        <div className="flex justify-end gap-2 text-xs text-gray-400 items-center">
          <button disabled={page === 0} onClick={() => setPage((x) => x - 1)} className="px-2 py-0.5 rounded border border-glass disabled:opacity-30">Prev</button>
          <span>{page * PAGE + 1}–{page * PAGE + data.rows.length}</span>
          <button disabled={data.rows.length < PAGE} onClick={() => setPage((x) => x + 1)} className="px-2 py-0.5 rounded border border-glass disabled:opacity-30">Next</button>
        </div>
      )}
    </div>
  );
}

function PosRow({ p, closed }: { p: Position; closed: boolean }) {
  const pnl = closed ? p.realized_pnl : p.cash_pnl;
  const outcomeTone = /^(no|under)$/i.test(p.outcome ?? "") ? "bg-crimson/15 text-crimson-light" : "bg-emerald/15 text-emerald-light";
  return (
    <tr className="border-t border-glass hover:bg-white/[0.03]">
      <td className="px-3 py-2">
        <div className="flex items-center gap-2 min-w-0">
          {p.icon ? <img src={p.icon} alt="" className="w-8 h-8 rounded shrink-0 object-cover" /> : <div className="w-8 h-8 rounded bg-white/5 shrink-0" />}
          <div className="min-w-0">
            <a href={p.event_slug ? `https://polymarket.com/event/${p.event_slug}` : undefined} target="_blank" rel="noreferrer"
              className="block truncate max-w-[420px] hover:underline" title={p.title ?? ""}>{p.title}</a>
            <div className="text-[11px] text-gray-600">
              {p.size != null && !closed && `${p.size.toLocaleString(undefined, { maximumFractionDigits: 1 })} shares`}
              {closed && p.closed_at && timeAgo(p.closed_at)}
              {p.end_date && ` · ends ${p.end_date.slice(0, 10)}`}
            </div>
          </div>
        </div>
      </td>
      <td className="px-3 py-2"><span className={clsx("text-xs px-1.5 py-0.5 rounded", outcomeTone)}>{p.outcome}</span></td>
      <td className="px-3 py-2 text-right font-mono text-gray-300">{cents(p.avg_price)}</td>
      <td className="px-3 py-2 text-right font-mono">{cents(p.cur_price)}</td>
      <td className="px-3 py-2 text-right font-mono text-gray-300">{formatUsd(closed ? (p.total_bought != null && p.avg_price != null ? p.total_bought * p.avg_price : null) : p.current_value)}</td>
      <td className="px-3 py-2 text-right">
        <Money v={pnl} signed />
        {!closed && p.percent_pnl != null && <div className="text-[10px] text-gray-500 font-mono">{p.percent_pnl.toFixed(1)}%</div>}
      </td>
    </tr>
  );
}

function ActivityTab({ wallet }: { wallet: string }) {
  const [type, setType] = useState("");
  const [page, setPage] = useState(0);
  useEffect(() => setPage(0), [type]);
  const { data, isLoading, error } = useQuery({
    queryKey: ["pm-act", wallet, type, page],
    queryFn: () => pm.activity(wallet, { type: type || undefined, limit: 50, offset: page * 50 }),
    refetchInterval: 30_000,
  });
  return (
    <div className="space-y-2">
      <Pills value={type} onChange={setType} options={[
        { key: "", label: "All" }, { key: "trade", label: "Trades" }, { key: "redeem", label: "Redeems" },
        { key: "split", label: "Splits" }, { key: "merge", label: "Merges" }, { key: "reward", label: "Rewards" },
      ]} />
      {isLoading && <Msg>Loading activity…</Msg>}
      {error && <Msg tone="error">{(error as Error).message}</Msg>}
      {data && data.rows.length === 0 && <Msg>No activity.</Msg>}
      {data && data.rows.length > 0 && (
        <div className="overflow-auto rounded-lg border border-glass">
          <table className="w-full text-sm">
            <thead className="bg-surface text-[11px] uppercase text-gray-400">
              <tr><th className="text-left px-3 py-2">When</th><th className="text-left px-3 py-2">Type</th><th className="text-left px-3 py-2">Market</th>
                <th className="text-left px-3 py-2">Outcome</th><th className="text-right px-3 py-2">Price</th><th className="text-right px-3 py-2">Shares</th><th className="text-right px-3 py-2">USDC</th></tr>
            </thead>
            <tbody>
              {data.rows.map((a, i) => (
                <tr key={i} className="border-t border-glass">
                  <td className="px-3 py-2 text-xs text-gray-500 whitespace-nowrap" title={new Date(a.ts * 1000).toLocaleString()}>{timeAgo(a.ts)}</td>
                  <td className="px-3 py-2 text-xs">
                    <span className={clsx("px-1.5 py-0.5 rounded",
                      a.side === "BUY" ? "bg-emerald/15 text-emerald-light" : a.side === "SELL" ? "bg-crimson/15 text-crimson-light" : "bg-white/5 text-gray-300")}>
                      {a.type === "TRADE" ? a.side : a.type}
                    </span>
                  </td>
                  <td className="px-3 py-2 truncate max-w-[380px]" title={a.title ?? ""}>{a.title}</td>
                  <td className="px-3 py-2 text-gray-400">{a.outcome}</td>
                  <td className="px-3 py-2 text-right font-mono">{a.type === "TRADE" ? cents(a.price) : "—"}</td>
                  <td className="px-3 py-2 text-right font-mono text-gray-300">{a.size?.toLocaleString(undefined, { maximumFractionDigits: 1 })}</td>
                  <td className="px-3 py-2 text-right font-mono">{formatUsd(a.usdc)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {data && (
        <div className="flex justify-end gap-2 text-xs text-gray-400">
          <button disabled={page === 0} onClick={() => setPage((x) => x - 1)} className="px-2 py-0.5 rounded border border-glass disabled:opacity-30">Prev</button>
          <button disabled={data.rows.length < 50} onClick={() => setPage((x) => x + 1)} className="px-2 py-0.5 rounded border border-glass disabled:opacity-30">Next</button>
        </div>
      )}
    </div>
  );
}
