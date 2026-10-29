// Create / edit a copy bot: pick the leader wallet, name the strategy, and set
// how buys and sells are mirrored plus the risk filters. Paper only.
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import clsx from "clsx";
import { ChevronDown, ChevronUp, HelpCircle, Loader2, RotateCcw, Search, X } from "lucide-react";
import { Avatar, Money } from "./pm";
import { formatPct, formatUsd } from "../lib/odds";
import { displayName, pm, shortAddr, walletAge } from "../lib/pm";
import { DEFAULT_CONFIG, bots, type Bot, type BotConfig } from "../lib/bots";

const CATEGORIES = ["POLITICS", "SPORTS", "CRYPTO", "CULTURE", "MENTIONS", "WEATHER", "ECONOMICS", "TECH", "FINANCE", "ESPORTS"];
const isAddr = (s: string) => /^0x[0-9a-fA-F]{40}$/.test(s.trim());

function Help({ text }: { text: string }) {
  return (
    <span className="relative group inline-flex">
      <HelpCircle size={12} className="text-gray-500" />
      <span className="pointer-events-none absolute left-4 top-0 z-20 hidden group-hover:block w-64 whitespace-pre-line rounded-md border border-glass bg-obsidian px-2.5 py-2 text-[11px] text-gray-300 shadow-xl">
        {text}
      </span>
    </span>
  );
}

function Seg<T extends string>({ value, options, onChange, tone = "violet" }: {
  value: T; options: { key: T; label: string }[]; onChange: (v: T) => void; tone?: "violet" | "emerald" | "crimson";
}) {
  const on = { violet: "bg-violet/25 text-violet-light", emerald: "bg-emerald/20 text-emerald-light", crimson: "bg-crimson/20 text-crimson-light" }[tone];
  return (
    <div className="grid rounded-md border border-glass overflow-hidden" style={{ gridTemplateColumns: `repeat(${options.length}, minmax(0,1fr))` }}>
      {options.map((o) => (
        <button key={o.key} type="button" onClick={() => onChange(o.key)}
          className={clsx("px-2 py-1.5 text-xs transition-colors", value === o.key ? on : "text-gray-400 hover:bg-white/5")}>
          {o.label}
        </button>
      ))}
    </div>
  );
}

/** Number input where empty means "no limit" (null). `scale` converts display units (e.g. ¢) to stored. */
function NumField({ value, onChange, unit, placeholder = "No limit", scale = 1, prefix }: {
  value: number | null; onChange: (v: number | null) => void; unit?: string; placeholder?: string; scale?: number; prefix?: string;
}) {
  const shown = value == null ? "" : String(+(value / scale).toFixed(4));
  const [txt, setTxt] = useState(shown);
  useEffect(() => setTxt(shown), [shown]);
  return (
    <div className="flex items-center rounded-md border border-glass bg-surface focus-within:border-violet/50">
      {prefix && <span className="pl-2 text-xs text-gray-500">{prefix}</span>}
      <input value={txt} inputMode="decimal" placeholder={placeholder}
        onChange={(e) => {
          setTxt(e.target.value);
          const t = e.target.value.trim();
          if (t === "") onChange(null);
          else if (!Number.isNaN(Number(t))) onChange(Number(t) * scale);
        }}
        className="w-full bg-transparent px-2 py-1.5 text-sm text-gray-100 placeholder:text-gray-600 focus:outline-none" />
      {unit && <span className="pr-2 text-xs text-gray-500">{unit}</span>}
    </div>
  );
}

function Row({ label, help, children }: { label: string; help: string; children: ReactNode }) {
  return (
    <div className="grid grid-cols-[minmax(0,1fr)_minmax(0,1.3fr)] items-center gap-3">
      <div className="text-sm text-gray-300 inline-flex items-center gap-1.5">{label} <Help text={help} /></div>
      <div>{children}</div>
    </div>
  );
}

function Label({ children, help }: { children: ReactNode; help?: string }) {
  return <div className="text-sm font-medium text-gray-200 inline-flex items-center gap-1.5 mb-1.5">{children}{help && <Help text={help} />}</div>;
}

