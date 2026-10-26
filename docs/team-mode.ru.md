# Командный режим за 10 минут

Для координатора за NAT доступен собственный relay компании: отдельные relay-токены и сквозной TLS координатора. Прямой TLS сохраняется; пакеты с relay используют v2. Настройка, границы доверия и ограничения: [relay.md](relay.md). Синтетическое процессное демо: `tools/team-demo.sh --via-relay`. P2P/Iroh и проверка в публичной NAT-сети в E7.7 не входят.

Координатор задач Brain позволяет нескольким машинам работать над одной доской задач
через TLS: одна машина держит координатор (`net serve`), остальные подключаются
пакетом, который выдаёт владелец (`net enroll` → `net join`). Задачи, заявки и
аренды живут в базе координации; у каждой учётки своя роль и свои пространства.

Всё ниже проверено на одной машине: координатор и клиент в **раздельных временных
HOME**, реальный вывод команд вставлен как есть. Токен учётки пишется только в
файл креденшела (0600) и нигде не печатается — в выводах ниже его нет.

Обозначения: `H1` — HOME координатора, `H2` — HOME клиента, `brain` — собранный
бинарник. Порты в примерах — фактические из прогона; у вас они будут другими.

---

## 0. Собрать бинарник

```console
$ CARGO_TARGET_DIR=$HOME/projects/brain-ed-target/GLM-guide cargo build -p brain-cli
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1m 15s
```

## 1. Инициализировать мозг и сеть координатора

```console
$ HOME=$H1 brain --offline init
{
  "database": "…/coordinator/.brain/brain.sqlite",
  "schema_version": 5,
  …
}

$ HOME=$H1 brain net init --state-dir $H1/.brain/net --name localhost --ip 127.0.0.1
{
  "ca_sha256_fingerprint": "0d6cf6a110392ab34f72cc17c75d1ef94df9a88a74c44b711d107cdb5b55cbc1",
  "created": true,
  "files": {
    "ca_key": "…/coordinator/.brain/net/tls/ca.key",
    …
```

`--name` и `--ip` попадают в SAN серверного сертификата. Для второй машины по сети
добавьте её DNS-имя (`--name laptop.local`) или IP (`--ip 192.0.2.10`); примеры
адресов в документации — только из зарезервированных диапазонов (192.0.2.x, 100.64.0.x).

## 2. Поднять координатор

`net serve` работает, пока открыт стандартный ввод. В терминале просто оставьте
окно открытым; в скрипте держите тру открытым:

```console
$ sleep infinity | HOME=$H1 brain net serve --state-dir $H1/.brain/net
{"bind":"127.0.0.1:36401","deadline_ms":5000,"lan":false,"protocol":"brain-coord/1",
 "state_dir":"…/coordinator/.brain/net","status":"serving","stop":"close standard input",
 "transport":"tls (rustls; TLS 1.2/1.3)"}
```

Строка с `bind` — фактический адрес (порт может быть любым свободным; зафиксировать:
`--port 7443`). Остановка — закрыть stdin (Ctrl-C тоже подходит).

Порт уже занят? Теперь это понятная ошибка с категорией, без путей и секретов:

```console
$ sleep 1 | HOME=$H1 brain net serve --state-dir $H1/.brain/net --port 36401
{
  "category": "address_in_use",
  "error": { "detail": { "error": "address_in_use", "message": "address_in_use" }, "http_status": 503 },
  "status": "not_serving"
}
```

## 3. Выдать пакеты: worker и reader

`--role worker` — чтение и запись, `--role reader` — только чтение. `--grant-space`
обязателен: учётка видит только свои пространства. `--dial` — адреса, по которым
клиент будет искать координатор (их попадёт в пакет); по умолчанию список пуст,
поэтому для стенда укажите явно.

