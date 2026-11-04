"""A throwaway MonsterGPT chain for trying the chat page without writing to production.

  python apps/monster-gpt/tests/scratch_stack.py        # up; Ctrl-C (or --down) tears it down

  scratch database monsteros_golden (every migration + a copy of the RAG index)
    -> monster-gpt :8185 (its tools read the live projection-service, as the real chat does)
    -> api-gateway :8191 (MONSTER_GPT_URL points at :8185; everything else as usual)
    -> dashboard :5191 (VITE_GATEWAY=http://localhost:8191)

Conversations, claims and simulation runs made on :5191 land in the scratch database,
which is dropped on the way down.
"""
import os
import subprocess
import sys
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import golden  # noqa: E402  (scratch_db, start, DB, ADMIN, ROOT)

GATEWAY_PORT, DASH_PORT = 8191, 5191
PIDS = os.path.join(golden.REPORTS, "scratch_stack.pids")


def wait(url, secs=120):
    for _ in range(secs):
        try:
            if urllib.request.urlopen(url, timeout=2).status < 500:
                return True
        except OSError:
            time.sleep(1)
    return False


def down():
    if os.path.exists(PIDS):
        for pid in open(PIDS).read().split():
            subprocess.run(["taskkill", "/F", "/T", "/PID", pid], capture_output=True)
        os.remove(PIDS)
    subprocess.run(golden.ADMIN + [f"DROP DATABASE IF EXISTS {golden.DB} WITH (FORCE)"], capture_output=True)
    print("scratch stack down; scratch database dropped")


def up():
    os.makedirs(golden.REPORTS, exist_ok=True)
    golden.scratch_db()
    os.environ["MONSTER_GPT_ORIGINS"] = f"http://localhost:{DASH_PORT},http://127.0.0.1:{DASH_PORT}"
    gpt = golden.start()
    env = dict(os.environ, PORT=str(GATEWAY_PORT), MONSTER_GPT_URL=f"http://localhost:{golden.PORT}")
    gw = subprocess.Popen([os.path.join(golden.ROOT, "target", "debug", "api-gateway.exe")], cwd=golden.ROOT, env=env,
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    dash = subprocess.Popen(f"npx vite --port {DASH_PORT} --strictPort", shell=True, cwd=os.path.join(golden.ROOT, "apps", "dashboard"),
                            env=dict(os.environ, VITE_GATEWAY=f"http://localhost:{GATEWAY_PORT}"), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    open(PIDS, "w").write(f"{gpt.pid} {gw.pid} {dash.pid}")
    ok = wait(f"http://localhost:{GATEWAY_PORT}/") and wait(f"http://localhost:{DASH_PORT}/")
    print(f"scratch stack {'up' if ok else 'NOT healthy'}: http://localhost:{DASH_PORT}/monster-gpt (monster-gpt :{golden.PORT}, gateway :{GATEWAY_PORT})")
    return ok


if __name__ == "__main__":
    if "--down" in sys.argv:
        down()
    elif "--up" in sys.argv:
        sys.exit(0 if up() else 1)
    else:
        try:
            up()
            while True:
                time.sleep(3600)
        except KeyboardInterrupt:
            pass
        finally:
            down()
