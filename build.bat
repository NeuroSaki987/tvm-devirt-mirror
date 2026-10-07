@echo off
call "D:\Visual Studio\Visual Studio\VC\Auxiliary\Build\vcvars64.bat" >nul
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
cd /d C:\Users\31513\Desktop\ACE\tools\tvm-devirt-fix-maxblocks
cargo %*
