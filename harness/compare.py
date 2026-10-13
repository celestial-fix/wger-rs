#!/usr/bin/env python3
"""Replay a traffic-derived request set against Django and wger-rs and diff JSON.

Normalizations (applied to both sides before compare):
- absolute URL host is rewritten to http://host
- next/previous query strings are parsed, unquoted and sorted
- author_history and total_authors_history arrays are sorted
- login/refresh token strings and methods[].at are redacted; JWT claims are checked separately
- powersync token string is redacted; aud/sub/lifetime are checked
- numeric values compare with an absolute epsilon of 1e-6
"""

import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

DJANGO = os.environ.get("DJANGO_BASE", "http://127.0.0.1:8001")
RUST = os.environ.get("RUST_BASE", "http://127.0.0.1:8002")
IDS = json.loads(Path(os.environ.get("WGER_IDS", "/tmp/wger-harness-ids.json")).read_text())
FIXTURES = Path(os.environ.get("WGER_FIXTURES", str(Path(__file__).resolve().parents[1] / "tests" / "fixtures")))

passes = []
fails = []
notes = []


def token_header():
    return {"Authorization": f"Token {IDS['token']}"}


def fetch(base, method, path, headers=None, body=None):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(base + path, data=data, method=method)
    for key, value in (headers or {}).items():
        req.add_header(key, value)
    if body is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            raw = resp.read()
            return resp.status, raw, resp.headers.get("Content-Type", "")
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read(), exc.headers.get("Content-Type", "")
    except urllib.error.URLError as exc:
        return 599, str(exc).encode(), ""


def rewrite(value):
    if isinstance(value, str):
        text = value
        for base in (DJANGO, RUST, "http://127.0.0.1:8000", "http://localhost:8000"):
            text = text.replace(base, "http://host")
        if text.startswith("http://host") and "?" in text:
            url = urllib.parse.urlsplit(text)
            pairs = urllib.parse.parse_qsl(url.query, keep_blank_values=True)
            pairs.sort()
            query = urllib.parse.urlencode(pairs, doseq=True)
            text = urllib.parse.urlunsplit((url.scheme, url.netloc, url.path, query, ""))
        return text
    if isinstance(value, list):
        return [rewrite(item) for item in value]
    if isinstance(value, dict):
        out = {key: rewrite(val) for key, val in value.items()}
        for key in ("author_history", "total_authors_history"):
            if isinstance(out.get(key), list) and all(isinstance(item, str) for item in out[key]):
                out[key] = sorted(out[key])
        return out
    return value


def redact_auth(body):
    if not isinstance(body, dict):
        return body
    meta = body.get("meta")
    if isinstance(meta, dict):
        for key in ("access_token", "refresh_token", "session_token"):
            if key in meta:
                meta[key] = "<token>"
    data = body.get("data")
    if isinstance(data, dict):
        for key in ("access_token", "refresh_token"):
            if key in data:
                data[key] = "<token>"
        methods = data.get("methods")
        if isinstance(methods, list):
            for method in methods:
                if isinstance(method, dict) and "at" in method:
                    method["at"] = 0
    return body


def close(left, right, path):
    if isinstance(left, float) or isinstance(right, float):
        try:
            return abs(float(left) - float(right)) <= 1e-6
        except (TypeError, ValueError):
            return False
    return left == right


def diff(left, right, path="$"):
    if close(left, right, path):
        return []
    if isinstance(left, dict) and isinstance(right, dict):
        out = []
        for key in sorted(set(left) | set(right)):
            if key not in left:
                out.append(f"{path}.{key}: missing on django")
            elif key not in right:
                out.append(f"{path}.{key}: missing on rust")
            else:
                out.extend(diff(left[key], right[key], f"{path}.{key}"))
        return out
    if isinstance(left, list) and isinstance(right, list):
        if len(left) != len(right):
            return [f"{path}: len {len(left)} != {len(right)}"]
        out = []
        for index, (a, b) in enumerate(zip(left, right)):
            out.extend(diff(a, b, f"{path}[{index}]"))
        return out
    return [f"{path}: {left!r} != {right!r}"]


def parse(raw):
    if not raw:
        return None
    return json.loads(raw.decode())


def record(name, ok, detail=""):
    if ok:
        passes.append(name)
    else:
        fails.append((name, detail))


