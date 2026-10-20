import { useEffect, useRef, useState } from "react";
import type { SimEvent } from "../../lib/sim";
import { teamAccent, teamColor } from "../../lib/sim";

// Broadcast field. Everything drawn comes from the event: the line of
// scrimmage and first-down line are the authoritative state after the play,
// the ball path runs from the event's start spot to its end spot, and the
// lateral lane is the simulated pass/run direction. The model does not track
// 22 players, so no player dots are invented.

const W = 1200; // 120 yards x 10
const H = 534; // 53.3 yards x 10
const X = (x: number) => 100 + x * 10;
const MID = H / 2;

function lane(e: SimEvent): number {
  if (!e.location || e.location === "middle") return MID;
  // away attacks to the right: its left is the far (top) side
  const leftIsTop = e.offense === 0;
  const top = MID - 150;
  const bottom = MID + 150;
  return e.location === "left" ? (leftIsTop ? top : bottom) : (leftIsTop ? bottom : top);
}

const ARC_KINDS = new Set(["pass_complete", "pass_incomplete", "interception", "punt", "kickoff", "field_goal", "extra_point"]);

function geometry(e: SimEvent) {
  const x0 = X(e.x_from);
  let x1 = X(e.x_to);
  const y0 = MID;
  let y1 = lane(e);
  if (e.kind === "pass_incomplete" || e.kind === "interception") {
    // pass depth is not simulated: show only the simulated direction
    x1 = x0;
  }
  if (["kickoff", "punt", "field_goal", "extra_point"].includes(e.kind)) y1 = MID;
  const arc = ARC_KINDS.has(e.kind);
  const cx = (x0 + x1) / 2;
  const cy = arc ? Math.min(y0, y1) - Math.max(110, Math.abs(x1 - x0) * 0.45) : (y0 + y1) / 2;
  return { x0, y0, x1, y1, cx, cy, arc };
}

const bez = (a: number, c: number, b: number, t: number) => (1 - t) * (1 - t) * a + 2 * (1 - t) * t * c + t * t * b;

