# Дистилляция за 15 минут

Дистилляция — необязательный шаг, который читает уже собранный архив переписок
(`brain archive`) и извлекает из него **прочные факты** — решения, ошибки, тупики,
находки и планы — в граф памяти. Архив остаётся приватным хранилищем переписок;
дистиллятор лишь выбирает сессии, отправляет их замаскированный текст провайдеру
и записывает подтверждённые факты. Подробная справка по всем внутренностям —
в [docs/distiller.md](distiller.md); здесь — рабочий маршрут с проверенным
реальным выводом.

Полная английская версия: [docs/distill-guide.md](distill-guide.md).

Всё ниже проверено на синтетическом архиве во временном HOME с фейковым
OpenAI-совместимым провайдером (скрипт в конце); никакая настоящая модель не
вызывалась, реальный архив не читался.

## 1. Провайдеры

Провайдер — то, что превращает текст сессий в факты. Настраивается один раз:

```console
$ brain llm add local --base-url http://127.0.0.1:8080 --model local-model --context-tokens 16384
```

Три вида провайдеров, разница — в безопасности:

| Вид | Что это | Гарантии |
|---|---|---|
| Локальный (llama.cpp и совместимые) | HTTP на `127.0.0.1` | текст не покидает машину; секреты всё равно маскируются до отправки |
| HTTPS-шлюз | облачный OpenAI-совместимый API | текст уходит провайдеру по его условиям; ключ — только через `api_key_env` (имя переменной окружения, сам секрет в конфиг не пишется) |
| Подписка `claude` | CLI-провайдер через подписку Claude | у провайдера **нет инструментов** — он не может ничего выполнить, только вернуть JSON с фактами |
| Подписка `codex` | CLI-провайдер через подписку Codex | провайдер работает в **read-only песочнице** — читать может, писать и запускать не может |

Для чувствительных архивов выбирайте локальную модель: маскировка снимает
распознанные секреты, но не всё приватное содержимое.

## 2. Синтетический архив и сбор

```console
$ brain --offline init
{ "database": "…/.brain/brain.sqlite", "schema_version": 5, "status": "initialized" }

$ brain archive enable --since-days 7
$ brain archive collect
{ "status": "ok", "stored_entries": { "claude": 4 }, … }
```

`collect` — тот же ограниченный прогон, что делает таймер: читает новые файлы
переписок, маскирует секреты и складывает записи в `archive/sessions.sqlite`.

## 3. Первый прогон

```console
$ brain distill run --dry-run
{
  "accepted": 0,
  "bytes": 520,
  "calls": 1,
  "chunks": 1,
  "facts_written": 0,
  "proposed": 0,
  "rejected": {},
  "roles": ["assistant", "tool_error", "user"],
  "sessions": 1,
  "status": "dry_run"
}
```

`--dry-run` готовит ровно те же запросы (bytes/chunks), но ничего не пишет и
никого не вызывает.

```console
$ brain distill run --session synth-4
distill session=1 chunk=1 bytes=262 state=started
distill session=1 chunk=1 bytes=262 seconds=0.530 proposed=2 accepted=2 rejected={} state=finished
{
  "accepted": 2,
  "bytes": 262,
  "calls": 1,
  "chunks": 1,
  "facts_written": 2,
  "proposed": 2,
  "rejected": {},
  "roles": ["assistant", "tool_error", "user"],
  "sessions": 1,
  "status": "ok"
}
```

`proposed` — сколько фактов вернул провайдер, `accepted` — сколько прошло проверку
(см. §5), `facts_written` — сколько записано в граф. Прогресс держится в базе:
повторный прогон той же сессии — `sessions: 0`, ничего не перечитывается и
не дублируется.

## 4. Полезные флаги

```console
# сырой ответ провайдера сохраняется в каталог (0600) — для разбора отказов
$ brain distill run --session synth-5 --keep-raw $T/raw
{ … "accepted": 2, "facts_written": 2, … }
$ ls $T/raw
chunk-3771822-1793040081216924877.json

# выбрать только сессии с записями этих ролей
$ brain distill run --roles user,assistant,tool_error

# ограничить размер запроса: не больше N сессий в одном чанке
$ brain distill run --batch-sessions 10 --max-sessions 20 --oldest-first

# список провайдеров = порядок fallback: первый недоступный — автоматический переход
$ brain distill run --session synth-6 --provider broken,fake
distill session=1 chunk=1 bytes=253 state=started
distill session=1 chunk=1 bytes=253 seconds=0.566 proposed=2 accepted=2 rejected={} state=finished
{ … "accepted": 2, "facts_written": 2, "status": "ok" }
```