function TraderPicker({ onPick }: { onPick: (w: string) => void }) {
  const [search, setSearch] = useState("");
  const [q, setQ] = useState("");
  useEffect(() => { const t = setTimeout(() => setQ(search.trim()), 350); return () => clearTimeout(t); }, [search]);
  const { data, isFetching } = useQuery({
    queryKey: ["pm-board-search", q],
    queryFn: () => pm.leaderboard({ q, limit: 8, window: "all" }),
    enabled: q.length >= 2 && !isAddr(q),
  });
  return (
    <div className="space-y-2">
      <div className="relative">
        <Search size={13} className="absolute left-2 top-1/2 -translate-y-1/2 text-gray-500" />
        <input autoFocus value={search} onChange={(e) => setSearch(e.target.value)} placeholder="Trader name (e.g. swisstony) or 0x wallet"
          className="w-full bg-surface border border-glass rounded-md pl-7 pr-2 py-2 text-sm focus:outline-none focus:border-violet/50" />
        {isFetching && <Loader2 size={13} className="absolute right-2 top-1/2 -translate-y-1/2 animate-spin text-gray-500" />}
      </div>
      {isAddr(q) && (
        <button type="button" onClick={() => onPick(q.toLowerCase())} className="w-full text-left rounded-md border border-violet/40 bg-violet/10 px-3 py-2 text-sm">
          Copy wallet <span className="font-mono">{shortAddr(q)}</span>
        </button>
      )}
      {data?.rows.map((r) => (
        <button key={r.wallet} type="button" onClick={() => onPick(r.wallet)}
          className="w-full flex items-center gap-2 rounded-md border border-glass px-3 py-2 hover:bg-white/5 text-left">
          <Avatar wallet={r.wallet} src={r.profile_image} size={26} />
          <div className="min-w-0 flex-1">
            <div className="text-sm truncate">{displayName(r.name, r.wallet)}</div>
            <div className="text-[11px] text-gray-500 font-mono">{shortAddr(r.wallet)} · #{r.rank} all-time</div>
          </div>
          <Money v={r.pnl} signed />
        </button>
      ))}
      {q.length >= 2 && !isAddr(q) && data && data.rows.length === 0 && (
        <div className="text-xs text-gray-500">No trader by that name on Polymarket's leaderboard. Paste their 0x wallet instead.</div>
      )}
    </div>
  );
}

function TraderCard({ wallet, onClear }: { wallet: string; onClear?: () => void }) {
  const { data: s, isLoading } = useQuery({ queryKey: ["pm-wallet", wallet], queryFn: () => pm.wallet(wallet), staleTime: 60_000 });
  return (
    <div className="rounded-lg border border-glass bg-obsidian/40 p-3 space-y-3">
      <div className="flex items-center gap-3">
        <Avatar wallet={wallet} src={s?.profile_image} size={40} />
        <div className="min-w-0 flex-1">
          <div className="font-medium truncate">{displayName(s?.name, wallet)}</div>
          <div className="text-[11px] text-gray-500 font-mono">{shortAddr(wallet)}</div>
        </div>
        <div className="text-right">
          <div className="font-mono text-sm">{formatUsd(s?.positions_value)}</div>
          <div className="text-[10px] text-gray-500">Open positions</div>
        </div>
        {onClear && <button type="button" onClick={onClear} className="text-gray-500 hover:text-gray-200"><X size={14} /></button>}
      </div>
      <div className="grid grid-cols-4 gap-2 text-[11px]">
        <div><div className="text-gray-500 uppercase">Volume</div><div className="font-mono text-gray-200">{formatUsd(s?.all_vol)}</div></div>
        <div><div className="text-gray-500 uppercase">Age</div><div className="text-gray-200">{walletAge(s?.age_days)}</div></div>
        <div><div className="text-gray-500 uppercase">Realized P&L</div><Money v={s?.all_pnl} signed /></div>
        <div><div className="text-gray-500 uppercase">Win rate</div><div className="font-mono text-gray-200">{s?.win_rate != null ? formatPct(s.win_rate, 1) : isLoading ? "…" : "—"}</div></div>
      </div>
    </div>
  );
}

