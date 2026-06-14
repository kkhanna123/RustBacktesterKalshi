#!/usr/bin/env python3
"""
to_canonical.py — turn ANY tabular market data into the backtester's canonical NDJSON.

WHY THIS EXISTS
===============
The Rust backtester reads a very specific newline-delimited-JSON (NDJSON) format via
``--source ndjson``. Every venue's raw data looks different (different column names, different
timestamp units, prices in cents vs dollars, "bid/ask" vs "buy/sell" vs "yes/no"). Rather than
write new Rust for every feed, this tool maps *your* columns onto the canonical contract using a
small JSON/TOML "ingest profile". Result: any feed integrates with ZERO Rust.

THE CANONICAL CONTRACT (must match exactly — confirmed against rust-backtester/src/data/ndjson.rs
and src/adapters/generic_ndjson.rs)
-------------------------------------------------------------------------------------------------
delta line:
  {"kind":"delta","ts_ns":<i64>,"instrument":"<str>","action":"ADD"|"UPDATE"|"DELETE",
   "side":"BUY"|"SELL","price":<f64 dollars in (0,1]>,"size":<f64 contracts>,
   "sequence":<i64>,"is_snapshot":0|1,"venue":"<TAG>","market_alias":""}

trade line:
  {"kind":"trade","ts_ns":<i64>,"instrument":"<str>","aggressor_side":"yes"|"no",
   "price":<f64 dollars>,"size":<f64>,"trade_id":"<str>","venue":"<TAG>"}

The order book is YES-native:
  * a BUY level  = a YES bid
  * a SELL level = a YES ask
  * a NO bid at price q  == a YES ask at (1 - q)   (use the NO->YES complement helper)

``instrument`` may be a bare symbol (e.g. "BTC-PERP") or "VENUE:symbol". Always set ``venue``.

USAGE (CLI)
-----------
  # Convert a CSV with an explicit profile:
  python tools/to_canonical.py --input data.csv --profile tools/profiles/l2_delta_csv.json \
      --out out.ndjson.gz

  # Guess a profile from column names and preview the first 5 canonical events (no file written):
  python tools/to_canonical.py --input data.csv --infer --preview 5

  # Parquet works too:
  python tools/to_canonical.py --input data.parquet --profile p.json --out out.ndjson.gz

USAGE (library)
---------------
  from to_canonical import convert, IngestProfile, CanonicalWriter
  convert("data.csv", "profile.json", "out.ndjson.gz")

This file is intentionally a single, dependency-light module (pandas + stdlib) so beginners can
read it top to bottom. The small conversion helpers near the top are independently unit-tested in
tools/tests/.
"""

from __future__ import annotations

import argparse
import gzip
import io
import json
import math
import os
import sys
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any, Dict, Iterable, Iterator, List, Optional, Tuple, Union

# pandas is only required for the DataFrame/CSV/Parquet conversion path. The pure helpers
# (to_ns, to_dollars, to_side, no_to_yes, ...) work with no third-party dependency, so we import
# pandas lazily inside the functions that need it. This keeps the unit tests for helpers fast and
# import-error-free even in a bare environment.


# ======================================================================================
# SECTION 1 — Small, composable conversion helpers (each one independently unit-tested).
# ======================================================================================

# ---- Timestamps ----------------------------------------------------------------------

# Multipliers to convert a numeric timestamp in the given unit up to nanoseconds.
_NS_PER_UNIT = {
    "s": 1_000_000_000,
    "ms": 1_000_000,
    "us": 1_000,
    "ns": 1,
}


def to_ns(value: Any, unit: str = "ns", fmt: Optional[str] = None) -> int:
    """Convert a timestamp into an i64 count of nanoseconds since the Unix epoch (UTC).

    Parameters
    ----------
    value : the raw timestamp. For numeric units it may be an int/float or a numeric string.
            For ``unit="iso"`` it must be a string (ISO-8601, optionally with a trailing 'Z').
    unit  : one of ``"s" | "ms" | "us" | "ns" | "iso"``.
              * "s"/"ms"/"us"/"ns": ``value`` is a number in that unit since the epoch.
              * "iso": ``value`` is an ISO-8601 datetime string. If ``fmt`` is given it is parsed
                with ``datetime.strptime`` using that strftime/strptime format instead.
    fmt   : optional strptime format string (only used when ``unit == "iso"``). Use this for
            non-ISO strings such as ``"%Y-%m-%d %H:%M:%S"``.

    Returns
    -------
    int : nanoseconds since the epoch.

    Examples
    --------
    >>> to_ns(1, "s")
    1000000000
    >>> to_ns("1700000000000", "ms")
    1700000000000000000
    >>> to_ns("2023-11-14T22:13:20Z", "iso")
    1700000000000000000
    """
    if unit == "iso":
        if not isinstance(value, str):
            raise ValueError(f"unit='iso' needs a string timestamp, got {type(value).__name__}")
        s = value.strip()
        if fmt:
            dt = datetime.strptime(s, fmt)
        else:
            # datetime.fromisoformat (3.8) does not accept a trailing 'Z'; normalise it.
            iso = s.replace("Z", "+00:00") if s.endswith("Z") else s
            dt = datetime.fromisoformat(iso)
        # Treat naive timestamps as UTC so results are deterministic across machines.
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=timezone.utc)
        epoch = datetime(1970, 1, 1, tzinfo=timezone.utc)
        return int(round((dt - epoch).total_seconds() * 1_000_000_000))

    if unit not in _NS_PER_UNIT:
        raise ValueError(
            f"unknown ts unit {unit!r}; expected one of s|ms|us|ns|iso"
        )
    # Accept numeric strings transparently.
    if isinstance(value, str):
        value = float(value) if ("." in value or "e" in value.lower()) else int(value)
    if isinstance(value, float):
        if not math.isfinite(value):
            raise ValueError(f"non-finite timestamp {value!r}")
        return int(round(value * _NS_PER_UNIT[unit]))
    return int(value) * _NS_PER_UNIT[unit]


