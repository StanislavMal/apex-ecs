import re

path = r'C:\My\Projects\Rust_projects\apex-ecs\crates\apex-scheduler\src\lib.rs'
with open(path, 'r', encoding='utf-8') as f:
    content = f.read()

# 1. add_auto_system('name', Struct) -> add_systems with system()
content = re.sub(
    r"(s\w*\.)add_auto_system\(([^)]+)\)",
    r"\1add_systems(StageLabel::Update, (system(\2),))",
    content
)

# 2. add_auto_system_to_stage('name', Struct, stage)
content = re.sub(
    r"(s\w*\.)add_auto_system_to_stage\(([^,]+),\s*([^,]+),\s*([^)]+)\)",
    r"\1add_systems(\4, (system(\2, \3),))",
    content
)

# 3. add_system('name', closure) — but not followed by .id()
content = re.sub(
    r"(s\w*\.)add_system\(([^)]+)\)(?!\s*\.\s*id)",
    r"\1add_systems(StageLabel::Update, (system_seq(\2),))",
    content
)

# 4. add_system('name', closure).id() — capture id differently
content = re.sub(
    r"(s\w*\.)add_system\(([^)]+)\)\s*\.\s*id\(\)",
    r"\1add_systems(StageLabel::Update, (system_seq(\2),)); \1find_id_by_name('UNKNOWN').unwrap()",
    content
)

# 5. add_system_to_stage('name', fn, stage)
content = re.sub(
    r"(s\w*\.)add_system_to_stage\(([^,]+),\s*([^,]+),\s*([^)]+)\)",
    r"\1add_systems(\4, (system_seq(\2, \3),))",
    content
)

# 6. add_startup_system('name', fn)
content = re.sub(
    r"(s\w*\.)add_startup_system\(([^)]+)\)",
    r"\1add_systems(StageLabel::Startup, (system_seq(\2),))",
    content
)

# 7. add_startup_auto_system('name', Struct)
content = re.sub(
    r"(s\w*\.)add_startup_auto_system\(([^)]+)\)",
    r"\1add_systems(StageLabel::Startup, (system(\2),))",
    content
)

# 8. add_par_system('name', Struct)
content = re.sub(
    r"(s\w*\.)add_par_system\(([^)]+)\)",
    r"\1add_systems(StageLabel::Update, (system(\2),))",
    content
)

# 9. add_par('name', fn)
content = re.sub(
    r"(s\w*\.)add_par\(([^)]+)\)",
    r"\1add_systems(StageLabel::Update, (system_par(\2),))",
    content
)

# 10. add_par_access('name', access, fn)
content = re.sub(
    r"(s\w*\.)add_par_access\(([^,]+),\s*([^,]+),\s*([^)]+)\)",
    r"\1add_systems(StageLabel::Update, (system_par_access(\2, \3, \4),))",
    content
)

# 11. pipeline: produced_by(id, 'name') -> produced_by('name')
content = re.sub(
    r"\.produced_by\(([^,]+),\s*([^)]+)\)",
    r".produced_by(\2)",
    content
)
content = re.sub(
    r"\.transformed_by\(([^,]+),\s*([^)]+)\)",
    r".transformed_by(\2)",
    content
)
content = re.sub(
    r"\.consumed_by\(([^,]+),\s*([^)]+)\)",
    r".consumed_by(\2)",
    content
)

with open(path, 'w', encoding='utf-8') as f:
    f.write(content)
print('Done')