```console
$ HOME=$H1 brain net enroll --state-dir $H1/.brain/net \
    --machine laptop-two --agent-id claude --role worker --grant-space team \
    --label "second laptop" --dial 127.0.0.1:36401 \
    --credential-out $T/worker.cred.pem --bundle-out $T/worker.bundle
{
  "bundle": "…/worker.bundle",
  "bundle_server_name": "localhost",
  "capabilities": ["work_board", "work_show", "work_claims", "work_add", "work_depend",
                   "work_claim", "work_claim_path", "work_renew", "work_release",
                   "work_set_state", "work_handoff", "work_verify", "work_whoami",
                   "share_pull", "share_query"],
  "credential_file": "…/worker.cred.pem",
  "expires_at": 1800803997,
  "principal": { "agent": "claude", "label": "second laptop",
                 "machine": "laptop-two", "role": "worker", "spaces": ["team"] },
  "secret": "written to credential_file (0600) only",
  "status": "ok",
  "token_id": "t17930279975e61c7dcb2fd93a8e3814461"
}
```

Аналогично для читателя:

```console
$ HOME=$H1 brain net enroll --state-dir $H1/.brain/net \
    --machine htpc --agent-id codex --role reader --grant-space team \
    --dial 127.0.0.1:36401 \
    --credential-out $T/reader.cred.pem --bundle-out $T/reader.bundle
```

`token_id` — не секрет, он печатается и нужен для `net revoke`. Сам токен в
`worker.cred.pem`; пакет `worker.bundle` содержит тот же креденшел плюс CA и адреса.

## 4. Перенести пакет и подключиться

Перенесите файл пакета как угодно (scp, флешка, менеджер секретов). Права на файл —
0600, содержимое равноценно паролю машины.

```console
$ chmod 600 worker.bundle && scp worker.bundle user@laptop-two:
$ HOME=$H2 brain net join ~/worker.bundle
{
  "bundle_consumed": false,
  "client_dir": "…/client/.brain/net/client",
  "dial": [ "127.0.0.1:36401" ],
  "note": "net status and net call now work without --dial/--ca/--server-name/--credential",
  "previous_client_dir": null,
  "refused_dials": [],
  "server_name": "localhost",
  "status": "ok"
}
```

После `net join` флаги `--dial/--ca/--server-name/--credential` больше не нужны —
клиент читает их из `endpoint.json`.

**Если машина ходит через Tailscale/overlay-сеть**, адрес попадает под политику
dial, и пакет её не примет без явного согласия: IPv6 ULA считается LAN
(`--allow-lan-dial`), CGNAT (RFC 6598) — CGNAT (`--allow-cgnat-dial`):

```console
$ HOME=$H2 brain net join ~/worker.bundle --allow-lan-dial --allow-cgnat-dial
```

Диалы вне политики отбрасываются по одному: разрешённые попадут в `endpoint.json`,
отклонённые — в `refused_dials` в выводе; если отказаны все, ничего не записывается.

## 5. Проверить связь и поработать с доской

Пространство передаётся глобальным флагом `--space` (здесь — `team`, то самое из
`--grant-space`):

```console
$ HOME=$H2 brain net status
{
  "capabilities": [ "work_board", … "work_whoami", "share_pull", "share_query" ],
  "endpoint": { "dial": "127.0.0.1:36401", "server_name": "localhost", "tls": true, … },
  …
}

$ HOME=$H2 brain --space team net call add inbox "Разобрать входящие письма" --request-id demo-1
{ … "task": { "id": 1, "state": "queued", "created_by": {"agent": "claude", "machine": "laptop-two"}, … } }

$ HOME=$H2 brain --space team net call claim 1 --request-id demo-2
{ … "claim": { "claim_id": 1, "agent": "claude", "expires_in_secs": 900, "fence": 1, … } }

$ HOME=$H2 brain --space team net call board --state active
{ … "counts": { "active": 1, … }, "tasks": [ { "title": "Разобрать входящие письма", "owner": {…} } ] }

$ HOME=$H2 brain --space team net call show 1
{ … "task": {…}, "recent_events": [ { "kind": "created", … } ], "live_claims": […] }

$ HOME=$H2 brain --space team net call state 1 1 completed --note "готово" --request-id demo-3
```

`--request-id` делает `add`/`claim`/`state` идемпотентными: повтор с тем же id не
создаёт дубликат (безопасно ретраить по таймауту). `state` требует `fence` — номер
из ответа `claim`, защита от перезаписи чужой активной заявки.

