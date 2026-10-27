# Team roles / Роли команды

Brain ED: `admin` (администратор координатора), `manager` (руководитель),
`worker` (работник). Ниже — регистрация за минуту **после настройки TLS и
доступности координатора**. Новый путь использует координатор как шлюз чтения;
ручной режим [team peer](team-mode-facts.md) продолжает работать.

## Русский: кто что может

| Роль | Возможности | Граница |
|---|---|---|
| `admin` | Создаёт приглашения, смотрит состав, отзывает участников | Локальный владелец каталога состояния координатора; удалённого admin-токена нет |
| `manager` | Читает срезы выбранных машин своего проекта через `team ask`, адаптер MCP и поверхности просмотра | Не публикует срезы и не пишет на машины работников; учётные данные источников не получает |
| `worker` | Публикует ограниченный срез своей машины после локального согласия | Не читает чужие срезы; локальное `team disable` немедленно закрывает доступ |

`manager` соответствует существующему principal `reader` и явному
`memory:read:team.PROJECT` grant. `worker` соответствует старому principal
`worker`, но приглашение **не выдаёт ему memory-read grants**. Проект команды
использует отдельный space `team.PROJECT`. Это граница опубликованных срезов,
не ACL личного архива. Архивы не открываются этими командами.

Новая схема auth.sqlite (v4) добавляет таблицы участников, приглашений и правил
адресации. Существующие `worker`/`reader`, токены, identity bindings, пространства
и grants сохраняются. `net enroll --role manager` — совместимый alias `reader`.
`team who` отображает старый `reader` как `manager` с `legacy: true`: это описание
прежних прав, не автоматическая выдача доступа к проекту или подключение к шлюзу.
Для нового онбординга используйте отдельное имя и приглашение. Одно имя принадлежит
одной машине/роли/проекту; занятое или отозванное имя автоматически не переиспользуется.

## Подготовка (администратор, один раз)

На координаторе, в одном терминале:

```sh
brain net init --state-dir "$BRAIN_HOME/net" --name localhost
brain net serve --state-dir "$BRAIN_HOME/net" --port 7443
```

В примерах `BRAIN_HOME` уже задан как абсолютный каталог выбранной установки.
Для обычной установки можно задать `BRAIN_HOME="$HOME/.brain"`; для проб используйте
новый временный каталог. Координатору не нужна локальная база личных воспоминаний.
Сервер по умолчанию слушает только loopback. Для разных машин предварительно
настройте разрешённую приватную сеть, сертификат с правильным SAN и явные opt-in
флаги; [политика TLS и адресов](network-transport.md) остаётся обязательной.

На работнике нужен доступный координатору `net serve` для чтения текущих срезов:

```sh
brain net init --state-dir "$BRAIN_HOME/net" --name localhost
brain net serve --state-dir "$BRAIN_HOME/net" --port 7444
```

У разных локальных тестовых установок должны быть разные `BRAIN_HOME` и порты.
На разных хостах замените `localhost` на SAN сертификата; при необходимости
передайте `--source-dial IP:PORT` при принятии. Автоматическое обнаружение адреса
проверяет pid, время старта и каталог состояния уже работающего `net serve`.
`accept` не устанавливает таймер и не запускает фоновую службу.

## Зарегистрировать руководителя за минуту

На координаторе, во втором терминале с тем же `BRAIN_HOME`:

```sh
brain team invite --role manager --name lead-anna --project checkout --expires 7d
```

Получится `lead-anna.invite.json` (0600). Передайте файл руководителю приватным
каналом. На его машине, с её собственным `BRAIN_HOME`:

```sh
brain team accept lead-anna.invite.json
brain team who
brain team ask 'Какие ошибки сейчас открыты?' --machines worker-1,worker-2,worker-3
```

`accept` выполняет join и регистрирует peer'ы проекта. Перед каждым `team ask`
список обновляется, поэтому новых работников можно приглашать после руководителя.
Выбираются только машины из `--machines`; неизвестные/недоступные источники
отмечаются явно. Если координатор недоступен или учётка отозвана, чтение завершается
ошибкой; старые факты не подставляются. Существующий stdio MCP `team_ask` использует
тот же путь и те же проверки; MCP доступен руководителю даже до первого работника.
Это не создание новой веб-панели (она относится к отдельной вехе M4).

