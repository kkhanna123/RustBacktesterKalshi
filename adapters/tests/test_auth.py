"""Unit tests for kalshi_auth — RSA-PSS signature round-trip with an ephemeral key."""

import base64
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from cryptography.hazmat.primitives import hashes  # noqa: E402
from cryptography.hazmat.primitives.asymmetric import padding, rsa  # noqa: E402

import kalshi_auth  # noqa: E402


class TestAuth(unittest.TestCase):
    def setUp(self):
        self.private_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        self.public_key = self.private_key.public_key()
        self.key_id = "test-key-id"
        self.path = "/trade-api/ws/v2"

    def test_headers_present(self):
        headers = kalshi_auth.build_headers(
            self.key_id, self.private_key, "GET", self.path, timestamp_ms=1700000000000
        )
        self.assertEqual(headers["KALSHI-ACCESS-KEY"], self.key_id)
        self.assertEqual(headers["KALSHI-ACCESS-TIMESTAMP"], "1700000000000")
        self.assertIn("KALSHI-ACCESS-SIGNATURE", headers)

    def test_signature_roundtrip_verifies(self):
        ts = "1700000000000"
        headers = kalshi_auth.build_headers(
            self.key_id, self.private_key, "GET", self.path, timestamp_ms=ts
        )
        message = f"{ts}GET{self.path}".encode("utf-8")
        sig = base64.b64decode(headers["KALSHI-ACCESS-SIGNATURE"])
        # verify() raises InvalidSignature on failure; passing == valid
        self.public_key.verify(
            sig,
            message,
            padding.PSS(
                mgf=padding.MGF1(hashes.SHA256()),
                salt_length=hashes.SHA256().digest_size,
            ),
            hashes.SHA256(),
        )

    def test_wrong_message_fails(self):
        from cryptography.exceptions import InvalidSignature

        ts = "1700000000000"
        headers = kalshi_auth.build_headers(
            self.key_id, self.private_key, "GET", self.path, timestamp_ms=ts
        )
        sig = base64.b64decode(headers["KALSHI-ACCESS-SIGNATURE"])
        bad_message = f"{ts}GET/wrong/path".encode("utf-8")
        with self.assertRaises(InvalidSignature):
            self.public_key.verify(
                sig,
                bad_message,
                padding.PSS(
                    mgf=padding.MGF1(hashes.SHA256()),
                    salt_length=hashes.SHA256().digest_size,
                ),
                hashes.SHA256(),
            )

    def test_sign_request_from_pem_file(self):
        import tempfile

        from cryptography.hazmat.primitives import serialization

        pem = self.private_key.private_bytes(
            encoding=serialization.Encoding.PEM,
            format=serialization.PrivateFormat.PKCS8,
            encryption_algorithm=serialization.NoEncryption(),
        )
        with tempfile.NamedTemporaryFile(suffix=".pem", delete=False) as tf:
            tf.write(pem)
            pem_path = tf.name
        try:
            headers = kalshi_auth.sign_request(
                self.key_id, pem_path, "GET", self.path, timestamp_ms="1700000000000"
            )
            message = f"1700000000000GET{self.path}".encode("utf-8")
            sig = base64.b64decode(headers["KALSHI-ACCESS-SIGNATURE"])
            self.public_key.verify(
                sig,
                message,
                padding.PSS(
                    mgf=padding.MGF1(hashes.SHA256()),
                    salt_length=hashes.SHA256().digest_size,
                ),
                hashes.SHA256(),
            )
        finally:
            os.unlink(pem_path)


if __name__ == "__main__":
    unittest.main()
