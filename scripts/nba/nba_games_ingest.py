#!/usr/bin/env python3
"""NBA games (scores + schedule) for the team model and game filter.
Historical completed games with scores come from stats.nba.com leaguegamelog
(PlayerOrTeam=T, one call/season); upcoming games come from cdn.nba.com
scheduleLeagueV2 (no auth). Writes nba_games.csv for loading into sports_games."""
import urllib.request, json, csv, sys, time

OUT = sys.argv[1] if len(sys.argv) > 1 else "."
FROM = int(sys.argv[2]) if len(sys.argv) > 2 else 2012
TO = int(sys.argv[3]) if len(sys.argv) > 3 else 2026

H = {"User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36",
     "Referer": "https://www.nba.com/", "Origin": "https://www.nba.com", "Accept": "application/json, text/plain, */*",
     "Accept-Language": "en-US,en;q=0.9", "x-nba-stats-origin": "stats", "x-nba-stats-token": "true"}

def get(u):
    return json.load(urllib.request.urlopen(urllib.request.Request(u, headers=H), timeout=60))

games = {}  # game_id -> dict

# historical completed games with scores
for y in range(FROM, TO + 1):
    season = f"{y}-{str(y+1)[-2:]}"
    url = (f"https://stats.nba.com/stats/leaguegamelog?Counter=0&Direction=DESC&LeagueID=00"
           f"&PlayerOrTeam=T&Season={season}&SeasonType=Regular+Season&Sorter=DATE")
    try:
        d = get(url)
    except Exception as e:
        sys.stderr.write(f"T {season} failed: {e}\n"); continue
    rs = d["resultSets"][0]; idx = {c: i for i, c in enumerate(rs["headers"])}
    def c(r, n): return r[idx[n]] if n in idx else None
    for r in rs["rowSet"]:
        gid = str(c(r, "GAME_ID")); matchup = str(c(r, "MATCHUP") or "")
        team = str(c(r, "TEAM_ABBREVIATION") or ""); pts = int(c(r, "PTS") or 0)
        gdate = str(c(r, "GAME_DATE") or "")[:10]
        sid = str(c(r, "SEASON_ID") or ""); seas = int(sid[1:]) if len(sid) > 1 and sid[1:].isdigit() else y
        opp = matchup.split()[-1] if matchup else ""
        home = " vs." in matchup or "vs." in matchup.split(" ")[1:2]
        g = games.setdefault(gid, {"season": seas, "date": gdate, "type": "REG", "status": "final",
                                   "home": "", "away": "", "hs": None, "as": None})
        if home:
            g["home"] = team; g["away"] = opp; g["hs"] = pts
        else:
            g["away"] = team; g["home"] = opp; g["as"] = pts
    sys.stderr.write(f"T {season}: {len(rs['rowSet'])} rows\n"); time.sleep(0.6)

# upcoming schedule (cdn, no auth)
try:
    s = get("https://cdn.nba.com/static/json/staticData/scheduleLeagueV2_1.json")
    sy = s["leagueSchedule"].get("seasonYear", "")  # "2026-27"
    season = int(sy.split("-")[0]) if sy[:4].isdigit() else TO
    for gd in s["leagueSchedule"]["gameDates"]:
        for g in gd["games"]:
            gid = str(g.get("gameId"))
            st = g.get("gameStatus")  # 1 sched, 2 live, 3 final
            if gid in games:
                continue  # already have completed w/ score
            home = (g.get("homeTeam") or {}).get("teamTricode", "")
            away = (g.get("awayTeam") or {}).get("teamTricode", "")
            if not home or not away or home == "" or away == "":
                continue
            date = (g.get("gameDateEst") or g.get("gameDate") or "")[:10]
            fin = st == 3
            games[gid] = {"season": season, "date": date, "type": "REG",
                          "status": "final" if fin else "scheduled",
                          "home": home, "away": away,
                          "hs": (g.get("homeTeam") or {}).get("score") if fin else None,
                          "as": (g.get("awayTeam") or {}).get("score") if fin else None}
except Exception as e:
    sys.stderr.write(f"schedule failed: {e}\n")

with open(f"{OUT}/nba_games.csv", "w", newline="", encoding="utf-8") as f:
    w = csv.writer(f)
    for gid, g in games.items():
        if not g["home"] or not g["away"]:
            continue
        w.writerow([gid, g["season"], g["date"] or "", g["type"], g["home"], g["away"],
                    g["hs"] if g["hs"] is not None else "", g["as"] if g["as"] is not None else "", g["status"]])
sys.stderr.write(f"wrote {len(games)} games to {OUT}/nba_games.csv\n")
