# Team security / Безопасность командных срезов (0.3, M3)

## Русский

Командный режим распространяет только явно опубликованные ограниченные срезы.
Личные архивы остаются локальными. Подпись подтверждает владение ключом машины
и целостность среза; она не доказывает истинность факта, авторство человека,
полноту истории или безопасность содержимого. Факты остаются недоверенными данными.

### Ключ и подписанный объект

`team accept` и `team configure` создают `team-ed25519.key` рядом с командным
состоянием. Это 32 случайных байта seed Ed25519, файл 0600, без вывода секрета.
Чтение отвергает symlink, специальный файл, другого владельца, неверную длину и
права. Используется уже закреплённый в Cargo.lock `ring 0.17.14`
(`Apache-2.0 AND ISC`), без новых транзитивных крейтов.

При принятии приглашения координатор закрепляет публичный ключ за
аутентифицированным именем машины. Руководитель получает ключи работников из
TLS-защищённого каталога координатора перед каждым запросом. Для ручного прямого
`team peer` нужен явный `--public-key HEX`; ключ надо передать доверенным каналом.
`team manifest` показывает только публичный ключ (`signing_public_key`).

`brain-team-slice/1` имеет `integrity=tls+blake3+ed25519`. `signature` содержит
`PUBLIC_KEY_HEX:SIGNATURE_HEX`. `snapshot_id` — BLAKE3 от JSON с очищенным
`snapshot_id` и `signature=null`: объект сериализован через `serde_json::Value`
с лексикографически упорядоченными ключами, без пробелов. Подписаны UTF-8 байты
`brain-team-slice-signature/1:SNAPSHOT_ID`. Таким образом подпись связывает
схему, машину, origin, проект, space, ревизию политики, время, диапазон событий,
watermarks, coverage и упорядоченные digest всех сущностей. Digest каждой сущности
связывает её полное разрешённое содержимое и provenance.

Источник проверяет локальное согласие, текущую политику, полный срез и свой
текущий ключ перед каждой страницей. Координатор читает и проверяет **весь**
ограниченный срез до выдачи первой страницы. `team ask` независимо проверяет
подпись по закреплённому ключу, digest, последовательность и границы среза;
проверки повторяются при чтении реплики и истории. Неизвестный ключ, чужая или
отсутствующая подпись, unsigned-срез и подмена дают отказ; при ошибке подписи
пользователь видит «подпись не прошла». Кэш не подставляется вместо отказавшего
источника. Подмена локальной реплики тоже закрывает ответ этой машины.

### Права руководителя

На координаторе:

```sh
brain team invite --role manager --name lead-anna --projects a,b --expires 30d
```

`--project a` остаётся совместимым способом выбрать один проект. Работник
принадлежит одному проекту; руководитель получает от 1 до 16 явных проектов.
`--expires` ограничивает **и приглашение, и права**, отсчитывая время от выдачи
приглашения, а не от принятия. Допустимы 60 секунд — 90 дней.

Токен формата 1 — `HEX(JSON).HEX(ED25519_SIGNATURE)`. Подписаны точные байты JSON:
`version`, `issuer` (ключ координатора), `subject` (имя руководителя),
`credential_id`, `projects`, `permission=read`, `issued_at`, `expires_at`.
Версия, подпись, издатель, разрешение, список проектов и время проверяются;
`now >= expires_at` означает отказ. Координатор дополнительно связывает токен с
текущими именем и credential id, проверяет live credential, grants и отзыв
работника при каждом запросе. Редактирование приглашения/записи токена не
расширяет подписанные права.

Координатор хранит токен в приватной auth.sqlite и пересылает его источнику
только внутри TLS-запроса. У источника `team-authority.pub` закрепляет ключ
координатора; источник независимо проверяет подпись, чтение, проект своего
`team.PROJECT` и срок. Без токена или trust anchor зарегистрированный источник
закрыт. Учётные данные источника остаются только у координатора; руководитель
не получает прямой доступ к работникам. Ручные прямые peers используют отдельные
явные локальные grants и ключи; они не получают права координатора автоматически.