`--provider broken,fake` — реальный прогон: `broken` слушал несуществующий порт,
дистиллятор перешёл к `fake` и записал факты.

## 5. Проверки фактов и отказы

Каждый предложенный факт проходит проверку схемы и связности; отклонённые
считаются по причинам. Реальный прогон с плохими фактами фейкового провайдера:

```console
{ "proposed": 2, "accepted": 0,
  "rejected": { "session_mismatch": 2 }, "status": "ok" }
{ "proposed": 4, "accepted": 0,
  "rejected": { "session_mismatch": 1, "unknown_status": 3 }, "status": "ok" }
```

Причины (все видны в `rejected` и в строке прогресса): `session_mismatch` —
evidence указывает на чужую сессию; `unknown_status` — статус не из списка
типа (у error: suspected/confirmed/fixed/…, у decision: proposed/adopted/…);
`over_max_facts`, `unknown_type`, `empty_what`, `field_too_long`, `no_evidence`,
`tool_result_only`, `mixed_projects`. Прогресс-строка пишет суммарное состояние:

```console
Error: distillation provider unavailable or returned invalid JSON (transport
error: Network Error); check provider timeout_seconds for slow local models;
cursor not advanced
```

Отказ транспорта не двигает курсор — просто повторите прогон. Состояние
`provider_limited` означает, что провайдер упёрся в лимит токенов на ответ:
уменьшите `--batch-sessions` (меньше сессий в одном чанке) или возьмите
провайдера с большим окном. Повторное `distill run` продолжит с курсора.

## 6. Статус и поиск

```console
$ brain distill status
{
  "completed_sessions": 4,
  "facts": { "decision:adopted": 1, "error:suspected": 1 },
  "remaining_sessions": 0,
  "status": "ok"
}

$ brain --offline ask "Synthetic failure in synth-4" --no-archive
{ … "found": 5, "results": [ { "kind": "error", "score": "0.5161…",
  "source": "archive-session://synth-4" }, … ] }
```

Дистиллированные факты — обычные узлы графа: их находит `ask`, у них есть
связи `has_session`/`caused_by` и bi-temporal поля.

## 7. Расписание (необязательно)

Таймер ставится через systemd user; здесь он проверен с фейковым `systemctl`
в PATH (ваш прогон будет включать настоящий юнит):

```console
$ brain distill schedule enable --provider local --every 6h --quiet-hours 23-7
{
  "status": "ok",
  "systemctl_daemon_reload": true,
  "systemctl_enable_or_disable": true,
  "unit_files": ["…/.config/systemd/user/brain-distill.service",
                 "…/.config/systemd/user/brain-distill.timer"]
}

$ brain distill schedule status
{ "active": true, "enabled": true, "last_run": null, "status": "ok",
  "unit_files_installed": true }

$ brain distill schedule disable
```

`--quiet-hours 23-7` — локальное окно (начало включительно, конец нет), в котором
провайдер не вызывается. Смена провайдера или периода — повторный `enable`
с новыми значениями.

---

## Приложение: фейковый провайдер для прогона без модели

```python
#!/usr/bin/env python3
# fake-provider.py PORT — OpenAI-совместимый сервер: на каждый запрос
# возвращает по одному факту на запись сессии (контракт дистиллятора).
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

def fact(eid, session, kind, status, what):
    return {"type": kind, "what": what, "why": "Observed synthetic evidence.",
            "status": status, "session": session, "project": "synthetic",
            "evidence": [eid],
            "status_evidence": {"entry_id": eid, "quote": "synthetic"}}

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        prompt = json.loads(self.rfile.read(n))["messages"][0]["content"]
        data = json.loads(prompt.split("Session data: ", 1)[1])
        facts = []
        for s in data["sessions"]:
            sid = s["session"]
            for i, e in enumerate(s["entries"]):
                if i == 0:
                    facts.append(fact(int(e["id"]), sid, "error", "suspected",
                                      f"Synthetic failure in {sid}"))
                else:
                    f = fact(int(e["id"]), sid, "decision", "adopted",
                             f"Synthetic remediation for {sid}")
                    f["links"] = [{"relation": "caused_by", "target_type": "error",
                                   "target_what": f"Synthetic failure in {sid}"}]
                    facts.append(f)
        body = json.dumps({"choices": [{"message":
            {"content": json.dumps({"facts": facts})}}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *a): pass

HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
```

Запуск: `python3 fake-provider.py 48971 &`, затем
`brain llm add fake --base-url http://127.0.0.1:48971 --model synthetic --context-tokens 8192`.
