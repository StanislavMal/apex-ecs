@echo off
chcp 65001 > nul
setlocal enabledelayedexpansion

REM ==========================================================
REM  UNIVERSAL PROJECT CONTEXT GENERATOR v3.2
REM  ✅ Автоматическое разбиение на части по лимиту AI контекста
REM  ✅ Равномерное распределение файлов по частям
REM  ✅ Корректная кодировка UTF-8
REM  ✅ Автоматическое чтение .gitignore для исключения файлов
REM ==========================================================

set "OUTPUT_BASE=project_context"
set "CODE_EXTENSIONS=rs ts js tsx jsx py go java cpp c cs php html css scss json yaml yml toml md"
set /a MAX_PART_SIZE=150000

REM Инициализируем EXCLUDE_PATTERNS базовыми значениями
set "EXCLUDE_PATTERNS=node_modules build dist .git .github venv __pycache__ *.dot *.csv project_context crates/apex-bench plans"

REM Читаем .gitignore и добавляем его паттерны
if exist ".gitignore" (
    echo [INFO] Читаю .gitignore для исключения файлов...
    set "GITIGNORE_PATTERNS="
    for /f "usebackq delims=" %%i in (".gitignore") do (
        set "line=%%i"
        REM Удаляем начальные и конечные пробелы
        for /f "tokens=*" %%j in ("!line!") do set "line=%%j"
        
        REM Пропускаем пустые строки и комментарии
        if not "!line!"=="" (
            if not "!line:~0,1!"=="#" (
                REM Удаляем начальный / если есть
                if "!line:~0,1!"=="/" set "line=!line:~1!"
                REM Удаляем начальный **/ если есть
                if "!line:~0,3!"=="**/" set "line=!line:~3!"
                REM Удаляем конечный / если есть
                if "!line:~-1!"=="/" set "line=!line:~0,-1!"
                
                REM Добавляем в GITIGNORE_PATTERNS, избегая дубликатов
                if not "!line!"=="" (
                    echo !GITIGNORE_PATTERNS! | find " !line! " > nul
                    if !errorlevel! == 1 (
                        set "GITIGNORE_PATTERNS=!GITIGNORE_PATTERNS! !line!"
                    )
                )
            )
        )
    )
    
    REM Объединяем базовые паттерны с паттернами из .gitignore
    set "EXCLUDE_PATTERNS=!EXCLUDE_PATTERNS!!GITIGNORE_PATTERNS!"
    echo [INFO] Добавлено паттернов из .gitignore:!GITIGNORE_PATTERNS!
)

echo ==============================================
echo  PROJECT CONTEXT GENERATOR v3.2
echo ==============================================
echo.
echo Выберите режим работы:
echo.
echo   [1] Создать один единый файл
echo   [2] Разбить на несколько частей (для больших проектов)
echo   [3] Указать сколько частей сделать вручную
echo.
set /p MODE="Введите номер режима [1]: "
if not defined MODE set MODE=1
echo.

if "!MODE!" == "1" (
    REM Один файл
    set MAX_PART_SIZE=1000000000
    set TOTAL_PARTS=1
)

if "!MODE!" == "2" (
    REM Автоматический режим 120 КБ на часть
    set MAX_PART_SIZE=150000
    set TOTAL_PARTS=~
    REM echo Будет создано примерно 2 части
    echo.
)

if "!MODE!" == "3" (
    REM Ручной режим
    set /p TOTAL_PARTS="Введите сколько частей сделать: "
    set /a MAX_PART_SIZE = 300000 / TOTAL_PARTS
    echo.
)

set /a PART_NUM=1
set "CURRENT_FILE=%OUTPUT_BASE%_!PART_NUM!.txt"

REM Создаём заголовок первой части
echo PROJECT CONTEXT FILE > "!CURRENT_FILE!"
echo Generated: %date% %time% >> "!CURRENT_FILE!"
echo Часть !PART_NUM! из !TOTAL_PARTS! >> "!CURRENT_FILE!"
echo ======================================== >> "!CURRENT_FILE!"
echo. >> "!CURRENT_FILE!"