export function CopyBotModal({ bot, wallet: presetWallet, onClose }: { bot?: Bot; wallet?: string; onClose: () => void }) {
  const qc = useQueryClient();
  const editing = !!bot;
  const [wallet, setWallet] = useState(bot?.wallet ?? presetWallet?.toLowerCase() ?? "");
  const [strategy, setStrategy] = useState(bot?.strategy ?? "");
  const [cfg, setCfg] = useState<BotConfig>({ ...DEFAULT_CONFIG, ...(bot?.config ?? {}) });
  const [advanced, setAdvanced] = useState(false);
  const [mirror, setMirror] = useState(false);
  const [mirrorErr, setMirrorErr] = useState<string | null>(null);
  const [catOpen, setCatOpen] = useState(false);
  const set = <K extends keyof BotConfig>(k: K, v: BotConfig[K]) => setCfg((c) => ({ ...c, [k]: v }));

  const profile = useQuery({ queryKey: ["pm-wallet", wallet], queryFn: () => pm.wallet(wallet), enabled: !!wallet, staleTime: 60_000 });
  // Default the strategy name to the trader's name.
  useEffect(() => {
    if (!editing && !strategy && profile.data) setStrategy(displayName(profile.data.name, wallet));
  }, [profile.data, editing, strategy, wallet]);

  const existing = useQuery({ queryKey: ["bots"], queryFn: bots.list });
  const advancedCount = useMemo(() => {
    const keys: (keyof BotConfig)[] = ["budget", "max_per_trade", "max_per_market", "min_price", "max_price", "max_slippage",
      "min_trigger", "min_volume", "min_liquidity", "max_resolution_days", "stop_loss_pct", "take_profit_pct", "expires_at"];
    return keys.filter((k) => cfg[k] != null).length + (cfg.category_mode !== "all" ? 1 : 0);
  }, [cfg]);

  const save = useMutation({
    mutationFn: () => editing
      ? bots.update(bot!.id, { strategy: strategy.trim(), config: cfg })
      : bots.create({ strategy: strategy.trim(), wallet, config: cfg, mirror_existing: mirror }),
    onSuccess: (b) => {
      qc.invalidateQueries({ queryKey: ["bots"] }); qc.invalidateQueries({ queryKey: ["bot"] });
      const err = b && "mirror_error" in b ? b.mirror_error : undefined;
      if (err) setMirrorErr(`Bot created, but copying existing positions failed: ${err}`); else onClose();
    },
  });

  const resetAdvanced = () => setCfg((c) => ({
    ...c, category_mode: "all", categories: [], budget: null, max_per_trade: null, max_per_market: null, min_price: null,
    max_price: null, max_slippage: null, min_trigger: null, min_volume: null, min_liquidity: null, max_resolution_days: null,
    stop_loss_pct: null, take_profit_pct: null, expires_at: null,
  }));
  const expiry = (days: number | null) => set("expires_at", days == null ? null : new Date(Date.now() + days * 86_400_000).toISOString());
  const expiryKey = cfg.expires_at == null ? "none" : "set";

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/70 p-3" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="w-full max-w-xl max-h-[92vh] flex flex-col rounded-xl border border-glass bg-surface shadow-2xl">
        <div className="flex items-center px-5 py-4 border-b border-glass">
          <h2 className="font-semibold">{editing ? "Edit Copy Bot" : "Copy Trade"}</h2>
          <span className="ml-2 text-[10px] px-1.5 py-0.5 rounded bg-amber/15 text-amber">PAPER</span>
          <button onClick={onClose} className="ml-auto text-gray-400 hover:text-gray-100"><X size={16} /></button>
        </div>

        <div className="flex-1 overflow-y-auto px-5 py-4 space-y-5">
          {wallet ? <TraderCard wallet={wallet} onClear={editing || presetWallet ? undefined : () => setWallet("")} /> : <TraderPicker onPick={setWallet} />}

          <div>
            <Label>Strategy Name</Label>
            <input value={strategy} onChange={(e) => setStrategy(e.target.value)} placeholder="e.g. Swisstony sports"
              maxLength={80} className="w-full bg-surface border border-glass rounded-md px-3 py-2 text-sm focus:outline-none focus:border-violet/50" />
          </div>

          {!editing && (existing.data?.rows.length ?? 0) > 0 && (
            <div>
              <Label help="Start from another bot's settings (sizes, sells and filters). The leader and name stay as set here.">Copy settings from</Label>
              <select defaultValue="" onChange={(e) => { const b = existing.data?.rows.find((x) => x.id === Number(e.target.value)); if (b) setCfg({ ...DEFAULT_CONFIG, ...b.config }); }}
                className="w-full bg-surface border border-glass rounded-md px-2 py-2 text-sm text-gray-200 focus:outline-none">
                <option value="">Defaults</option>
                {existing.data?.rows.map((b) => <option key={b.id} value={b.id}>{b.strategy}</option>)}
              </select>
            </div>
          )}

          <div>
            <Label help={"Standard: copy after the leader's trade is confirmed on Polymarket (we poll their activity every ~15 s).\n\nPriority (mempool, seconds earlier) isn't available: it needs a Polygon mempool feed."}>Copy signal</Label>
            <Seg value="standard" onChange={() => {}} options={[{ key: "standard", label: "Standard (confirmed trades)" }]} />
          </div>

          {!editing && (
            <label className="flex items-start gap-2.5 rounded-lg border border-glass px-3 py-2.5 cursor-pointer hover:bg-white/[0.03]">
              <input type="checkbox" checked={mirror} onChange={(e) => setMirror(e.target.checked)} className="accent-violet-500 mt-0.5" />
              <span className="text-sm">
                <span className="text-gray-200">Also copy their current open positions</span>
                <span className="block text-[11px] text-gray-500">
                  Off by default: the bot only copies trades made after it starts. When on, it buys into the trader's open positions now
                  (largest 50) at today's ask, sized and filtered by the settings below. You pay today's price, not theirs.
                </span>
              </span>
            </label>
          )}

          <div className="space-y-2">
            <Label help={"Percentage: match a % of the leader's trade size\n\nRange: a % with min/max dollar limits\n\nFixed: the same dollar amount every copy\n\nPortfolio %: mirror their allocation. If a trade is 1% of their open-position value, spend 1% of your bankroll."}>Buy Mode</Label>
            <Seg tone="emerald" value={cfg.buy_mode} onChange={(v) => set("buy_mode", v)}
              options={[{ key: "percentage", label: "Percentage" }, { key: "range", label: "Range" }, { key: "fixed", label: "Fixed" }, { key: "portfolio", label: "Portfolio %" }]} />
            {(cfg.buy_mode === "percentage" || cfg.buy_mode === "range") && (
              <Row label="Position percentage" help="Your copy = this % of the leader's dollar size. 100% = same size as them.">
                <NumField value={cfg.buy_pct} onChange={(v) => set("buy_pct", v ?? 0)} unit="%" placeholder="10" />
              </Row>
            )}
            {cfg.buy_mode === "range" && (
              <Row label="Min / max per copy" help="Each copy is clamped into this dollar range.">
                <div className="grid grid-cols-2 gap-2">
                  <NumField value={cfg.range_min} onChange={(v) => set("range_min", v)} prefix="$" placeholder="Min" />
                  <NumField value={cfg.range_max} onChange={(v) => set("range_max", v)} prefix="$" placeholder="Max" />
                </div>
              </Row>
            )}
            {cfg.buy_mode === "fixed" && (
              <Row label="Amount per copy" help="Spend exactly this on every copied buy (min $1).">
                <NumField value={cfg.buy_fixed} onChange={(v) => set("buy_fixed", v ?? 0)} prefix="$" placeholder="10" />
              </Row>
            )}
            {cfg.buy_mode === "portfolio" && (
              <Row label="Your bankroll" help="Paper bankroll the allocation % is applied to.">
                <NumField value={cfg.bankroll} onChange={(v) => set("bankroll", v ?? 0)} prefix="$" placeholder="1000" />
              </Row>
            )}
          </div>

          <div className="space-y-2">
            <Label help={"When the leader sells:\n\nSame %: sell the same share of our position as they sold of theirs\n\nFixed: sell a fixed dollar amount\n\nCustom %: sell this % of our position\n\nManual: don't auto-sell, just alert"}>Sell Mode</Label>
            <Seg tone="crimson" value={cfg.sell_mode} onChange={(v) => set("sell_mode", v)}
              options={[{ key: "same", label: "Same %" }, { key: "fixed", label: "Fixed" }, { key: "custom", label: "Custom %" }, { key: "manual", label: "Manual" }]} />
            {cfg.sell_mode === "custom" && (
              <Row label="Sell percentage" help="Share of our copied position sold whenever the leader sells.">
                <NumField value={cfg.sell_pct} onChange={(v) => set("sell_pct", v ?? 0)} unit="%" placeholder="100" />
              </Row>
            )}
            {cfg.sell_mode === "fixed" && (
              <Row label="Amount per sell" help="Dollars sold whenever the leader sells (capped at what we hold).">
                <NumField value={cfg.sell_fixed} onChange={(v) => set("sell_fixed", v ?? 0)} prefix="$" placeholder="10" />
              </Row>
            )}
          </div>

          <div className="rounded-lg border border-glass">
            <div className="flex items-center gap-2 px-3 py-2.5">
              <button type="button" onClick={() => setAdvanced((a) => !a)} className="flex items-center gap-2 text-sm font-medium flex-1 text-left">
                Advanced Settings <span className="text-[10px] px-1.5 rounded bg-white/10 text-gray-300">{advancedCount}</span>
              </button>
              <button type="button" onClick={resetAdvanced} className="text-xs text-gray-400 hover:text-gray-200 inline-flex items-center gap-1"><RotateCcw size={11} /> Reset</button>
              <button type="button" onClick={() => setAdvanced((a) => !a)} className="text-gray-400">{advanced ? <ChevronUp size={15} /> : <ChevronDown size={15} />}</button>
            </div>
            {advanced && (
              <div className="px-3 pb-3 space-y-3 border-t border-glass pt-3">
                <Row label="Category filter" help="Copy only (Include) or never (Exclude) trades in these Polymarket categories. Category comes from the market's Polymarket tags.">
                  <Seg value={cfg.category_mode} onChange={(v) => { set("category_mode", v); if (v !== "all") setCatOpen(true); }}
                    options={[{ key: "all", label: "All" }, { key: "include", label: "Include" }, { key: "exclude", label: "Exclude" }]} />
                </Row>
                {cfg.category_mode !== "all" && (
                  <div>
                    <button type="button" onClick={() => setCatOpen((o) => !o)}
                      className="w-full flex items-center justify-between rounded-md border border-glass px-3 py-1.5 text-xs text-gray-300">
                      {cfg.categories.length ? cfg.categories.map((c) => c.charAt(0) + c.slice(1).toLowerCase()).join(", ") : "Select categories…"}
                      {catOpen ? <ChevronUp size={13} /> : <ChevronDown size={13} />}
                    </button>
                    {catOpen && (
                      <div className="mt-1 grid grid-cols-2 gap-1 rounded-md border border-glass p-2">
                        {CATEGORIES.map((c) => (
                          <label key={c} className="flex items-center gap-2 text-xs text-gray-300 px-1 py-0.5 cursor-pointer">
                            <input type="checkbox" checked={cfg.categories.includes(c)} className="accent-violet-500"
                              onChange={(e) => set("categories", e.target.checked ? [...cfg.categories, c] : cfg.categories.filter((x) => x !== c))} />
                            {c.charAt(0) + c.slice(1).toLowerCase()}
                          </label>
                        ))}
                      </div>
                    )}
                  </div>
                )}
                <Row label="Budget" help="Total dollars this bot may spend on buys. Once used up, new buys are skipped."><NumField value={cfg.budget} onChange={(v) => set("budget", v)} prefix="$" /></Row>
                <Row label="Stop loss" help="Sell the whole position at the bid when its price falls this % below our average entry."><NumField value={cfg.stop_loss_pct} onChange={(v) => set("stop_loss_pct", v)} unit="%" placeholder="Off" /></Row>
                <Row label="Take profit" help="Sell the whole position at the bid when its price rises this % above our average entry."><NumField value={cfg.take_profit_pct} onChange={(v) => set("take_profit_pct", v)} unit="%" placeholder="Off" /></Row>
                <Row label="Entry price" help="Only copy buys whose fill price is inside this window (in cents).">
                  <div className="grid grid-cols-2 gap-2">
                    <NumField value={cfg.min_price} onChange={(v) => set("min_price", v)} unit="¢" placeholder="Min" scale={0.01} />
                    <NumField value={cfg.max_price} onChange={(v) => set("max_price", v)} unit="¢" placeholder="Max" scale={0.01} />
                  </div>
                </Row>
                <Row label="Max slippage" help="Skip a buy when the live ask is more than this many cents above the leader's price."><NumField value={cfg.max_slippage} onChange={(v) => set("max_slippage", v)} unit="¢" scale={0.01} /></Row>
                <Row label="Max per market" help="Most this bot will put into any one market."><NumField value={cfg.max_per_market} onChange={(v) => set("max_per_market", v)} prefix="$" /></Row>
                <Row label="Max per trade" help="Cap on any single copied buy."><NumField value={cfg.max_per_trade} onChange={(v) => set("max_per_trade", v)} prefix="$" /></Row>
                <Row label="Min volume" help="Only copy markets above this total volume."><NumField value={cfg.min_volume} onChange={(v) => set("min_volume", v)} prefix="$" /></Row>
                <Row label="Min liquidity" help="Only copy markets with at least this much liquidity."><NumField value={cfg.min_liquidity} onChange={(v) => set("min_liquidity", v)} prefix="$" /></Row>
                <Row label="Min trigger" help="Ignore the leader's small trades: only copy trades of at least this many dollars."><NumField value={cfg.min_trigger} onChange={(v) => set("min_trigger", v)} prefix="$" /></Row>
                <Row label="Max resolution" help="Skip markets that resolve further out than this.">
                  <Seg value={cfg.max_resolution_days == null ? "any" : String(cfg.max_resolution_days)}
                    onChange={(v) => set("max_resolution_days", v === "any" ? null : Number(v))}
                    options={[{ key: "any", label: "Any" }, { key: "1", label: "1D" }, { key: "7", label: "7D" }, { key: "30", label: "30D" }]} />
                </Row>
                <Row label="Copy expiry" help="The bot stops itself after this.">
                  <div className="space-y-1">
                    <Seg value={expiryKey} onChange={(v) => v === "none" && expiry(null)}
                      options={[{ key: "none", label: "None" }, { key: "set", label: cfg.expires_at ? new Date(cfg.expires_at).toLocaleDateString() : "Set…" }]} />
                    <div className="flex gap-1">
                      {[1, 7, 30].map((d) => (
                        <button key={d} type="button" onClick={() => expiry(d)} className="flex-1 text-[11px] rounded border border-glass py-0.5 text-gray-400 hover:bg-white/5">+{d}D</button>
                      ))}
                    </div>
                  </div>
                </Row>
              </div>
            )}
          </div>

          {mirrorErr && <div className="text-sm text-amber">{mirrorErr}</div>}
          {save.error && <div className="text-sm text-crimson-light">{(save.error as Error).message}</div>}
        </div>

        <div className="flex items-center gap-2 px-5 py-3 border-t border-glass">
          <span className="text-[11px] text-gray-500 flex-1">Paper trading: copies are simulated at the live order book. No orders are placed.</span>
          <button onClick={onClose} className="px-4 py-2 text-sm rounded-md border border-glass hover:bg-white/5">Cancel</button>
          <button disabled={!wallet || !strategy.trim() || save.isPending || !!mirrorErr} onClick={() => save.mutate()}
            className="px-4 py-2 text-sm rounded-md bg-violet text-white font-medium hover:bg-violet/90 disabled:opacity-40 inline-flex items-center gap-2">
            {save.isPending && <Loader2 size={13} className="animate-spin" />}{editing ? "Save" : "Start Copying"}
          </button>
        </div>
      </div>
    </div>
  );
}
