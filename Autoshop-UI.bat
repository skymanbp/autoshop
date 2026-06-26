@echo off
REM ==========================================================================
REM  Autoshop web UI launcher.
REM  - Double-click  -> serves your whole library (D:\Photography\Raw).
REM  - Drag a folder onto this file -> serves just that folder (faster).
REM  Opens the browser automatically. Outputs go to D:\Projects\Autoshop\out.
REM ==========================================================================

REM cd into the project so .env (OpenAI key) and ./out resolve correctly.
cd /d "D:\Projects\Autoshop"

set "DIR=%~1"
if "%DIR%"=="" set "DIR=D:\Photography\Raw"

echo.
echo   Autoshop UI  -  serving: %DIR%
echo   Browser will open at http://127.0.0.1:8080 in ~2s.
echo   Close this window to stop the server.
echo.

REM Open the browser shortly after the server starts (detached).
start "" cmd /c "timeout /t 2 /nobreak >nul & start "" http://127.0.0.1:8080"

"D:\Projects\Autoshop\target\release\autoshop.exe" serve "%DIR%" --port 8080
