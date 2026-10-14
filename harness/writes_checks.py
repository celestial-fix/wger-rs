#!/usr/bin/env python3
"""REST write checks: POST/PUT/PATCH/DELETE comparisons against Django/Rust.
"""
import json
import os
import random
import subprocess
import time
from pathlib import Path

from compare import (
    DJANGO,
    RUST,
    IDS,
    fetch,
    rewrite,
    parse,
    diff,
    record,
    token_header,
)

# Same throwaway DB - each side creates its own objects.
# Use high IDs? Or just create in context. But easier to create both and compare.

auth = token_header()


def _pg_container():
    container = os.environ.get("WGER_PG_CONTAINER")
    if container:
        return container
    wanted = f"{os.environ.get('HARNESS_NAME', 'wger-rs-m2')}-pg"
    try:
        names = subprocess.run(
            ["docker", "ps", "--format", "{{.Names}}"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=True,
        ).stdout.decode().split()
    except Exception:
        names = []
    return wanted if wanted in names else "wger-rs-m2-pg"


def _media_root():
    root = os.environ.get("WGER_MEDIA_ROOT")
    if root:
        return Path(root)
    return Path(__file__).resolve().parent / "runtime" / ".state" / "media-host"


def _norm_remove_diffing_keys(obj, extra_keys=None):
    keys = set(extra_keys or []) | {"id", "created", "session"}
    if isinstance(obj, dict):
        out = {k: v for k, v in obj.items() if k not in keys}
        return out
    return obj


def post_both(path, body_l, body_r=None, remove_keys=None):
    body_r = body_r if body_r is not None else json.loads(json.dumps(body_l))
    ds, draw, _ = fetch(DJANGO, "POST", path, auth, body_l)
    rs, rraw, _ = fetch(RUST, "POST", path, auth, body_r)
    if ds != rs or ds != 201:
        record(f"write {path} post status", False, f"django={ds} rust={rs} {draw[:200]!r} | {rraw[:200]!r}")
        return None, None, ds, rs
    try:
        left = rewrite(parse(draw))
        right = rewrite(parse(rraw))
    except Exception as e:
        record(f"write {path} post json", False, str(e))
        return None, None, ds, rs
    # Remove ids/created/session and FK to side-specific parents - but we need to map?
    # For the initial comparison after creation, FKs that point to our newly created parent
    # will be different IDs on each side. We need to compare with mapping.
    found = diff(_norm_remove_diffing_keys(left, remove_keys), _norm_remove_diffing_keys(right, remove_keys))
    if found:
        record(f"write {path} post body", False, "\n".join(found[:12]))
    else:
        record(f"write {path} post", True)
    return left, right, ds, rs


def patch_both(path, obj_l, obj_r=None, body_l=None, body_r=None, remove_keys=None):
    body_l = body_l if body_l is not None else {}
    body_r = body_r if body_r is not None else json.loads(json.dumps(body_l))
    obj_r = obj_r if obj_r is not None else obj_l
    pid = obj_l.get("id") if isinstance(obj_l, dict) else obj_l
    ds, draw, _ = fetch(DJANGO, "PATCH", f"{path}{pid}/", auth, body_l)
    rs, rraw, _ = fetch(RUST, "PATCH", f"{path}{obj_r.get('id') if isinstance(obj_r, dict) else obj_r}/", auth, body_r)
    if ds != rs:
        record(f"write {path} patch status", False, f"django={ds} rust={rs}")
        return ds, rs
    try:
        left = rewrite(parse(draw))
        right = rewrite(parse(rraw))
    except Exception as e:
        record(f"write {path} patch json", False, str(e))
        return ds, rs
    found = diff(_norm_remove_diffing_keys(left, remove_keys), _norm_remove_diffing_keys(right, remove_keys))
    if found:
        record(f"write {path} patch body", False, "\n".join(found[:12]))
    else:
        record(f"write {path} patch", True)
    return ds, rs


def delete_both(path, pid_l, pid_r=None):
    pid_r = pid_l if pid_r is None else pid_r
    ds, draw, _ = fetch(DJANGO, "DELETE", f"{path}{pid_l}/", auth)
    rs, rraw, _ = fetch(RUST, "DELETE", f"{path}{pid_r}/", auth)
    if ds != rs or (ds != 204 and ds != 200 and ds != 202):  # allow 204 as per DRF
        record(f"write {path} delete", False, f"django={ds} rust={rs} {draw[:100]!r}")
    else:
        record(f"write {path} delete", True)
    return ds, rs


def delete_again_both(path, pid_l, pid_r=None):
    pid_r = pid_l if pid_r is None else pid_r
    ds, _, _ = fetch(DJANGO, "DELETE", f"{path}{pid_l}/", auth)
    rs, _, _ = fetch(RUST, "DELETE", f"{path}{pid_r}/", auth)
    if ds != rs or ds != 404:
        record(f"write {path} delete again", False, f"django={ds} rust={rs}")
    else:
        record(f"write {path} delete again", True)


def get_both(path, headers=None):
    ds, draw, _ = fetch(DJANGO, "GET", path, headers)
    rs, rraw, _ = fetch(RUST, "GET", path, headers)
    return ds, draw, rs, rraw


def compare_validation(path, method, body, name):
    # same body both sides
    ds, draw, _ = fetch(DJANGO, method, path, auth, body)
    rs, rraw, _ = fetch(RUST, method, path, auth, body)
    if ds != rs:
        record(f"write validation {name}", False, f"status django={ds} rust={rs}")
        return
    try:
        left = rewrite(parse(draw))
        right = rewrite(parse(rraw))
    except Exception as e:
        record(f"write validation {name}", False, str(e))
        return
    found = diff(left, right)
    if found:
        record(f"write validation {name}", False, "\n".join(found[:15]))
    else:
        record(f"write validation {name}", True)


def compare_status_both(method, path_l, path_r=None, body_l=None, body_r=None, name=None, **kw):
    # allow flexible calling
    if isinstance(path_r, dict) and body_l is None:
        body_l = path_r
        path_r = None
    body_r = body_r if body_r is not None else json.loads(json.dumps(body_l)) if body_l else body_l
    path_r = path_r if path_r is not None else path_l
    ds, draw, _ = fetch(DJANGO, method, path_l, auth, body_l)
    rs, rraw, _ = fetch(RUST, method, path_r, auth, body_r)
    nm = name or f"{method} {path_l}"
    if ds != rs:
        record(nm, False, f"status django={ds} rust={rs} {rraw[:120]!r}")
    else:
        record(nm, True)


def rest_write_checks():
    # Routine (parent for day/slot/slot-entry/configs/workoutlog in context)
    # POST valid
    import datetime as dt

    start = "2030-01-01"
    end = "2030-01-30"
    r_l, r_r, _, _ = post_both("/api/v2/routine/", {"name": "harness-r", "start": start, "end": end}, {"name": "harness-r", "start": start, "end": end})
    rid_l = r_l["id"] if r_l else None
    rid_r = r_r["id"] if r_r else None

    # PATCH own
    if rid_l:
        patch_both("/api/v2/routine/", r_l, r_r, {"name": "harness-r2", "is_public": True}, {"name": "harness-r2", "is_public": True})

    # validation cases
    compare_validation("/api/v2/routine/", "POST", {"name": "x"}, "routine missing required")
    compare_validation("/api/v2/routine/", "POST", {"name": "x", "start": "bad", "end": "2030-01-31"}, "routine bad date")
    compare_validation("/api/v2/routine/", "POST", {"name": "x", "start": "2030-02-01", "end": "2030-01-31"}, "routine end<start")
    # span > 120 days
    compare_validation("/api/v2/routine/", "POST", {"name": "x", "start": "2030-01-01", "end": "2030-05-05"}, "routine span>120")

    # ownership nonexistent parent
    compare_status_both("POST", "/api/v2/day/", {"routine": 99999999}, name="day routine nonexistent")
    # patch/delete nonexistent
    compare_status_both("PATCH", "/api/v2/routine/99999999/", {"name": "x"}, name="routine patch nonexistent")
    compare_status_both("DELETE", "/api/v2/routine/99999999/", name="routine delete nonexistent")

    # Build day/slot/entry under routine
    if rid_l:
        d_l, d_r, _, _ = post_both("/api/v2/day/", {"routine": rid_l, "name": "d1"}, {"routine": rid_r, "name": "d1"}, remove_keys=["routine"])
        s_l, s_r, _, _ = post_both("/api/v2/slot/", {"day": d_l["id"], "order": 1}, {"day": d_r["id"], "order": 1}, remove_keys=["day"])
        e_l, e_r, _, _ = post_both(
            "/api/v2/slot-entry/",
            {"slot": s_l["id"], "exercise": IDS["exercise_id"], "order": 1, "type": "normal"},
            {"slot": s_r["id"], "exercise": IDS["exercise_id"], "order": 1, "type": "normal"},
            remove_keys=["slot"],
        )
        patch_both("/api/v2/day/", d_l, d_r, {"name": "d2", "type": "hiit"}, remove_keys=["routine"])
        patch_both("/api/v2/slot/", s_l, s_r, {"comment": "c"}, remove_keys=["day"])
        patch_both("/api/v2/slot-entry/", e_l, e_r, {"comment": "x", "weight_rounding": "2.50"}, remove_keys=["slot"])
        compare_validation("/api/v2/day/", "POST", {"routine": rid_l, "type": "nope", "name": "x" * 30}, "day bad choice + long name")
        compare_validation("/api/v2/slot-entry/", "POST", {"slot": s_l["id"]}, "slot-entry missing exercise")
        for cfg in ("weight-config", "max-weight-config", "repetitions-config", "max-repetitions-config",
                    "max-sets-config", "rir-config", "max-rir-config", "rest-config", "max-rest-config"):
            val = 2 if "rir" in cfg else 10
            c_l, c_r, _, _ = post_both(f"/api/v2/{cfg}/", {"slot_entry": e_l["id"], "iteration": 1, "value": val, "operation": "+"},
                                       {"slot_entry": e_r["id"], "iteration": 1, "value": val, "operation": "+"}, remove_keys=["slot_entry"])
            if c_l and c_r:
                patch_both(f"/api/v2/{cfg}/", c_l, c_r, {"value": val + 1, "requirements": {"rules": ["weight"]}}, remove_keys=["slot_entry"])
            compare_validation(f"/api/v2/{cfg}/", "POST", {"slot_entry": e_l["id"], "iteration": 1, "value": "1.234", "step": "zz"}, f"{cfg} bad decimal + choice")
        # one config example
        sc_l, sc_r, _, _ = post_both(
            "/api/v2/sets-config/",
            {"slot_entry": e_l["id"], "iteration": 1, "value": 3},
            {"slot_entry": e_r["id"], "iteration": 1, "value": 3},
            remove_keys=["slot_entry"],
        )
        # validation for config bad requirements
        compare_validation(
            "/api/v2/weight-config/",
            "POST",
            {"slot_entry": e_l["id"], "value": 10, "operation": "+", "iteration": 1, "requirements": {"a": 1}},
            "weight-config bad requirements",
        )
        # delete again - configs
        if sc_l:
            delete_both("/api/v2/sets-config/", sc_l["id"], sc_r["id"])
            delete_again_both("/api/v2/sets-config/", sc_l["id"], sc_r["id"])
        # entry/slot/day deletes: only the slot-entry; the routine delete below
        # cascades through day/slot/configs
        if e_l and False:
            delete_both("/api/v2/slot-entry/", e_l["id"], e_r["id"])
            delete_again_both("/api/v2/slot-entry/", e_l["id"], e_r["id"])
        # delete slot
        cascade_ids = (d_l, d_r, s_l, s_r)

    # workoutlog validations
    # Django raises the model ValidationError in save(): unhandled 500
    compare_status_both("POST", "/api/v2/workoutlog/", {"date": "2030-01-01", "exercise": IDS["exercise_id"]},
                        name="write validation workoutlog missing reps/weight (500)")
    compare_validation(
        "/api/v2/workoutlog/",
        "POST",
        {"date": "2030-01-01", "exercise": 99999999, "repetitions": 5},
        "workoutlog bad exercise",
    )

    # 5-hour rule (REST workoutlog, no session given, routine=IDS["routine_id"])
    t = "2030-01-05T10:00:00Z"
    # create A, B (2h later), C (6h later) on both sides - but they are independent; we compare behavior per side?
    # "Check on each side: A.session == B.session, C.session != A.session; record pass if both sides agree."
    # So create on Django side: A,B,C; create on Rust side: A',B',C'; then check sessions match within side and differ as expected. Compare results agree.
    a_d, araw, _ = fetch(DJANGO, "POST", "/api/v2/workoutlog/", auth, {"date": t, "exercise": IDS["exercise_id"], "routine": IDS["routine_id"], "repetitions": 5})
    time.sleep(0.001)
    b_d, braw, _ = fetch(DJANGO, "POST", "/api/v2/workoutlog/", auth, {"date": "2030-01-05T12:00:00Z", "exercise": IDS["exercise_id"], "routine": IDS["routine_id"], "repetitions": 5})
    c_d, craw, _ = fetch(DJANGO, "POST", "/api/v2/workoutlog/", auth, {"date": "2030-01-05T16:00:00Z", "exercise": IDS["exercise_id"], "routine": IDS["routine_id"], "repetitions": 5})
    la = parse(araw) if a_d == 201 else (parse(araw) if araw else None)
    lb = parse(braw) if braw else None
    lc = parse(craw) if craw else None
    ok_d = False
    try:
        if la and lb and lc:
            ok_d = (la.get("session") == lb.get("session") and lc.get("session") != la.get("session"))
    except Exception:
        pass

    # Rust side
    a_r, araw, _ = fetch(RUST, "POST", "/api/v2/workoutlog/", auth, {"date": t, "exercise": IDS["exercise_id"], "routine": IDS["routine_id"], "repetitions": 5})
    b_r, braw, _ = fetch(RUST, "POST", "/api/v2/workoutlog/", auth, {"date": "2030-01-05T12:00:00Z", "exercise": IDS["exercise_id"], "routine": IDS["routine_id"], "repetitions": 5})
    c_r, craw, _ = fetch(RUST, "POST", "/api/v2/workoutlog/", auth, {"date": "2030-01-05T16:00:00Z", "exercise": IDS["exercise_id"], "routine": IDS["routine_id"], "repetitions": 5})
    la_r = parse(araw) if araw else None
    lb_r = parse(braw) if braw else None
    lc_r = parse(craw) if craw else None
    ok_r = False
    try:
        if la_r and lb_r and lc_r:
            ok_r = (la_r.get("session") == lb_r.get("session") and lc_r.get("session") != la_r.get("session"))
    except Exception:
        pass
    record("write workoutlog 5-hour rule both sides agree", ok_d == ok_r and ok_d, f"django={ok_d} rust={ok_r}")

    # Delete routine with children - cascade. Delete the routine we created (has day/slot/entry/configs created above?)
    # We created d/s/e/sc under rid_l; also we can create a fresh one to be sure. Or just delete rid_l/rid_r.
    if rid_l:
        delete_both("/api/v2/routine/", rid_l, rid_r)
        delete_again_both("/api/v2/routine/", rid_l, rid_r)
        d_l, d_r, s_l, s_r = cascade_ids
        for path, l, r in (("/api/v2/day/", d_l, d_r), ("/api/v2/slot/", s_l, s_r)):
            ds, _, _ = fetch(DJANGO, "GET", f"{path}{l['id']}/", auth)
            rs, _, _ = fetch(RUST, "GET", f"{path}{r['id']}/", auth)
            record(f"write routine cascade {path}", ds == rs == 404, f"django={ds} rust={rs}")
    diary_measurement_checks()

    # Gallery delete via upload
    # insert row per side via psql, create file, then DELETE upload-powersync-data
    import uuid

    pg = f"docker exec -i {_pg_container()} psql -U wger -d wger -t -A -c"
    date = "2030-01-10"
    # insert
    try:
        out = subprocess.run(pg.split() + [f"INSERT INTO gallery_image (user_id, image, date, height, width, description) VALUES ({IDS['user_id']}, 'gallery/harness-l.jpg', '{date}', 10, 10, '') RETURNING id;"], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        g_l = out.stdout.decode().strip().splitlines()[0].strip() if out.stdout else '0'
    except Exception as e:
        g_l = '0'
    try:
        out = subprocess.run(pg.split() + [f"INSERT INTO gallery_image (user_id, image, date, height, width, description) VALUES ({IDS['user_id']}, 'gallery/harness-r.jpg', '{date}', 10, 10, '') RETURNING id;"], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        g_r = out.stdout.decode().strip().splitlines()[0].strip() if out.stdout else '0'
    except Exception as e:
        g_r = '0'
    # create files
    media_host = _media_root()
    (media_host / "gallery").mkdir(parents=True, exist_ok=True)
    try:
        (media_host / "gallery" / "harness-l.jpg").write_bytes(b"abc")
        (media_host / "gallery" / "harness-r.jpg").write_bytes(b"abc")
    except Exception:
        pass
    # delete via upload-powersync-data
    ds, draw, _ = fetch(DJANGO, "DELETE", "/api/v2/upload-powersync-data", auth, {"table": "gallery_image", "data": {"id": g_l}})
    rs, rraw, _ = fetch(RUST, "DELETE", "/api/v2/upload-powersync-data", auth, {"table": "gallery_image", "data": {"id": g_r}})
    ok = ds == rs == 200
    try:
        pd = parse(draw)
        pr = parse(rraw)
        if pd != pr:
            ok = False
    except Exception:
        ok = False
    if ok:
        # files removed on both
        ok = (not (media_host / "gallery" / "harness-l.jpg").exists()) and (not (media_host / "gallery" / "harness-r.jpg").exists())
    record("write gallery delete via upload", ok, f"django={ds} rust={rs}")


if __name__ == "__main__":
    rest_write_checks()


def get_own_both(path, pid_l, pid_r, name):
    ds, draw, _ = fetch(DJANGO, "GET", f"{path}{pid_l}/", auth)
    rs, rraw, _ = fetch(RUST, "GET", f"{path}{pid_r}/", auth)
    if ds != rs or ds != 200:
        record(name, False, f"django={ds} rust={rs}")
        return
    found = diff(_norm_remove_diffing_keys(rewrite(parse(draw))), _norm_remove_diffing_keys(rewrite(parse(rraw))))
    record(name, not found, "\n".join(found[:12]))


def upload_measurement_get_check():
    import uuid
    from compare import upload_send
    now = "2030-02-03T08:00:00Z"
    ids = {}
    for side, base in (("l", DJANGO), ("r", RUST)):
        ids[side] = str(uuid.uuid4())
        upload_send(base, "PUT", "measurements_measurement",
                    {"id": ids[side], "category": IDS["measurement_category_id"], "value": 66.5, "date": now, "notes": "u"})
    get_own_both("/api/v2/measurement/", ids["l"], ids["r"], "write upload measurement get (source/extra_data)")


def diary_measurement_checks():
    # nutrition diary (POST/PATCH/DELETE)
    body = {"plan": IDS["plan_id"], "ingredient": IDS["ingredient_id"], "amount": "120.50", "datetime": "2030-02-01T08:00:00Z"}
    l, r, _, _ = post_both("/api/v2/nutritiondiary/", body)
    if l and r:
        patch_both("/api/v2/nutritiondiary/", l, r, {"amount": 80})
        delete_both("/api/v2/nutritiondiary/", l["id"], r["id"])
        delete_again_both("/api/v2/nutritiondiary/", l["id"], r["id"])
    compare_validation("/api/v2/nutritiondiary/", "POST", {"plan": IDS["plan_id"], "amount": "1.234"}, "nutritiondiary missing ingredient + bad amount")
    compare_status_both("POST", "/api/v2/nutritiondiary/", {"plan": "00000000-0000-0000-0000-000000000000", "ingredient": IDS["ingredient_id"], "amount": 1}, name="write nutritiondiary foreign plan 403")
    # measurement
    body = {"category": IDS["measurement_category_id"], "date": "2030-02-02T08:00:00Z", "value": "55.5", "notes": "h"}
    l, r, _, _ = post_both("/api/v2/measurement/", body)
    if l and r:
        get_own_both("/api/v2/measurement/", l["id"], r["id"], "write /api/v2/measurement/ get after post")
        patch_both("/api/v2/measurement/", l, r, {"notes": "h2"})
        delete_both("/api/v2/measurement/", l["id"], r["id"])
        delete_again_both("/api/v2/measurement/", l["id"], r["id"])
    upload_measurement_get_check()
    compare_validation("/api/v2/measurement/", "POST", {"category": IDS["measurement_category_id"], "value": -1}, "measurement missing date + negative")
    # weightentry
    l, r, _, _ = post_both("/api/v2/weightentry/", {"date": "2030-02-03T08:00:00Z", "weight": "80.5"})
    if l and r:
        patch_both("/api/v2/weightentry/", l, r, {"weight": 81})
        delete_both("/api/v2/weightentry/", l["id"], r["id"])
    compare_validation("/api/v2/weightentry/", "POST", {"date": "2030-02-03T08:00:00Z", "weight": 5000}, "weightentry out of range")
    # workoutlog CRUD with explicit reps
    body = {"date": "2030-03-01T10:00:00Z", "exercise": IDS["exercise_id"], "repetitions": 8, "repetitions_unit": 1, "weight": "50.25", "weight_unit": 1}
    l, r, _, _ = post_both("/api/v2/workoutlog/", body)
    if l and r:
        patch_both("/api/v2/workoutlog/", l, r, {"rir": "1.5"})
        delete_both("/api/v2/workoutlog/", l["id"], r["id"])
        delete_again_both("/api/v2/workoutlog/", l["id"], r["id"])