# ---- Prices --------------------------------------------------------------------------

# Factor to multiply a raw price by to obtain *dollars* (probability in 0..1 for these markets).
_DOLLAR_FACTOR = {
    "dollars": 1.0,       # already in dollars / probability
    "prob": 1.0,          # probability 0..1 == dollars here
    "cents": 0.01,        # 42 -> 0.42
    "bps": 0.0001,        # 4200 bps -> 0.42
}


def to_dollars(value: Any, scale: str = "dollars", factor: Optional[float] = None) -> float:
    """Convert a raw price into dollars (a YES probability in (0, 1] for binary markets).

    Parameters
    ----------
    value  : numeric price (or numeric string).
    scale  : one of ``"dollars" | "prob" | "cents" | "bps"``.
    factor : optional custom multiplier. If provided it OVERRIDES ``scale`` — the returned value
             is ``value * factor``. Use this for any odd unit (e.g. ``factor=0.001`` for milli-dollars).

    Returns
    -------
    float : price in dollars.

    Examples
    --------
    >>> to_dollars(42, "cents")
    0.42
    >>> to_dollars(4200, "bps")
    0.42
    >>> to_dollars(7, scale="dollars", factor=0.1)
    0.7
    """
    if isinstance(value, str):
        value = float(value)
    value = float(value)
    if factor is not None:
        return value * float(factor)
    if scale not in _DOLLAR_FACTOR:
        raise ValueError(f"unknown price scale {scale!r}; expected dollars|prob|cents|bps")
    return value * _DOLLAR_FACTOR[scale]


# ---- Sides ---------------------------------------------------------------------------

# Tokens that mean "this is a YES bid" (-> canonical "BUY") vs "this is a YES ask" (-> "SELL").
# All matching is case-insensitive. '+' means buy/bid, '-' means sell/ask.
_BUY_TOKENS = {"yes", "bid", "buy", "b", "+", "1", "true"}
_SELL_TOKENS = {"no", "ask", "sell", "s", "a", "-", "0", "false"}


def to_side(token: Any) -> str:
    """Map any side token to the canonical delta ``side`` value ``"BUY"`` or ``"SELL"``.

    Accepts (case-insensitive): yes/no, bid/ask, buy/sell, B/S, A, and the signs +/-.
    Remember the book is YES-native: BUY == YES bid, SELL == YES ask.

    Note: a *NO bid* is NOT simply "SELL" — it is a YES ask at the complement price. Use
    :func:`no_to_yes` to convert the price first, then pass "SELL"/"ask" here.

    >>> to_side("bid"), to_side("ASK"), to_side("+"), to_side("S")
    ('BUY', 'SELL', 'BUY', 'SELL')
    """
    t = str(token).strip().lower()
    if t in _BUY_TOKENS:
        return "BUY"
    if t in _SELL_TOKENS:
        return "SELL"
    raise ValueError(
        f"unrecognised side token {token!r}; expected yes/no, bid/ask, buy/sell, B/S, +/-"
    )


def to_aggressor(token: Any) -> str:
    """Map a trade aggressor token to canonical ``"yes"`` or ``"no"``.

    Accepts the same vocabulary as :func:`to_side` (a buy/bid/+ aggressor took YES).

    >>> to_aggressor("buy"), to_aggressor("no"), to_aggressor("-")
    ('yes', 'no', 'no')
    """
    return "yes" if to_side(token) == "BUY" else "no"


def no_to_yes(price_dollars: float) -> float:
    """Convert a NO-side price into its YES-native equivalent: ``1 - price``.

    A NO bid at q dollars is a YES ask at (1 - q); a NO ask at q is a YES bid at (1 - q).
    So: complement the PRICE with this function, and flip the SIDE (bid<->ask) yourself.

    >>> no_to_yes(0.3)
    0.7
    """
    return 1.0 - float(price_dollars)


def flip_side(side: str) -> str:
    """Flip a canonical side: BUY<->SELL. Useful alongside :func:`no_to_yes`."""
    return "SELL" if side == "BUY" else "BUY"


# ======================================================================================
# SECTION 2 — Canonical event dataclasses + the validating writer.
# ======================================================================================