## Зарегистрировать работника за минуту

На координаторе:

```sh
brain team invite --role worker --name worker-1 --project checkout
```

На работнике:

```sh
brain team accept worker-1.invite.json --root /absolute/project --publish
brain team who
```

Одна команда выполняет join, настройку проекта и получателя-координатора,
явно включает публикацию и создаёт первый срез. Публикуются только классы и поля
[ограниченного командного среза](team-mode-facts.md); путь проекта не отправляется.

Без `--publish` интерактивный терминал задаёт вопрос `[y/N]`; пустой ответ,
отказ или неинтерактивный stdin оставляют публикацию **выключенной**. Саму
регистрацию можно выполнить и до запуска локального сервера. Позже, запустив
`net serve` для того же состояния, явно включите и опубликуйте срез:

```sh
brain team enable
brain team publish
brain team disable
```

`publish` регистрирует актуальный endpoint на координаторе и требует явного
включения; после `disable` никакой удалённый grant не может снова включить машину.
Публикация остаётся ручной, пока владелец отдельно не настроит существующий таймер.

Для источника в приватной LAN администратор должен разрешить этот класс адресов
при выдаче приглашения (`--allow-lan-source`), а работник — при принятии тем же
флагом. Для CGNAT используются отдельные `--allow-cgnat-source` флаги. Для доступа
самого клиента к координатору применяются `accept --allow-lan-dial` или
`--allow-cgnat-dial`. По умолчанию все эти разрешения выключены. Поле адреса из
приглашения не расширяет политику адресов клиента автоматически.

## Состав, отзыв и секреты

```sh
brain team who
brain team remove lead-anna
brain team remove worker-1
```

`who` показывает имена, роли, проект, факт регистрации/отзыва, время последнего
запроса и свежесть последнего **наблюдавшегося** среза. До чтения среза свежесть
`unknown`; после 24 часов — `stale`. Это не мониторинг доступности в реальном времени.
Адреса, сертификаты, локальные пути и секреты в вывод не входят. Список ограничен
200 строками; ограничение указано в JSON.

`invite` и `remove` выполняются локальным администратором координатора. Отзыв
учётки прекращает следующие запросы руководителя, даже если peer'ы и ранее
полученные срезы остались на его машине. Отзыв работника закрывает его срезы
на шлюзе и запрещает последующую регистрацию endpoint. Уже полученные данные
удалённо стереть невозможно. Старые вручную настроенные прямые peer'ы — отдельный
путь: их доступ отзывается на соответствующем источнике.

Приглашение действует 7 дней по умолчанию, `--expires` принимает секунды и суффиксы
`s`, `m`, `h`, `d` (от 60 секунд до 90 дней). Срок проверяет координатор: изменение
JSON не продлевает его. При принятии в транзакции приглашение помечается
использованным, старый токен отзывается, а отдельный токен участника передаётся
только по TLS и сохраняется 0600. Копия приглашения повторно не работает.
Токен участника и токен чтения источника действуют до 90 дней; автоматического
продления нет. После истечения нужна явная повторная регистрация с новым именем.
При потере ответа во время принятия результат может быть неизвестен: не пытайтесь
возвращать приглашению силу редактированием файла; администратор отзывает учётку
и выдаёт новое приглашение с новым именем.

Не печатайте и не коммитьте файлы приглашений, `credential.pem`,
`team-source.credential` или каталог состояния. SQLite координатора содержит
секрет чтения каждого источника и защищён как приватное состояние (0600).
Новые команды отвергают существующую общедоступную базу/sidecar-файлы и каталог
состояния с правом записи для группы или остальных; права автоматически не меняются.
Приглашения-симлинки, общедоступные, специальные и слишком большие файлы отвергаются.
Для записи приглашения можно указать `--out FILE`; существующий файл не затирается.
`--state-dir DIR` выбирает локальный каталог команды; клиентские файлы размещаются
в `DIR/client`. Если сервер запущен с другим Brain home, используйте явный `--dial`
при выдаче и `--source-dial` при принятии.

