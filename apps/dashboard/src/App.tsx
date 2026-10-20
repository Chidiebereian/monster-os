import { BrowserRouter, Navigate, Route, Routes } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { Layout } from "./components/Layout";
import { Odds } from "./pages/Odds";
import { EVFinder } from "./pages/EVFinder";
import { Projections } from "./pages/Projections";
import { Arbitrage } from "./pages/Arbitrage";
import { WhaleWatch } from "./pages/WhaleWatch";
import { SmartSignals } from "./pages/SmartSignals";
import { TopTraders } from "./pages/TopTraders";
import { Backtest } from "./pages/Backtest";
import { Cheatsheets } from "./pages/Cheatsheets";
import { HitRates } from "./pages/HitRates";
import { MlbSim } from "./pages/MlbSim";
import { BroadcastSim } from "./pages/BroadcastSim";
import { MonsterGpt } from "./pages/MonsterGpt";

const queryClient = new QueryClient({
  defaultOptions: { queries: { refetchOnWindowFocus: false, retry: 1 } },
});

export default function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <BrowserRouter>
        <Routes>
          <Route element={<Layout />}>
            <Route index element={<Navigate to="/odds" replace />} />
            <Route path="/odds" element={<Odds />} />
            <Route path="/ev" element={<EVFinder />} />
            <Route path="/projections" element={<Projections />} />
            <Route path="/cheatsheets" element={<Cheatsheets />} />
            <Route path="/hit-rates" element={<HitRates />} />
            <Route path="/mlb-sim" element={<MlbSim />} />
            <Route path="/broadcast" element={<BroadcastSim />} />
            <Route path="/arbitrage" element={<Arbitrage />} />
            <Route path="/whale-watch" element={<WhaleWatch />} />
            <Route path="/signals" element={<SmartSignals />} />
            <Route path="/copy" element={<TopTraders />} />
            <Route path="/backtest" element={<Backtest />} />
            <Route path="/monster-gpt" element={<MonsterGpt />} />
            <Route path="*" element={<Navigate to="/odds" replace />} />
          </Route>
        </Routes>
      </BrowserRouter>
    </QueryClientProvider>
  );
}