def compare_json(name, method, path, headers=None, body=None, redact=None):
    ds, draw, _ = fetch(DJANGO, method, path, headers, body)
    rs, rraw, _ = fetch(RUST, method, path, headers, body)
    if ds != rs:
        record(name, False, f"status django={ds} rust={rs} body={rraw[:300]!r}")
        return ds, rs
    try:
        left = rewrite(parse(draw))
        right = rewrite(parse(rraw))
    except json.JSONDecodeError as exc:
        record(name, False, f"json {exc} django={draw[:200]!r} rust={rraw[:200]!r}")
        return ds, rs
    if redact:
        left = redact(left)
        right = redact(right)
    found = diff(left, right)
    if found:
        record(name, False, "\n".join(found[:12]))
    else:
        record(name, True)
    return ds, rs


def compare_status(name, method, path, headers=None, body=None):
    ds, draw, _ = fetch(DJANGO, method, path, headers, body)
    rs, rraw, _ = fetch(RUST, method, path, headers, body)
    if ds != rs:
        record(name, False, f"status django={ds} rust={rs} rust_body={rraw[:240]!r} django_body={draw[:240]!r}")
    else:
        record(name, True)


def jwt_claims(token):
    import jwt

    return jwt.decode(token, options={"verify_signature": False})


UP = "/api/v2/upload-powersync-data"


def upload_send(base, method, table, data, headers=None):
    return fetch(base, method, UP, headers or token_header(), {"table": table, "data": data})


def compare_upload(name, method, table, left_data, right_data=None):
    """Same request shape to both sides; bodies may differ (distinct ids)."""
    right_data = left_data if right_data is None else right_data
    ds, draw, _ = upload_send(DJANGO, method, table, left_data)
    rs, rraw, _ = upload_send(RUST, method, table, right_data)
    ok = ds == rs
    detail = f"status django={ds} rust={rs}"
    if ok:
        try:
            found = diff(rewrite(parse(draw)), rewrite(parse(rraw)))
        except json.JSONDecodeError as exc:
            found = [f"json {exc}"]
        if found:
            ok = False
            detail += "\n" + "\n".join(found[:8])
        else:
            detail += f" body={parse(draw)}"
    else:
        detail += f" django_body={draw[:200]!r} rust_body={rraw[:200]!r}"
    record(f"upload {name}", ok, detail)
    return ds, rs, parse(draw) if ds == 200 else None


