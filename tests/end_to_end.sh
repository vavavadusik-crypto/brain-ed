#!/usr/bin/env bash
# Сквозные проверки обещаний продукта. Каждый тест — отдельная база в /var/tmp.
set -uo pipefail

BRAIN="${BRAIN_BIN:-$(dirname "$0")/../target/release/brain}"
# Tests that assert the "no model installed" path must not read the developer's
# own model cache, or they pass or fail depending on whose machine they run on.
NO_MODELS_DIR="$(mktemp -u /var/tmp/brain-it-no-models-XXXXXX)"
PASS=0
FAIL=0

check() {
  local name="$1" condition="$2"
  if eval "$condition"; then
    printf '  OK   %s\n' "$name"; PASS=$((PASS + 1))
  else
    printf '  FAIL %s\n' "$name"; FAIL=$((FAIL + 1))
  fi
}

new_db() {
  local db; db="$(mktemp -u /var/tmp/brain-it-XXXXXX.sqlite)"
  "$BRAIN" --database "$db" init >/dev/null 2>&1
  printf '%s' "$db"
}

echo "IT-01 память переживает перезапуск процесса"
DB="$(new_db)"
"$BRAIN" --database "$DB" --offline remember "морская свинка не свинка и не морская" --kind fact >/dev/null 2>&1
FOUND="$("$BRAIN" --database "$DB" --offline recall "морская свинка" 2>/dev/null | grep -c '"id"')"
check "запись найдена новым процессом" "[ \"$FOUND\" -ge 1 ]"

echo "IT-06 поиск работает без модели эмбеддингов"
OUT="$("$BRAIN" --database "$DB" --models-dir "$NO_MODELS_DIR" --offline ask "свинка" 2>&1)"
check "результат найден на BM25" "echo \"\$OUT\" | grep -q '\"found\": [1-9]'"
check "предупреждение о недоступной модели показано" "echo \"\$OUT\" | grep -qi 'semantic model'"

echo "IT-08 хук укладывается в бюджет 50 мс"
START="$(date +%s%N)"
echo '{"session_id":"it","cwd":"/tmp","tool_name":"Read","tool_input":{"file_path":"/tmp/x"}}' \
  | "$BRAIN" hook --event PostToolUse --client claude >/dev/null 2>&1
ELAPSED_MS=$(( ($(date +%s%N) - START) / 1000000 ))
check "хук ответил за ${ELAPSED_MS} мс (бюджет 50)" "[ $ELAPSED_MS -le 50 ]"

echo "IT-11 секреты не доходят до хранилища"
SECRET_DB="$(new_db)"
echo '{"session_id":"it","cwd":"/tmp","tool_name":"Bash","tool_input":{"command":"psql postgres://u:SeCrEtPass99@db/x"}}' \
  | BRAIN_DATABASE="$SECRET_DB" "$BRAIN" hook --event PostToolUse --client claude >/dev/null 2>&1
check "пароль отсутствует в базе" "! grep -qa 'SeCrEtPass99' \"$SECRET_DB\" 2>/dev/null"

echo "IT-12 индексатор строит граф кода"
CODE_DB="$(new_db)"
FIXTURE="$(mktemp -d /var/tmp/brain-fixture-XXXXXX)"
cat > "$FIXTURE/sample.py" <<'PY'
def helper():
    return 1

def caller():
    return helper()

class Widget:
    def render(self):
        return helper()
PY
IDX="$("$BRAIN" --database "$CODE_DB" --offline index "$FIXTURE" 2>&1)"
check "функции найдены" "echo \"\$IDX\" | grep -q '\"functions\": [3-9]'"
check "класс найден" "echo \"\$IDX\" | grep -q '\"classes\": 1'"
check "узлы записаны в базу" "echo \"\$IDX\" | grep -q '\"nodes_persisted\": [1-9]'"
check "связи вызовов записаны" "echo \"\$IDX\" | grep -q '\"edges_persisted\": [1-9]'"
CODE_FOUND="$("$BRAIN" --database "$CODE_DB" --offline ask "helper" 2>/dev/null | grep -c '"id"')"
check "код находится поиском" "[ \"$CODE_FOUND\" -ge 1 ]"
rm -rf "$FIXTURE"

echo "IT-07 MCP отдаёт весь набор инструментов"
TOOLS="$(printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"it","version":"1"}}}\n{"jsonrpc":"2.0","id":2,"method":"tools/list"}\n' \
  | timeout 20 "$BRAIN" serve 2>/dev/null | grep -o '"name":"memory_[a-z]*"' | sort -u | wc -l)"
check "инструментов ровно 10 (получено $TOOLS)" "[ \"$TOOLS\" -eq 10 ]"

rm -f /var/tmp/brain-it-*.sqlite*
printf '\nитого: %d пройдено, %d провалено\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
