// Header bell for copy-bot alerts: polls the bot event feed, pops a toast (and
// a desktop notification when allowed) whenever a followed wallet trades.
import { useEffect, useRef, useState } from "react";
import { Link } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import clsx from "clsx";
import { Bell, BellRing, X } from "lucide-react";
import { Avatar } from "./pm";
import { timeAgo } from "../lib/pm";
import { bots, describeEvent, type BotAlert } from "../lib/bots";

interface Toast { key: number; title: string; body: string; alert?: BotAlert }

const canNotify = () => typeof Notification !== "undefined";

export function BotAlerts() {
  const qc = useQueryClient();
  const { data } = useQuery({ queryKey: ["bot-alerts"], queryFn: () => bots.alerts({ limit: 30 }), refetchInterval: 10_000 });
  const [open, setOpen] = useState(false);
  const [toasts, setToasts] = useState<Toast[]>([]);
  const [perm, setPerm] = useState(canNotify() ? Notification.permission : "denied");
  // Highest event id already announced; the first load sets the baseline so
  // old alerts don't all pop at once.
  const lastId = useRef<number | null>(null);

  useEffect(() => {
    if (!data) return;
    const maxId = data.rows.reduce((m, r) => Math.max(m, r.id), 0);
    if (lastId.current == null) { lastId.current = maxId; return; }
    const fresh = data.rows.filter((r) => r.id > lastId.current! && !r.seen).reverse();
    lastId.current = Math.max(lastId.current, maxId);
    if (fresh.length === 0) return;
    const add: Toast[] = fresh.length > 3
      ? [{ key: fresh[fresh.length - 1].id, title: `${fresh.length} new copy-bot alerts`, body: [...new Set(fresh.map((f) => f.strategy))].join(", ") }]
      : fresh.map((a) => ({ key: a.id, title: `${a.strategy}: ${a.trader_name ?? "leader"} traded`, body: `${describeEvent(a)} · ${a.title ?? ""}`, alert: a }));
    setToasts((t) => [...add, ...t].slice(0, 4));
    add.forEach((t) => setTimeout(() => setToasts((all) => all.filter((x) => x.key !== t.key)), 9000));
    qc.invalidateQueries({ queryKey: ["bots"] });
    if (canNotify() && Notification.permission === "granted" && document.visibilityState !== "visible") {
      add.forEach((t) => new Notification(t.title, { body: t.body, icon: t.alert?.icon ?? undefined, tag: `bot-${t.key}` }));
    }
  }, [data, qc]);

  const seen = useMutation({
    mutationFn: () => bots.markSeen(),
    onSuccess: () => { qc.invalidateQueries({ queryKey: ["bot-alerts"] }); qc.invalidateQueries({ queryKey: ["bots"] }); qc.invalidateQueries({ queryKey: ["bot"] }); },
  });
  const unseen = data?.unseen ?? 0;

  return (
    <>
      <div className="relative">
        <button onClick={() => setOpen((o) => !o)} title="Copy-bot alerts"
          className="relative p-1.5 rounded-md text-gray-400 hover:text-gray-100 hover:bg-white/5">
          {unseen > 0 ? <BellRing size={17} className="text-violet-light" /> : <Bell size={17} />}
          {unseen > 0 && (
            <span className="absolute -top-0.5 -right-0.5 min-w-[16px] h-4 px-1 rounded-full bg-violet text-[10px] leading-4 text-white text-center">
              {unseen > 99 ? "99+" : unseen}
            </span>
          )}
        </button>
        {open && (
          <div className="absolute right-0 top-9 z-40 w-[380px] max-w-[calc(100vw-24px)] rounded-lg border border-glass bg-surface shadow-2xl">
            <div className="flex items-center gap-2 px-3 py-2 border-b border-glass">
              <span className="text-sm font-medium">Copy-bot alerts</span>
              {unseen > 0 && <button onClick={() => seen.mutate()} className="ml-auto text-xs text-violet-light hover:underline">Mark all read</button>}
              <button onClick={() => setOpen(false)} className={clsx("text-gray-500 hover:text-gray-200", unseen === 0 && "ml-auto")}><X size={14} /></button>
            </div>
            {canNotify() && perm !== "granted" && perm !== "denied" && (
              <button onClick={() => Notification.requestPermission().then(setPerm)}
                className="w-full text-left px-3 py-2 text-xs text-gray-300 bg-violet/10 hover:bg-violet/15 border-b border-glass">
                Turn on desktop notifications so you hear about trades while this tab is in the background
              </button>
            )}
            <div className="max-h-[420px] overflow-y-auto">
              {(data?.rows.length ?? 0) === 0 && <div className="px-3 py-6 text-center text-xs text-gray-500">No alerts yet. Create a copy bot in Top Traders → Bots.</div>}
              {data?.rows.map((a) => (
                <div key={a.id} className={clsx("flex gap-2 px-3 py-2 border-b border-glass last:border-0", !a.seen && "bg-violet/[0.07]")}>
                  <Avatar wallet={a.wallet} src={a.trader_image} size={26} />
                  <div className="min-w-0 flex-1">
                    <div className="text-xs"><b className="text-gray-100">{a.strategy}</b> <span className="text-gray-500">· {timeAgo(Date.parse(a.ts) / 1000)}</span></div>
                    <div className={clsx("text-xs", a.action === "copied" ? "text-emerald-light" : "text-gray-400")}>{describeEvent(a)}</div>
                    <div className="text-[11px] text-gray-500 truncate" title={a.title ?? ""}>{a.title}</div>
                  </div>
                </div>
              ))}
            </div>
            <Link to="/copy?tab=bots" onClick={() => setOpen(false)} className="block text-center text-xs text-violet-light hover:underline py-2 border-t border-glass">
              Manage copy bots
            </Link>
          </div>
        )}
      </div>

      <div className="fixed bottom-4 right-4 z-50 space-y-2 w-[340px] max-w-[calc(100vw-32px)]">
        {toasts.map((t) => (
          <div key={t.key} className="rounded-lg border border-violet/40 bg-surface/95 backdrop-blur shadow-2xl p-3 flex gap-2">
            {t.alert ? <Avatar wallet={t.alert.wallet} src={t.alert.trader_image} size={30} /> : <BellRing size={18} className="text-violet-light mt-0.5" />}
            <div className="min-w-0 flex-1">
              <div className="text-sm font-medium truncate">{t.title}</div>
              <div className="text-xs text-gray-400 line-clamp-2">{t.body}</div>
            </div>
            <button onClick={() => setToasts((all) => all.filter((x) => x.key !== t.key))} className="text-gray-500 hover:text-gray-200 self-start"><X size={13} /></button>
          </div>
        ))}
      </div>
    </>
  );
}