echo [1/3] Добавляю структуру проекта и файлы конфигурации
echo. >> "!CURRENT_FILE!"
echo ===== PROJECT STRUCTURE ===== >> "!CURRENT_FILE!"
echo. >> "!CURRENT_FILE!"

REM Создаем временный файл для структуры проекта
echo PROJECT STRUCTURE > "%TEMP%\project_tree.txt"
echo Generated: %date% %time% >> "%TEMP%\project_tree.txt"
echo. >> "%TEMP%\project_tree.txt"

REM Генерируем подробную структуру проекта с основными файлами
echo Directory structure with main files (excluding .git, target, plans, etc.): >> "%TEMP%\project_tree.txt"
echo ==================================================================== >> "%TEMP%\project_tree.txt"
echo. >> "%TEMP%\project_tree.txt"

REM Детальная структура с основными файлами
echo . >> "%TEMP%\project_tree.txt"
echo ├── .gitignore >> "%TEMP%\project_tree.txt"
echo ├── Apex_ECS_Руководство_пользователя.md >> "%TEMP%\project_tree.txt"
echo ├── Cargo.toml >> "%TEMP%\project_tree.txt"
echo ├── generate_context.bat >> "%TEMP%\project_tree.txt"
echo └── crates/ >> "%TEMP%\project_tree.txt"
echo     ├── apex-bench/ (excluded from context) >> "%TEMP%\project_tree.txt"
echo     │   ├── Cargo.toml >> "%TEMP%\project_tree.txt"
echo     │   ├── benches/ >> "%TEMP%\project_tree.txt"
echo     │   │   ├── benchmark.rs >> "%TEMP%\project_tree.txt"
echo     │   │   ├── comparison.rs >> "%TEMP%\project_tree.txt"
echo     │   │   ├── graph_bench.rs >> "%TEMP%\project_tree.txt"
echo     │   │   └── specialized.rs >> "%TEMP%\project_tree.txt"
echo     │   └── src/ >> "%TEMP%\project_tree.txt"
echo     │       ├── lib.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── add_remove.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── commands_bench.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── fragmented_iter.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── relations_bench.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── schedule.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── simple_insert.rs >> "%TEMP%\project_tree.txt"
echo     │       └── simple_iter.rs >> "%TEMP%\project_tree.txt"
echo     ├── apex-core/ >> "%TEMP%\project_tree.txt"
echo     │   ├── Cargo.toml >> "%TEMP%\project_tree.txt"
echo     │   └── src/ >> "%TEMP%\project_tree.txt"
echo     │       ├── lib.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── access.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── archetype.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── commands.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── component.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── entity.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── events.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── query.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── relations.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── resources.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── sub_world.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── system_param.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── template.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── transform.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── world.rs >> "%TEMP%\project_tree.txt"
echo     │       └── storage/ >> "%TEMP%\project_tree.txt"
echo     │           ├── mod.rs >> "%TEMP%\project_tree.txt"
echo     │           └── sparse_set.rs >> "%TEMP%\project_tree.txt"
echo     ├── apex-examples/ >> "%TEMP%\project_tree.txt"
echo     │   ├── Cargo.toml >> "%TEMP%\project_tree.txt"
echo     │   └── examples/ >> "%TEMP%\project_tree.txt"
echo     │       ├── basic.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── hot_reload_test.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── perf.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── prefab_isolated.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── scripting.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── serialization_hot_reload.rs >> "%TEMP%\project_tree.txt"
echo     │       └── transform_example.rs >> "%TEMP%\project_tree.txt"
echo     ├── apex-graph/ >> "%TEMP%\project_tree.txt"
echo     │   ├── Cargo.toml >> "%TEMP%\project_tree.txt"
echo     │   └── src/ >> "%TEMP%\project_tree.txt"
echo     │       ├── lib.rs >> "%TEMP%\project_tree.txt"
echo     │       └── algorithms.rs >> "%TEMP%\project_tree.txt"
echo     ├── apex-hot-reload/ >> "%TEMP%\project_tree.txt"
echo     │   ├── Cargo.toml >> "%TEMP%\project_tree.txt"
echo     │   └── src/ >> "%TEMP%\project_tree.txt"
echo     │       ├── lib.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── asset_registry.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── plugin.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── prefab_plugin.rs >> "%TEMP%\project_tree.txt"
echo     │       └── watcher.rs >> "%TEMP%\project_tree.txt"
echo     ├── apex-isolated/ >> "%TEMP%\project_tree.txt"
echo     │   ├── Cargo.toml >> "%TEMP%\project_tree.txt"
echo     │   └── src/ >> "%TEMP%\project_tree.txt"
echo     │       └── lib.rs >> "%TEMP%\project_tree.txt"
echo     ├── apex-macros/ >> "%TEMP%\project_tree.txt"
echo     │   ├── Cargo.toml >> "%TEMP%\project_tree.txt"
echo     │   └── src/ >> "%TEMP%\project_tree.txt"
echo     │       └── lib.rs >> "%TEMP%\project_tree.txt"
echo     ├── apex-scheduler/ >> "%TEMP%\project_tree.txt"
echo     │   ├── Cargo.toml >> "%TEMP%\project_tree.txt"
echo     │   └── src/ >> "%TEMP%\project_tree.txt"
echo     │       ├── lib.rs >> "%TEMP%\project_tree.txt"
echo     │       └── stage.rs >> "%TEMP%\project_tree.txt"
echo     ├── apex-scripting/ >> "%TEMP%\project_tree.txt"
echo     │   ├── Cargo.toml >> "%TEMP%\project_tree.txt"
echo     │   ├── README_SCRIPTING.md >> "%TEMP%\project_tree.txt"
echo     │   └── src/ >> "%TEMP%\project_tree.txt"
echo     │       ├── lib.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── context.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── error.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── field.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── iterators.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── registrar.rs >> "%TEMP%\project_tree.txt"
echo     │       ├── rhai_api.rs >> "%TEMP%\project_tree.txt"
echo     │       └── script_engine.rs >> "%TEMP%\project_tree.txt"
echo     └── apex-serialization/ >> "%TEMP%\project_tree.txt"
echo         ├── Cargo.toml >> "%TEMP%\project_tree.txt"
echo         └── src/ >> "%TEMP%\project_tree.txt"
echo             ├── lib.rs >> "%TEMP%\project_tree.txt"
echo             ├── prefab.rs >> "%TEMP%\project_tree.txt"
echo             ├── serializer.rs >> "%TEMP%\project_tree.txt"
echo             └── snapshot.rs >> "%TEMP%\project_tree.txt"

