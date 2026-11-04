"""MonsterGPT v2 schema guarantees (migrations 0040-0043, docs/architecture/monster-gpt-v2.md).

Runs on a scratch database (monsteros_v2) built from every migration; production is
only read (that the migrations are applied). Checks: claims are never edited, a finished
simulation never changes and a run's inputs never change, the services can read the
source registry but not change it, and the seeded registry says what is known.

  python -m pytest -q scripts/ops/test_schema_v2.py
"""
import os
import subprocess
import sys

import psycopg
import pytest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import dbenv  # noqa: E402

OWNER = dbenv.url("OPS_DATABASE_URL")
APP = dbenv.url("APP_DATABASE_URL")
DB = "monsteros_v2"
MIG = os.path.join(dbenv.ROOT, "infrastructure", "postgres", "migrations")
ADMIN = ["docker", "exec", "monster-os-postgres-1", "psql", "-U", "monster", "-d", "postgres", "-q", "-c"]


def on_db(url, db):
    return url.rsplit("/", 1)[0] + "/" + db


@pytest.fixture(scope="module")
def scratch():
    subprocess.run(ADMIN + [f"DROP DATABASE IF EXISTS {DB}"], capture_output=True)
    subprocess.run(ADMIN + [f"CREATE DATABASE {DB}"], check=True, capture_output=True)
    try:
        for f in sorted(x for x in os.listdir(MIG) if x.endswith(".sql")):
            sql = open(os.path.join(MIG, f), encoding="utf-8").read().replace("ON DATABASE monsteros TO", f"ON DATABASE {DB} TO")
            r = subprocess.run(["docker", "exec", "-i", "monster-os-postgres-1", "psql", "-U", "monster", "-d", DB, "-v", "ON_ERROR_STOP=1", "-q"],
                               input=sql, text=True, capture_output=True)
            assert r.returncode == 0, f"{f}: {r.stderr[:400]}"
        with psycopg.connect(on_db(OWNER, DB), autocommit=True) as cx:
            cx.execute("INSERT INTO gpt_conversations (id, owner, title) VALUES ('c1', 'golden', 't')")
            cx.execute("INSERT INTO gpt_messages (id, conversation_id, role, content) VALUES ('m1', 'c1', 'assistant', 'x')")
        yield
    finally:
        subprocess.run(ADMIN + [f"DROP DATABASE IF EXISTS {DB} WITH (FORCE)"], capture_output=True)


def app(sql, params=None):
    with psycopg.connect(on_db(APP, DB), autocommit=True) as cx:
        cur = cx.execute(sql, params)
        return cur.fetchall() if cur.description else cur.rowcount


def refused(sql, params=None):
    try:
        app(sql, params)
    except psycopg.Error:
        return True
    return False


def test_claims_are_written_once_and_go_with_their_conversation(scratch):
    app("INSERT INTO gpt_claims (message_id, sentence_ix, text, status, facts) VALUES ('m1', 0, 'X averaged 27.4', 'verified', "
        "'[{\"fact_id\":\"F1\",\"value\":27.4,\"source_id\":\"nba_stats\"}]')")
    assert refused("UPDATE gpt_claims SET status='unverified' WHERE message_id='m1'")
    assert refused("INSERT INTO gpt_claims (message_id, sentence_ix, text, status) VALUES ('m1', 1, 'y', 'guessed')")  # unknown status
    assert refused("INSERT INTO gpt_claims (message_id, sentence_ix, text, status) VALUES ('nope', 0, 'y', 'verified')")  # no such message
    app("DELETE FROM gpt_conversations WHERE id='c1'")  # a user deleting their history takes the claims with it
    assert app("SELECT count(*) FROM gpt_claims")[0][0] == 0


def test_a_simulation_run_keeps_its_inputs_and_is_final_once_finished(scratch):
    app("INSERT INTO gpt_sim_runs (run_id, sport, game_id, engine, seed, n, requested_by) VALUES ('r1', 'nba', 'g', 'nba-sim', 42, 100000, 'golden')")
    assert refused("UPDATE gpt_sim_runs SET seed=43 WHERE run_id='r1'")
    assert refused("UPDATE gpt_sim_runs SET overrides='{\"out\":[\"1\"]}' WHERE run_id='r1'")
    app("UPDATE gpt_sim_runs SET status='running' WHERE run_id='r1'")
    app("UPDATE gpt_sim_runs SET status='done', result='{\"home_win\":0.61}', finished_at=now() WHERE run_id='r1'")
    assert refused("UPDATE gpt_sim_runs SET result='{\"home_win\":0.70}' WHERE run_id='r1'")
    assert refused("DELETE FROM gpt_sim_runs WHERE run_id='r1'")
    assert refused("INSERT INTO gpt_sim_runs (run_id, sport, game_id, engine, seed, n, requested_by) VALUES ('r2', 'nba', 'g', 'e', 1, 2000000, 'golden')")


def test_services_read_the_source_registry_but_cannot_change_it(scratch):
    assert app("SELECT count(*) FROM data_sources")[0][0] >= 10
    assert refused("UPDATE data_sources SET status='approved' WHERE source_id='espn_site'")
    assert refused("INSERT INTO data_sources (source_id, name, access, priority) VALUES ('x', 'x', 'open_data', 3)")
    assert refused("DELETE FROM data_sources WHERE source_id='espn_site'")


def test_the_seeded_registry_claims_no_review_that_has_not_happened(scratch):
    rows = dict((r[0], r[1:]) for r in app("SELECT source_id, status, terms_reviewed_at, access, cost FROM data_sources"))
    reviewed = {k for k, v in rows.items() if v[0] == "approved"}
    assert reviewed == {"monster_os"}, "only Monster OS's own outputs are approved; approving a source is the owner's decision"
    # 0044 records the terms review; sources whose terms could not be read carry no review date
    assert rows["sportsgameodds"][1] is None and rows["kalshi"][1] is None
    assert all(rows[k][1] is not None for k in ("espn_site", "nba_stats", "mlb_statsapi", "nhl_api", "nflverse", "moneypuck"))
    paid = {k for k, v in rows.items() if v[2] == "paid_api"}
    assert paid == {"the_odds_api", "sportsgameodds"} and all(rows[k][3].startswith("paid") for k in paid)


def test_entities_resolve_through_aliases(scratch):
    app("INSERT INTO entities (entity_id, kind, sport, name) VALUES ('tm:nfl:WAS', 'team', 'nfl', 'Washington Commanders')")
    app("INSERT INTO entity_aliases (entity_id, alias, norm, valid_to) VALUES ('tm:nfl:WAS', 'Washington Redskins', 'washingtonredskins', '2020-07-13')")
    app("INSERT INTO entity_ids (entity_id, source, source_id) VALUES ('tm:nfl:WAS', 'espn_site', '28')")
    assert refused("INSERT INTO entities (entity_id, kind, sport, name) VALUES ('x', 'stadium', 'nfl', 'x')")
    assert refused("INSERT INTO entity_ids (entity_id, source, source_id) VALUES ('tm:nfl:WAS', 'espn_site', '28')")  # one owner per source id
    assert app("SELECT e.name FROM entity_aliases a JOIN entities e USING (entity_id) WHERE a.norm='washingtonredskins'") == [("Washington Commanders",)]


def test_production_has_the_v2_tables():
    with psycopg.connect(APP) as cx:
        have = {r[0] for r in cx.execute("SELECT tablename FROM pg_tables WHERE schemaname='public'")}
    assert {"entities", "entity_ids", "entity_aliases", "data_sources", "gpt_claims", "gpt_sim_runs"} <= have
