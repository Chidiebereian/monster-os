import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "../lib/api";
import { useSettings } from "../store/settings";
import { formatAmerican, formatPct, formatUsd, kellyFraction } from "../lib/odds";
import { Card, Input, Msg, PageTitle, Pills, Stat, Table, Td, Th } from "../components/ui";

// Module 6: +EV Finder. Surfaces the odds-board rows where the best available
// number beats the no-vig fair (positive edge), with quarter-Kelly sizing; plus
// a Calculators panel exposing pricing-engine + kelly-service tools.
export function EVFinder() {
  return (
    <div className="space-y-6">
      <PageTitle title="+EV Finder" subtitle="positive expected value vs no-vig fair · line calculators" />
      <PositiveEv />
      <Calculators />
    </div>
  );
}

function PositiveEv() {
  const { sportKey, bankroll } = useSettings();
  const { data, isLoading, error } = useQuery({
    queryKey: ["odds-screen", sportKey],
    queryFn: () => api.oddsScreen(sportKey),
    refetchInterval: 10_000,
  });
  const rows = (data ?? [])
    .filter((r) => r.edge != null && r.edge > 0 && r.fair_probability != null)
    .sort((a, b) => (b.edge ?? 0) - (a.edge ?? 0));

  return (
    <div className="space-y-2">
      <h2 className="font-medium">Positive-EV board</h2>
      {isLoading && <Msg>Scanning the board…</Msg>}
      {error && <Msg tone="error">{(error as Error).message}</Msg>}
      {data && rows.length === 0 && <Msg>No +EV numbers on the board right now (all best prices ≤ fair).</Msg>}
      {rows.length > 0 && (
        <Table head={<tr><Th>Game</Th><Th>Outcome</Th><Th className="text-right">Best</Th><Th>Book</Th><Th className="text-right">Fair</Th><Th className="text-right">Edge</Th><Th className="text-right">¼-Kelly</Th></tr>}>
          {rows.map((r, i) => {
            const k = kellyFraction(r.fair_probability as number, r.best_price);
            return (
              <tr key={i} className="border-t border-glass hover:bg-white/[0.03]">
                <Td className="text-gray-300 whitespace-nowrap">{r.game}</Td>
                <Td className="font-medium">{r.outcome}</Td>
                <Td className="text-right font-mono text-emerald-light">{formatAmerican(r.best_price)}</Td>
                <Td className="text-gray-400">{r.best_book}</Td>
                <Td className="text-right font-mono text-gray-400">{formatAmerican(r.fair_odds)}</Td>
                <Td className="text-right font-mono text-emerald-light">+{formatPct(r.edge)}</Td>
                <Td className="text-right font-mono">{formatUsd(k * bankroll)} <span className="text-gray-600 text-xs">({formatPct(k)})</span></Td>
              </tr>
            );
          })}
        </Table>
      )}
    </div>
  );
}

// ---- Calculators (pricing-engine + kelly-service) ----

const CALCS = ["No-vig", "Implied", "EV", "Kelly", "Hedge", "Parlay"];

function Calculators() {
  const [tab, setTab] = useState("No-vig");
  return (
    <Card className="space-y-4">
      <div className="flex items-center gap-3">
        <h2 className="font-medium">Calculators</h2>
        <Pills options={CALCS.map((c) => ({ key: c, label: c }))} value={tab} onChange={setTab} />
      </div>
      {tab === "No-vig" && <NoVigCalc />}
      {tab === "Implied" && <ImpliedCalc />}
      {tab === "EV" && <EvCalc />}
      {tab === "Kelly" && <KellyCalc />}
      {tab === "Hedge" && <HedgeCalc />}
      {tab === "Parlay" && <ParlayCalc />}
    </Card>
  );
}

function Field({ label, value, set }: { label: string; value: string; set: (v: string) => void }) {
  return <label className="text-xs text-gray-500">{label}<Input type="number" value={value} onChange={set} className="w-full mt-1" /></label>;
}

function Run({ onClick }: { onClick: () => void }) {
  return <button onClick={onClick} className="px-3 py-1.5 text-sm rounded-md bg-violet/20 text-violet-light border border-violet/30 hover:bg-violet/30">Compute</button>;
}

function NoVigCalc() {
  const [a, setA] = useState("-150");
  const [b, setB] = useState("130");
  const q = useQuery({ queryKey: ["novig", a, b], queryFn: () => api.novig(Number(a), Number(b)), enabled: false });
  return (
    <div className="grid md:grid-cols-4 gap-4 items-end">
      <Field label="Odds A" value={a} set={setA} />
      <Field label="Odds B" value={b} set={setB} />
      <Run onClick={() => q.refetch()} />
      {q.data && <div className="col-span-4 grid grid-cols-4 gap-4 pt-3 border-t border-glass">
        <Stat label="Hold" value={formatPct(q.data.hold)} />
        <Stat label="Fair A" value={formatPct(q.data.fair_probability_a)} />
        <Stat label="Fair odds A" value={formatAmerican(q.data.fair_odds_a)} />
        <Stat label="Fair odds B" value={formatAmerican(q.data.fair_odds_b)} />
      </div>}
    </div>
  );
}

