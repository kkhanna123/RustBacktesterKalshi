# Kalshi tick-level backtester — convenience targets.
# Cross-platform note: these assume a unix shell. On Windows see howToRunWindows.md and use the
# underlying cargo / python commands directly.

ROOT := $(shell pwd)
CH   := $(ROOT)/bin/clickhouse
PY   := $(ROOT)/.venv/bin/python
TICK := $(ROOT)/data/tick/natgas_tick_demo.ndjson.gz   # real tick capture (deltas + trades)

.PHONY: help build test demo demo-latency collector-test ch-server ch-init ch-stop dashboard clean

help:  ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

build:  ## Build the Rust backtester (release)
	cd backtester && cargo build --release

test:  ## Run Rust + collector tests
	cd backtester && cargo test
	cd adapters && $(PY) -m unittest discover -s tests
	cd adapters && $(PY) -m unittest discover -s feeds/tests
	cd adapters/convert && $(PY) -m unittest discover -s tests

demo: build  ## Backtest on real NatGas TICK data -> report.json + tearsheet + dashboard exports
	cd backtester && ./target/release/kalshi-backtest backtest \
		--source ndjson --ndjson $(TICK) \
		--instrument 'KXNATGASD-%' --strategy imbalance \
		--out-dir $(ROOT)/data/exports/demo --tearsheet $(ROOT)/figures/demo_tearsheet.html

demo-latency: build  ## Same demo WITH 1s order latency, to show the latency fill model's effect
	cd backtester && ./target/release/kalshi-backtest backtest \
		--source ndjson --ndjson $(TICK) \
		--instrument 'KXNATGASD-%' --strategy imbalance --latency-ns 1000000000 \
		--out-dir $(ROOT)/data/exports/demo_latency

collector-test:  ## Run the collector unit tests
	cd adapters && $(PY) -m unittest discover -s tests -v

ch-server:  ## Start the local ClickHouse native server (http :8123, tcp :9000)
	@mkdir -p clickhouse/_data
	nohup $(CH) server -- --path=./clickhouse/_data/ --http_port=8123 --tcp_port=9000 --mysql_port=0 > /tmp/ch_server.out 2>&1 &
	@echo "ClickHouse starting; check http://localhost:8123/ping"

ch-init:  ## Load the schema into a running ClickHouse server
	$(CH) client --port 9000 --multiquery < clickhouse/schema/01_tables.sql && echo "schema loaded"

ch-stop:  ## Stop the local ClickHouse server
	@pkill -f 'clickhouse server' || true ; echo "stopped"

dashboard: demo  ## Build the interactive backtest dashboard from the demo export
	cd dashboard && $(PY) build_dashboard.py --export-dir $(ROOT)/data/exports/demo --out dashboard.html
	@echo "Open dashboard/dashboard.html in any browser"

clean:  ## Remove build artifacts (keeps the ClickHouse binary + tick data)
	cd backtester && cargo clean
	rm -rf data/exports/* figures/*.html
