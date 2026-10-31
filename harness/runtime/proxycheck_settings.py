"""Settings overlay for the production-like Django used by proxy-check.sh.

The wger 2.7 image reads the real names for CSRF_TRUSTED_ORIGINS,
X_FORWARDED_PROTO_HEADER_SET, SECURE_PROXY_SSL_HEADER, and
USE_X_FORWARDED_HOST. It has no environment switch for the cookie flags, so
this overlay applies the two `DJANGO_*_COOKIE_SECURE` variables used by the
harness.
"""

import os

from settings.main import *  # noqa: F401,F403


def _flag(name, default):
    return os.environ.get(name, str(default)).strip().lower() in (
        "1",
        "true",
        "yes",
        "on",
    )


CSRF_COOKIE_SECURE = _flag("DJANGO_CSRF_COOKIE_SECURE", True)
SESSION_COOKIE_SECURE = _flag("DJANGO_SESSION_COOKIE_SECURE", True)
