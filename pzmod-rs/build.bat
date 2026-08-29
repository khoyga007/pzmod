@echo off
rem vcvars64 is mandatory: git-bash's coreutils link.exe shadows MSVC's linker,
rem and without LIB/INCLUDE the link dies with LNK1181 kernel32.lib.
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul
cd /d "%~dp0"
python sync-ui.py || exit /b 1
cargo build --release %*
