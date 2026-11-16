@echo off
setlocal EnableExtensions

set "ROOT=%~dp0"
if "%ROOT:~-1%"=="\" set "ROOT=%ROOT:~0,-1%"

if not defined AGENT_MODEL set "AGENT_MODEL=qwen3.5:latest"
if not defined AGENT_CTX set "AGENT_CTX=131072"
if not defined AGENT_MAX_TOKENS set "AGENT_MAX_TOKENS=69632"
if not defined OLLAMA_FLASH_ATTENTION set "OLLAMA_FLASH_ATTENTION=1"
if not defined OLLAMA_KV_CACHE_TYPE set "OLLAMA_KV_CACHE_TYPE=q4_0"
if not defined OLLAMA_CONTEXT_LENGTH set "OLLAMA_CONTEXT_LENGTH=%AGENT_CTX%"
if not defined OLLAMA_API_KEY set "OLLAMA_API_KEY=ollama"
if not defined DSH_TELEMETRY_MODE set "DSH_TELEMETRY_MODE=DISABLED"
if not defined DSH_PERMISSION_MODE set "DSH_PERMISSION_MODE=workspace-write"

if /I "%~1"=="ornith" (
  set "AGENT_MODEL=hf.co/deepreinforce-ai/Ornith-1.0-9B-GGUF:latest"
  shift
)

if /I "%~1"=="shortcut" (
  powershell -NoProfile -Command "$s=(New-Object -ComObject WScript.Shell).CreateShortcut([Environment]::GetFolderPath('Desktop') + '\AgentPenguin Local LLM.lnk'); $s.TargetPath='%ROOT%\start-win.cmd'; $s.WorkingDirectory='%ROOT%'; $s.Save()"
  echo Created Desktop shortcut: AgentPenguin Local LLM.lnk
  exit /b 0
)

where ollama >nul 2>nul || (
  echo [ERROR] Ollama is not installed or not in PATH.
  pause
  exit /b 1
)

curl.exe -sf http://127.0.0.1:11434/api/tags >nul 2>nul
if not errorlevel 1 goto ollama_ready

echo Starting Ollama server ^(ctx=%OLLAMA_CONTEXT_LENGTH%, kv=%OLLAMA_KV_CACHE_TYPE%, flash=%OLLAMA_FLASH_ATTENTION%^)...
start "Ollama Server" /min cmd /c "ollama serve"
set "_tries=0"
:wait_ollama
ping -n 2 127.0.0.1 >nul
curl.exe -sf http://127.0.0.1:11434/api/tags >nul 2>nul
if not errorlevel 1 goto ollama_ready
set /a _tries+=1
if %_tries% LSS 20 goto wait_ollama
echo [ERROR] Ollama server did not respond on 127.0.0.1:11434.
pause
exit /b 1
:ollama_ready

ollama list | findstr /I /C:"%AGENT_MODEL%" >nul 2>nul
if errorlevel 1 (
  echo Pulling %AGENT_MODEL%...
  ollama pull "%AGENT_MODEL%" || exit /b 1
)

if /I "%~1"=="claude" (
  shift
  set "CLAUDE_CODE_MAX_CONTEXT_TOKENS=%AGENT_CTX%"
  if "%~1"=="" (
    ollama launch claude --model "%AGENT_MODEL%"
  ) else (
    ollama launch claude --model "%AGENT_MODEL%" -- %1 %2 %3 %4 %5 %6 %7 %8 %9
  )
  exit /b %errorlevel%
)

if /I "%~1"=="headless" (
  shift
  call :run_dsh --profile headless --patch "%ROOT%\dsh\base.patch.yml" --patch "%ROOT%\dsh\headless.patch.yml" %1 %2 %3 %4 %5 %6 %7 %8 %9
  exit /b %errorlevel%
)

echo Launching DeepSeek Harness Web UI ^(%AGENT_MODEL%, ctx=%AGENT_CTX%, maxTokens=%AGENT_MAX_TOKENS%^)...
call :run_dsh --profile web --patch "%ROOT%\dsh\base.patch.yml" --patch "%ROOT%\dsh\web.patch.yml" %1 %2 %3 %4 %5 %6 %7 %8 %9
if errorlevel 1 pause
exit /b %errorlevel%

:run_dsh
where dsh.cmd >nul 2>nul
if not errorlevel 1 (
  call dsh.cmd %*
  exit /b %errorlevel%
)
call npx.cmd -y @deepseek-ai/dsh %*
exit /b %errorlevel%
