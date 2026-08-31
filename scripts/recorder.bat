@echo off
rem ── AgentEar 录音工具（Windows 启动脚本）─────────────────────────
rem
rem ⚠️ 这个脚本**没有在真实 Windows 上跑过**（开发机是 macOS）。
rem    里面两段 Python 单行命令单独验过，cmd.exe 的语法部分未验证。
rem    跑不起来请把报错发回来，别猜。
rem
rem 为什么要起 HTTP 服务而不是直接双击 HTML：
rem   getUserMedia 只在**安全上下文**里可用。file:// 不算，
rem   而 http://127.0.0.1 算（规范把 localhost 列为可信来源）。
rem   直接开文件麦克风一定拿不到，报的错还会指向权限，很难查。
rem
rem 用法：scripts\recorder.bat [端口]        端口默认 8899
setlocal EnableDelayedExpansion

set "ROOT=%~dp0.."
set "DOCS=%ROOT%\docs"
set "PORT=%~1"
if "%PORT%"=="" set "PORT=8899"

echo %PORT%| findstr /r "^[1-9][0-9]*$" >nul
if errorlevel 1 goto badport
if %PORT% LSS 1024 goto badport
if %PORT% GTR 65535 goto badport

set "PY="
where python >nul 2>&1 && set "PY=python"
if not defined PY where py >nul 2>&1 && set "PY=py -3"
if not defined PY (
  echo !! 需要 Python 3（只用它的 http.server）：https://www.python.org/downloads/
  exit /b 1
)

if not exist "%DOCS%\recorder.html" ( echo !! 缺文件：%DOCS%\recorder.html & exit /b 1 )
if not exist "%DOCS%\recorder-core.js" ( echo !! 缺文件：%DOCS%\recorder-core.js & exit /b 1 )

rem 端口被占就直接说。用 connect_ex 而不是 netstat —— netstat 的输出格式
rem 随系统语言和版本变，而 Python 上面已经确认存在。
rem connect_ex 返回 0 表示连上了 = 有人在听 = 端口被占。
%PY% -c "import socket,sys; s=socket.socket(); s.settimeout(0.4); c=s.connect_ex(('127.0.0.1',%PORT%)); s.close(); sys.exit(0 if c==0 else 1)"
if not errorlevel 1 (
  set /a NEXT=%PORT%+1
  echo !! 端口 %PORT% 已被占用。换一个：scripts\recorder.bat !NEXT!
  exit /b 1
)

echo ==^> 录音工具  http://127.0.0.1:%PORT%/recorder.html
echo     泰语语料页 http://127.0.0.1:%PORT%/thai-recorder.html
echo     音频只在本机，不上传任何地方。关掉这个窗口即停止。
echo.

start "" /b %PY% -m http.server %PORT% --bind 127.0.0.1 --directory "%DOCS%" >nul 2>&1

rem 等它真的起来再开浏览器，否则会开出一个连接失败的页面
set READY=0
for /l %%i in (1,1,40) do (
  if !READY!==0 (
    %PY% -c "import sys,urllib.request; sys.exit(0 if urllib.request.urlopen('http://127.0.0.1:%PORT%/recorder.html',timeout=1).status==200 else 1)" >nul 2>&1
    if not errorlevel 1 (set READY=1) else (ping -n 1 -w 150 127.0.0.1 >nul)
  )
)
if !READY!==0 (
  echo !! 服务没起来，或 http://127.0.0.1:%PORT%/ 打不开
  exit /b 1
)

start "" "http://127.0.0.1:%PORT%/recorder.html"
echo 服务在后台运行。关掉这个窗口停止。
pause >nul
exit /b 0

:badport
echo !! 端口要是 1024-65535 的整数，当前是 "%PORT%"
exit /b 1
