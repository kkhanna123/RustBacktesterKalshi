"""book.py — PURE orderbook reconstruction for Kalshi WS messages (no network).

Turns Kalshi ``orderbook_snapshot`` / ``orderbook_delta`` / ``trade`` messages into
the SHARED NDJSON contract rows consumed by the Rust backtester and mirrored into the
ClickHouse ``kalshi.orderbook_deltas`` / ``kalshi.trades`` tables.

YES-native book mapping (critical):
  * A ``yes`` level at price ``p`` with ``q`` contracts  -> book side ``BUY``  at price ``p``.
  * A ``no``  level at price ``q`` with ``n`` contracts  -> book side ``SELL`` at price
    ``round(1 - q, 2)`` with the same ``n`` contracts. (A buyer of NO at q is a seller of
    YES at 1 - q.)

Encoding caveats (centralized in the parse helpers below so they are trivial to flip):
  * ``yes_dollars_fp`` / ``no_dollars_fp`` snapshot entries are ``[price_str, contracts_str]``
    where BOTH are plain decimal strings ("0.0800" = $0.08, "300.00" = 300 contracts).
  * ``orderbook_delta.msg`` carries ``price_dollars`` (decimal $) and ``delta_fp`` (contracts).
  * ``FP_DIVISOR`` scales the contracts/delta numbers. Per spec it is 1.0 (the strings are
    already plain decimals). If Kalshi turns out to use integer fixed-point (e.g. ÷100),
    flip this single constant.

All prices are stored internally as integer cents (1..99) for exact book keying, and
emitted in the rows as float dollars rounded to 2 decimals.
"""

import time

# --- tunable encoding knobs (flip these if Kalshi's encoding differs) ---------
FP_DIVISOR = 1.0  # contracts / delta scaling. 1.0 == plain decimals per spec.
PRICE_DECIMALS = 2  # dollars stored rounded to 2 decimals (== integer cents)
_EPS = 1e-9  # a level with <= this many contracts is considered empty (DELETE)


# --- parsing helpers (centralized so field-name/encoding guesses live in one place) --
def _to_float(x):
    """Parse a price/size that may arrive as a decimal string or a number."""
    if x is None:
        return 0.0
    if isinstance(x, str):
        return float(x.strip())
    return float(x)


def parse_price_dollars(x):
    """Price in dollars, rounded to PRICE_DECIMALS."""
    return round(_to_float(x), PRICE_DECIMALS)


def parse_contracts(x):
    """Contracts / delta count, scaled by FP_DIVISOR."""
    return _to_float(x) / FP_DIVISOR


def price_to_cents(price_dollars):
    """Exact integer cents key for a dollar price (0.42 -> 42)."""
    return int(round(price_dollars * 100))


def cents_to_dollars(cents):
    return round(cents / 100.0, PRICE_DECIMALS)


def complement_cents(cents):
    """NO price -> equivalent YES-ask price, in cents. 1 - q  ==  100 - q_cents."""
    return 100 - cents


def _first(msg, *keys, default=None):
    """Return the first present (non-None) value among ``keys`` in ``msg``."""
    for k in keys:
        if k in msg and msg[k] is not None:
            return msg[k]
    return default


def now_ns():
    return time.time_ns()


def ts_ms_to_ns(ts_ms):
    return int(ts_ms) * 1_000_000


def iso_to_ns(s):
    """Parse an ISO-8601 UTC timestamp (e.g. '2026-06-12T10:26:29.466991Z') to unix ns, or None."""
    if not s:
        return None
    import datetime as _dt
    txt = str(s).strip().replace("Z", "+00:00")
    try:
        d = _dt.datetime.fromisoformat(txt)
    except ValueError:
        try:
            d = _dt.datetime.strptime(txt[:19], "%Y-%m-%dT%H:%M:%S").replace(tzinfo=_dt.timezone.utc)
        except ValueError:
            return None
    return int(d.timestamp() * 1_000_000_000)