class ValidationError(ValueError):
    """Raised when an event cannot be made to conform to the canonical contract."""


@dataclass
class DeltaEvent:
    """A single order-book delta row in the canonical (YES-native) format."""

    ts_ns: int
    instrument: str
    action: str          # "ADD" | "UPDATE" | "DELETE"
    side: str            # "BUY" | "SELL"
    price: float         # dollars in (0, 1]
    size: float          # contracts
    sequence: int = 0
    is_snapshot: int = 0  # 0 or 1
    venue: str = "GENERIC"
    market_alias: str = ""

    def to_dict(self) -> Dict[str, Any]:
        return {
            "kind": "delta",
            "ts_ns": int(self.ts_ns),
            "instrument": self.instrument,
            "action": self.action,
            "side": self.side,
            "price": float(self.price),
            "size": float(self.size),
            "sequence": int(self.sequence),
            "is_snapshot": int(self.is_snapshot),
            "venue": self.venue,
            "market_alias": self.market_alias,
        }


@dataclass
class TradeEvent:
    """A single trade print in the canonical format."""

    ts_ns: int
    instrument: str
    aggressor_side: str   # "yes" | "no"
    price: float          # dollars
    size: float
    trade_id: str = ""
    venue: str = "GENERIC"

    def to_dict(self) -> Dict[str, Any]:
        return {
            "kind": "trade",
            "ts_ns": int(self.ts_ns),
            "instrument": self.instrument,
            "aggressor_side": self.aggressor_side,
            "price": float(self.price),
            "size": float(self.size),
            "trade_id": str(self.trade_id),
            "venue": self.venue,
        }


_VALID_ACTIONS = {"ADD", "UPDATE", "DELETE"}
_VALID_SIDES = {"BUY", "SELL"}
_VALID_AGGRESSORS = {"yes", "no"}


class CanonicalWriter:
    """Emits validated, gzip-compressed (or plain) canonical NDJSON.

    Use as a context manager so the file is flushed/closed cleanly::

        with CanonicalWriter("out.ndjson.gz") as w:
            w.write_delta(DeltaEvent(...))
            w.write_trade(TradeEvent(...))
        print(w.stats)

    Validation rules
    ----------------
    * delta ``price`` must be in (0, 1]; ``action`` in {ADD,UPDATE,DELETE}; ``side`` in {BUY,SELL};
      ``is_snapshot`` coerced to 0/1.
    * trade ``price`` must be > 0; ``aggressor_side`` in {yes,no}.
    * ``ts_ns`` non-monotonic is allowed (the backtester sorts), but it is COUNTED and a single
      warning is emitted, because it usually signals a wrong timestamp unit.

    A ``DELETE`` delta is allowed to carry size 0 and is exempt from the price>0 lower-bound only
    in the sense that its level is being removed; we still require a sane price in (0, 1].
    """

    def __init__(
        self,
        path: Optional[str] = None,
        *,
        fileobj: Optional[io.TextIOBase] = None,
        strict_price: bool = True,
        warn: bool = True,
    ) -> None:
        """Open ``path`` for writing. If it ends with ``.gz`` it is gzip-compressed.

        Pass ``fileobj`` instead of ``path`` to write to an already-open text stream (used by
        ``--preview`` to write to stdout). ``strict_price=False`` downgrades out-of-range price
        from an error to a warning (the row is still written). ``warn=False`` silences warnings.
        """
        if (path is None) == (fileobj is None):
            raise ValueError("provide exactly one of path= or fileobj=")
        self.path = path
        self._owns_file = fileobj is None
        if fileobj is not None:
            self._fh: io.TextIOBase = fileobj
        elif path.endswith(".gz"):
            self._fh = io.TextIOWrapper(gzip.open(path, "wb"), encoding="utf-8")
        else:
            self._fh = open(path, "w", encoding="utf-8")
        self.strict_price = strict_price
        self.warn = warn
        self._last_ts: Optional[int] = None
        self.stats = {
            "deltas": 0,
            "trades": 0,
            "snapshots": 0,
            "non_monotonic": 0,
            "warnings": 0,
        }
        self._warned_monotonic = False

    # -- context manager plumbing --
    def __enter__(self) -> "CanonicalWriter":
        return self

    def __exit__(self, *exc) -> None:
        self.close()

    def close(self) -> None:
        if self._owns_file:
            self._fh.close()

    def _warn(self, msg: str) -> None:
        self.stats["warnings"] += 1
        if self.warn:
            print(f"[warn] {msg}", file=sys.stderr)

    def _check_ts(self, ts_ns: int) -> int:
        ts_ns = int(ts_ns)
        if self._last_ts is not None and ts_ns < self._last_ts:
            self.stats["non_monotonic"] += 1
            if not self._warned_monotonic:
                self._warn(
                    f"timestamps are not monotonic (ts {ts_ns} < previous {self._last_ts}). "
                    "The backtester sorts events, so this is tolerated — but double-check your "
                    "ts_unit if you did not expect out-of-order rows."
                )
                self._warned_monotonic = True
        self._last_ts = ts_ns
        return ts_ns

    def _check_price(self, price: float, *, what: str) -> float:
        price = float(price)
        if not math.isfinite(price):
            raise ValidationError(f"{what} price is not finite: {price!r}")
        ok = 0.0 < price <= 1.0
        if not ok:
            msg = (
                f"{what} price {price} is outside (0, 1]. Prices must be in DOLLARS / probability. "
                "If your source is in cents use price_scale='cents'; for NO-side quotes use the "
                "NO->YES complement."
            )
            if self.strict_price:
                raise ValidationError(msg)
            self._warn(msg)
        return price

    def write_delta(self, ev: DeltaEvent) -> None:
        if ev.action not in _VALID_ACTIONS:
            raise ValidationError(f"bad action {ev.action!r}; expected one of {_VALID_ACTIONS}")
        if ev.side not in _VALID_SIDES:
            raise ValidationError(f"bad side {ev.side!r}; expected BUY or SELL")
        ev.ts_ns = self._check_ts(ev.ts_ns)
        ev.price = self._check_price(ev.price, what="delta")
        ev.is_snapshot = 1 if int(ev.is_snapshot) != 0 else 0
        if not ev.instrument:
            raise ValidationError("delta has empty instrument")
        self._fh.write(json.dumps(ev.to_dict()) + "\n")
        self.stats["deltas"] += 1
        if ev.is_snapshot:
            self.stats["snapshots"] += 1

    def write_trade(self, ev: TradeEvent) -> None:
        if ev.aggressor_side not in _VALID_AGGRESSORS:
            raise ValidationError(
                f"bad aggressor_side {ev.aggressor_side!r}; expected 'yes' or 'no'"
            )
        ev.ts_ns = self._check_ts(ev.ts_ns)
        ev.price = self._check_price(ev.price, what="trade")
        if not ev.instrument:
            raise ValidationError("trade has empty instrument")
        self._fh.write(json.dumps(ev.to_dict()) + "\n")
        self.stats["trades"] += 1

    def write(self, ev: Union[DeltaEvent, TradeEvent]) -> None:
        """Dispatch to :meth:`write_delta` / :meth:`write_trade` by type."""
        if isinstance(ev, DeltaEvent):
            self.write_delta(ev)
        elif isinstance(ev, TradeEvent):
            self.write_trade(ev)
        else:
            raise TypeError(f"cannot write {type(ev).__name__}")


