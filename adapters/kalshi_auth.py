"""kalshi_auth.py — Kalshi RSA-PSS request signing (pure, unit-testable).

Kalshi authenticates both REST and the WS handshake with three headers:
  KALSHI-ACCESS-KEY        : <key_id>
  KALSHI-ACCESS-TIMESTAMP  : <unix_ms>
  KALSHI-ACCESS-SIGNATURE  : base64( RSA-PSS-sign( SHA256, message ) )

where message = f"{timestamp_ms}{method}{path}". For the WS handshake the method is
GET and the path is "/trade-api/ws/v2".

The signature uses PSS padding with MGF1(SHA256) and salt_length == digest length.
"""

import base64
import time

from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding


def load_private_key(private_key_pem_path, password=None):
    """Load an RSA private key from a PEM file path."""
    with open(private_key_pem_path, "rb") as f:
        return serialization.load_pem_private_key(f.read(), password=password)


def sign_message(private_key, message):
    """RSA-PSS sign a unicode message, returning a base64 string."""
    if isinstance(message, str):
        message = message.encode("utf-8")
    signature = private_key.sign(
        message,
        padding.PSS(
            mgf=padding.MGF1(hashes.SHA256()),
            salt_length=hashes.SHA256().digest_size,
        ),
        hashes.SHA256(),
    )
    return base64.b64encode(signature).decode("ascii")


def build_headers(key_id, private_key, method, path, timestamp_ms=None):
    """Build the 3 Kalshi auth headers given an already-loaded private key."""
    if timestamp_ms is None:
        timestamp_ms = int(time.time() * 1000)
    timestamp_ms = str(timestamp_ms)
    message = f"{timestamp_ms}{method}{path}"
    signature = sign_message(private_key, message)
    return {
        "KALSHI-ACCESS-KEY": key_id,
        "KALSHI-ACCESS-TIMESTAMP": timestamp_ms,
        "KALSHI-ACCESS-SIGNATURE": signature,
    }


def sign_request(key_id, private_key_pem_path, method, path, timestamp_ms=None):
    """Load the PEM key and return the 3 Kalshi auth headers for (method, path)."""
    private_key = load_private_key(private_key_pem_path)
    return build_headers(key_id, private_key, method, path, timestamp_ms=timestamp_ms)
