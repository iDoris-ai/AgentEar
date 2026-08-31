@echo off
rem ── AgentEar 录音工具（Windows 启动脚本）─────────────────────────
rem
rem ⚠️ 这个脚本**没有在真实 Windows 上跑过**（开发机是 macOS）。
rem    里面的 Python 单行命令都在 macOS 上做了等价验证（端口探测、就绪探测、
rem    带 PID 文件的服务、按 PID 停止），cmd.exe 的语法部分**未验证**。
rem    跑不起来请把报错原样发回来，别猜。
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

rem 找 Python。**括号是必须的** —— 不加的话 `&&` 可能绑在整条 if 上而不是
rem `where py` 上，于是装了 python 的机器也会被改写成 py -3，
rem 而「需要 Python 3」那个分支永远不触发（PY 确实 defined），报错指向别处。
rem 这一改不依赖对 cmd 解析细节的判断：加括号在两种解析下都对。
set "PY="
where python >nul 2>&1 && set "PY=python"
if not defined PY ( where py >nul 2>&1 && set "PY=py -3" )
if not defined PY (
  echo !! 需要 Python 3（只用它的 http.server）：https://www.python.org/downloads/
  exit /b 1
)

if not exist "%DOCS%\recorder.html" ( echo !! 缺文件：%DOCS%\recorder.html & exit /b 1 )
if not exist "%DOCS%\recorder-core.js" ( echo !! 缺文件：%DOCS%\recorder-core.js & exit /b 1 )

rem 端口被占就直接说。用 connect_ex 而不是 netstat —— netstat 的输出格式随
rem 系统语言和版本变（LISTENING 是本地化的），而 Python 上面已经确认存在。
rem connect_ex 返回 0 = 连上了 = 有人在听 = 被占。
%PY% -c "import socket,sys; s=socket.socket(); s.settimeout(0.4); c=s.connect_ex(('127.0.0.1',%PORT%)); s.close(); sys.exit(0 if c==0 else 1)"
if not errorlevel 1 (
  set /a NEXT=%PORT%+1
  echo !! 端口 %PORT% 已被占用。换一个：scripts\recorder.bat !NEXT!
  exit /b 1
)

set "PIDFILE=%TEMP%\agentear-recorder-%PORT%.pid"
if exist "%PIDFILE%" del "%PIDFILE%" >nul 2>&1

echo ==^> 录音工具  http://127.0.0.1:%PORT%/recorder.html
echo     泰语语料页 http://127.0.0.1:%PORT%/thai-recorder.html
echo     音频只在本机，不上传任何地方。
echo.

rem 服务自己把 PID 写进文件 —— 退出时才停得掉。用 `start /b` 起的进程，
rem batch 拿不到它的 PID；靠 netstat 反查又要解析本地化输出。
rem 只绑 127.0.0.1，不暴露到局域网。
start "" /b %PY% -c "import os,sys,functools,http.server,socketserver; open(r'%PIDFILE%','w').write(str(os.getpid())); h=functools.partial(http.server.SimpleHTTPRequestHandler,directory=r'%DOCS%'); socketserver.TCPServer.allow_reuse_address=True; socketserver.TCPServer(('127.0.0.1',%PORT%),h).serve_forever()" >nul 2>&1

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
  call :cleanup
  exit /b 1
)

start "" "http://127.0.0.1:%PORT%/recorder.html"
echo 按任意键停止服务。
pause >nul
call :cleanup
exit /b 0

:cleanup
rem 不清的话 http.server 会留成孤儿，继续占着这个端口 —— 下次再跑撞上
rem 上面那道端口检查，人会以为是别的程序占了，一天攒好几个。
if exist "%PIDFILE%" (
  set /p SRVPID=<"%PIDFILE%"
  if defined SRVPID taskkill /f /pid !SRVPID! >nul 2>&1
  del "%PIDFILE%" >nul 2>&1
)
goto :eof

:badport
echo !! 端口要是 1024-65535 的整数，当前是 "%PORT%"
exit /b 1
