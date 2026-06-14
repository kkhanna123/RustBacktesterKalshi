"""Generate a realistic synthetic Kalshi NatGas tick demo with a TRUE logistic strike ladder.

The event has a ladder of "above $K" binary strikes. At each minute the risk-neutral survival
S(K)=P(settle>K) follows a logistic: S(K)=1/(1+exp((K-mu)/s)), with mu (the implied fair value /
median) drifting over time and s (the scale, ~ implied vol) roughly constant. Each strike's YES
book is a bid/ask straddling S(K); we emit a full-book snapshot then UPDATE deltas as prices move,
plus occasional trades. Deterministic (seeded). This is what fit_logistic.rs is meant to recover.
"""
import json, gzip, math, random
random.seed(7)
EVENT = "KXNATGASD-26JUN1517"
STRIKES = [round(2.80 + 0.025*i, 3) for i in range(28)]   # 2.80 .. 3.475, 28 strikes
T0 = 1_781_000_000 * 1_000_000_000                          # ns
STEP_NS = 60 * 1_000_000_000                                # 1 minute
N = 160                                                     # minutes
S_SCALE = 0.075                                             # logistic scale (dispersion ~ implied vol)

def survival(K, mu, s): return 1.0/(1.0+math.exp((K-mu)/s))
def clamp(p): return min(0.97, max(0.03, p))
def cents(p): return round(p, 2)
def ticker(K): return f"{EVENT}-T{K:.3f}"

rows, seq = [], 0
mu = 3.10
last_bid, last_ask = {}, {}
for t in range(N):
    ts = T0 + t*STEP_NS
    mu += random.gauss(0, 0.004) + 0.12*math.sin(t/40.0)*0.01   # gentle drift
    mu = min(3.35, max(2.95, mu))
    for K in STRIKES:
        p = clamp(survival(K, mu, S_SCALE))
        half = random.choice([0.01, 0.015, 0.02])
        bid = cents(clamp(p - half)); ask = cents(clamp(p + half))
        if bid >= ask: ask = cents(min(0.97, bid + 0.01))
        size_b = float(random.randint(20, 400)); size_a = float(random.randint(20, 400))
        tk = ticker(K)
        snap = tk not in last_bid
        seq += 1
        if snap:
            # full-book snapshot: one bid level + one ask level
            rows.append((ts, {"kind":"delta","ts_ns":ts,"instrument":tk,"action":"ADD","side":"BUY",
                              "price":bid,"size":size_b,"sequence":seq,"is_snapshot":1,"venue":"KALSHI","market_alias":""}))
            rows.append((ts, {"kind":"delta","ts_ns":ts,"instrument":tk,"action":"ADD","side":"SELL",
                              "price":ask,"size":size_a,"sequence":seq,"is_snapshot":0,"venue":"KALSHI","market_alias":""}))
        else:
            # move the single resting bid/ask: delete old level, add new (UPDATE-style)
            ob, oa = last_bid[tk], last_ask[tk]
            if bid != ob:
                rows.append((ts, {"kind":"delta","ts_ns":ts,"instrument":tk,"action":"DELETE","side":"BUY","price":ob,"size":0.0,"sequence":seq,"is_snapshot":0,"venue":"KALSHI","market_alias":""}))
                rows.append((ts, {"kind":"delta","ts_ns":ts,"instrument":tk,"action":"ADD","side":"BUY","price":bid,"size":size_b,"sequence":seq,"is_snapshot":0,"venue":"KALSHI","market_alias":""}))
            if ask != oa:
                rows.append((ts, {"kind":"delta","ts_ns":ts,"instrument":tk,"action":"DELETE","side":"SELL","price":oa,"size":0.0,"sequence":seq,"is_snapshot":0,"venue":"KALSHI","market_alias":""}))
                rows.append((ts, {"kind":"delta","ts_ns":ts,"instrument":tk,"action":"ADD","side":"SELL","price":ask,"size":size_a,"sequence":seq,"is_snapshot":0,"venue":"KALSHI","market_alias":""}))
        last_bid[tk], last_ask[tk] = bid, ask
        # occasional trade at the mid
        if random.random() < 0.04:
            mid = cents((bid+ask)/2)
            rows.append((ts+1, {"kind":"trade","ts_ns":ts+1,"instrument":tk,"aggressor_side":random.choice(["yes","no"]),
                                "price":mid,"size":float(random.randint(1,20)),"trade_id":f"s{seq}","venue":"KALSHI"}))
rows.sort(key=lambda r: r[0])
dest="/Users/nitinkhanna/Desktop/OracleTrading/kalshi-backtester-src/data/tick/natgas_tick_demo.ndjson.gz"
with gzip.open(dest,"wt") as f:
    for _,r in rows: f.write(json.dumps(r)+"\n")
import os
nd=sum(1 for _,r in rows if r["kind"]=="delta"); nt=len(rows)-nd
print(f"wrote {len(rows)} events ({nd} deltas, {nt} trades), {len(STRIKES)} strikes, {N} minutes; {round(os.path.getsize(dest)/1e6,2)} MB")
