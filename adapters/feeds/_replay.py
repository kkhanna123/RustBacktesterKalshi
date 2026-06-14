"""_replay.py — a tiny file source so stub feeds are usable WITHOUT live access.

``FileReplaySource`` opens a local NDJSON (one JSON object per line) or CSV file and
yields one parsed record per ``recv()`` call, raising :class:`feeds.base.StopFeed` at
EOF. Stub feeds (Polymarket, Hyperliquid) compose this so their ``normalize`` path can
be exercised today against recorded venue messages — no creds, no network.
"""

from __future__ import annotations

import csv
import json

from .base import StopFeed


class FileReplaySource:
    """Yield parsed records from an NDJSON or CSV file, one per ``recv()``.

    Args:
        path: file to replay.
        fmt: ``"ndjson"`` (default; each line a JSON object) or ``"csv"``
            (each row a dict keyed by the header).
    """

    def __init__(self, path: str, fmt: str = "ndjson"):
        self.path = path
        self.fmt = fmt
        self._iter = None
        self._fh = None

    def connect(self) -> None:
        self._fh = open(self.path, "r", encoding="utf-8")
        if self.fmt == "csv":
            self._iter = iter(csv.DictReader(self._fh))
        else:
            self._iter = (line for line in self._fh if line.strip())

    def recv(self):
        if self._iter is None:
            raise StopFeed
        try:
            rec = next(self._iter)
        except StopIteration:
            raise StopFeed
        if self.fmt == "csv":
            return rec
        return json.loads(rec)

    def close(self) -> None:
        if self._fh is not None:
            try:
                self._fh.close()
            except Exception:  # noqa: BLE001
                pass
            self._fh = None
        self._iter = None
