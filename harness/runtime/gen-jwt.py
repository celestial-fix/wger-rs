#!/usr/bin/env python3
"""Generate an RS256 keypair as a jwt.env file.

Usage: gen-jwt.py <output-path>
"""
import base64
import json
import os
import sys
from pathlib import Path

from cryptography.hazmat.primitives.asymmetric import rsa
from jwt.algorithms import RSAAlgorithm

if len(sys.argv) != 2:
    sys.exit("usage: gen-jwt.py <output-path>")

rsa_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
priv = json.loads(RSAAlgorithm.to_jwk(rsa_key))
priv["alg"] = "RS256"
priv["kid"] = "wger-test"
pub = {k: priv[k] for k in ("kty", "n", "e", "alg", "kid")}


def b64(doc):
    return base64.urlsafe_b64encode(json.dumps(doc).encode()).decode()


out = Path(sys.argv[1])
out.parent.mkdir(parents=True, exist_ok=True)
out.write_text(f"JWT_PRIVATE_KEY={b64(priv)}\nJWT_PUBLIC_KEY={b64(pub)}\n")
os.chmod(out, 0o600)
print(f"wrote {out} ({out.stat().st_size} bytes)")
