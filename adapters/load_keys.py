"""Load Kalshi creds from the gitignored KalshiAPIKeysDONOTPUSH/.api_keys file.

Format: an id line (bare id, or labelled like "API Key ID: <uuid>") followed by an RSA
private-key PEM block. Writes the PEM to KalshiAPIKeysDONOTPUSH/kalshi_private_key.pem
(also gitignored) and returns (key_id, pem_path). Centralizes secret handling so nothing
else hardcodes paths.
"""
import os

DEFAULT_KEYS = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                            "KalshiAPIKeysDONOTPUSH", ".api_keys")


def load_keys(path=None):
    path = path or os.environ.get("KALSHI_KEYS_FILE", DEFAULT_KEYS)
    with open(path) as f:
        raw = f.read()
    lines = raw.splitlines()
    # The id line may be bare or labelled, e.g. "API Key ID: <uuid>"; take text after the last ':'.
    id_line = next(l.strip() for l in lines if l.strip() and "BEGIN" not in l)
    key_id = id_line.split(":")[-1].strip() if ":" in id_line else id_line
    begin = raw.find("-----BEGIN")
    pem = raw[begin:].strip() + "\n"
    pem_path = os.path.join(os.path.dirname(path), "kalshi_private_key.pem")
    with open(pem_path, "w") as f:
        f.write(pem)
    os.chmod(pem_path, 0o600)
    return key_id, pem_path


if __name__ == "__main__":
    kid, pem = load_keys()
    print("key_id chars:", len(kid))
    print("pem_path:", pem)
    print("pem starts:", open(pem).readline().strip())