echo. >> "%TEMP%\project_tree.txt"
echo Summary: >> "%TEMP%\project_tree.txt"
echo - Root files: 4 (.gitignore, Cargo.toml, generate_context.bat, Apex_ECS_Руководство_пользователя.md) >> "%TEMP%\project_tree.txt"
echo - Crates: 10 (apex-core, apex-graph, apex-scheduler, apex-serialization, apex-hot-reload, apex-isolated, apex-examples, apex-bench, apex-macros, apex-scripting) >> "%TEMP%\project_tree.txt"
echo - Source files in apex-core: 17 >> "%TEMP%\project_tree.txt"
echo - Source files in apex-graph: 2 >> "%TEMP%\project_tree.txt"
echo - Source files in apex-scheduler: 2 >> "%TEMP%\project_tree.txt"
echo - Source files in apex-hot-reload: 5 >> "%TEMP%\project_tree.txt"
echo - Source files in apex-isolated: 1 >> "%TEMP%\project_tree.txt"
echo - Source files in apex-macros: 1 >> "%TEMP%\project_tree.txt"
echo - Source files in apex-scripting: 8 (+1 README) >> "%TEMP%\project_tree.txt"
echo - Source files in apex-serialization: 4 >> "%TEMP%\project_tree.txt"
echo - Example files: 7 >> "%TEMP%\project_tree.txt"
echo - Source files in apex-bench: 8 (+4 benches, excluded from context) >> "%TEMP%\project_tree.txt"
echo - Total main files shown: ~70 (including apex-bench) >> "%TEMP%\project_tree.txt"

REM Добавляем структуру проекта в контекст
type "%TEMP%\project_tree.txt" >> "!CURRENT_FILE!"
echo. >> "!CURRENT_FILE!"
echo. >> "!CURRENT_FILE!"

del "%TEMP%\project_tree.txt" 2>nul

