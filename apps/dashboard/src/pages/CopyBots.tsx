// Copy Bots: every strategy following a Polymarket wallet, with what each one
// copied or skipped and how the (paper) positions are doing.
import { Fragment, useState } from "react";
import { Link } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import clsx from "clsx";
import { AlertTriangle, ChevronDown, ChevronRight, Loader2, Pause, Pencil, Play, Plus, RefreshCw, Trash2 } from "lucide-react";
import { Msg, Pills } from "../components/ui";
import { Avatar, Money } from "../components/pm";
import { CopyBotModal } from "../components/CopyBotModal";
import { formatPct, formatUsd } from "../lib/odds";
import { cents, displayName, shortAddr, timeAgo } from "../lib/pm";
import { bots, type Bot, type BotConfig, type BotEvent, type BotPosition } from "../lib/bots";

const ago = (iso: string | null) => (iso ? timeAgo(Date.parse(iso) / 1000) : "—");

function sizingLabel(c: BotConfig): string {
  switch (c.buy_mode) {
    case "fixed": return `$${c.buy_fixed} per copy`;
    case "range": return `${c.buy_pct}% ($${c.range_min ?? 0}–${c.range_max != null ? "$" + c.range_max : "∞"})`;
    case "portfolio": return `portfolio % of $${c.bankroll}`;
    default: return `${c.buy_pct}% of leader size`;
  }
}