export function FootballField({ event, teams, speed, carrier }: {
  event?: SimEvent;
  teams: [string, string];
  speed: number;
  carrier?: string;
}) {
  const [t, setT] = useState(1);
  const raf = useRef<number | null>(null);

  useEffect(() => {
    if (!event) return;
    const dur = Math.max(250, (ARC_KINDS.has(event.kind) ? 1500 : 1100) / speed);
    const start = performance.now();
    setT(0);
    const tick = (now: number) => {
      const k = Math.min(1, (now - start) / dur);
      setT(k);
      if (k < 1) raf.current = requestAnimationFrame(tick);
    };
    raf.current = requestAnimationFrame(tick);
    return () => {
      if (raf.current) cancelAnimationFrame(raf.current);
    };
  }, [event?.seq, speed]); // eslint-disable-line react-hooks/exhaustive-deps

  const g = event ? geometry(event) : null;
  const ease = t < 0.5 ? 2 * t * t : 1 - Math.pow(-2 * t + 2, 2) / 2;
  const bx = g ? bez(g.x0, g.cx, g.x1, ease) : X(50);
  const by = g ? bez(g.y0, g.cy, g.y1, ease) : MID;
  const state = event?.state;
  const showLines = event && !["game_end", "kickoff", "extra_point", "two_point", "end_period"].includes(event.kind) && state && state.down >= 1;
  const los = state ? X(state.ball_x) : null;
  const fd = state?.first_down_x != null ? X(state.first_down_x) : null;
  const path = g ? `M ${g.x0} ${g.y0} Q ${g.cx} ${g.cy} ${g.x1} ${g.y1}` : "";
  const goodArc = event && (event.kind === "pass_complete" || event.touchdown);

  return (
    <div className="relative w-full" style={{ perspective: "1500px" }}>
      <div className="mx-auto" style={{ transform: "rotateX(38deg) scale(0.96)", transformOrigin: "50% 60%", transformStyle: "preserve-3d" }}>
        <svg viewBox={`-14 -14 ${W + 28} ${H + 28}`} className="w-full drop-shadow-[0_30px_40px_rgba(0,0,0,0.6)]">
          <defs>
            <linearGradient id="ez-away" x1="0" x2="1">
              <stop offset="0" stopColor={teamColor(teams[0])} stopOpacity="0.95" />
              <stop offset="1" stopColor={teamColor(teams[0])} stopOpacity="0.7" />
            </linearGradient>
            <linearGradient id="ez-home" x1="1" x2="0">
              <stop offset="0" stopColor={teamColor(teams[1])} stopOpacity="0.95" />
              <stop offset="1" stopColor={teamColor(teams[1])} stopOpacity="0.7" />
            </linearGradient>
            <filter id="glow" x="-50%" y="-50%" width="200%" height="200%">
              <feGaussianBlur stdDeviation="5" result="b" />
              <feMerge><feMergeNode in="b" /><feMergeNode in="SourceGraphic" /></feMerge>
            </filter>
          </defs>
          {/* surround */}
          <rect x={-14} y={-14} width={W + 28} height={H + 28} rx={6} fill="#0b2a17" />
          {/* 5-yard stripes */}
          {Array.from({ length: 20 }, (_, i) => (
            <rect key={i} x={100 + i * 50} y={0} width={50} height={H} fill={i % 2 ? "#1e6a3a" : "#226f3f"} />
          ))}
          {/* end zones */}
          <rect x={0} y={0} width={100} height={H} fill="url(#ez-away)" />
          <rect x={1100} y={0} width={100} height={H} fill="url(#ez-home)" />
          <text x={50} y={MID} fill={teamAccent(teams[0])} fontSize={64} fontWeight={800} textAnchor="middle"
            transform={`rotate(-90 50 ${MID})`} dominantBaseline="middle" opacity={0.85} letterSpacing={8}>{teams[0]}</text>
          <text x={1150} y={MID} fill={teamAccent(teams[1])} fontSize={64} fontWeight={800} textAnchor="middle"
            transform={`rotate(90 1150 ${MID})`} dominantBaseline="middle" opacity={0.85} letterSpacing={8}>{teams[1]}</text>
          {/* yard lines + numbers */}
          {Array.from({ length: 21 }, (_, i) => {
            const x = 100 + i * 50;
            const yd = i * 5;
            const num = yd % 10 === 0 && yd > 0 && yd < 100 ? (yd <= 50 ? yd : 100 - yd) : null;
            return (
              <g key={i}>
                <line x1={x} y1={0} x2={x} y2={H} stroke="white" strokeOpacity={i === 0 || i === 20 ? 0.95 : 0.55} strokeWidth={i === 0 || i === 20 ? 4 : 2} />
                {num !== null && (
                  <>
                    <text x={x} y={H - 46} fill="white" fillOpacity={0.75} fontSize={34} fontWeight={700} textAnchor="middle">{num}</text>
                    <text x={x} y={58} fill="white" fillOpacity={0.75} fontSize={34} fontWeight={700} textAnchor="middle"
                      transform={`rotate(180 ${x} 46)`}>{num}</text>
                  </>
                )}
              </g>
            );
          })}
          {/* hash marks */}
          {Array.from({ length: 99 }, (_, i) => {
            const x = 110 + i * 10;
            return (
              <g key={`h${i}`} stroke="white" strokeOpacity={0.45} strokeWidth={1.5}>
                <line x1={x} y1={4} x2={x} y2={14} />
                <line x1={x} y1={H - 14} x2={x} y2={H - 4} />
                <line x1={x} y1={MID - 36} x2={x} y2={MID - 28} />
                <line x1={x} y1={MID + 28} x2={x} y2={MID + 36} />
              </g>
            );
          })}
          <rect x={0} y={0} width={W} height={H} fill="none" stroke="white" strokeWidth={5} />
          {/* line of scrimmage (blue) and first-down line (yellow) for the next snap */}
          {showLines && los !== null && (
            <line x1={los} y1={0} x2={los} y2={H} stroke="#38BDF8" strokeWidth={5} opacity={t >= 1 ? 0.95 : 0.35} />
          )}
          {showLines && fd !== null && (
            <line x1={fd} y1={0} x2={fd} y2={H} stroke="#FACC15" strokeWidth={5} opacity={t >= 1 ? 0.95 : 0.35} />
          )}
          {/* play path */}
          {g && path && (
            <path d={path} fill="none" stroke={goodArc ? "#FBBF24" : event?.turnover ? "#F87171" : "#E5E7EB"} strokeWidth={7}
              strokeLinecap="round" pathLength={1} strokeDasharray="1" strokeDashoffset={1 - ease} filter="url(#glow)" opacity={0.9} />
          )}
          {/* ball shadow + ball */}
          <ellipse cx={bx} cy={g ? bez(g.y0, (g.y0 + g.y1) / 2, g.y1, ease) + 6 : MID + 6} rx={13} ry={6} fill="black" opacity={0.35} />
          <g transform={`translate(${bx} ${by})`}>
            <ellipse rx={15} ry={9} fill="#8B4513" stroke="#3b1d07" strokeWidth={2} />
            <line x1={-6} y1={0} x2={6} y2={0} stroke="white" strokeWidth={2} />
            {[-3, 0, 3].map((d) => <line key={d} x1={d} y1={-3} x2={d} y2={3} stroke="white" strokeWidth={1.5} />)}
          </g>
          {carrier && t >= 1 && g && (
            <g transform={`translate(${g.x1} ${g.y1 - 34})`}>
              <rect x={-90} y={-24} width={180} height={34} rx={6} fill="rgba(0,0,0,0.65)" />
              <text x={0} y={0} fill="white" fontSize={22} fontWeight={600} textAnchor="middle">{carrier}</text>
            </g>
          )}
        </svg>
      </div>
    </div>
  );
}