## 6. Reader читает, но не пишет

Подключим тем же клиентом пакет читателя (замена требует `--replace`; прежний
креденшел сохраняется рядом как `.previous`):

```console
$ HOME=$H2 brain net join ~/reader.bundle
{ "error": { "code": "invalid_config", "message": "…/net/client already holds a joined endpoint;
  pass --replace to swap it (the old one is kept as .previous); nothing was written" }, … }

$ HOME=$H2 brain net join --replace ~/reader.bundle
{ … "previous_client_dir": { … }, "status": "ok" }

$ HOME=$H2 brain --space team net call board      # ок: reader может читать
{ … "principal": { "label": "", "machine": "htpc" }, "result": { "counts": {…} } }

$ HOME=$H2 brain --space team net call add inbox "Нельзя — только чтение"
{ "error": { "detail": { "error": "forbidden", … } }, "status": "refused" }   # код 4
```

## 7. Отзыв доступа

Одна учётка или вся машина целиком:

```console
$ HOME=$H1 brain net revoke --state-dir $H1/.brain/net --token-id t1793028034d4a908a5ceb366f64ead13ee
{ "effective": "next request; no cached replay is served",
  "revoked": { "token_id": "t1793028034d4a908a5ceb366f64ead13ee" }, "status": "ok" }

$ HOME=$H1 brain net revoke --state-dir $H1/.brain/net --machine htpc
{ "effective": "next request; no cached replay is served",
  "revoked": { "all_agents_and_credentials": true, "machine": "htpc" }, "status": "ok" }

$ HOME=$H2 brain net status
{ "error": { "detail": { "error": "unauthorized", "http_status": 401 }, … } }   # учётка отозвана
```

Список учёток (без секретов и хешей): `net principals --state-dir $H1/.brain/net`.

## 8. Ротация сертификатов

`--rotate` перезаписывает CA и серверный сертификат; сервер надо перезапустить,
а машины — перезачислить:

```console
$ HOME=$H1 brain net init --state-dir $H1/.brain/net --name localhost --ip 127.0.0.1 --rotate
{ "created": false, "rotated": true, … }

# перезапустить net serve, затем:
$ HOME=$H1 brain net enroll … --bundle-out $T/worker2.bundle   # новый пакет
$ HOME=$H2 brain net join --replace ~/worker2.bundle
{ … "dial": [ "127.0.0.1:36401" ], "status": "ok" }

$ HOME=$H2 brain --space team net call whoami
{ … "principal": { "agent": "claude", "machine": "laptop-two", "role": "worker", "spaces": ["team"] } }
```

---

## Частые ошибки

| Что видно | Почему | Что делать |
|---|---|---|
| `"category": "address_in_use"` при `net serve` | порт занят другим процессом (или уже висит второй координатор) | взять другой `--port` или остановить прежний сервер |
| `bundle dial list is empty; nothing was written` | в `net enroll` не передали `--dial` | указать адрес координатора: `--dial 192.0.2.10:7443` |
| `already holds a joined endpoint; pass --replace` | на клиенте уже есть подключение | `net join --replace …` (старый сохранится как `.previous`) |
| `the bundle's credential has expired; enroll the machine again` | пакет лежал дольше TTL учётки (по умолчанию 90 дней, `--ttl-secs`) | выдать новый пакет: `net enroll` |
| ошибка `tls_failure` при `net status`/`net call` | имя в `--server-name` не совпадает с SAN сертификата (проверено: `wrong.host`) | взять имя из `endpoint.json` или перевыпустить сертификат с нужным `--name` |
| `"error": "forbidden"` | роль не позволяет операцию (reader при записи) | worker для записи |
| `"error": "unauthorized"` (HTTP 401) | учётка отозвана (`net revoke`) или отключена машина | выдать новый пакет: `net enroll` |

## Что дальше

- Справка по любой команде: `brain net --help`, `brain net call --help`.
- Роли и их возможности: `worker` пишет и читает, `reader` только читает;
  перечень capability виден в `net status`.
- Локальная доска без сети — `brain work …` (та же база координации).