Просроченный токен или чужой проект дают отказ с понятной ошибкой. При недоступном
координаторе правдоподобный старый ответ не выдаётся. Изменение локальных часов
может повлиять на срок; доверие к часам хоста входит в модель угроз.

### Ротация, отзыв, компрометация

На зарегистрированной машине:

```sh
brain team rekey
brain team publish
```

Новый ключ сначала сохраняется 0600 как `team-ed25519.pending`. Запрос ротации
аутентифицирован существующим credential и подписан старым ключом; подпись
связывает версию операции, имя машины, старый и новый ключ. Координатор атомарно
заменяет pin, увеличивает epoch и записывает старый/новый публичные ключи и время
в `team_key_audit`. Повтор точного последнего перехода допустим для восстановления
после потерянного ответа. Затем машина атомарно продвигает pending-ключ и закрывает
старое поколение. Нужна новая публикация. История со старым ключом не выдаётся,
даже если старый срез криптографически корректен. `team ask` обновляет pins и
закрывает старые локальные реплики при изменении ключа.

`team remove NAME` на координаторе прекращает следующие запросы этой учётной
записи и выдачу её срезов. Локальное `team disable` независимо закрывает источник.
Уже полученные данные удалённо стереть невозможно. Автоматического отзыва
отдельного права на источнике без координатора нет: доступ к источнику требует
его отдельный gateway credential, а подписанный токен ограничен сроком.

При краже **ключа работника** злоумышленник может подписывать ложные срезы под его
именем, если также контролирует публикацию/endpoint. Администратор отзывает машину;
после очистки хоста нужна новая регистрация с новым именем. Обычная ротация требует
действующий старый ключ и credential, поэтому утраченный ключ нельзя обойти
самостоятельным `rekey`. При компрометации **координатора** доверие к выдаваемым
правам, каталогу и pin-ам потеряно: восстановите доверенный координатор и заново
выдайте приглашения/закрепите его ключ на источниках. Автоматической ротации ключа
издателя и внешней PKI-привязки нет. Компрометация ОС/локального владельца позволяет
заменить программу и доверенное состояние; файловые права от этого не защищают.

Auth schema v5 сохраняет старые principals/credentials/grants; старые командные
участники без ключей и подписанных прав не получают silent downgrade. Для них
нужна явная новая регистрация; ручному источнику — configure и новая публикация,
а прямому peer — доверенный pin. Это ограниченная реализация протокола и тесты атак,
не формальное доказательство, не ACL личного архива и не заявление о готовности
к корпоративному развёртыванию.

### Воспроизведение проверок

Из корня рабочей копии (тесты используют синтетические данные и временные HOME):

```sh
export CARGO_TARGET_DIR="$HOME/projects/brain-ed-target/CX-M3"
export CARGO_BUILD_JOBS=4
nice -n 10 cargo build --locked --workspace --all-features
nice -n 10 cargo test --locked --workspace --all-features
BRAIN_PROBE_BIN="$CARGO_TARGET_DIR/debug/brain" \
  nice -n 10 python3 -B -m unittest -v tests.test_team_security
nice -n 10 python3 -B tools/check-team-security-mutations.py
```

Мутационный прогон копирует исходники во временный каталог и по очереди отключает
шесть защит: проверку подписи изменённой реплики, pin чужого ключа, pin после
ротации, проект, срок и отзыв машины. Каждый соответствующий тест обязан упасть
с assertion failure; ошибка компиляции не засчитывается. Рабочая копия не меняется.
Отдельно проверяются отсутствие подписи, отказ старой истории и точный повтор
последней ротации без дублирования журнала. Это проверки на Linux и loopback TLS
с временными процессами, а не свидетельство работы на реальном стенде, другой ОС
или при всех возможных сетевых сбоях. Подпись не скрывает содержимое среза от
получателя; конфиденциальность в пути обеспечивается TLS.

## English

Team mode shares only explicitly published bounded projections. Private archives
remain local. An Ed25519 signature authenticates a machine key and snapshot bytes;
it does not prove factual truth, human authorship, complete history or safe text.
Treat every shared fact as untrusted data.

