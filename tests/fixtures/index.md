# wger 2.7 live API fixtures (fitness.heavenly.cl)

Captured 2026-10-08 ~17:57 CLT inside `wger-web` (wger/server:2.7, reports 2.7.0) with a Django shell and DRF `APIClient`:
`force_authenticate(user=admin)` (Pablo, user id 1), so no password, login or session row. The whole run was inside a
`SET TRANSACTION READ ONLY` transaction that was rolled back, with `USE_CELERY` switched off in-process so ingredient
serialization could not queue image fetches. The barcode used is one that already exists locally, so there was no Open Food Facts fallback.
Requests used `SERVER_NAME=fitness.heavenly.cl` over https. Emails, tokens and keys are redacted (`<REDACTED>` / `<REDACTED_EMAIL>`); usernames are kept.
Search fixtures use small `limit` values (the app uses `limit=100`); the response shape is the same.

Routines for admin (newest first): 5 'Hypertrophy/Strength 4day', 1 'Strength + Hypertrophy'

| File | Request | Status | Notes |
|---|---|---:|---|
| `version.json` | `GET /api/v2/version/` (anonymous) | 200 | string '2.7.0' |
| `min-app-version.json` | `GET /api/v2/min-app-version/` (anonymous) | 200 | string '2.1.0' |
| `userprofile.json` | `GET /api/v2/userprofile/` (force_authenticate(admin)) | 200 | keys: username, email, email_verified, is_trustworthy, date_joined, gym, weight_rounding, repetitions_rounding |
| `routine-5-structure.json` | `GET /api/v2/routine/5/structure/` (force_authenticate(admin)) | 200 | keys: id, name, description, created, start, end, fit_in_week, days |
| `routine-5-date-sequence-display.json` | `GET /api/v2/routine/5/date-sequence-display/` (force_authenticate(admin)) | 200 | list of 84 |
| `routine-5-date-sequence-gym.json` | `GET /api/v2/routine/5/date-sequence-gym/` (force_authenticate(admin)) | 200 | list of 84 |
| `routine-1-structure.json` | `GET /api/v2/routine/1/structure/` (force_authenticate(admin)) | 200 | keys: id, name, description, created, start, end, fit_in_week, days |
| `routine-1-date-sequence-display.json` | `GET /api/v2/routine/1/date-sequence-display/` (force_authenticate(admin)) | 200 | list of 35 |
| `routine-1-date-sequence-gym.json` | `GET /api/v2/routine/1/date-sequence-gym/` (force_authenticate(admin)) | 200 | list of 35 |
| `ingredientinfo-search-name.json` | `GET /api/v2/ingredientinfo/?name__search=banana&language__code=en&limit=3` (force_authenticate(admin)) | 200 | count=1000, results=3 |
| `ingredientinfo-search-barcode.json` | `GET /api/v2/ingredientinfo/?code=9300633749033` (force_authenticate(admin)) | 200 | count=1, results=1 |
| `exerciseinfo-search-name.json` | `GET /api/v2/exerciseinfo/?name__search=bench%20press&language__code=en&limit=3` (force_authenticate(admin)) | 200 | count=30, results=3 |
| `powersync-token.json` | `GET /api/v2/powersync-token` (force_authenticate(admin)) | 200 | keys: token, powersync_url |
