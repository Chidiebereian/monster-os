@echo off
rem Run by the Windows scheduled tasks "MonsterOS backup" (daily) and "MonsterOS restore check"
rem (weekly); see docs/operations/backup-recovery.md. Usage: backup-task.cmd backup ^| restore_check
if /i not "%~1"=="backup" if /i not "%~1"=="restore_check" (echo usage: backup-task.cmd backup^|restore_check & exit /b 2)
set "REPORTS=%LOCALAPPDATA%\MonsterOS\reports"
if not exist "%REPORTS%" mkdir "%REPORTS%"
set "LOG=%REPORTS%\backup_task.log"
cd /d "%~dp0..\.."
echo [%date% %time%] %~1 >> "%LOG%"
python scripts\ops\%~1.py >> "%LOG%" 2>&1
set RC=%ERRORLEVEL%
echo [%date% %time%] %~1 exit %RC% >> "%LOG%"
exit /b %RC%
