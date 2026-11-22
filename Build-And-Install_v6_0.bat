@echo off
setlocal EnableExtensions
title Hls2Udp v6.0 LTS (Rust Production) Builder and Installer
cd /d "%~dp0"

echo ============================================================================
echo  HLS to UDP/Multicast Broadcast Gateway v6.0 LTS (Rust Production Edition)
echo ============================================================================
echo Folder: %CD%
echo.

where cargo >nul 2>&1
if errorlevel 1 (
    echo [ERROR] Rust cargo not found in PATH.
    echo Install Rust from https://rustup.rs/ or use prebuilt Hls2Udp_v6_0.exe.
    pause
    exit /b 1
)

echo [1/3] Stopping running Hls2UdpService and GUI if active...
sc stop Hls2UdpService >nul 2>&1
taskkill /F /IM Hls2Udp.exe >nul 2>&1
timeout /t 1 /nobreak >nul

echo [2/3] Building release binary with cargo build --release ...
cargo build --release
if errorlevel 1 (
    echo [ERROR] Cargo build failed!
    pause
    exit /b 1
)
copy /Y "target\release\Hls2Udp.exe" "%~dp0Hls2Udp.exe" >nul
echo [OK] Built: %~dp0Hls2Udp.exe

echo [3/3] Checking Administrator privileges for Windows Service install...
net session >nul 2>&1
if errorlevel 1 (
    echo [WARN] Not running as Administrator. Skipping Windows Service install.
    echo Launching Hls2Udp.exe in standalone mode...
    start "" "%~dp0Hls2Udp.exe"
    pause
    exit /b 0
)

"%~dp0Hls2Udp.exe" --install
start "" "%~dp0Hls2Udp.exe"
echo [DONE] Hls2Udp v6.0 LTS (Rust) built and running.
pause
exit /b 0