# ======================================================================================
# SECTION 3 — Snapshot -> delta differ.
# ======================================================================================


@dataclass
class _BookState:
    """Per-instrument last-known full book: maps (side, price) -> size."""

    levels: Dict[Tuple[str, float], float] = field(default_factory=dict)


class SnapshotDiffer:
    """Turn successive FULL-BOOK snapshots into canonical deltas.

    Many feeds publish a complete book every tick instead of incremental deltas. Feeding those
    raw into the backtester would be wrong (it expects incremental ADD/UPDATE/DELETE). This class
    diffs each new snapshot against the previous one per instrument:

      * brand-new instrument  -> emit every level as ``is_snapshot=1`` ADD rows.
      * a level whose size changed -> ``UPDATE``.
      * a brand-new level -> ``ADD``.
      * a level that vanished -> ``DELETE`` (size 0).

    Feed it one *snapshot* at a time via :meth:`add_level` / :meth:`end_snapshot`, or use the
    convenience :meth:`diff_snapshot` with a list of (side, price, size) levels. It yields
    :class:`DeltaEvent` objects you write to a :class:`CanonicalWriter`.

    Rounding: prices are rounded to ``price_decimals`` (default 4 = sub-cent) so float jitter does
    not create spurious ADD/DELETE churn.
    """

    def __init__(self, venue: str = "GENERIC", price_decimals: int = 4) -> None:
        self.venue = venue
        self.price_decimals = price_decimals
        self._state: Dict[str, _BookState] = {}

    def _round(self, p: float) -> float:
        return round(float(p), self.price_decimals)

    def diff_snapshot(
        self,
        instrument: str,
        ts_ns: int,
        levels: Iterable[Tuple[str, float, float]],
        sequence: int = 0,
    ) -> List[DeltaEvent]:
        """Diff one full snapshot (an iterable of ``(side, price, size)``) for ``instrument``.

        ``side`` is anything :func:`to_side` accepts; ``price`` is already in DOLLARS.
        Returns the list of :class:`DeltaEvent`s representing the change since the previous
        snapshot of this instrument.
        """
        new_book: Dict[Tuple[str, float], float] = {}
        for side, price, size in levels:
            cside = to_side(side)
            key = (cside, self._round(price))
            new_book[key] = float(size)

        first_time = instrument not in self._state
        old = self._state.get(instrument, _BookState()).levels
        out: List[DeltaEvent] = []

        # Additions / updates (iterate sorted for deterministic output).
        for (cside, price), size in sorted(new_book.items()):
            old_size = old.get((cside, price))
            if first_time:
                action, snap = "ADD", 1
            elif old_size is None:
                action, snap = "ADD", 0
            elif old_size != size:
                action, snap = "UPDATE", 0
            else:
                continue  # unchanged
            out.append(
                DeltaEvent(
                    ts_ns=ts_ns, instrument=instrument, action=action, side=cside,
                    price=price, size=size, sequence=sequence, is_snapshot=snap, venue=self.venue,
                )
            )

        # Deletions: levels present before but gone now.
        if not first_time:
            for (cside, price), _old_size in sorted(old.items()):
                if (cside, price) not in new_book:
                    out.append(
                        DeltaEvent(
                            ts_ns=ts_ns, instrument=instrument, action="DELETE", side=cside,
                            price=price, size=0.0, sequence=sequence, is_snapshot=0, venue=self.venue,
                        )
                    )

        self._state[instrument] = _BookState(levels=new_book)
        return out