echo ===== PROJECT MANIFEST FILES ===== >> "!CURRENT_FILE!"
echo. >> "!CURRENT_FILE!"

for %%f in (package.json Cargo.toml go.mod pyproject.toml pom.xml build.gradle requirements.txt README.md .gitignore Dockerfile docker-compose.yml) do (
    if exist "%%f" (
        echo === %%f === >> "!CURRENT_FILE!"
        type "%%f" >> "!CURRENT_FILE!"
        echo. >> "!CURRENT_FILE!"
        echo. >> "!CURRENT_FILE!"
    )
)

echo [2/3] Сканирую исходные коды
echo. >> "!CURRENT_FILE!"
echo ===== SOURCE CODE FILES ===== >> "!CURRENT_FILE!"
echo. >> "!CURRENT_FILE!"

set /a FILE_COUNT=0
set /a PROCESSED_COUNT=0

echo [INFO] Сканирую файлы с расширениями: %CODE_EXTENSIONS%...

REM Используем dir для быстрого поиска файлов с нужными расширениями, исключая папки target, apex-bench и plans
(
    for %%x in (%CODE_EXTENSIONS%) do (
        dir /b /s /a-d "*.%%x" 2>nul | findstr /v /i "\\target\\" | findstr /v /i "\\apex-bench\\" | findstr /v /i "\\plans\\"
    )
) > "%TEMP%\filelist.txt" 2>nul

REM Читаем файлы из временного списка
for /f "usebackq delims=" %%f in ("%TEMP%\filelist.txt") do (
    set /a FILE_COUNT+=1
    set "file_!FILE_COUNT!=%%f"
)

del "%TEMP%\filelist.txt" 2>nul

echo [INFO] Найдено файлов: !FILE_COUNT!
echo.

if !FILE_COUNT! == 0 (
    echo [WARNING] Не найдено файлов с указанными расширениями!
    echo.
)

REM ==========================================================
REM Фильтрация файлов и вычисление общего размера только неисключенных
REM ==========================================================
set /a TOTAL_SIZE=0
set /a FILTERED_FILE_COUNT=0

echo [INFO] Фильтрую файлы и вычисляю размеры...
for /l %%i in (1,1,!FILE_COUNT!) do (
    set "filepath=!file_%%i!"
    
    set "exclude=0"
    
    REM Быстрая проверка исключений - сначала проверяем пути
    for %%e in (%EXCLUDE_PATTERNS%) do (
        REM Пропускаем паттерны с * на этом этапе
        echo %%e | find "*" > nul
        if !errorlevel! == 1 (
            REM Паттерн без звездочки - проверяем как путь
            if "!filepath:\%%e\=!" NEQ "!filepath!" (
                set exclude=1
            )
        )
    )
    
    if !exclude! == 0 (
        REM Дополнительная проверка для паттернов с *
        for %%e in (%EXCLUDE_PATTERNS%) do (
            echo %%e | find "*" > nul
            if !errorlevel! == 0 (
                REM Паттерн содержит * - проверяем расширение
                set "pattern=%%e"
                REM Удаляем * из начала
                set "pattern=!pattern:~1!"
                for %%f in ("!filepath!") do set "file_ext=%%~xf"
                if /i "!file_ext!" == "!pattern!" (
                    set exclude=1
                )
            )
        )
    )
    
    if !exclude! == 0 (
        REM Файл не исключен - учитываем его размер
        for %%s in ("!filepath!") do set /a "filesize=%%~zs"
        set /a TOTAL_SIZE+=filesize
        set /a FILTERED_FILE_COUNT+=1
        set "filtered_file_!FILTERED_FILE_COUNT!=!filepath!"
        set "filtered_filesize_!FILTERED_FILE_COUNT!=!filesize!"
    )
)

echo [INFO] После фильтрации осталось файлов: !FILTERED_FILE_COUNT!
echo [INFO] Общий размер неисключенных файлов: !TOTAL_SIZE! байт

REM Обновляем FILE_COUNT на количество отфильтрованных файлов
set /a FILE_COUNT=FILTERED_FILE_COUNT

