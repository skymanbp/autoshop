@echo off
REM ── Autoshop web UI launcher ─────────────────────────────────────────────
REM Starts the local web server and opens it in your browser.
REM Optional: pass a photo folder as the first argument; otherwise it defaults
REM to your Pictures folder. You can also change the working folder inside the UI.
setlocal
cd /d "%~dp0"
set "FOLDER=%~1"
if "%FOLDER%"=="" set "FOLDER=%USERPROFILE%\Pictures"
set "PORT=8080"
echo Serving "%FOLDER%" on http://127.0.0.1:%PORT% ...
start "Autoshop Web UI" "target\release\autoshop.exe" serve "%FOLDER%" --port %PORT%
timeout /t 2 /nobreak >nul
start "" "http://127.0.0.1:%PORT%"