export function CopyBots() {
  const qc = useQueryClient();
  const { data, isLoading, error, isFetching, refetch } = useQuery({ queryKey: ["bots"], queryFn: bots.list, refetchInterval: 15_000 });
  const [open, setOpen] = useState<number | null>(null);
  const [modal, setModal] = useState<{ bot?: Bot } | null>(null);
  const runNow = useMutation({ mutationFn: bots.run, onSettled: () => qc.invalidateQueries({ queryKey: ["bots"] }) });
  const rows = data?.rows ?? [];
  const active = rows.filter((b) => b.status === "active").length;

  return (
    <div className="space-y-3">
      <div className="flex items-center gap-3 flex-wrap">
        <h2 className="font-semibold">Copied Traders</h2>
        <span className="text-xs px-1.5 rounded bg-white/10 text-gray-300">{rows.length}</span>
        <span className="text-xs px-2 py-0.5 rounded-full bg-emerald/15 text-emerald-light inline-flex items-center gap-1">
          <span className="h-1.5 w-1.5 rounded-full bg-emerald" /> {active} active
        </span>
        <span className="text-[11px] px-1.5 py-0.5 rounded bg-amber/15 text-amber">PAPER — no real orders</span>
        <div className="ml-auto flex items-center gap-2">
          <button onClick={() => setModal({})} className="inline-flex items-center gap-1.5 rounded-md bg-violet px-3 py-1.5 text-sm font-medium text-white hover:bg-violet/90">
            <Plus size={14} /> Create Copy Bot
          </button>
          <button onClick={() => { runNow.mutate(); refetch(); }} className="inline-flex items-center gap-1.5 rounded-md border border-glass px-3 py-1.5 text-sm text-gray-300 hover:bg-white/5">
            <RefreshCw size={13} className={clsx((isFetching || runNow.isPending) && "animate-spin")} /> Refresh
          </button>
        </div>
      </div>

      {isLoading && <Msg>Loading bots…</Msg>}
      {error && <Msg tone="error">{(error as Error).message}</Msg>}
      {data && rows.length === 0 && (
        <Msg>
          No copy bots yet. Click <b className="text-gray-200">Create Copy Bot</b>, search a trader (e.g. swisstony) and name the strategy,
          or hit <b className="text-gray-200">Copy Trade</b> on any wallet's profile. You'll get an alert every time they trade.
        </Msg>
      )}
      {rows.length > 0 && (
        <div className="overflow-auto rounded-lg border border-glass">
          <table className="w-full text-sm">
            <thead className="bg-surface text-gray-400 text-[11px] uppercase tracking-wide">
              <tr>
                <th className="w-8" />
                <th className="text-left px-3 py-2">Strategy / Trader</th>
                <th className="text-right px-3 py-2">Positions</th>
                <th className="text-right px-3 py-2">Spent</th>
                <th className="text-right px-3 py-2">Budget</th>
                <th className="text-right px-3 py-2">Record</th>
                <th className="text-right px-3 py-2">P&L</th>
                <th className="text-right px-3 py-2">ROI</th>
                <th className="text-left px-3 py-2">Last active</th>
                <th className="text-right px-3 py-2">Actions</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((b) => (
                <Fragment key={b.id}>
                  <BotRow b={b} expanded={open === b.id} onToggle={() => setOpen(open === b.id ? null : b.id)} onEdit={() => setModal({ bot: b })} />
                  {open === b.id && (
                    <tr className="bg-obsidian/40"><td colSpan={10} className="p-3"><BotDetailPanel id={b.id} /></td></tr>
                  )}
                </Fragment>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {modal && <CopyBotModal bot={modal.bot} onClose={() => setModal(null)} />}
    </div>
  );
}

function StatusBadge({ s }: { s: Bot["status"] }) {
  return (
    <span className={clsx("text-[10px] px-1.5 py-0.5 rounded font-medium",
      s === "active" ? "bg-emerald/15 text-emerald-light" : s === "paused" ? "bg-amber/15 text-amber" : "bg-white/10 text-gray-400")}>
      {s.charAt(0).toUpperCase() + s.slice(1)}
    </span>
  );
}

function BotRow({ b, expanded, onToggle, onEdit }: { b: Bot; expanded: boolean; onToggle: () => void; onEdit: () => void }) {
  const qc = useQueryClient();
  const s = b.summary;
  const toggle = useMutation({
    mutationFn: () => bots.update(b.id, { status: b.status === "active" ? "paused" : "active" }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["bots"] }),
  });
  const del = useMutation({ mutationFn: () => bots.remove(b.id), onSuccess: () => qc.invalidateQueries({ queryKey: ["bots"] }) });
  const graded = s.wins + s.losses;
  return (
    <tr className="border-t border-glass hover:bg-white/[0.03] cursor-pointer" onClick={onToggle}>
      <td className="pl-3 text-gray-500">{expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}</td>
      <td className="px-3 py-2.5">
        <div className="flex items-center gap-2.5 min-w-0">
          <Avatar wallet={b.wallet} src={b.trader_image} size={32} />
          <div className="min-w-0">
            <div className="font-medium flex items-center gap-2">
              <span className="truncate max-w-[200px]">{b.strategy}</span> <StatusBadge s={b.status} />
              {(s.unseen ?? 0) > 0 && <span className="text-[10px] px-1.5 rounded-full bg-violet text-white">{s.unseen} new</span>}
            </div>
            <div className="text-[11px] text-gray-500">
              <Link to={`/wallets/${b.wallet}`} onClick={(e) => e.stopPropagation()} className="hover:underline">{displayName(b.trader_name, b.wallet)}</Link>
              {" · "}<span className="font-mono">{shortAddr(b.wallet)}</span>{" · "}{sizingLabel(b.config)}
            </div>
          </div>
        </div>
      </td>
      <td className="px-3 py-2 text-right">
        <div className="font-mono">{s.positions_open}</div>
        <div className="text-[10px] text-gray-500">{s.positions_redeemable ? `${s.positions_redeemable} redeemable` : "open"}</div>
      </td>
      <td className="px-3 py-2 text-right">
        <div className="font-mono">{formatUsd(s.spent)}</div>
        <div className="text-[10px] text-gray-500">{s.buys} buys · {s.skipped ?? 0} skipped</div>
      </td>
      <td className="px-3 py-2 text-right">
        <div className="font-mono">{b.config.budget != null ? formatUsd(b.config.budget) : "No limit"}</div>
        {b.config.budget != null && <div className="text-[10px] text-gray-500">{formatPct(Math.min(1, s.spent / b.config.budget), 0)} used</div>}
      </td>
      <td className="px-3 py-2 text-right">
        <div className="font-mono">{s.wins}-{s.losses}</div>
        <div className="text-[10px] text-gray-500">{graded ? `${formatPct(s.win_rate, 0)} win rate` : "no graded trades"}</div>
      </td>
      <td className="px-3 py-2 text-right">
        <div><Money v={s.total_pnl} signed /></div>
        <div className="text-[10px] text-gray-500">realized <Money v={s.realized_pnl} signed /></div>
      </td>
      <td className="px-3 py-2 text-right font-mono">
        <span className={clsx(s.roi == null ? "text-gray-600" : s.roi >= 0 ? "text-emerald-light" : "text-crimson-light")}>
          {s.roi == null ? "—" : `${s.roi >= 0 ? "+" : ""}${s.roi.toFixed(1)}%`}
        </span>
      </td>
      <td className="px-3 py-2">
        <div className={b.last_trade_at ? "text-gray-200" : "text-gray-500"} title={b.last_trade_at ? undefined : "No trades by this leader since the bot was created"}>
          {b.last_trade_at ? ago(b.last_trade_at) : "Waiting"}
        </div>
        <div className="text-[10px] text-gray-500 flex items-center gap-1">
          {b.last_error ? <><AlertTriangle size={10} className="text-amber" /><span title={b.last_error}>poll error</span></>
            : b.status === "active" ? `checked ${ago(b.last_checked_at)}` : b.status}
        </div>
      </td>
      <td className="px-3 py-2" onClick={(e) => e.stopPropagation()}>
        <div className="flex justify-end gap-1.5">
          <button title="Edit" onClick={onEdit} className="p-1.5 rounded border border-glass text-gray-300 hover:bg-white/5"><Pencil size={13} /></button>
          <button title={b.status === "active" ? "Pause" : "Resume (skips trades made while paused)"} onClick={() => toggle.mutate()}
            className={clsx("p-1.5 rounded border border-glass", b.status === "active" ? "text-amber hover:bg-amber/10" : "text-emerald-light hover:bg-emerald/10")}>
            {toggle.isPending ? <Loader2 size={13} className="animate-spin" /> : b.status === "active" ? <Pause size={13} /> : <Play size={13} />}
          </button>
          <button title="Delete" onClick={() => confirm(`Delete "${b.strategy}" and its history?`) && del.mutate()}
            className="p-1.5 rounded border border-glass text-crimson-light hover:bg-crimson/10"><Trash2 size={13} /></button>
        </div>
      </td>
    </tr>
  );
}

function BotDetailPanel({ id }: { id: number }) {
  const qc = useQueryClient();
  const { data, isLoading, error } = useQuery({ queryKey: ["bot", id], queryFn: () => bots.get(id), refetchInterval: 15_000 });
  const [tab, setTab] = useState("open");
  const seen = useMutation({ mutationFn: () => bots.markSeen(id), onSuccess: () => { qc.invalidateQueries({ queryKey: ["bots"] }); qc.invalidateQueries({ queryKey: ["bot-alerts"] }); } });
  if (isLoading) return <div className="text-sm text-gray-500 py-4 text-center"><Loader2 size={14} className="inline animate-spin mr-2" />Loading positions…</div>;
  if (error || !data) return <div className="text-sm text-crimson-light">{(error as Error)?.message}</div>;
  const by = (st: BotPosition["status"]) => data.positions.filter((p) => p.status === st);
  const unseen = data.events.filter((e) => !e.seen).length;
  return (
    <div className="space-y-3">
      <div className="flex items-center gap-3 flex-wrap">
        <Pills value={tab} onChange={setTab} options={[
          { key: "open", label: `Active ${by("open").length}` },
          { key: "redeemable", label: `Redeemable ${by("redeemable").length}` },
          { key: "closed", label: `Closed ${by("closed").length}` },
          { key: "activity", label: `Leader trades ${data.events.length}` },
        ]} />
        {unseen > 0 && <button onClick={() => seen.mutate()} className="text-xs text-violet-light hover:underline">Mark {unseen} alerts read</button>}
        <div className="ml-auto flex gap-5 text-right text-[11px]">
          <div><div className="text-gray-500 uppercase">Open value</div><span className="font-mono">{formatUsd(data.summary.open_value)}</span></div>
          <div><div className="text-gray-500 uppercase">Unrealized</div><Money v={data.summary.unrealized_pnl} signed /></div>
          <div><div className="text-gray-500 uppercase">Realized</div><Money v={data.summary.realized_pnl} signed /></div>
          <div><div className="text-gray-500 uppercase">Record</div><span className="font-mono">{data.summary.wins}-{data.summary.losses}</span></div>
        </div>
      </div>
      <div className="text-[11px] text-gray-500">
        Watching since {new Date(data.created_at).toLocaleString()}. Only trades the leader makes from then on are copied — positions they already held aren't.
      </div>
      {tab === "activity" ? <EventList events={data.events} /> : <PositionList rows={by(tab as BotPosition["status"])} status={tab} />}
    </div>
  );
}

function MarketCell({ title, slug, event_slug, icon, outcome }: { title: string | null; slug: string | null; event_slug: string | null; icon: string | null; outcome: string | null }) {
  return (
    <div className="flex items-center gap-2 min-w-0">
      {icon && <img src={icon} alt="" className="w-6 h-6 rounded shrink-0" />}
      <div className="min-w-0">
        <a href={event_slug || slug ? `https://polymarket.com/event/${event_slug ?? slug}` : undefined} target="_blank" rel="noreferrer"
          className="block truncate max-w-[340px] text-gray-200 hover:underline" title={title ?? ""}>{title ?? "—"}</a>
        <div className="text-[11px] text-gray-500">{outcome}</div>
      </div>
    </div>
  );
}

function PositionList({ rows, status }: { rows: BotPosition[]; status: string }) {
  if (rows.length === 0) {
    return <div className="rounded-lg border border-dashed border-glass py-8 text-center text-sm text-gray-500">No {status === "open" ? "active" : status} positions for this bot.</div>;
  }
  return (
    <div className="overflow-auto rounded-lg border border-glass">
      <table className="w-full text-sm">
        <thead className="text-gray-500 text-[10px] uppercase tracking-wide bg-surface">
          <tr>
            <th className="text-left px-3 py-1.5">Market</th>
            <th className="text-right px-3 py-1.5">Shares</th>
            <th className="text-right px-3 py-1.5">Avg</th>
            <th className="text-right px-3 py-1.5">{status === "redeemable" ? "Settled" : "Now"}</th>
            <th className="text-right px-3 py-1.5">Bought</th>
            <th className="text-right px-3 py-1.5">{status === "closed" ? "Sold" : "Value"}</th>
            <th className="text-right px-3 py-1.5">P&L</th>
            <th className="text-right px-3 py-1.5">Fills</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((p) => (
            <tr key={p.asset} className="border-t border-glass">
              <td className="px-3 py-1.5"><MarketCell {...p} /></td>
              <td className="px-3 py-1.5 text-right font-mono">{p.shares ? p.shares.toFixed(1) : "—"}</td>
              <td className="px-3 py-1.5 text-right font-mono">{p.shares ? cents(p.cost / p.shares) : "—"}</td>
              <td className="px-3 py-1.5 text-right font-mono">{cents(p.price)}</td>
              <td className="px-3 py-1.5 text-right font-mono">{formatUsd(p.bought_usdc)}</td>
              <td className="px-3 py-1.5 text-right font-mono">{formatUsd(status === "closed" ? p.sold_usdc : p.value)}</td>
              <td className="px-3 py-1.5 text-right">
                <Money v={p.pnl} signed />
                {p.pnl_pct != null && <div className="text-[10px] text-gray-500">{p.pnl_pct >= 0 ? "+" : ""}{p.pnl_pct.toFixed(1)}%</div>}
              </td>
              <td className="px-3 py-1.5 text-right text-[11px] text-gray-400">{p.buys}B / {p.sells}S</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

export function EventList({ events }: { events: BotEvent[] }) {
  if (events.length === 0) {
    return <div className="rounded-lg border border-dashed border-glass py-8 text-center text-sm text-gray-500">Watching for the leader's next trade…</div>;
  }
  return (
    <div className="overflow-auto rounded-lg border border-glass max-h-[420px]">
      <table className="w-full text-sm">
        <thead className="text-gray-500 text-[10px] uppercase tracking-wide bg-surface sticky top-0">
          <tr>
            <th className="text-left px-3 py-1.5">When</th>
            <th className="text-left px-3 py-1.5">Market</th>
            <th className="text-left px-3 py-1.5">Leader</th>
            <th className="text-left px-3 py-1.5">Bot</th>
          </tr>
        </thead>
        <tbody>
          {events.map((e) => (
            <tr key={e.id} className={clsx("border-t border-glass", !e.seen && "bg-violet/[0.06]")}>
              <td className="px-3 py-1.5 text-[11px] text-gray-400 whitespace-nowrap">{ago(e.ts)}</td>
              <td className="px-3 py-1.5"><MarketCell {...e} /></td>
              <td className="px-3 py-1.5 whitespace-nowrap text-xs">
                <span className={clsx("px-1.5 py-0.5 rounded text-[10px] font-semibold mr-1.5", e.side === "BUY" ? "bg-emerald/15 text-emerald-light" : "bg-crimson/15 text-crimson-light")}>{e.side}</span>
                {e.trader_usdc != null ? <span className="font-mono">{formatUsd(e.trader_usdc)} @ {cents(e.trader_price)}</span> : <span className="text-gray-500">auto exit</span>}
              </td>
              <td className="px-3 py-1.5 text-xs">
                {e.action === "copied"
                  ? <span className="text-emerald-light">Copied <span className="font-mono">{formatUsd(e.copy_usdc)} @ {cents(e.copy_price)}</span>{e.reason && <span className="text-gray-500"> · {e.reason}</span>}</span>
                  : <span className="text-gray-400">Skipped · {e.reason}</span>}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