# ======================================================================================
# SECTION 4 — The ingest profile.
# ======================================================================================


@dataclass
class IngestProfile:
    """Declarative mapping from source columns/options to the canonical fields.

    Load from JSON or TOML with :meth:`from_file`, or build in Python. Every field below is
    documented in tools/README.md (the "profile reference"). The guiding idea: each canonical
    field is either taken from a *column* (``*_col``) or pinned to a *constant* (``*_const``).

    Common options
    --------------
    kind            : "delta" | "trade" | None. If None, ``kind_col`` selects per-row.
    venue           : constant venue TAG (e.g. "POLYMARKET").
    instrument_col / instrument_const : which symbol each row belongs to.
    ts_col, ts_unit, ts_format        : timestamp column + how to parse it (s|ms|us|ns|iso, +fmt).
    price_col, price_scale, price_factor : price column + unit (dollars|cents|bps|prob|custom).

    Delta-specific
    --------------
    side_col / side_const         : YES-native side token (BUY/SELL/bid/ask/yes/no/...).
    action_col / action_const     : ADD|UPDATE|DELETE (default constant "ADD").
    size_col / size_const
    sequence_col / sequence_const  : i64 sequence (default auto-increment).
    is_snapshot_col / is_snapshot_const
    no_side                        : if True, treat side tokens as NO-side and apply NO->YES
                                     complement (price -> 1-price AND side flipped).

    Trade-specific
    --------------
    aggressor_col / aggressor_const : yes|no (or any side token).
    trade_id_col / trade_id_const

    Snapshot mode
    -------------
    snapshot_mode   : if True, rows are full-book levels grouped by (instrument, ts) and diffed
                      into deltas via :class:`SnapshotDiffer`. Requires side_col, price_col,
                      size_col, instrument + ts. ``snapshot_group_col`` optionally identifies a
                      snapshot id; otherwise (instrument, ts_ns) groups a snapshot.
    """

    # routing
    kind: Optional[str] = None
    kind_col: Optional[str] = None
    venue: str = "GENERIC"

    # shared
    instrument_col: Optional[str] = None
    instrument_const: Optional[str] = None
    ts_col: Optional[str] = None
    ts_unit: str = "ns"
    ts_format: Optional[str] = None
    price_col: Optional[str] = None
    price_scale: str = "dollars"
    price_factor: Optional[float] = None
    size_col: Optional[str] = None
    size_const: Optional[float] = None

    # delta
    side_col: Optional[str] = None
    side_const: Optional[str] = None
    action_col: Optional[str] = None
    action_const: str = "ADD"
    sequence_col: Optional[str] = None
    sequence_const: Optional[int] = None
    is_snapshot_col: Optional[str] = None
    is_snapshot_const: int = 0
    no_side: bool = False

    # trade
    aggressor_col: Optional[str] = None
    aggressor_const: Optional[str] = None
    trade_id_col: Optional[str] = None
    trade_id_const: Optional[str] = None

    # snapshot mode
    snapshot_mode: bool = False
    snapshot_group_col: Optional[str] = None
    price_decimals: int = 4

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> "IngestProfile":
        known = {f for f in cls.__dataclass_fields__}  # type: ignore[attr-defined]
        unknown = set(d) - known
        if unknown:
            raise ValueError(
                f"unknown profile key(s): {sorted(unknown)}. Valid keys: {sorted(known)}"
            )
        return cls(**d)

    @classmethod
    def from_file(cls, path: str) -> "IngestProfile":
        """Load a profile from a ``.json`` or ``.toml`` file."""
        with open(path, "rb") as fh:
            raw = fh.read()
        if path.endswith(".toml"):
            try:
                import tomllib  # py3.11+
            except ModuleNotFoundError:  # pragma: no cover
                import tomli as tomllib  # type: ignore
            d = tomllib.loads(raw.decode("utf-8"))
        else:
            d = json.loads(raw.decode("utf-8"))
        return cls.from_dict(d)


# ======================================================================================
# SECTION 5 — The conversion engine.
# ======================================================================================


def _col(row: Dict[str, Any], name: str) -> Any:
    if name not in row:
        raise ValidationError(
            f"profile references column {name!r} which is not in the input. "
            f"Available columns: {sorted(row.keys())}"
        )
    return row[name]