def upload_checks():
    import uuid as _uuid

    def uid():
        return str(_uuid.uuid4())

    now = "2026-10-09T12:00:00Z"
    day = "2026-10-09"

    # Stateless refusals (same body both sides)
    ds, draw, _ = fetch(DJANGO, "PUT", UP, token_header(), {})
    rs, rraw, _ = fetch(RUST, "PUT", UP, token_header(), {})
    found = diff(rewrite(parse(draw)), rewrite(parse(rraw))) if ds == rs == 200 else [f"status {ds}/{rs}"]
    record(
        "upload missing fields",
        not found and parse(draw) == {"error": "Missing required fields: table, data"},
        f"django={ds} rust={rs} body={draw[:160]!r}",
    )
    for method, table, data in (
        ("PUT", "no_such_table", {}),
        ("PUT", "core_userprofile", {"id": IDS["user_id"], "height": 180}),
        ("PUT", "manager_routine", {"id": 1, "name": "x"}),
        ("PUT", "gallery_image", {"id": 1, "date": day}),
    ):
        compare_upload(f"{method} {table} refusal", method, table, data)

    # Unauthenticated
    ds, draw, _ = fetch(DJANGO, "PUT", UP, None, {"table": "nope", "data": {}})
    rs, rraw, _ = fetch(RUST, "PUT", UP, None, {"table": "nope", "data": {}})
    record("upload unauthenticated", ds == rs == 403, f"django={ds} rust={rs}")

    # Profile PATCH (shared row; same values both sides)
    compare_upload(
        "profile patch",
        "PATCH",
        "core_userprofile",
        {"id": IDS["user_id"], "height": 175, "workout_reminder_active": 1, "calories": None},
    )

    # Category: create / patch / delete with per-side ids
    cat_l, cat_r = uid(), uid()
    compare_upload(
        "category create",
        "PUT",
        "measurements_category",
        {"id": cat_l, "name": "harness-l", "unit": "kg"},
        {"id": cat_r, "name": "harness-r", "unit": "kg", "chart_config": "{}"},
    )
    compare_upload(
        "category patch",
        "PATCH",
        "measurements_category",
        {"id": cat_l, "name": "harness-l2"},
        {"id": cat_r, "name": "harness-r2"},
    )
    compare_upload(
        "category metric_type immutable",
        "PATCH",
        "measurements_category",
        {"id": cat_l, "metric_type": "height"},
        {"id": cat_r, "metric_type": "height"},
    )
    compare_upload("category delete", "DELETE", "measurements_category", {"id": cat_l}, {"id": cat_r})
    compare_upload("category delete missing", "DELETE", "measurements_category", {"id": cat_l}, {"id": cat_r})

    # Measurement under the seeded category
    m_l, m_r = uid(), uid()
    compare_upload(
        "measurement create",
        "PUT",
        "measurements_measurement",
        {"id": m_l, "category": IDS["measurement_category_id"], "value": 81.5, "date": now},
        {
            "id": m_r,
            "category": IDS["measurement_category_id"],
            "value": 82.5,
            "date": now,
            "extra_data": "{}",
        },
    )
    compare_upload(
        "measurement patch",
        "PATCH",
        "measurements_measurement",
        {"id": m_l, "value": 80.25, "notes": "x"},
        {"id": m_r, "value": 83.25, "notes": "x"},
    )
    compare_upload(
        "measurement negative value",
        "PATCH",
        "measurements_measurement",
        {"id": m_l, "value": -1},
        {"id": m_r, "value": -1},
    )
    compare_upload(
        "measurement foreign category",
        "PUT",
        "measurements_measurement",
        {"id": uid(), "category": uid(), "value": 1},
    )
    compare_upload("measurement delete", "DELETE", "measurements_measurement", {"id": m_l}, {"id": m_r})
    compare_upload("measurement patch missing", "PATCH", "measurements_measurement", {"id": m_l, "value": 1})

    # Idempotent create
    dup_l, dup_r = uid(), uid()
    body_l = {"id": dup_l, "category": IDS["measurement_category_id"], "value": 70}
    body_r = {"id": dup_r, "category": IDS["measurement_category_id"], "value": 71}
    compare_upload("measurement create (dup first)", "PUT", "measurements_measurement", body_l, body_r)
    compare_upload("measurement create (dup second)", "PUT", "measurements_measurement", body_l, body_r)
    compare_upload("measurement delete dup", "DELETE", "measurements_measurement", {"id": dup_l}, {"id": dup_r})

    # Nutrition plan / meal / mealitem / logitem
    plan_l, plan_r = uid(), uid()
    compare_upload(
        "plan create",
        "PUT",
        "nutrition_nutritionplan",
        {"id": plan_l, "description": "harness"},
        {"id": plan_r, "description": "harness"},
    )
    meal_l, meal_r = uid(), uid()
    compare_upload(
        "meal create",
        "PUT",
        "nutrition_meal",
        {"id": meal_l, "plan": plan_l, "name": "breakfast", "time": "08:30:00"},
        {"id": meal_r, "plan": plan_r, "name": "breakfast", "time": "08:30:00"},
    )
    compare_upload(
        "meal patch",
        "PATCH",
        "nutrition_meal",
        {"id": meal_l, "name": "brunch"},
        {"id": meal_r, "name": "brunch"},
    )
    item_l, item_r = uid(), uid()
    compare_upload(
        "mealitem create",
        "PUT",
        "nutrition_mealitem",
        {"id": item_l, "meal": meal_l, "ingredient": IDS["ingredient_id"], "amount": 100},
        {"id": item_r, "meal": meal_r, "ingredient": IDS["ingredient_id"], "amount": 150},
    )
    compare_upload(
        "mealitem patch",
        "PATCH",
        "nutrition_mealitem",
        {"id": item_l, "amount": 120},
        {"id": item_r, "amount": 170},
    )
    log_l, log_r = uid(), uid()
    compare_upload(
        "logitem create",
        "PUT",
        "nutrition_logitem",
        {"id": log_l, "plan": plan_l, "ingredient": IDS["ingredient_id"], "amount": 90, "datetime": now},
        {"id": log_r, "plan": plan_r, "ingredient": IDS["ingredient_id"], "amount": 95, "datetime": now},
    )
    compare_upload("mealitem delete", "DELETE", "nutrition_mealitem", {"id": item_l}, {"id": item_r})
    compare_upload("logitem delete", "DELETE", "nutrition_logitem", {"id": log_l}, {"id": log_r})
    compare_upload("meal delete", "DELETE", "nutrition_meal", {"id": meal_l}, {"id": meal_r})
    compare_upload("plan delete", "DELETE", "nutrition_nutritionplan", {"id": plan_l}, {"id": plan_r})

    # Manager session / log
    sess_l, sess_r = uid(), uid()
    compare_upload(
        "session create",
        "PUT",
        "manager_workoutsession",
        {"id": sess_l, "routine": IDS["routine_id"], "datetime_start": now, "impression": "2"},
        {"id": sess_r, "routine": IDS["routine_id"], "datetime_start": now, "impression": "2"},
    )
    compare_upload(
        "session bad impression",
        "PATCH",
        "manager_workoutsession",
        {"id": sess_l, "impression": "9"},
        {"id": sess_r, "impression": "9"},
    )
    compare_upload(
        "session patch",
        "PATCH",
        "manager_workoutsession",
        {"id": sess_l, "notes": "felt good"},
        {"id": sess_r, "notes": "felt good"},
    )
    logl_l, logl_r = uid(), uid()
    compare_upload(
        "workoutlog create",
        "PUT",
        "manager_workoutlog",
        {
            "id": logl_l,
            "date": now,
            "exercise": IDS["exercise_id"],
            "routine": IDS["routine_id"],
            "session": sess_l,
            "weight": 100,
            "repetitions": 5,
        },
        {
            "id": logl_r,
            "date": now,
            "exercise": IDS["exercise_id"],
            "routine": IDS["routine_id"],
            "session": sess_r,
            "weight": 101,
            "repetitions": 5,
        },
    )
    compare_upload(
        "workoutlog patch",
        "PATCH",
        "manager_workoutlog",
        {"id": logl_l, "weight": 102.5},
        {"id": logl_r, "weight": 103.5},
    )
    compare_upload(
        "workoutlog missing exercise fk",
        "PUT",
        "manager_workoutlog",
        {"id": uid(), "date": now, "exercise": 99999999},
    )
    compare_upload("workoutlog delete", "DELETE", "manager_workoutlog", {"id": logl_l}, {"id": logl_r})
    compare_upload("session delete", "DELETE", "manager_workoutsession", {"id": sess_l}, {"id": sess_r})

    # Routine PATCH on the seeded routine (restore value afterwards)
    compare_upload("routine patch", "PATCH", "manager_routine", {"id": IDS["routine_id"], "name": "harness routine"})
    compare_upload("routine create refusal", "PUT", "manager_routine", {"id": 999, "name": "x"})

    # The upload suite must not disturb rows it did not create: the seeded
    # meal and its items have to survive (guards against unscoped DELETEs).
    status, raw, _ = fetch(RUST, "GET", f"/api/v2/meal/?plan={IDS['plan_id']}", token_header())
    ok = status == 200 and any(m["id"] == IDS["meal_id"] for m in json.loads(raw).get("results", []))
    record("upload suite left seeded meal intact", ok, f"status {status} {raw[:200]!r}")
    status, raw, _ = fetch(RUST, "GET", f"/api/v2/mealitem/?meal={IDS['meal_id']}", token_header())
    ok = status == 200 and any(m["id"] == IDS["mealitem_id"] for m in json.loads(raw).get("results", []))
    record("upload suite left seeded mealitem intact", ok, f"status {status} {raw[:200]!r}")


