@echo off
REM ==========================================================================
REM  Autoshop web UI launcher (portable - resolves paths from this file's
REM  own folder, so it works wherever the repo is cloned).
REM  - Double-click             -> serves %USERPROFILE%\Pictures
REM  - Drag a folder onto this  -> serves just that folder (faster).
REM  Opens the browser automatically. Outputs go to <project>\out.
REM ==========================================================================

REM cd into the project (this .bat's own folder) so .env and ./out resolve.
cd /d "%~dp0"

set "DIR=%~1"
if "%DIR%"=="" set "DIR=%USERPROFILE%\Pictures"

set "EXE=%~dp0target\release\autoshop.exe"
if not exist "%EXE%" (
  echo   autoshop.exe not found. Build it first:  cargo build --release
  echo   ^(expected at: %EXE%^)
  pause
  exit /b 1
)

echo.
echo   Autoshop UI  -  serving: %DIR%
echo   Browser will open at http://127.0.0.1:8080 in ~2s.
echo   Close this window to stop the server.
echo.

REM Open the browser shortly after the server starts (detached).
start "" cmd /c "timeout /t 2 /nobreak >nul & start "" http://127.0.0.1:8080"

"%EXE%" serve "%DIR%" --port 8080
