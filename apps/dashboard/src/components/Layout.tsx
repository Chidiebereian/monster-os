import { NavLink, Outlet } from "react-router-dom";
import { Activity, TrendingUp, BarChart3, Scale, Waves, Radar, Users, Bot, Dog, Gauge, ClipboardList, Target, Dices, Tv } from "lucide-react";
import clsx from "clsx";
import { SPORTS, useSettings } from "../store/settings";

const NAV = [
  { to: "/odds", label: "Odds Screen", icon: Activity },
  { to: "/ev", label: "+EV Finder", icon: TrendingUp },
  { to: "/projections", label: "Projections", icon: BarChart3 },
  { to: "/cheatsheets", label: "Cheat Sheets", icon: ClipboardList },
  { to: "/hit-rates", label: "Hit Rates", icon: Target },
  { to: "/broadcast", label: "Game Broadcast", icon: Tv },
  { to: "/mlb-sim", label: "MLB Game Sim", icon: Dices },
  { to: "/arbitrage", label: "Arbitrage", icon: Scale },
  { to: "/whale-watch", label: "Whale Watch", icon: Waves },
  { to: "/signals", label: "Smart Signals", icon: Radar },
  { to: "/copy", label: "Top Traders", icon: Users },
  { to: "/backtest", label: "Performance", icon: Gauge },
  { to: "/monster-gpt", label: "MonsterGPT", icon: Bot },
];

export function Layout() {
  const { sportKey, setSport, bankroll, setBankroll } = useSettings();
  return (
    <div className="flex h-full">
      {/* Sidebar */}
      <aside className="hidden md:flex w-56 shrink-0 border-r border-glass bg-surface/60 flex-col">
        <div className="h-14 flex items-center gap-2 px-4 border-b border-glass">
          <Dog className="text-violet" size={20} />
          <span className="font-semibold tracking-tight">Monster<span className="text-violet">OS</span></span>
        </div>
        <nav className="flex-1 p-2 space-y-1">
          {NAV.map(({ to, label, icon: Icon }) => (
            <NavLink
              key={to}
              to={to}
              className={({ isActive }) =>
                clsx(
                  "flex items-center gap-3 rounded-md px-3 py-2 text-sm transition-colors",
                  isActive ? "bg-violet/15 text-violet-light" : "text-gray-400 hover:text-gray-100 hover:bg-white/5",
                )
              }
            >
              <Icon size={16} />
              {label}
            </NavLink>
          ))}
        </nav>
        <div className="p-3 text-[11px] text-gray-500 border-t border-glass">
          Math & simulation precede interface. Numbers come from the backend.
        </div>
      </aside>

      {/* Main */}
      <div className="flex-1 flex flex-col min-w-0">
        <header className="h-14 shrink-0 flex items-center gap-4 px-4 md:px-5 border-b border-glass bg-surface/40">
          <select
            value={sportKey}
            onChange={(e) => setSport(e.target.value)}
            className="bg-surface border border-glass rounded-md px-2 py-1 text-sm focus:outline-none focus:border-violet/50"
          >
            {SPORTS.map((s) => (
              <option key={s.key} value={s.key}>
                {s.label}
              </option>
            ))}
          </select>
          <label className="flex items-center gap-2 text-sm text-gray-400">
            Bankroll
            <input
              type="number"
              value={bankroll}
              min={1}
              onChange={(e) => setBankroll(Number(e.target.value) || 0)}
              className="w-24 bg-surface border border-glass rounded-md px-2 py-1 text-sm text-gray-100 focus:outline-none focus:border-violet/50"
            />
          </label>
          <div className="ml-auto hidden sm:flex items-center gap-2 text-xs text-gray-500">
            <span className="h-2 w-2 rounded-full bg-emerald animate-pulse" />
            gateway · :8090
          </div>
        </header>
        {/* small screens: the sidebar collapses into a scrollable nav row */}
        <nav className="md:hidden flex gap-1 overflow-x-auto border-b border-glass px-3 py-2">
          {NAV.map(({ to, label, icon: Icon }) => (
            <NavLink key={to} to={to}
              className={({ isActive }) => clsx("flex shrink-0 items-center gap-1.5 rounded-md px-2.5 py-1.5 text-xs",
                isActive ? "bg-violet/15 text-violet-light" : "text-gray-400")}>
              <Icon size={14} />{label}
            </NavLink>
          ))}
        </nav>
        <main className="flex-1 overflow-auto p-4 md:p-5">
          <Outlet />
        </main>
      </div>
    </div>
  );
}