def check_login(base, name):
    status, raw, _ = fetch(
        base,
        "POST",
        "/allauth/app/v1/auth/login",
        body={"username": IDS["username"], "password": IDS["password"]},
    )
    if status != 200:
        record(name, False, f"status {status} {raw[:300]!r}")
        return None
    body = json.loads(raw)
    meta = body.get("meta") or {}
    access = meta.get("access_token")
    refresh = meta.get("refresh_token")
    try:
        access_claims = jwt_claims(access)
        refresh_claims = jwt_claims(refresh)
    except Exception as exc:  # noqa: BLE001
        record(name, False, f"jwt {exc}")
        return None
    ok = (
        access_claims.get("token_use") == "access"
        and refresh_claims.get("token_use") == "refresh"
        and access_claims.get("sub") == str(IDS["user_id"])
        and access_claims.get("exp", 0) - access_claims.get("iat", 0) == 300
        and body.get("status") == 200
        and body["data"]["user"]["username"] == "admin"
    )
    record(name, ok, "" if ok else json.dumps({"access": access_claims, "user": body["data"]["user"]})[:400])
    return refresh


def main():
    auth = token_header()
    i = IDS
    gets = [
        ("GET /api/v2/", "GET", "/api/v2/", auth),
        ("GET version", "GET", "/api/v2/version/", None),
        ("GET min-app-version", "GET", "/api/v2/min-app-version/", None),
        ("GET min-server-version", "GET", "/api/v2/min-server-version/", None),
        ("GET userprofile", "GET", "/api/v2/userprofile/", auth),
        ("GET language", "GET", "/api/v2/language/?limit=20", None),
        ("GET license", "GET", "/api/v2/license/", None),
        ("GET setting-repetitionunit", "GET", "/api/v2/setting-repetitionunit/", None),
        ("GET setting-weightunit", "GET", "/api/v2/setting-weightunit/", None),
        ("GET exerciseinfo id", "GET", f"/api/v2/exerciseinfo/{i['exercise_id']}/", auth),
        ("GET exerciseinfo search", "GET", "/api/v2/exerciseinfo/?name__search=bench%20press&language__code=en&limit=3", auth),
        ("GET exerciseinfo q ignored", "GET", "/api/v2/exerciseinfo/?q=bench&limit=5", None),
        ("GET exerciseinfo id__in", "GET", f"/api/v2/exerciseinfo/?id__in={i['exercise_id']},{i['other_exercise_id']}", auth),
        ("GET exercise list", "GET", "/api/v2/exercise/?limit=20", None),
        ("GET exercise q ignored", "GET", "/api/v2/exercise/?q=bench", None),
        ("GET exercise one", "GET", f"/api/v2/exercise/{i['exercise_id']}/", None),
        ("GET translation language", "GET", f"/api/v2/exercise-translation/?language={i['language_id']}&limit=20", None),
        ("GET translation name exact", "GET", "/api/v2/exercise-translation/?name=Bench%20press", None),
        ("GET equipment", "GET", "/api/v2/equipment/", None),
        ("GET category", "GET", "/api/v2/exercisecategory/", None),
        ("GET muscle", "GET", "/api/v2/muscle/", None),
        ("GET exerciseimage", "GET", "/api/v2/exerciseimage/?limit=20", None),
        ("GET video", "GET", "/api/v2/video/?limit=20", None),
        ("GET comment", "GET", "/api/v2/exercisecomment/?limit=20", None),
        ("GET alias", "GET", "/api/v2/exercisealias/?limit=20", None),
        ("GET deletion-log", "GET", "/api/v2/deletion-log/", None),
        ("GET routine list", "GET", "/api/v2/routine/?limit=20", auth),
        ("GET routine detail", "GET", f"/api/v2/routine/{i['routine_id']}/", auth),
        ("GET routine structure", "GET", f"/api/v2/routine/{i['routine_id']}/structure/", auth),
        ("GET date-sequence-display", "GET", f"/api/v2/routine/{i['routine_id']}/date-sequence-display/", auth),
        ("GET date-sequence-gym", "GET", f"/api/v2/routine/{i['routine_id']}/date-sequence-gym/", auth),
        ("GET templates", "GET", "/api/v2/templates/?limit=20", auth),
        ("GET public-templates", "GET", "/api/v2/public-templates/?limit=20", auth),
        ("GET routine is_template", "GET", "/api/v2/routine/?is_template=true", auth),
        ("GET day", "GET", f"/api/v2/day/?routine={i['routine_id']}", auth),
        ("GET slot", "GET", f"/api/v2/slot/?day={i['day_id']}", auth),
        ("GET slot-entry", "GET", f"/api/v2/slot-entry/?slot={i['slot_id']}", auth),
        ("GET slot-entry one", "GET", f"/api/v2/slot-entry/{i['entry_id']}/", auth),
        ("GET sets-config", "GET", f"/api/v2/sets-config/?slot_entry={i['entry_id']}", auth),
        ("GET reps-config", "GET", f"/api/v2/repetitions-config/?slot_entry={i['entry_id']}", auth),
        ("GET weight-config", "GET", f"/api/v2/weight-config/?slot_entry={i['entry_id']}", auth),
        ("GET rest-config", "GET", f"/api/v2/rest-config/?slot_entry={i['entry_id']}", auth),
        ("GET rir-config", "GET", f"/api/v2/rir-config/?slot_entry={i['entry_id']}", auth),
        ("GET max-sets", "GET", "/api/v2/max-sets-config/?limit=20", auth),
        ("GET max-reps one", "GET", f"/api/v2/max-repetitions-config/{i['max_reps_id']}/", auth),
        ("GET max-weight", "GET", "/api/v2/max-weight-config/?limit=20", auth),
        ("GET max-rest", "GET", "/api/v2/max-rest-config/?limit=20", auth),
        ("GET max-rir", "GET", "/api/v2/max-rir-config/?limit=20", auth),
        ("GET session", "GET", f"/api/v2/workoutsession/?routine={i['routine_id']}", auth),
        ("GET session ordering", "GET", "/api/v2/workoutsession/?ordering=datetime_start&limit=20", auth),
        ("GET workoutlog session", "GET", f"/api/v2/workoutlog/?session={i['session_id']}", auth),
        ("GET workoutlog exercise", "GET", f"/api/v2/workoutlog/?exercise={i['exercise_id']}", auth),
        ("GET workoutlog date", "GET", "/api/v2/workoutlog/?date__gte=2024-01-01&date__lte=2024-01-31", auth),
        ("GET ingredient search", "GET", "/api/v2/ingredient/?name__search=banana&language__code=en&limit=5", None),
        ("GET ingredient one", "GET", f"/api/v2/ingredient/{i['ingredient_id']}/", None),
        ("GET ingredientinfo search", "GET", "/api/v2/ingredientinfo/?name__search=banana&language__code=en&limit=3", auth),
        ("GET ingredientinfo barcode", "GET", f"/api/v2/ingredientinfo/?name__search={i['barcode']}", auth),
        ("GET ingredientinfo code", "GET", f"/api/v2/ingredientinfo/?code={i['barcode']}", auth),
        ("GET ingredientinfo id__in", "GET", f"/api/v2/ingredientinfo/?id__in={i['ingredient_id']}", auth),
        ("GET ingredient list", "GET", "/api/v2/ingredient/?limit=20&offset=0", None),
        ("GET weight units", "GET", "/api/v2/ingredientweightunit/?limit=20", None),
        ("GET nutritionplan", "GET", "/api/v2/nutritionplan/?limit=20", auth),
        ("GET nutritionplan one", "GET", f"/api/v2/nutritionplan/{i['plan_id']}/", auth),
        ("GET nutritional_values", "GET", f"/api/v2/nutritionplan/{i['plan_id']}/nutritional_values/", auth),
        ("GET nutritionplaninfo", "GET", f"/api/v2/nutritionplaninfo/{i['plan_id']}/", auth),
        ("GET meal", "GET", f"/api/v2/meal/?plan={i['plan_id']}", auth),
        ("GET mealitem", "GET", f"/api/v2/mealitem/?meal={i['meal_id']}", auth),
        ("GET diary date", "GET", f"/api/v2/nutritiondiary/?datetime__date=2024-01-03&plan={i['plan_id']}&limit=20", auth),
        ("GET diary ordering", "GET", f"/api/v2/nutritiondiary/?plan={i['plan_id']}&ordering=-datetime", auth),
        ("GET weightentry", "GET", "/api/v2/weightentry/?ordering=-date&limit=20", auth),
        ("GET weightentry date", "GET", "/api/v2/weightentry/?date__gte=2024-01-01", auth),
        ("GET measurement", "GET", "/api/v2/measurement/?limit=20", auth),
        ("GET measurement-category", "GET", "/api/v2/measurement-category/?limit=20", auth),
        ("GET user-statistics", "GET", "/api/v2/user-statistics/", auth),
        ("GET gallery", "GET", "/api/v2/gallery/?limit=20", auth),
        ("GET powersync-keys", "GET", "/api/v2/powersync-keys", auth),
    ]
    for name, method, path, headers in gets:
        compare_json(name, method, path, headers)

    compare_status("HEAD version", "HEAD", "/api/v2/version/", None)
    compare_status("HEAD routine", "HEAD", "/api/v2/routine/", auth)
    compare_status("no auth userprofile", "GET", "/api/v2/userprofile/", None)
    compare_status("bad token", "GET", "/api/v2/userprofile/", {"Authorization": "Token nope"})
    compare_status("bad bearer", "GET", "/api/v2/version/", {"Authorization": "Bearer nope"})
    compare_status("missing exercise", "GET", "/api/v2/exerciseinfo/999999/", None)
    compare_status("exercise/search 404", "GET", "/api/v2/exercise/search/", None)
    compare_status("bad login", "POST", "/allauth/app/v1/auth/login", body={"username": "admin", "password": "wrong"})
    compare_status(
        "login missing password",
        "POST",
        "/allauth/app/v1/auth/login",
        body={"username": "admin"},
    )

    # Login mutates the password hash (pbkdf2 -> argon2). Compare the envelope
    # before either side has logged in, then check each implementation's tokens.
    compare_json(
        "POST login envelope",
        "POST",
        "/allauth/app/v1/auth/login",
        body={"username": IDS["username"], "password": IDS["password"]},
        redact=redact_auth,
    )
    django_refresh = check_login(DJANGO, "django login claims")
    rust_refresh = check_login(RUST, "rust login claims")
    if django_refresh:
        compare_status(
            "django refresh of its own token via django",
            "POST",
            "/allauth/app/v1/tokens/refresh",
            body={"refresh_token": django_refresh},
        )
        # Informational: 200 means Django refresh works in Rust (no phone re-login);
        # 400 means cutover re-login is required. Neither is a harness failure.
        rs, raw, _ = fetch(RUST, "POST", "/allauth/app/v1/tokens/refresh", body={"refresh_token": django_refresh})
        if rs == 200:
            notes.append("django refresh accepted by rust (compatible cutover)")
            record("rust accepts django refresh", True, f"status {rs}")
        elif rs == 400:
            notes.append("django refresh rejected by rust (cutover re-login needed)")
            record("rust django refresh cutover note", True, f"status {rs} {raw[:120]!r}")
        else:
            record("rust django refresh unexpected", False, f"status {rs} {raw[:180]!r}")
    if rust_refresh:
        status, raw, _ = fetch(RUST, "POST", "/allauth/app/v1/tokens/refresh", body={"refresh_token": rust_refresh})
        ok = status == 200 and "access_token" in json.loads(raw).get("data", {})
        record("rust refresh rotates", ok, f"status {status} {raw[:180]!r}")
    compare_status("bad refresh", "POST", "/allauth/app/v1/tokens/refresh", body={"refresh_token": "nope"})

    ds, draw, _ = fetch(DJANGO, "GET", "/api/v2/powersync-token", auth)
    rs, rraw, _ = fetch(RUST, "GET", "/api/v2/powersync-token", auth)
    if ds == rs == 200:
        left = rewrite(json.loads(draw))
        right = rewrite(json.loads(rraw))
        for side in (left, right):
            side.pop("token", None)
        found = diff(left, right)
        record("powersync-token body", not found, "\n".join(found[:8]))
        claims = jwt_claims(json.loads(rraw)["token"])
        ok = claims.get("aud") == "powersync" and claims.get("sub") == str(i["user_id"]) and claims.get("exp", 0) - claims.get("iat", 0) == 600
        record("powersync claims", ok, str(claims)[:300])
        bearer = {"Authorization": f"Bearer {json.loads(rraw)['token']}"}
        compare_status("powersync token is not an API bearer", "GET", "/api/v2/userprofile/", bearer)
    else:
        record("powersync-token", False, f"django={ds} rust={rs} {rraw[:200]!r}")

    # Rust serves the slash-less form with the same body. Django redirects.
    for path in ("/api/v2/version", f"/api/v2/exerciseinfo/{i['exercise_id']}"):
        slash_status, slash_body, _ = fetch(RUST, "GET", path + "/", None if "version" in path else auth)
        noslash_status, noslash_body, _ = fetch(RUST, "GET", path, None if "version" in path else auth)
        record(
            f"rust no-slash {path}",
            slash_status == noslash_status == 200 and slash_body == noslash_body,
            f"{slash_status}/{noslash_status}",
        )

    status, raw, _ = fetch(RUST, "GET", "/healthz")
    record("healthz", status == 200 and json.loads(raw).get("status") == "ok", f"{status} {raw[:80]!r}")

    status, raw, ctype = fetch(RUST, "GET", "/media/probe.bin")
    record("media full", status == 200 and raw == b"0123456789" * 200, f"{status} {len(raw)} {ctype}")
    req = urllib.request.Request(RUST + "/media/probe.bin", method="GET")
    req.add_header("Range", "bytes=0-9")
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            ranged = resp.read()
            record("media range", resp.status == 206 and ranged == b"0123456789", f"{resp.status} {ranged!r}")
    except urllib.error.HTTPError as exc:
        record("media range", False, f"{exc.code}")

    upload_checks()
    # writes_checks imports `compare`; alias it to this running module so its
    # records land in the same pass/fail lists.
    sys.modules.setdefault("compare", sys.modules[__name__])
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from writes_checks import rest_write_checks

    rest_write_checks()
    shape_check()
    print(f"PASS {len(passes)}  FAIL {len(fails)}")
    for name, detail in fails:
        print(f"FAIL {name}")
        print(detail[:800])
        print("---")
    if notes:
        print("NOTES")
        for note in notes:
            print(note)
    return 0 if not fails else 1


