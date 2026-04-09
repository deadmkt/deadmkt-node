@echo off
REM build.bat — Build and tag deadmkt-node Docker image from Cargo.toml version
for /f "tokens=3 delims= " %%v in ('findstr /b "version" Cargo.toml') do set VERSION=%%~v
set VERSION=%VERSION:"=%
echo Building deadmkt-node v%VERSION%
docker build --no-cache -t deadmkt-node:%VERSION% -t deadmkt-node:latest %* .
echo Tagged: deadmkt-node:%VERSION%, deadmkt-node:latest