REM Если режим 2 (автоматический), вычисляем TOTAL_PARTS на основе MAX_PART_SIZE
if "!MODE!" == "2" (
    if !TOTAL_SIZE! == 0 (
        set /a TOTAL_PARTS=1
    ) else (
        REM Убедимся, что MAX_PART_SIZE не ноль
        if !MAX_PART_SIZE! == 0 set MAX_PART_SIZE=120000
        set /a "TOTAL_PARTS=(TOTAL_SIZE + MAX_PART_SIZE - 1) / MAX_PART_SIZE"
        if !TOTAL_PARTS! == 0 set /a TOTAL_PARTS=1
    )
    echo [INFO] Автоматически определено частей: !TOTAL_PARTS!
)

REM Если TOTAL_PARTS равно ~ (тильда), устанавливаем в 1
if "!TOTAL_PARTS!" == "~" set /a TOTAL_PARTS=1

REM Вычисляем целевой размер части
if !TOTAL_PARTS! == 0 set /a TOTAL_PARTS=1
REM Убедимся, что TOTAL_PARTS не ноль (на всякий случай)
if !TOTAL_PARTS! == 0 set /a TOTAL_PARTS=1
set /a "TARGET_SIZE=(TOTAL_SIZE + TOTAL_PARTS - 1) / TOTAL_PARTS"
echo [INFO] Целевой размер части: !TARGET_SIZE! байт (максимум !MAX_PART_SIZE! байт)

REM ==========================================================
REM Обрабатываем каждый файл с равномерным распределением
REM ==========================================================
set /a CURRENT_PART_SIZE=0

for /l %%i in (1,1,!FILE_COUNT!) do (
    set "filepath=!filtered_file_%%i!"
    set "filesize=!filtered_filesize_%%i!"
    
    REM Показываем прогресс каждые 20 файлов
    set /a PROCESSED_COUNT+=1
    set /a "MOD=PROCESSED_COUNT %% 20"
    if !MOD! == 0 (
        echo Обработано !PROCESSED_COUNT! из !FILE_COUNT! файлов...
    )
    
    REM Файл уже отфильтрован, исключения не проверяем
    
    REM Проверяем, не превышает ли добавление этого файла целевой размер части
    set /a "NEW_SIZE=CURRENT_PART_SIZE + filesize"
    if !NEW_SIZE! GTR !TARGET_SIZE! (
        if !CURRENT_PART_SIZE! GTR 0 (
            REM Проверяем, не достигли ли мы уже максимального количества частей
            if !PART_NUM! LSS !TOTAL_PARTS! (
                echo --- КОНЕЦ ЧАСТИ !PART_NUM! --- >> "!CURRENT_FILE!"
                set /a PART_NUM += 1
                set "CURRENT_FILE=%OUTPUT_BASE%_!PART_NUM!.txt"
                
                echo PROJECT CONTEXT FILE > "!CURRENT_FILE!"
                echo Generated: %date% %time% >> "!CURRENT_FILE!"
                echo Часть !PART_NUM! из !TOTAL_PARTS! >> "!CURRENT_FILE!"
                echo Продолжение предыдущей части >> "!CURRENT_FILE!"
                echo ======================================== >> "!CURRENT_FILE!"
                echo. >> "!CURRENT_FILE!"
                set /a CURRENT_PART_SIZE=0
            )
        )
    )
    
    echo === FILE: !filepath! === >> "!CURRENT_FILE!"
    type "!filepath!" >> "!CURRENT_FILE!"
    echo. >> "!CURRENT_FILE!"
    echo. >> "!CURRENT_FILE!"
    set /a CURRENT_PART_SIZE+=filesize
)

REM Обновляем TOTAL_PARTS на реальное количество созданных частей (но не больше заданного)
if !PART_NUM! LSS !TOTAL_PARTS! set /a TOTAL_PARTS=PART_NUM

echo --- КОНЕЦ ЧАСТИ !PART_NUM! --- >> "!CURRENT_FILE!"

echo [3/3] Готово!
echo.
echo ✅ Создано файлов контекста: !PART_NUM! шт.
echo.
if !PART_NUM! GTR 1 (
    echo Каждая часть примерно !TARGET_SIZE! байт - идеально подходит для окна контекста AI
    echo В конце каждого файла есть отметка "КОНЕЦ ЧАСТИ N"
    echo.
)

endlocal