## English: register a manager / worker in one minute

Prerequisite: a reachable TLS coordinator and a separate Brain home on each
machine. Run `net init` / `net serve` as shown above. A worker needs its own
reachable `net serve` before publishing. The commands do not install services.
Loopback is the default; private LAN and CGNAT require explicit opt-in on both
sides and a matching certificate SAN. This milestone has process evidence on
127.0.0.1 only. Invite onboarding currently uses direct TLS; the existing manual
peer/relay workflow remains available separately.

| Role | Authority |
|---|---|
| `admin` | Local owner of coordinator state; issues invitations and revokes accounts; no remotely issued admin credential |
| `manager` | Existing read-only `reader` principal plus an explicit project memory-read grant; reads selected workers through the gateway, CLI and stdio MCP |
| `worker` | Existing worker principal without grants to read other slices; publishes its own bounded projection with local consent |

On the coordinator:

```sh
brain team invite --role manager --name lead-anna --project checkout --expires 7d
brain team invite --role worker --name worker-1 --project checkout
```

Privately transfer each 0600 invitation to its intended machine. On the manager:

```sh
brain team accept lead-anna.invite.json
brain team who
brain team ask 'Which errors are open?' --machines worker-1
```

On the worker:

```sh
brain team accept worker-1.invite.json --root /absolute/project --publish
```

Without `--publish`, an interactive terminal asks `[y/N]`. Noninteractive use,
no response or a negative response leaves publication OFF. Registration itself
can happen before starting the worker's server; later explicitly run
`team enable` and `team publish`. `team disable` immediately blocks old slices.
The coordinator fetches every page from the worker, never an offline cached
answer. Managers never receive worker credentials or write to workers.

The manager's project peer list refreshes before every query, including workers
registered later. The same flow powers the existing `team_ask` MCP adapter.
A web dashboard is a separate milestone. `team who` shows safe membership and
observed freshness, without endpoints or secrets; unknown means no observed
slice, stale means older than 24 hours. The bounded roster contains at most 200 rows.

On the coordinator, revoke with:

```sh
brain team remove lead-anna
brain team remove worker-1
```

Revocation takes effect on the next request, including cached peer configurations.
Previously received data cannot be erased remotely. Legacy direct peer access
must still be revoked at its source. An invitation is redeemed once on the server;
the invitation token is revoked and exchanged for a separate 90-day member token
in a transaction. Source read tokens also expire after 90 days; there is no
automatic renewal. Invitation expiration is server-enforced (default 7 days,
60 seconds to 90 days), even if its JSON is edited. If redemption loses its
response, ask the local admin to revoke the account and invite a new name.

Auth schema v4 preserves legacy credentials, identity bindings, spaces and grants;
`reader` remains accepted and is displayed as legacy manager by `team who`.
This does not automatically join old accounts to a project or grant new rights.
`net enroll --role manager` is a compatible reader alias. One new team account
belongs to one name, role and project. Existing/revoked names are not silently reused.
Project scopes apply to shared projections, never to private archives.

`invite --out FILE` writes a new file without overwriting; it never prints the
secret. Symlinks, public files, special files and oversized invitation files are
rejected. Keep invitations, client credentials, source credentials and coordinator
state out of source control. The coordinator's private auth database holds source
read credentials. Use `--state-dir DIR` for isolated installations (`DIR/client`
holds joined credentials). Use explicit `--dial` / `--source-dial` if the corresponding
server belongs to another Brain home. There is no formal security or fleet readiness
claim; signatures and scoped signed tokens belong to M3.

Synthetic reproducible demonstration:

```sh
tools/team-demo.sh --machines 3
```

The default demo uses invitations for a separate admin, one manager and three
workers, temporary homes, TLS on 127.0.0.1, no models and no external APIs. The
`--via-relay` compatibility demo exercises the older explicit enroll/join/peer path.