class BookReconstructor:
    """Maintains a single YES-priced two-sided book for one instrument.

    book = {"BUY": {price_cents -> size}, "SELL": {price_cents -> size}}
    """

    def __init__(self, instrument):
        self.instrument = instrument
        self.book = {"BUY": {}, "SELL": {}}
        self.last_seq = None  # last seq we accepted; None == nothing seen yet

    # --- sequence handling ----------------------------------------------------
    def check_seq(self, seq):
        """Return False (gap) if seq is not exactly last_seq + 1.

        The first seq ever seen is always accepted. Does NOT mutate last_seq;
        callers update last_seq via on_snapshot/on_delta which set it explicitly.
        """
        if seq is None:
            return True
        if self.last_seq is None:
            return True
        return seq == self.last_seq + 1

    # --- side / price mapping --------------------------------------------------
    @staticmethod
    def _map_side(kalshi_side, price_cents):
        """Map a Kalshi (side, price_cents) onto YES-native (book_side, level_cents).

        yes -> BUY at price_cents; no -> SELL at 100 - price_cents.
        """
        side = (kalshi_side or "").lower()
        if side == "yes":
            return "BUY", price_cents
        elif side == "no":
            return "SELL", complement_cents(price_cents)
        # default defensively to yes/BUY
        return "BUY", price_cents

    # --- row builder ----------------------------------------------------------
    def _row(self, ts_ns, action, book_side, level_cents, size, sequence, is_snapshot):
        return {
            "kind": "delta",
            "ts_ns": int(ts_ns),
            "instrument": self.instrument,
            "action": action,
            "side": book_side,
            "price": cents_to_dollars(level_cents),
            "size": float(size),
            "sequence": int(sequence) if sequence is not None else 0,
            "is_snapshot": int(is_snapshot),
            "venue": "KALSHI",
            "market_alias": "",
        }

    # --- snapshot -------------------------------------------------------------
    def on_snapshot(self, msg, recv_ns=None, seq=None):
        """Reset the book and load all levels. Returns delta rows; the FIRST row
        has is_snapshot=1 (book reset marker), the rest is_snapshot=0, all ADD."""
        if recv_ns is None:
            recv_ns = now_ns()
        if seq is None:
            seq = _first(msg, "seq", default=self.last_seq)

        # reset
        self.book = {"BUY": {}, "SELL": {}}
        rows = []

        yes_levels = _first(msg, "yes_dollars_fp", "yes", default=[]) or []
        no_levels = _first(msg, "no_dollars_fp", "no", default=[]) or []

        # build ordered (book_side, level_cents, size) list: yes -> BUY, no -> SELL
        ordered = []
        for entry in yes_levels:
            price_cents = price_to_cents(parse_price_dollars(entry[0]))
            size = parse_contracts(entry[1])
            ordered.append(("yes", price_cents, size))
        for entry in no_levels:
            price_cents = price_to_cents(parse_price_dollars(entry[0]))
            size = parse_contracts(entry[1])
            ordered.append(("no", price_cents, size))

        first = True
        for kalshi_side, price_cents, size in ordered:
            if size <= _EPS:
                continue
            book_side, level_cents = self._map_side(kalshi_side, price_cents)
            self.book[book_side][level_cents] = size
            rows.append(
                self._row(recv_ns, "ADD", book_side, level_cents, size, seq, 1 if first else 0)
            )
            first = False

        # If the snapshot was empty, still emit a single is_snapshot=1 marker row so
        # downstream knows the book was reset. Use a zero-size BUY@0 DELETE-less marker.
        if not rows:
            rows.append(self._row(recv_ns, "ADD", "BUY", 0, 0.0, seq, 1))

        if seq is not None:
            self.last_seq = seq
        return rows

    # --- delta ----------------------------------------------------------------
    def on_delta(self, msg, recv_ns=None, seq=None):
        """Apply one delta: new_size = old_size + delta. Emit ADD if the level was
        absent, UPDATE if it changed, DELETE if new_size <= 0 (and remove it)."""
        ts_ms = _first(msg, "ts_ms", "ts")
        if ts_ms is not None:
            ts_ns = ts_ms_to_ns(ts_ms)
        elif recv_ns is not None:
            ts_ns = recv_ns
        else:
            ts_ns = now_ns()

        if seq is None:
            seq = _first(msg, "seq", default=self.last_seq)

        price_cents = price_to_cents(parse_price_dollars(_first(msg, "price_dollars", "price")))
        delta = parse_contracts(_first(msg, "delta_fp", "delta", default=0))
        kalshi_side = _first(msg, "side", default="yes")

        book_side, level_cents = self._map_side(kalshi_side, price_cents)
        levels = self.book[book_side]
        old_size = levels.get(level_cents)
        existed = old_size is not None
        new_size = (old_size or 0.0) + delta

        if seq is not None:
            self.last_seq = seq

        if new_size <= _EPS:
            if existed:
                del levels[level_cents]
            return [self._row(ts_ns, "DELETE", book_side, level_cents, 0.0, seq, 0)]

        levels[level_cents] = new_size
        action = "ADD" if not existed else "UPDATE"
        return [self._row(ts_ns, action, book_side, level_cents, new_size, seq, 0)]

    # --- trade ----------------------------------------------------------------
    def trade_row(self, msg, recv_ns=None):
        """Build a trade row from a (defensively parsed) Kalshi trade message.

        Kalshi sends the trade price as ``yes_price_dollars`` / ``no_price_dollars`` (decimal-dollar
        strings) and the size as ``count_fp``, with the timestamp in ``created_time`` (ISO-8601).
        Older/alternate field names (yes_price, count, ts_ms/ts) are accepted as fallbacks.
        """
        ts = _first(msg, "ts_ms", "ts", "created_ts")
        if ts is not None:
            # ts may be seconds or ms; Kalshi 'ts' is unix seconds, 'ts_ms' is ms.
            # Heuristic: values < 1e12 are seconds.
            ts_val = int(ts)
            if ts_val < 1_000_000_000_000:
                ts_ns = ts_val * 1_000_000_000
            else:
                ts_ns = ts_val * 1_000_000
        else:
            ts_ns = iso_to_ns(_first(msg, "created_time", "ts_time"))
            if ts_ns is None:
                ts_ns = recv_ns if recv_ns is not None else now_ns()

        # Trade price in YES dollars: prefer the YES price; else derive from the NO price (1 - q).
        yp = _first(msg, "yes_price_dollars", "yes_price", "price")
        if yp is not None:
            price = parse_price_dollars(yp)
        else:
            no_p = _first(msg, "no_price_dollars", "no_price")
            price = round(1.0 - parse_price_dollars(no_p), PRICE_DECIMALS) if no_p is not None else 0.0
        size = parse_contracts(_first(msg, "count_fp", "count", "size", default=0))
        aggressor = (_first(msg, "taker_side", "aggressor_side", default="yes") or "yes").lower()
        trade_id = str(_first(msg, "trade_id", "id", default=""))

        return {
            "kind": "trade",
            "ts_ns": int(ts_ns),
            "instrument": self.instrument,
            "aggressor_side": aggressor,
            "price": price,
            "size": float(size),
            "trade_id": trade_id,
            "venue": "KALSHI",
        }