`team accept` / `team configure` create a 32-byte Ed25519 seed in
`team-ed25519.key`, mode 0600, beside team state. Secret bytes are never printed.
Reads refuse symlinks, special files, wrong owner, length or permissions. The
backend is the already locked `ring 0.17.14` (Apache-2.0 AND ISC); no new transitive
crates are added. Acceptance pins the public key to the authenticated machine.
Managers refresh worker pins through the coordinator's TLS directory. Manual
`team peer` requires an explicit trusted `--public-key HEX`.

A signed `brain-team-slice/1` uses `integrity=tls+blake3+ed25519` and
`signature=PUBLIC_KEY_HEX:SIGNATURE_HEX`. Its BLAKE3 snapshot id covers compact,
lexicographically keyed JSON (`serde_json::Value`) with `snapshot_id` cleared and
`signature=null`. Ed25519 signs the UTF-8 string
`brain-team-slice-signature/1:SNAPSHOT_ID`. This binds machine, origin, project,
space, policy revision, timestamps, sequence range, coverage, watermarks and all
entity digests; each entity digest binds its complete allowed content/provenance.
The source checks its current policy/key, the coordinator verifies a complete
bounded generation before serving its first page, and `team ask` independently
checks the pinned key, all facts and sequence boundaries. Replica/history reads
repeat verification. Missing/invalid/foreign signatures and unsigned snapshots
fail closed with «подпись не прошла» (signature failed), without cached facts.

`team invite --role manager --name lead-anna --projects a,b --expires 30d`
issues read scope for 1..16 explicit projects; `--project a` remains supported.
Workers have one project. The expiry limits both redemption and rights from the
invitation's issue time (60 seconds through 90 days). The version-1 capability is
`HEX(JSON).HEX(ED25519_SIGNATURE)`, signing exact JSON bytes containing version,
issuer key, subject, credential id, projects, read-only permission, issue time and
expiry. The coordinator verifies signature, identity/credential binding, grants,
project, clock and live membership. It privately stores the capability and forwards
it over TLS; the source independently verifies issuer against `team-authority.pub`,
signature, read permission, its own project and expiry. `now >= expires_at` denies
access. A joined source without a token/trust anchor refuses access. Source
credentials stay at the coordinator. Manual direct peers retain separate explicit
host-local grants and pins. Host clocks are part of the trust model.

`team rekey`, followed by `team publish`, performs authenticated rotation with a
proof signed by the old key over operation version, machine name and both keys.
A private pending key supports retries after uncertain responses. The coordinator
atomically changes the pin/epoch and journals public keys and time in
`team_key_audit`; only the exact last transition can be retried. Old-key snapshots
and history are refused after rotation, including valid old signatures. A missing
old key requires administrator revocation and a new registration/name.

`team remove NAME` takes effect on subsequent coordinator requests; local
`team disable` independently closes the source. Received data cannot be remotely
erased. A stolen worker key can sign false facts if the attacker also controls
publication/the endpoint; revoke the machine and restore the host before enrolling
again. Coordinator compromise breaks trust in capabilities, directory and pins:
restore it and re-enrol/re-pin sources. Issuer-key rotation is not automated.
Direct source access still requires the separate gateway credential; a source does
not independently poll coordinator revocations. OS/owner compromise can replace
both software and trust state. Auth schema v5 preserves legacy transport accounts
but does not silently grant unsigned legacy team access: explicit re-enrolment or
manual configure/publish/pin is required. These are bounded protocol/test claims,
not a formal proof, private archive ACLs or enterprise readiness.

To reproduce the checks, run the commands in the Russian section from the
checkout root. They build an isolated binary and use synthetic data and temporary
homes. The mutation runner copies sources into a temporary directory, disables
each of the six protections in turn and requires its attack test to fail an
assertion; compilation failures do not count. It never patches the checkout.
Additional tests cover missing signatures, replay into snapshot history and exact
last-transition retries without duplicate rotation audit rows. Linux and loopback
TLS results do not establish live multi-machine, other-OS or exhaustive network
failure coverage. Signing does not hide snapshot contents from recipients; TLS
provides transport confidentiality.

See [roles](team-roles.md), [snapshot contract](team-mode-facts.md) and
[ADR 0002](adr/0002-team-facts.md).
