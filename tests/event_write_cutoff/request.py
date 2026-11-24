"""Make one signed fixture request to the exact Bot receiver over loopback."""

import hashlib
import hmac
import json
import os
import secrets
import sys
import time
import urllib.error
import urllib.request

port, key, raw = sys.argv[1:]
body = raw.encode()
timestamp = str(int(time.time()))
nonce = secrets.token_hex(16)
route = "/internal/actions"
canonical = f"POST\n{route}\n{timestamp}\n{nonce}\n{hashlib.sha256(body).hexdigest()}"
signature = hmac.new(os.environ["SYNTHETIC_HMAC_KEY"].encode(), canonical.encode(), "sha256").hexdigest()
request = urllib.request.Request(f"http://127.0.0.1:{port}{route}", body, {
    "Content-Type": "application/json", "X-Two-Key-Id": "rehearsal", "X-Two-Timestamp": timestamp,
    "X-Two-Nonce": nonce, "X-Two-Signature": f"sha256={signature}", "Idempotency-Key": key,
})
try:
    response = urllib.request.urlopen(request, timeout=15)
except urllib.error.HTTPError as error:
    response = error
with response:
    value = json.load(response)
    print(json.dumps({
        "status": response.status, "replay": response.headers.get("Idempotent-Replay") == "true",
        "code": value.get("error", {}).get("code"), "outcome": value.get("result", {}).get("outcome"),
    }))
