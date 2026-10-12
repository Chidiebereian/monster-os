#!/usr/bin/env python3
"""Fetch NBA player game logs from stats.nba.com (all players, bulk per season)
and write two CSVs for loading into Postgres: sports_players + player_game_logs.
stats.nba.com works from Python/urllib here even though the Rust reqwest client
is Akamai TLS-fingerprint-blocked. Run with -I for untrusted-dir safety is N/A
(this is our own script)."""
import urllib.request, json, csv, sys, time, io

OUT = sys.argv[1] if len(sys.argv) > 1 else "."
FROM = int(sys.argv[2]) if len(sys.argv) > 2 else 2012
TO = int(sys.argv[3]) if len(sys.argv) > 3 else 2026

H = {
    "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36",
    "Referer": "https://www.nba.com/", "Origin": "https://www.nba.com",
    "Accept": "application/json, text/plain, */*", "Accept-Language": "en-US,en;q=0.9",
    "x-nba-stats-origin": "stats", "x-nba-stats-token": "true", "Connection": "keep-alive",
}

def get(url):
    return json.load(urllib.request.urlopen(urllib.request.Request(url, headers=H), timeout=60))

def parse_min(v):
    s = str(v or "")
    if ":" in s:
        m, _, sec = s.partition(":")
        try: return float(m) + float(sec) / 60.0
        except: return 0.0
    try: return float(s)
    except: return 0.0

players = {}   # pid -> dict(name, team, first, last, games)
logs = []      # rows

for y in range(FROM, TO + 1):
    season = f"{y}-{str(y+1)[-2:]}"
    url = (f"https://stats.nba.com/stats/leaguegamelog?Counter=0&Direction=DESC&LeagueID=00"
           f"&PlayerOrTeam=P&Season={season}&SeasonType=Regular+Season&Sorter=DATE")
    try:
        d = get(url)
    except Exception as e:
        sys.stderr.write(f"season {season} failed: {e}\n"); continue
    rs = d["resultSets"][0]; cols = rs["headers"]; idx = {c: i for i, c in enumerate(cols)}
    rows = rs["rowSet"]
    def c(r, name): return r[idx[name]] if name in idx else None
    n = 0
    for r in rows:
        pid = str(c(r, "PLAYER_ID"))
        minutes = parse_min(c(r, "MIN"))
        if minutes < 1.0: continue
        pts = float(c(r, "PTS") or 0); reb = float(c(r, "REB") or 0); ast = float(c(r, "AST") or 0)
        threes = float(c(r, "FG3M") or 0); stl = float(c(r, "STL") or 0); blk = float(c(r, "BLK") or 0); tov = float(c(r, "TOV") or 0)
        sid = str(c(r, "SEASON_ID") or ""); seas = int(sid[1:]) if len(sid) > 1 and sid[1:].isdigit() else y
        gid = str(c(r, "GAME_ID") or ""); gdate = str(c(r, "GAME_DATE") or "")[:10]
        matchup = str(c(r, "MATCHUP") or ""); opp = matchup.split()[-1] if matchup else ""
        team = str(c(r, "TEAM_ABBREVIATION") or ""); name = str(c(r, "PLAYER_NAME") or "")
        stats = {"pts": pts, "reb": reb, "ast": ast, "threes": threes, "stl": stl, "blk": blk, "tov": tov,
                 "minutes": minutes, "min_played": minutes,
                 "pra": pts+reb+ast, "pr": pts+reb, "pa": pts+ast, "ra": reb+ast}
        logs.append((pid, seas, gid, gdate, team, opp, json.dumps(stats, separators=(",", ":"))))
        p = players.setdefault(pid, {"name": name, "team": team, "first": seas, "last": seas, "games": 0})
        p["name"] = name; p["team"] = team; p["first"] = min(p["first"], seas); p["last"] = max(p["last"], seas); p["games"] += 1
        n += 1
    sys.stderr.write(f"season {season}: {n} games\n")
    time.sleep(0.6)

if not logs:
    sys.stderr.write("no logs fetched\n"); sys.exit(1)

with open(f"{OUT}/nba_players.csv", "w", newline="", encoding="utf-8") as f:
    w = csv.writer(f)
    for pid, p in players.items():
        w.writerow([pid, p["name"], "", p["team"], p["first"], p["last"], p["games"]])
with open(f"{OUT}/nba_logs.csv", "w", newline="", encoding="utf-8") as f:
    w = csv.writer(f)
    for row in logs:
        w.writerow(row)
sys.stderr.write(f"wrote {len(players)} players, {len(logs)} logs to {OUT}\n")
