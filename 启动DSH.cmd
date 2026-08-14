@echo off
rem DSH Launcher - double-click entry (Tauri 版)
cd /d "%~dp0"

if exist "DSH启动器\DSH启动器.exe" (
    start "" "DSH启动器\DSH启动器.exe"
    exit /b 0
)

echo 未找到 DSH启动器\DSH启动器.exe，请检查文件是否完整。
pause
exit /b 1