def _resolve(row: Dict[str, Any], col: Optional[str], const: Any) -> Any:
    """Return the row's column value if ``col`` is set, else the constant."""
    if col is not None:
        return _col(row, col)
    return const


def _row_to_event(
    row: Dict[str, Any],
    profile: IngestProfile,
    seq_counter: List[int],
) -> Union[DeltaEvent, TradeEvent]:
    """Convert a single source row (dict) into a canonical event using ``profile``."""
    kind = profile.kind or (str(_col(row, profile.kind_col)).strip().lower()
                            if profile.kind_col else "delta")

    ts_ns = to_ns(_col(row, profile.ts_col), profile.ts_unit, profile.ts_format)
    instrument = _resolve(row, profile.instrument_col, profile.instrument_const)
    if instrument is None:
        raise ValidationError("no instrument: set instrument_col or instrument_const")
    instrument = str(instrument)
    price = to_dollars(_col(row, profile.price_col), profile.price_scale, profile.price_factor)
    size = float(_resolve(row, profile.size_col, profile.size_const) or 0.0)

    if kind == "trade":
        aggressor_raw = _resolve(row, profile.aggressor_col, profile.aggressor_const)
        if aggressor_raw is None:
            raise ValidationError("trade needs aggressor_col or aggressor_const")
        return TradeEvent(
            ts_ns=ts_ns, instrument=instrument,
            aggressor_side=to_aggressor(aggressor_raw),
            price=price, size=size,
            trade_id=str(_resolve(row, profile.trade_id_col, profile.trade_id_const) or ""),
            venue=profile.venue,
        )

    # delta
    side_raw = _resolve(row, profile.side_col, profile.side_const)
    if side_raw is None:
        raise ValidationError("delta needs side_col or side_const")
    side = to_side(side_raw)
    if profile.no_side:
        # NO-side quote: complement the price and flip the side to YES-native.
        price = no_to_yes(price)
        side = flip_side(side)
    action = str(_resolve(row, profile.action_col, profile.action_const)).strip().upper()
    if profile.sequence_col is not None:
        sequence = int(_col(row, profile.sequence_col))
    elif profile.sequence_const is not None:
        sequence = int(profile.sequence_const)
    else:
        seq_counter[0] += 1
        sequence = seq_counter[0]
    is_snap = int(_resolve(row, profile.is_snapshot_col, profile.is_snapshot_const) or 0)
    return DeltaEvent(
        ts_ns=ts_ns, instrument=instrument, action=action, side=side,
        price=price, size=size, sequence=sequence, is_snapshot=is_snap, venue=profile.venue,
    )


def _load_dataframe(src: Union[str, "Any"]):
    """Load a CSV/Parquet path or pass through a pandas DataFrame."""
    import pandas as pd

    if isinstance(src, pd.DataFrame):
        return src
    if not isinstance(src, str):
        raise TypeError(f"expected a path or DataFrame, got {type(src).__name__}")
    if src.endswith(".parquet") or src.endswith(".pq"):
        return pd.read_parquet(src)
    if src.endswith(".csv") or src.endswith(".csv.gz") or src.endswith(".tsv"):
        sep = "\t" if src.endswith(".tsv") else ","
        return pd.read_csv(src, sep=sep)
    # Fallback: try CSV.
    return pd.read_csv(src)


def iter_events(
    src: Union[str, "Any"],
    profile: IngestProfile,
) -> Iterator[Union[DeltaEvent, TradeEvent]]:
    """Yield canonical events from a source (path or DataFrame) under ``profile`` WITHOUT writing.

    This powers ``--preview`` and is handy in notebooks. For snapshot_mode it diffs full books
    into deltas on the fly.
    """
    import pandas as pd  # noqa: F401  (ensures a clear error if pandas missing)

    df = _load_dataframe(src)
    records = df.to_dict(orient="records")
    seq_counter = [0]

    if profile.snapshot_mode:
        differ = SnapshotDiffer(venue=profile.venue, price_decimals=profile.price_decimals)
        # Group consecutive rows by (instrument, ts_ns) or an explicit group column, preserving
        # input order (snapshots must already be ordered in time).
        cur_key = object()
        cur_levels: List[Tuple[str, float, float]] = []
        cur_inst: Optional[str] = None
        cur_ts: Optional[int] = None

        def flush() -> Iterator[DeltaEvent]:
            if cur_inst is not None and cur_levels:
                yield from differ.diff_snapshot(cur_inst, cur_ts, cur_levels)

        for row in records:
            inst = str(_resolve(row, profile.instrument_col, profile.instrument_const))
            ts_ns = to_ns(_col(row, profile.ts_col), profile.ts_unit, profile.ts_format)
            if profile.snapshot_group_col is not None:
                key = (inst, _col(row, profile.snapshot_group_col))
            else:
                key = (inst, ts_ns)
            if key != cur_key:
                yield from flush()
                cur_key = key
                cur_levels = []
                cur_inst = inst
                cur_ts = ts_ns
            side = _resolve(row, profile.side_col, profile.side_const)
            price = to_dollars(_col(row, profile.price_col), profile.price_scale, profile.price_factor)
            if profile.no_side:
                price = no_to_yes(price)
                side = flip_side(to_side(side))
            size = float(_resolve(row, profile.size_col, profile.size_const) or 0.0)
            cur_levels.append((side, price, size))
        yield from flush()
        return

    for row in records:
        yield _row_to_event(row, profile, seq_counter)


