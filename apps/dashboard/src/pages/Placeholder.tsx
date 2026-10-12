import { Construction } from "lucide-react";

// Routes whose modules land in later phases (Projections, Arbitrage, Whale
// Watch, MonsterGPT). The backend endpoints already exist; the UI is pending.
export function Placeholder({ title, phase }: { title: string; phase: string }) {
  return (
    <div className="space-y-4">
      <h1 className="text-lg font-semibold">{title}</h1>
      <div className="rounded-lg border border-glass bg-surface/60 px-5 py-10 flex items-center gap-4 text-gray-400">
        <Construction className="text-amber" size={28} />
        <div>
          <div className="text-gray-200">{title} module — {phase}.</div>
          <div className="text-sm text-gray-500">
            The backend is live; this view is the next build phase.
          </div>
        </div>
      </div>
    </div>
  );
}
