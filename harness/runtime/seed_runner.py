#!/usr/bin/env python3
"""Run the repo's harness/seed.py inside the Django container.

The seed script hardcodes container-era paths, so rewrite them for the
wger/server image. When JWT keys are present, rewrite the generated harness env
so the host-running Rust binary reaches postgres and the harness ports.
"""
import os
import runpy
from pathlib import Path

seed_src = Path("/tmp/seed_orig.py").read_text()
seed_src = seed_src.replace('ROOT = Path("/tmp/wger-src/server-2.7")', 'ROOT = Path("/home/wger/src")')
seed_src = seed_src.replace('DJANGO_DB_HOST", "127.0.0.1")', 'DJANGO_DB_HOST", "pg")')
seed_src = seed_src.replace('Path("/tmp/wger-harness-ids.json")', 'Path("/tmp/wger-media/wger-harness-ids.json")')
seed_src = seed_src.replace('Path("/tmp/wger-harness.env")', 'Path("/tmp/wger-media/wger-harness.env")')
tail = r'''
import os
from pathlib import Path
pg_port = os.environ.get("HARNESS_PG_PORT", "55432")
rust_port = os.environ.get("HARNESS_RUST_PORT", "8012")
priv = os.environ.get("JWT_PRIVATE_KEY", "")
pub = os.environ.get("JWT_PUBLIC_KEY", "")
if priv and pub:
    env_path = Path("/tmp/wger-media/wger-harness.env")
    lines = []
    if env_path.exists():
        for line in env_path.read_text().splitlines():
            if line.startswith("JWT_PRIVATE_KEY=") or line.startswith("JWT_PUBLIC_KEY="):
                continue
            lines.append(line)
    lines.append(f"JWT_PRIVATE_KEY={priv}")
    lines.append(f"JWT_PUBLIC_KEY={pub}")
    rewritten = []
    for line in lines:
        if line.startswith("DJANGO_DB_HOST="):
            rewritten.append("DJANGO_DB_HOST=127.0.0.1")
        elif line.startswith("DJANGO_DB_PORT="):
            rewritten.append(f"DJANGO_DB_PORT={pg_port}")
        elif line.startswith("DATABASE_URL="):
            rewritten.append(f"DATABASE_URL=postgres://wger:wger@127.0.0.1:{pg_port}/wger")
        elif line.startswith("SITE_URL="):
            rewritten.append(f"SITE_URL=http://127.0.0.1:{rust_port}")
        else:
            rewritten.append(line)
    rewritten.append(f"BIND=127.0.0.1:{rust_port}")
    env_path.write_text("\n".join(rewritten) + "\n")
    print("rewrote harness env for host rust")
'''
Path("/tmp/seed_run.py").write_text(seed_src + "\n" + tail)
runpy.run_path("/tmp/seed_run.py", run_name="__main__")