def convert(
    src: Union[str, "Any"],
    profile: Union[IngestProfile, str, Dict[str, Any]],
    out: str,
    *,
    strict_price: bool = True,
    warn: bool = True,
) -> Dict[str, int]:
    """Convert a CSV/Parquet/DataFrame into canonical NDJSON at ``out`` (``.gz`` => gzip).

    Parameters
    ----------
    src     : path to a .csv/.parquet file, or a pandas DataFrame.
    profile : an :class:`IngestProfile`, a path to a .json/.toml profile, or a dict.
    out     : output path. Ends with ``.gz`` to gzip-compress.

    Returns
    -------
    dict : writer stats (counts of deltas, trades, snapshots, non_monotonic, warnings).

    >>> # convert("data.csv", "profile.json", "out.ndjson.gz")
    """
    prof = _coerce_profile(profile)
    with CanonicalWriter(out, strict_price=strict_price, warn=warn) as w:
        for ev in iter_events(src, prof):
            w.write(ev)
    return dict(w.stats)


def _coerce_profile(profile: Union[IngestProfile, str, Dict[str, Any]]) -> IngestProfile:
    if isinstance(profile, IngestProfile):
        return profile
    if isinstance(profile, dict):
        return IngestProfile.from_dict(profile)
    if isinstance(profile, str):
        return IngestProfile.from_file(profile)
    raise TypeError(f"cannot use {type(profile).__name__} as a profile")


# ======================================================================================
# SECTION 6 — Profile inference from column names.
# ======================================================================================

# Candidate source column names for each canonical concept (lower-cased match).
_INFER_CANDIDATES = {
    "ts": ["ts_ns", "timestamp_ns", "ts", "timestamp", "time", "datetime", "ts_event", "recv_time"],
    "instrument": ["instrument", "symbol", "ticker", "market", "sym", "asset", "pair"],
    "price": ["price", "px", "p", "level_price", "yes_price"],
    "size": ["size", "qty", "quantity", "amount", "contracts", "volume"],
    "side": ["side", "direction", "bid_ask", "buy_sell"],
    "action": ["action", "type", "update_type", "op"],
    "sequence": ["sequence", "seq", "seq_num", "sequence_number"],
    "is_snapshot": ["is_snapshot", "snapshot", "is_snap"],
    "aggressor": ["aggressor_side", "aggressor", "taker_side", "maker_taker"],
    "trade_id": ["trade_id", "tradeid", "id", "exec_id"],
    "kind": ["kind", "record_type", "event_type", "msg_type"],
}


def infer_profile(
    columns: Iterable[str],
    venue: str = "GENERIC",
    sample_row: Optional[Dict[str, Any]] = None,
) -> IngestProfile:
    """Best-effort guess of an :class:`IngestProfile` from a list of column names.

    Matching is case-insensitive. Detects trade-vs-delta by the presence of an aggressor/trade_id
    column, guesses the timestamp unit from the column name (``*_ns`` => ns, ``*_ms`` => ms,
    name containing 'iso'/'datetime' => iso) and the price scale from the name ('cent' => cents).

    If ``sample_row`` (a dict of one data row) is given, it sniffs the timestamp column's value:
    a non-numeric string (e.g. ``"2023-11-14T22:13:20Z"``) forces ``ts_unit="iso"``, and a numeric
    magnitude is used to refine s/ms/us/ns when the name was ambiguous.

    This is a convenience for exploration (``--infer``). Always eyeball the result with
    ``--preview`` before trusting it on real data.
    """
    cols = list(columns)
    lower = {c.lower(): c for c in cols}

    def find(concept: str) -> Optional[str]:
        for cand in _INFER_CANDIDATES[concept]:
            if cand in lower:
                return lower[cand]
        # loose contains-match
        for cand in _INFER_CANDIDATES[concept]:
            for lc, orig in lower.items():
                if cand in lc:
                    return orig
        return None

    ts_col = find("ts")
    price_col = find("price")
    side_col = find("side")
    aggressor_col = find("aggressor")
    trade_id_col = find("trade_id")

    # ts unit guess
    ts_unit = "ns"
    if ts_col:
        lc = ts_col.lower()
        if "iso" in lc or "datetime" in lc or "date" in lc:
            ts_unit = "iso"
        elif lc.endswith("_ns") or lc.endswith("ns"):
            ts_unit = "ns"
        elif lc.endswith("_ms") or lc.endswith("ms") or "milli" in lc:
            ts_unit = "ms"
        elif lc.endswith("_us") or lc.endswith("us") or "micro" in lc:
            ts_unit = "us"
        elif lc.endswith("_s") or lc == "timestamp" or lc == "time" or "second" in lc:
            ts_unit = "s"

    # Refine ts_unit by sniffing an actual value when available.
    if ts_col and sample_row is not None and ts_col in sample_row:
        val = sample_row[ts_col]
        sval = str(val).strip()
        is_numeric = sval.replace(".", "", 1).replace("-", "", 1).isdigit()
        if not is_numeric:
            ts_unit = "iso"  # looks like a datetime string
        else:
            # Refine by magnitude: ~1e9 s, ~1e12 ms, ~1e15 us, ~1e18 ns (year ~2001-2100).
            try:
                mag = abs(float(sval))
                if mag > 1e17:
                    ts_unit = "ns"
                elif mag > 1e14:
                    ts_unit = "us"
                elif mag > 1e11:
                    ts_unit = "ms"
                elif mag > 1e8:
                    ts_unit = "s"
            except ValueError:
                pass

    # price scale guess
    price_scale = "dollars"
    if price_col and "cent" in price_col.lower():
        price_scale = "cents"

    is_trade = aggressor_col is not None or (trade_id_col is not None and side_col is None)
    kind_col = find("kind")

    prof = IngestProfile(
        venue=venue,
        kind=None if kind_col else ("trade" if is_trade else "delta"),
        kind_col=kind_col,
        instrument_col=find("instrument"),
        ts_col=ts_col,
        ts_unit=ts_unit,
        price_col=price_col,
        price_scale=price_scale,
        size_col=find("size"),
    )
    if is_trade:
        prof.aggressor_col = aggressor_col
        prof.trade_id_col = trade_id_col
    else:
        prof.side_col = side_col
        prof.action_col = find("action")
        prof.sequence_col = find("sequence")
        prof.is_snapshot_col = find("is_snapshot")
    return prof