def keys_of(value):
    if isinstance(value, dict):
        return {key: keys_of(val) for key, val in value.items()}
    if isinstance(value, list):
        return [keys_of(value[0])] if value else []
    return type(value).__name__


def shape_check():
    version = json.loads((FIXTURES / "version.json").read_text())["body"]
    status, raw, _ = fetch(RUST, "GET", "/api/v2/version/")
    record("fixture version", status == 200 and json.loads(raw) == version, raw[:80].decode())
    minimum = json.loads((FIXTURES / "min-app-version.json").read_text())["body"]
    status, raw, _ = fetch(RUST, "GET", "/api/v2/min-app-version/")
    record("fixture min-app", status == 200 and json.loads(raw) == minimum, raw[:80].decode())
    profile_keys = set(json.loads((FIXTURES / "userprofile.json").read_text())["body"])
    status, raw, _ = fetch(RUST, "GET", "/api/v2/userprofile/", token_header())
    if status == 200:
        got = set(json.loads(raw))
        record("fixture userprofile keys", profile_keys <= got, f"missing {profile_keys - got}")
    else:
        record("fixture userprofile keys", False, f"status {status}")
    info = json.loads((FIXTURES / "exerciseinfo-search-name.json").read_text())["body"]["results"][0]
    status, raw, _ = fetch(RUST, "GET", f"/api/v2/exerciseinfo/{IDS['exercise_id']}/", token_header())
    if status == 200:
        record(
            "fixture exerciseinfo keys",
            set(info) <= set(json.loads(raw)),
            f"missing {set(info) - set(json.loads(raw))}",
        )
    else:
        record("fixture exerciseinfo keys", False, f"status {status}")
    ing = json.loads((FIXTURES / "ingredientinfo-search-name.json").read_text())["body"]["results"][0]
    status, raw, _ = fetch(RUST, "GET", f"/api/v2/ingredientinfo/{IDS['ingredient_id']}/", token_header())
    if status == 200:
        record(
            "fixture ingredientinfo keys",
            set(ing) <= set(json.loads(raw)),
            f"missing {set(ing) - set(json.loads(raw))}",
        )
    else:
        record("fixture ingredientinfo keys", False, f"status {status}")
    ps = json.loads((FIXTURES / "powersync-token.json").read_text())["body"]
    status, raw, _ = fetch(RUST, "GET", "/api/v2/powersync-token", token_header())
    if status == 200:
        record("fixture powersync keys", set(ps) <= set(json.loads(raw)), f"missing {set(ps) - set(json.loads(raw))}")
    else:
        record("fixture powersync keys", False, f"status {status}")
    notes.append("routine structure/date-sequence fixtures are exact-matched by cargo test engine::")


if __name__ == "__main__":
    sys.exit(main())