function ImpliedCalc() {
  const [o, setO] = useState("-150");
  const q = useQuery({ queryKey: ["implied", o], queryFn: () => api.implied(Number(o)), enabled: false });
  return (
    <div className="grid md:grid-cols-4 gap-4 items-end">
      <Field label="American odds" value={o} set={setO} />
      <Run onClick={() => q.refetch()} />
      {q.data && <Stat label="Implied probability" value={formatPct(q.data.implied_probability)} />}
    </div>
  );
}

function EvCalc() {
  const stake0 = String(useSettings.getState().bankroll);
  const [p, setP] = useState("0.55");
  const [o, setO] = useState("120");
  const [stake, setStake] = useState(stake0);
  const q = useQuery({ queryKey: ["ev", p, o, stake], queryFn: () => api.ev(Number(p), Number(o), Number(stake)), enabled: false });
  return (
    <div className="grid md:grid-cols-5 gap-4 items-end">
      <Field label="Model prob" value={p} set={setP} />
      <Field label="Odds" value={o} set={setO} />
      <Field label="Stake" value={stake} set={setStake} />
      <Run onClick={() => q.refetch()} />
      {q.data && <div className="col-span-5 grid grid-cols-3 gap-4 pt-3 border-t border-glass">
        <Stat label="EV" value={formatUsd(q.data.ev)} tone={q.data.ev >= 0 ? "pos" : "neg"} />
        <Stat label="ROI" value={formatPct(q.data.roi)} tone={q.data.roi >= 0 ? "pos" : "neg"} />
        <Stat label="Win / lose" value={`${formatUsd(q.data.profit_if_win)} / -${formatUsd(q.data.loss_if_lose)}`} />
      </div>}
    </div>
  );
}

function KellyCalc() {
  const [p, setP] = useState("0.55");
  const [o, setO] = useState("120");
  const [f, setF] = useState("0.25");
  const q = useQuery({ queryKey: ["kelly", p, o, f], queryFn: () => api.kelly(Number(p), Number(o), Number(f)), enabled: false });
  return (
    <div className="grid md:grid-cols-5 gap-4 items-end">
      <Field label="Model prob" value={p} set={setP} />
      <Field label="Odds" value={o} set={setO} />
      <Field label="Fraction" value={f} set={setF} />
      <Run onClick={() => q.refetch()} />
      {q.data && <div className="col-span-5 grid grid-cols-3 gap-4 pt-3 border-t border-glass">
        <Stat label="Full Kelly" value={formatPct(q.data.full_kelly)} />
        <Stat label="Staked fraction" value={formatPct(q.data.stake_fraction)} tone="pos" />
        <Stat label="Edge / unit" value={q.data.edge_per_unit.toFixed(3)} />
      </div>}
    </div>
  );
}

function HedgeCalc() {
  const [oa, setOa] = useState("150");
  const [sa, setSa] = useState("100");
  const [ob, setOb] = useState("-120");
  const q = useQuery({ queryKey: ["hedge", oa, sa, ob], queryFn: () => api.hedge(Number(oa), Number(sa), Number(ob)), enabled: false });
  return (
    <div className="grid md:grid-cols-5 gap-4 items-end">
      <Field label="Odds A" value={oa} set={setOa} />
      <Field label="Stake A" value={sa} set={setSa} />
      <Field label="Odds B" value={ob} set={setOb} />
      <Run onClick={() => q.refetch()} />
      {q.data && <div className="col-span-5 grid grid-cols-3 gap-4 pt-3 border-t border-glass">
        <Stat label="Hedge stake" value={formatUsd(q.data.hedge_stake)} />
        <Stat label="Guaranteed profit" value={formatUsd(q.data.guaranteed_profit)} tone={q.data.guaranteed_profit >= 0 ? "pos" : "neg"} />
        <Stat label="Total outlay" value={formatUsd(q.data.total_outlay)} />
      </div>}
    </div>
  );
}

function ParlayCalc() {
  const [legs, setLegs] = useState("-110,-110,120");
  const q = useQuery({ queryKey: ["parlay", legs], queryFn: () => api.parlay(legs), enabled: false });
  return (
    <div className="grid md:grid-cols-4 gap-4 items-end">
      <label className="text-xs text-gray-500 md:col-span-2">Legs (comma-separated American)<Input value={legs} onChange={setLegs} className="w-full mt-1" /></label>
      <Run onClick={() => q.refetch()} />
      {q.data && <div className="col-span-4 grid grid-cols-3 gap-4 pt-3 border-t border-glass">
        <Stat label="Combined odds" value={formatAmerican(q.data.combined_odds)} />
        <Stat label="Decimal" value={q.data.combined_decimal.toFixed(2)} />
        <Stat label="Implied prob" value={formatPct(q.data.combined_probability)} />
      </div>}
    </div>
  );
}
