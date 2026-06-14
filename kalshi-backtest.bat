@echo off
REM kalshi-backtest.bat — Windows launcher for the Rust tick-level Kalshi backtester.
REM
REM Builds the release binary if needed, then runs it, forwarding all arguments. Examples:
REM
REM   kalshi-backtest.bat --help
REM   kalshi-backtest.bat list-strategies
REM   kalshi-backtest.bat backtest --config run.toml
REM
REM Set KALSHI_BT_FEATURES to pass cargo features (e.g. KALSHI_BT_FEATURES=clickhouse).
setlocal enabledelayedexpansion

set "SCRIPT_DIR=%~dp0"
set "CRATE_DIR=%SCRIPT_DIR%backtester"
set "BIN=%CRATE_DIR%\target\release\kalshi-backtest.exe"

set "FEATURE_ARGS="
if not "%KALSHI_BT_FEATURES%"=="" set "FEATURE_ARGS=--features %KALSHI_BT_FEATURES%"

if not exist "%BIN%" echo [kalshi-backtest] building release binary (first run)... 1>&2
pushd "%CRATE_DIR%"
cargo build --release %FEATURE_ARGS% 1>&2
if errorlevel 1 ( popd & exit /b 1 )
popd

"%BIN%" %*
exit /b %errorlevel%