# ======================================================================================
# SECTION 7 — CLI.
# ======================================================================================


def _build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="to_canonical.py",
        description="Convert tabular market data (CSV/Parquet) into the backtester's canonical "
        "NDJSON. See tools/README.md for the contract and profile reference.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "examples:\n"
            "  python tools/to_canonical.py --input d.csv --profile p.json --out out.ndjson.gz\n"
            "  python tools/to_canonical.py --input d.csv --infer --preview 5\n"
        ),
    )
    p.add_argument("--input", "-i", required=True, help="input .csv or .parquet file")
    p.add_argument("--profile", "-p", help="ingest profile .json/.toml (omit with --infer)")
    p.add_argument("--out", "-o", help="output NDJSON path (.gz to gzip). Required unless --preview")
    p.add_argument("--infer", action="store_true",
                   help="guess a profile from column names instead of --profile")
    p.add_argument("--venue", default="GENERIC",
                   help="venue TAG for --infer (default GENERIC)")
    p.add_argument("--preview", type=int, metavar="N",
                   help="print the first N canonical events to stdout and do NOT write a file")
    p.add_argument("--no-strict-price", action="store_true",
                   help="warn (do not error) on prices outside (0,1]")
    p.add_argument("--quiet", "-q", action="store_true", help="suppress warnings")
    return p


def main(argv: Optional[List[str]] = None) -> int:
    args = _build_parser().parse_args(argv)

    if not os.path.exists(args.input):
        print(f"error: input file not found: {args.input}", file=sys.stderr)
        return 2
    if not args.infer and not args.profile:
        print("error: provide --profile, or use --infer to guess one.", file=sys.stderr)
        return 2
    if not args.preview and not args.out:
        print("error: provide --out PATH (or use --preview N to dry-run).", file=sys.stderr)
        return 2

    # Resolve profile.
    try:
        if args.infer:
            import pandas as pd
            _df = _load_dataframe(args.input)
            sample = _df.iloc[0].to_dict() if len(_df) else None
            profile = infer_profile(_df.columns, venue=args.venue, sample_row=sample)
            print("[infer] guessed profile:", file=sys.stderr)
            print(json.dumps(
                {k: v for k, v in profile.__dict__.items() if v not in (None, False)},
                indent=2), file=sys.stderr)
        else:
            profile = IngestProfile.from_file(args.profile)
    except Exception as e:  # friendly, actionable
        print(f"error: could not load profile: {e}", file=sys.stderr)
        return 2

    # Preview mode: print first N events, no file.
    if args.preview is not None:
        try:
            count = 0
            for ev in iter_events(args.input, profile):
                print(json.dumps(ev.to_dict()))
                count += 1
                if count >= args.preview:
                    break
        except Exception as e:
            print(f"error during preview: {e}", file=sys.stderr)
            return 1
        print(f"[preview] showed {count} event(s); no file written.", file=sys.stderr)
        return 0

    # Full conversion.
    try:
        stats = convert(
            args.input, profile, args.out,
            strict_price=not args.no_strict_price, warn=not args.quiet,
        )
    except Exception as e:
        print(f"error during conversion: {e}", file=sys.stderr)
        return 1
    print(
        f"[ok] wrote {args.out}: "
        f"{stats['deltas']} deltas, {stats['trades']} trades, "
        f"{stats['snapshots']} snapshot rows, {stats['non_monotonic']} out-of-order, "
        f"{stats['warnings']} warning(s).",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
