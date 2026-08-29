@echo off
call "%~dp0..\pz-paths.bat"
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul
cd /d "%~dp0"
python sync-ui.py || exit /b 1
cargo run %*
