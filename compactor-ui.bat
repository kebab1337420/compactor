@echo off
setlocal
rem Launches the Compactor web interface and opens it in the default browser.
rem Double-click this file; close the window (or Ctrl-C) to stop the server.

cd /d "%~dp0"

set PORT=8787
set LEVEL=6
if not "%~1"=="" set PORT=%~1
if not "%~2"=="" set LEVEL=%~2

set EXE=target\release\compactor.exe
if not exist "%EXE%" (
    echo compactor.exe not found, building it once...
    where cargo >nul 2>&1
    if errorlevel 1 (
        echo.
        echo Cargo is not installed and the binary is missing.
        echo Install Rust from https://rustup.rs then run this file again.
        echo.
        pause
        exit /b 1
    )
    cargo build --release
    if errorlevel 1 (
        echo.
        echo Build failed. See the messages above.
        echo.
        pause
        exit /b 1
    )
)

echo Compactor UI: http://127.0.0.1:%PORT%
echo Default level %LEVEL%. Close this window to stop the server.
echo.

rem Give the server a moment to bind before the browser asks for the page.
start "" /b cmd /c "timeout /t 1 /nobreak >nul & start "" http://127.0.0.1:%PORT%"

"%EXE%" serve --port %PORT% -l %LEVEL%
set RC=%ERRORLEVEL%
if not "%RC%"=="0" (
    echo.
    echo The server exited with code %RC%.
    echo If the port is already in use, run: compactor-ui.bat 8788
    echo.
    pause
)
endlocal
