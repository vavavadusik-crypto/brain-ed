# Work map — M1 core (`brain-core::map`)

Status: M1–M3a core, M3b CLI + stdio MCP, M3c read-only project map UI (`/map`) with a genuine 3D view; actual tests: brain-core lib 149, brain-cli 134 unit + 4 map process tests, tests/test_map3d.js. User confirmation is **not** implemented. The goal is **not** complete.

## Files and versions

* `$BRAIN_HOME/map/map.sqlite`, separate from `brain.sqlite`, the archive and `coord.sqlite` (none are opened or changed).
* `application_id = 0x42524E4D` ("BRNM"), `PRAGMA user_version = 1`, `meta.format = brain-map/1`, `meta.machine_id`,
  `meta.relations = brain-relations/1`, `meta.ingest = brain-ingest/1` (tables `ingest_cursors`, `ingest_seen`, `archive_installs`). Any other app id, version, format, relation policy or machine id → `incompatible`
  (no guessing, no migration). brain-map/1 is unreleased; the relation policy was added to it before any real map file exists.
* Directory `0700`, file `0600`, owned by the current uid (`/proc/self`), not a symlink (file or directory); otherwise
  refused. The connection opens with `SQLITE_OPEN_NOFOLLOW` and without CREATE, so a symlink swapped in after the check is
  still refused. `-wal`/`-shm` are set to `0600`.

## Ledger `brain-event/1`

* `events` is append-only: `BEFORE UPDATE` / `BEFORE DELETE` triggers `RAISE(ABORT)`.
* `event_id = <machine>:<seq 12 digits>`; `seq` from `meta.next_seq`, monotonic, independent of time.
* `relation` is an explicit field, required for `link_*` and `correction`, and must be in the relation policy below.
  Slot = `(space, subject, relation)`.
* Content hash: **BLAKE3** (existing dependency; the design said sha256, no sha2 crate is present) over explicit
  canonical bytes: `brain-event/1\n` then `name=<byte-len>:<value>\n` for every field in fixed order, including
  `authority`, `recorded_at`, each evidence ref and the canonical payload. `received_at`/`local_seq` are local and not hashed.
* Limits (bytes, refused not truncated, so UTF-8 is never split): space ≤64 ASCII `[A-Za-z0-9._-]`; refs ≤256 with
  grammar `<prefix>:<scope>/<id>`; client/agent ≤64; ≤16 evidence refs; payload ≤4096; no control characters.
* Payload: JSON object with allowlisted scalar keys only (`note`≤500, `reason`≤200, `command`≤500, `output_sha256`≤64,
  `exit_code`/`duration_ms`/`count` integers); stored re-serialised with sorted keys. No field can carry archive text.
* Confidence in thousandths, refused above the basis ceiling (user_confirmed 1000 … time_overlap_heuristic 300).
* All SQL uses bound parameters; the only formatted SQL fragments are compile-time constants.

## Relation cardinality policy `brain-relations/1`

A fixed, built-in table in code (`events::RELATIONS`); no event field, agent, replayed record or actor can add a
relation or change its cardinality. A relation outside the table is `invalid_input` on append **and** replay.

| relation | cardinality | object prefixes |
|---|---|---|
| `has_session` | multi | `archive-session` |
| `has_decision` | multi | `memory`, `archive`, `coord-event` |
| `has_error` | multi | `archive`, `coord-event`, `memory` |
| `declared_root` | multi | `project` |
| `depends_on` | multi | `task` |
| `current_status` | single | `status` |
| `outcome` | single | `outcome` |

The relation name is hashed in every event and the policy version is pinned in `meta`, so a replay on any machine
reads the same cardinality. Changing the table means a new policy version and a refused (not reinterpreted) old file.

## Authority (caller responsibility)

`Authority` is a typed argument to `append`, separate from the event's `asserted_kind`/`basis`.
`link_confirmed`, `link_rejected`, `basis=user_confirmed` and `asserted_kind=user` require `Authority::LocalUser`;
otherwise `forbidden`. Event text cannot grant it. `LocalUserGrant::from_direct_local_action()` is a **contract, not
proof of identity**: only code where a human at this machine acted directly (CLI prompt, UI POST after preview) may build it.
MCP tools and any agent input must pass `Authority::Unverified`; they must never construct a grant. The library cannot
tell who typed a command and a static constructor does not make a caller human: the trusted host owns that guarantee. Replayed events keep their origin's authority, but a
confirmation from another machine counts here only as a proposal; a remote rejection has no effect.

## Replay

`replay(event)`: version must be 1; hash recomputed; draft validity, relation policy and authority are checked **before**
the idempotency lookup, so an identical copy of an invalid record is refused, not called a duplicate; identical id+hash → `Duplicate` (no-op); same id other content →
`conflict`; per-origin seq must be exactly next (no gaps); events of the local machine not already held are refused.

## Corrections

A correction names an event that is held, in the same space/subject/relation, by the same actor
(origin machine + asserted_kind + client + agent), not already corrected (chains stay linear: correct the head).
Empty object = retraction. Correcting a chain rooted in a user confirmation needs local authority.

## Projection `links` (one row per slot × object)

Pure function of the event set in `(origin_machine, origin_seq)` order: arrival order and clocks never pick a winner.
* **Multi-valued**: every object is its own association. Different objects from different origins are never a conflict.
  Per object: `conflict` (a local user both confirmed and rejected this same object, both live; `contested=true`) ·
  `rejected` · `confirmed` (local user; other objects are untouched, never `overridden`) · `proposed` · `superseded`.
  The dispute ends only when the user corrects one of their own events (needs `LocalUser`).
* **Single-valued**: `contested` = two different actors ever asserted different non-rejected values (superseded
  assertions included) or the slot holds several live values. A correction therefore cannot silently clear a conflict;
  only a local user resolves. status: `confirmed` (single local user confirmation) · `overridden` (other values once
  resolved) · `rejected` · `conflict` · `superseded` · `proposed`. All origins and events stay on every row.
* Same object from several actors is agreement (`proposed`, all actors listed), not conflict.
* Appends re-project only their slot (index `events_slot`); no full-space scan per append.
* Rows keep `actors`, `current_events`, `all_events`; nothing is reduced to one head event.
* `rebuild()` drops and recomputes; `links_dump()` is compared byte-for-byte in tests.
* `links_as_of(space, T)` projects events with local `received_at ≤ T` (what this Brain knew at T).
* Paging: `events_page(space, cursor, limit)` (cursor `local_seq`, space-scoped), `links_page(space, …)` (keyset
  cursor), `links_as_of(space, …)` (offset); limit 1..=100. Every caller-facing read takes an explicit space.
  `trusted_export_page` is the only cross-space read, for replication between the user's own machines; never expose it
  to agents, MCP or UI. Projections refuse above 100 000 events in scope (`conflict`).

## Concurrency

Every append/replay is one `BEGIN IMMEDIATE` transaction holding sequence read, event insert, `next_seq` update and
slot projection; SQLite `busy_timeout` 5 s serialises writers, so concurrent `MapStore`s on one file never fork a seq.

## Source policy for free text

The payload allowlist keeps structure scalar, but `note`, `reason` and `command` are free strings a caller writes. The
map does **not** guarantee archive text cannot be typed or pasted into them. Policy: no code path fills them from the
archive automatically, no excerpt is copied by default, and the data stays local (no publication in M1).

## M2 ingestion (`map/ingest.rs`, library API only)

Sources are **explicit paths** given by the trusted host; the map never discovers or opens a live database on its own.

**Source safety (both sources).** Absolute, normalised UTF-8 path ≤1024 B; parent directory and file not symlinks,
owned by this uid, no group/other bits; `-wal`/`-shm`/`-journal` side files, if present, the same. Opened
`READ_ONLY | NOFOLLOW`, `query_only`, busy wait ≤500 ms, device/inode re-checked after open. All reads of a batch run
in one read transaction (snapshot). Missing → `not_found`; symlink, permissions, foreign `application_id`, other
`user_version`/schema marker, missing table or any column-layout difference (exact names, types, order) →
`incompatible`, before anything is written.

* Coordinator: `application_id 0x42524E43`, `user_version 1`, all six tables, exact `events` columns.
* Archive: `application_id 0`, `user_version 0`, `archive_meta.schema = brain_session_archive:3`, exact `sources`
  and `entries` columns (schema 2 without `cwd` is refused). Active `quarantine` rows are not read.

**Cursors.** Keyed by `(source kind, device:inode identity, space)`, not by path. A different file at a known path →
`conflict` until `reset_ingest_cursor(kind, path, space, LocalUserGrant)`; the grant is the trusted host's
explicit action, never agent input. Each cursor stores the last source row id and a BLAKE3 of that row's selected
fields; if that row is gone or differs, ingestion stops with `conflict`. **Limit:** in-place rewrites of rows strictly
before the cursor row are not detected; they need a manual reset and re-check. `ingest_seen` keeps content keys,
so re-reading identical rows after a reset appends nothing.

**Archive install identity.** Each archive is given as `ArchiveSource { install, path }`; `install`
(`[a-z0-9][a-z0-9._-]{0,63}`) is the trusted host's durable, configured installation id; device/inode is only the
local replacement check (cursor identity `install@dev:ino`). `archive_installs` binds one install to one path and
one path to one install (a mismatch is `conflict`). Evidence refs are `archive:<install>/<entry>`, so two archives with
the same client and entry ids give distinct refs. `resolve_archive_ref(ref, trusted)` returns the host's path for that
install only if it equals the bound path; the ref never supplies a path. The id says which archive, not who wrote an
entry. `archive-session:<client>/<session>` objects stay client-scoped (a session id is the client's).

**Budget.** `IngestBudget { max_runtime (default 10 s), max_vm_steps }`, enforced by SQLite's progress handler
(rusqlite `hooks` feature, no new crate) every 64 VM steps on the source **and** the map connection, so prepare,
query, collect, root lookup, appends and projection are all interruptible; the handler interrupts once so the
rollback runs. Result `unavailable`, nothing from the batch kept, cursor unchanged. This bounds SQL CPU work
cooperatively; it is not a hard real-time or I/O guarantee (blocked reads and the ≤500 ms busy wait are not cut).

**Atomicity.** One map `BEGIN IMMEDIATE` holds every appended event, its seen-key and the cursor. Any error (a row
over bounds, the 10 s batch time budget, storage) keeps nothing from that batch; the cursor waits at the bad row
(fail closed, not skipped), and the budget interrupting any statement keeps nothing. Batches are 1..=100 rows; per-space cursors mean reading space A never advances or loses B.

**Coordinator → timeline.** `ingest_coordinator(path, space, limit)` reads only `id, task_id, claim_id, kind, agent,
machine, fence, at` (bounded with `substr`; never `note` or `data`). kind/agent/machine must be ≤32/64/64 bytes,
printable, kind `[a-z_]`. Each row becomes one event, `basis coordinator_record` (900), `asserted_kind coordinator`,
`authority unverified`, evidence `coord-event:<space>/<id>`, `occurred_at` = source clock (`coordinator_clock`),
payload `coord_kind, coord_id, claim_id, fence, actor_machine, source_at`:
* `completed` → `outcome_claimed`, object `outcome:<space>/claimed_completed`; `failed` → `claimed_failed`;
  `lease_expired` (only when the coordinator recorded it) → `unknown_lease_expired`.
* everything else → `coord_recorded`. Nothing is ever `outcome_observed`; no task's mutable current state is read.
`timeline(space, task?, after_local_seq, limit ≤100, as_of_recorded?)` is a query over the ledger itself (receipt
order, `received_at ≤ T` for as-of), so `rebuild` and replay on another machine give the same history.
Source clock (`source_at`) and map clocks (`recorded_at`, `received_at`) are kept apart.

**Archive metadata (opt-in).** `ingest_archive_metadata(path, space, include_archive_metadata, limit)` does nothing
(and opens nothing) unless the flag is true. It reads `entries.id, sources.client, session_id, cwd, record_sha256`,
never `content`. Roots come only from live (`proposed`/`confirmed`) `declared_root` links with at least one evidence
ref, object `project:<scope>/<absolute path>`. A cwd matches a root by `Path::starts_with` (path **components**:
`/a/project` is not under `/a/proj`); relative cwds or ones with `.`/`..` never match. Each (root project, session)
gets one `link_proposed has_session archive-session:<client>/<session>`, `basis cwd_evidence` (600),
`asserted_kind collector`, evidence `archive:<client>/<first entry id>` plus the root's evidence, payload
`count` (matching entries in that batch) and `candidates` (roots the cwd matched; >1 = ambiguous, all kept, none
chosen). Never confirmed. cwd strings and message text are not written to the map. Oversized/odd client or session
ids are skipped and counted `unmatched`. `time_overlap_heuristic` is not implemented in this slice.
Roots declared after entries were read do not re-match them unless the cursor is reset (seen-keys prevent duplicates).

## M3a: read-only access and context packet (`map/packet.rs`, library API only)

**Read-only.** `MapStore::open_read_only(path)` and `map_status(path)` never create a directory, file or schema,
never migrate, rebuild or chmod. Missing → `not_found` / `MapStatus::NotConfigured` (a later context may fall back to
the coordinator, labelled so). The file, its directory and existing `-wal`/`-shm` are checked like `open`
(owner, exact mode, not a symlink), opened `READ_ONLY | NOFOLLOW`, `query_only`; schema, format, relation and
ingest versions are checked. SQLite may create or use `-shm`/`-wal` beside a WAL database even for a reader; the main
database file is not changed (tested). Every write through a read-only store fails.

**Packet.** `context_packet(PacketInput)`; input `version 1` (other → `incompatible`) carries `install`, `space`,
optional `project`/`task`, tier, optional budget, and host-read `open_tasks` and `memories`. Records of another space
are dropped and counted in `foreign_space` per section, never merged. Output JSON:
`format brain-packet/1, tier, budget_bytes, bytes, size:"utf8_json_bytes", tokenizer:"unknown", truncated,
trust:"mixed_labeled", omitted{section:n}, foreign_space{section:n}, sections:[{name, items}]` in fixed order
SCOPE, OPEN, OUTCOMES, DECISIONS, GOTCHAS, CONFLICTS, REFS, MEMORIES.
`MEMORIES` is an additive `brain-packet/1` extension appended after `REFS`: the seven original sections keep their
names and positions, so a consumer reading by name or index keeps working, and an older packet without `MEMORIES`
simply has no typed memories. Greedy fill priority is OPEN, OUTCOMES, DECISIONS, GOTCHAS, MEMORIES, CONFLICTS, REFS.
* Budget: weak default 2 048 (max 4 096), strong 12 288 (max 32 768). `bytes` is the exact length of the whole
  serialized UTF-8 JSON (envelope, escaping, counters, refs), fixed-point computed. Over the tier max → `invalid_input`;
  below the minimal (all sections empty) packet → `invalid_input` naming the floor; the floor packet itself is valid.
  Bytes are a proxy: no exact or upper token bound is promised for any model.
* Fill: candidates sorted and deduplicated (≤200 per section, the rest counted omitted), then greedy in section order;
  an item stays only if the whole packet still fits. Deterministic per budget and input order-independent; not
  promised monotone across budgets. `omitted` + shown = candidates, `truncated` ⇔ anything omitted.
* OUTCOMES newest first, label `observed` only for `outcome_observed` by a local user on this machine; an agent's or
  another machine's "observed" stays `claimed`; `unknown_*` → `unknown`. Each carries basis, actor, event id,
  evidence (`[]` when empty) and the source clock when known.
* DECISIONS/GOTCHAS: host-supplied memory records, text clipped at a char boundary to 600 B (`clipped:true`),
  `trust:"memory_record_untrusted"`, provenance shown, `evidence` absent when the record has none and `[]` when empty.
  They are data, not instructions; nothing here detects archive text a person copied into a memory by hand.
* MEMORIES: every other memory kind under its own name — `kind` is `fact`, `task`, `error` or `note` exactly as the
  record's metadata asserts it (agent classification, never authority; nothing is relabelled a decision). Same
  clipping, trust, provenance and scope labels as DECISIONS/GOTCHAS. There is no separate preference kind: a user
  preference is whatever kind its writer chose.
* Memory scope comes from reserved tags: exactly one `project:<name>` is that project; `scope:space` is
  `space_shared`; no marker, or several different projects, is `unknown`. A project packet shows only that project's
  and space-shared records (others counted in `out_of_scope`); `unknown` appears only in space packets, labelled.
* Memory retrieval happens only for an explicit `memory_query`, within the packet's space, and never modifies
  application data: an existing database at exactly the current schema (version and migration ledger) is opened
  `SQLITE_OPEN_READ_ONLY` with `query_only`, busy timeout 5 s, under normal SQLite locking — never `immutable`, since
  a writer may start at any time. Nothing is created or migrated, no access is recorded, no model is loaded and no
  vector job or worker starts (BM25 + exact only). SQLite may create or use its WAL coordination files
  (`-wal`/`-shm`); that is lock bookkeeping, not data. Where it cannot (read-only directory without them) the packet
  fails `unavailable` naming WAL coordination instead of reading without locks. Each SQL statement reads one
  consistent snapshot; a search's several statements are not wrapped in one read transaction (the long-lived storage
  actor holds none, so checkpoints are never blocked). A frozen `immutable` read would need a separate, explicit
  trusted-snapshot contract. A missing database yields no memories and stays missing.
* For a project packet the host keeps only that project's and `scope:space` hits before its 20-record limit, so
  other projects cannot crowd a matching record out (unknown/other-project hits are then not counted in
  `out_of_scope`). The search's own candidate pool (per-signal candidates, 200 fused) still bounds what is seen.
* CONFLICTS: only rows the projection marks `conflict` (single-valued disputes or a user's confirm+reject of one
  object); two sessions of a task are not a conflict. Open-task titles are clipped to 200 B, labelled `agent_supplied`.
* REFS: sorted, deduplicated refs (tasks, outcome evidence, memories and their evidence, conflict subjects/objects,
  the scoped task's or project's live link objects and evidence); archive content only as `archive:<install>/<entry>`.
* No LLM, no archive text, no hidden reasoning; the map never stores any.

**Project scope (M3b fix).** A packet with `project` shows only that project:
* OPEN: tasks whose coordinator record has exactly that project.
* OUTCOMES (no task given): only tasks the coordinator recorded under that project (`TaskProject`, basis
  `coordinator_record`, shown as `project_basis`); a task under two projects is **ambiguous**: listed in
  `SCOPE.ambiguous_tasks`, its outcomes dropped, never assumed.
* CONFLICTS: rows on the project ref or on its tasks. Memories: `MemoryScope::Project(p)` of that project or
  `MemoryScope::Space` (declared shared, labelled `space_shared`); unknown or other-project memories are dropped.
* Dropped same-space records are counted in `out_of_scope{section}`; other-space input in `foreign_space{section}`.
* Without `project` the packet is a whole-space packet, `SCOPE.kind = "space"`, each memory labelled with its scope.
  With a task, `SCOPE.task_project.state` is `in_project` / `other_project` / `known` / `ambiguous` / `unknown`.
* SCOPE is part of the minimal packet and is never dropped; a budget too small for it is `invalid_input`.
* `context_packet_unmapped` (no map file): `map: not_configured`, `SOURCE coordinator_only+memory`, OUTCOMES and
  CONFLICTS empty because there is no map, not because nothing happened.

## Команды (M3b): `brain map …` и MCP `map_*`

Пути, идентификатор установки и полномочия задаёт хост: карта — `$BRAIN_HOME/map/map.sqlite`, координатор — его обычный
файл, установка — id этой установки. Ни флаг CLI, ни аргумент MCP не принимает путь, базу, install или «доверенного
пользователя»; лишние аргументы MCP отклоняются (`invalid_input`). Подтверждать, отклонять и «наблюдать» здесь нельзя:
это будущее действие пользователя в UI, а не параметр модели. Всё, что записывается, — `unverified`.

```sh
brain map status                       # только чтение: not_configured или ready со счётчиками; ничего не создаёт
brain map init                         # единственная команда, создающая карту (0700/0600)
brain --space default map ingest-coordinator --limit 100   # следующий пакет событий координатора
brain map timeline --task 12           # claimed_completed / claimed_failed / unknown_lease_expired
brain map links --limit 50             # первая страница связей
brain map packet --project brain --tier strong --budget 8000 --memory-query "решения по SQLite"
```

* `status`, `timeline`, `links`, `packet` не создают ни карту, ни каталог, ни `machine-id`. Нет карты → `timeline`/`links`/
  `ingest-coordinator` отвечают `not_configured`; `packet` строится как `coordinator_only` и так и помечен.
  Повреждённая или чужая карта — ошибка, без тихого отката.
* `packet` печатает сам пакет JSON без обёртки и **без** перевода строки: длина stdout в байтах ≤ бюджета (тест сравнивает
  точную длину stdout).
* `links --limit N [--subject REF] [--cursor C]`: постранично; `next_cursor` из ответа передаётся как `--cursor`, пока не
  станет `null`, — так достижима каждая связь.
* `--memory-query` — явный поиск по графовой памяти этого space. **Ограничение:** read-only пути поиска у основного
  хранилища нет, поэтому с этим флагом база памяти открывается обычным хранилищем (чтение-запись, возможна миграция); без
  флага база памяти не открывается вовсе, а UI поиск по памяти не использует; берутся только decision/gotcha, не больше 10, текст
  обрезается до 600 байт. Проект памяти берётся из тегов `project:<имя>` или `scope:space`; иначе «unknown» и в проектный
  пакет не попадает. Текст архива сюда не попадает никогда; память, в которую человек вручную вставил текст архива,
  распознать нельзя.
* MCP: `map_status`, `map_timeline`, `map_packet`, `map_ingest_coordinator`. `map_packet` возвращает
  `{packet_json, packet_bytes, budget_applies_to}`: бюджет считается только по `packet_json`; обёртка и конверт MCP
  (`content[].text`, `structuredContent`) в бюджет **не** входят. Тесты проверяют наш stdio-сервер, а не конкретных
  LLM-клиентов.
* Проверено процессами на синтетическом HOME: отсутствие карты ничего не создаёт; init → ingest → timeline → проектный
  пакет без задач проекта B; отказ на слишком малый/большой бюджет и плохой проект; флаги `--trusted-user`/`--install`/
  `--path` не существуют; MCP initialize/tools/list/tools/call, отказ на `install`/`path`/`source`.

## Карта проектов в окне (M3c): `brain ui` → `/map`

Отдельная страница `/map` (ссылки «Карта проектов» есть в обозревателе графа и в настройках); граф памяти, обозреватель и
старый 3D-вид не изменены. Страница — один HTML без внешних файлов, CDN и npm, плюс один статический скрипт `/map/map3d.js`,
который отдаёт тот же бинарь (как `/graph/graph3d.js` для графа). Есть переключатель **2D/3D**: плоский вид рисует проекты по
кругу, у выбранного проекта — его задачи (слева прошлое, сверху текущее, справа запланированное); объёмный вид —
это настоящие координаты XYZ через одну перспективную проекцию.

* **3D — это настоящий объём, а не плоское движение.** Все экранные позиции, размеры и порядок рисования считает одна
  функция `project` (`assets/map3d.js`): точка позади глаза возвращает `null` и не рисуется, размер убывает с расстоянием,
  дальние узлы рисуются первыми (порядок по глубине), привязка выбора (picking) идёт по той же функции, так что картинка,
  попадание курсора и подписи совпадают. Камера — орбита (перетаскивание), приближение (колесо, `+`/`-`), сброс вида (`F`
  и кнопка «общий вид»), панорама (Shift/правая кнопка). Мышь и касание работают через `pointer`-события.
* **Компоновка детерминированная и ограниченная.** Каждый проект стоит на сфере Фибоначчи по рангу имени; задачи выбранного
  проекта растут тремя ветвями от его центра — по одной на фазу. Сцена ограничена документированным пределом
  `CAPS.nodes = 300`; всё, что за пределом, доступно списками с постраничностью, и это показано. Число подписей и предел
  `devicePixelRatio` тоже ограничены. Рисование только по изменению данных или камеры (нет бесконечного `requestAnimationFrame`),
  в фоне вкладки кадр не рисуется. При `prefers-reduced-motion` камера переставляется без анимации.
* **Доступность без графики.** Список проектов и задач — настоящие фокусируемые кнопки: `Tab` + `Enter` выбирает проект или
  открывает задачу, графики для этого не нужно. Все подписи сцены и текст — только через `textContent`/`fillText`.
* **Полный путь по прошлому/текущему/запланированному.** В левом списке проект можно раскрыть вручную, выбрав его (клик или
  `Enter`); справа под заголовком «Задачи» каждая задача помечена фазой, а кнопка «Ещё задачи» догружает следующую страницу
  (по 25), так что доступны все задачи проекта. История каждой задачи — в «Истории задачи #N» с кнопкой «Ещё события».

* **Проекты смысловые**, а не каталоги: имена проектов задач координатора и субъекты `project:<space>/<имя>` из карты.
  Фаза берётся только из записанного состояния задачи: `completed`/`failed` — прошлое, `active` — сейчас,
  `queued`/`paused` — в плане (это поставленные задачи, а не прогноз). Счётчики ограничены 1000 задач/связей и это
  показывается.
* Проект: задачи постранично (заголовки помечены как текст агента), связи проекта постранично по cursor со статусом,
  основанием, уверенностью, числом источников и событий; спор источников и решение пользователя выделены. История задачи —
  по `timeline` постранично: «заявлено», «неизвестно» и «отмечено как наблюдение (основание)»; пустые доказательства
  («пусто») и отсутствующее поле различаются. Ссылки `memory:` открываются через существующий читатель `/api/explore/node`;
  ссылки архива только показываются: сопоставить install с архивом может только хост.
* Пакет контекста (weak) показывается как текст; бюджет считается только по самому пакету; UI не ищет по памяти.
* API (только с ключом окна): `GET /api/map/status|projects|project|timeline|links|packet` с параметром `space`;
  путей, баз, install и полномочий не принимают. Чтение никогда не создаёт карту: без неё ответ `not_configured`.
  `POST /api/map/init` и `POST /api/map/ingest` — явные действия пользователя (кнопка и диалог подтверждения); нужны ключ
  **и** Origin этого окна (отсутствующий Origin отклоняется). Записи из координатора остаются `unverified`.
* Смена space увеличивает поколение запросов и очищает все панели и сцену; ответы старого поколения отбрасываются. Ответ
  403 (устаревший ключ) очищает данные и показывает кнопку «Переподключиться».
* Весь текст попадает в DOM только через `textContent`/`fillText` (тест проверяет, что в странице нет `innerHTML`/`eval`).
* **Подтверждения пользователем нет.** Кнопки подтверждения или отклонения связей нет и не имитируется: это отдельный
  шаг, где POST должен привязываться к выбранным event id, space и версии и обрабатывать устаревшие/повторные цели.
  Ключ окна и тот же пользователь ОС — договорённость с хостом, а не доказательство присутствия человека.
* Проверено тестами маршрутов и процессов; в настоящем браузере страница этим исполнителем не открывалась.

## Known limitations / decisions to review

* `contested` (single-valued) is conservative: an actor self-correcting to another's value still leaves the slot contested until a user confirms.
* After a user confirmation, new agent proposals show as `overridden` until the user acts again.
* `received_at` comes from the local clock (kept monotonic); as_of on different machines differs by design.
* A remote rejection has no effect here and a remote confirmation counts as a proposal; there is no cross-machine trust yet.
* `links_as_of` loads the space (capped) rather than an index snapshot; fine for M1 sizes, revisit before 100k events.
* Coordinator rows over the bounds block that space's ingestion until the source is fixed or the cursor reset (fail closed).
* M3+: context packets, CLI/MCP/UI, `previews`, `clock_skew` storage (only `Event::clock_skew()` exists).

## Compact `map_packet` response (optional)

`map_packet` accepts `compact_response: true` — a response-format option, no
authority, no paths. Without it (or with `false`) the legacy response is
unchanged: `packet_json` is an escaped string whose own length is bounded by
`budget_bytes`, the wrapper is not counted, and the result is repeated in
`structuredContent`.

With it, `result.content[0].text` is
`{"status":"ok","response":"compact","budget_applies_to":"mcp_content_text_utf8_bytes","requested_budget_bytes":N,"packet":{…}}`
and **the whole UTF-8 content text is at most N** (`budget_bytes`, or the tier
default; weak ≤ 4096, strong ≤ 32768). The packet is embedded as a JSON object
with its exact bytes, gets the budget minus the envelope (its own
`budget_bytes` is that remainder), and the reply carries no
`structuredContent` copy. Deterministic trimming, `truncated`/`omitted`, scope,
provenance, trust, kinds and refs are those of the normal packet. A budget too
small for the envelope plus a minimal packet (e.g. 900 bytes for a project
packet: 125 + 957) is an `invalid_input` error. Over-maximum budgets are refused, never widened.

Not counted: JSON-RPC framing and the escaping of the text inside it on the
wire, and however a client renders it. Bytes are not tokens. Retrieval stays
bounded upstream (local memory: 20 snippets out of a capped candidate search;
shared: 10 out of a 2000-fact scoped scan). `truncated` only describes what
the packet itself dropped.

## 3D brain overview (map page, «3D-мозг»)

The 3D view draws the work map inside a stylized brain. It has two
hemispheres (projected outlines with a light fill, plus a faint wireframe), a
cerebellum and a stem. Everything is drawn on demand on the existing canvas
projection; there is no idle animation, and reduced motion is honoured. The
projects sit inside the hemispheres, and the selected project's task branches,
picking, orbit/zoom, paging and the 2D view are unchanged.

There are eight surface regions, listed accessibly in «Области мозга» on the left.
Clicking a region marker or list entry opens its detail on the right. The
regions are a product proposal for sorting memory, not anatomy.

| region | source | shows |
|---|---|---|
| Проекты и знания | loaded `/api/map/projects` rows | number of loaded projects |
| Активная работа | `current` counts of those rows (coordinator board) | declared/claimed state, not observed file reads or edits; fetch time, stale after 5 min |
| Планы | `planned` counts | as above |
| Координация | `links` + `conflicts` | as above |
| Предпочтения и профиль, Решения, Ошибки и уроки | not loaded by the map | "не здесь" + link to «Граф памяти» (`/graph`) |
| Эпизоды и разговоры | not loaded by the map | "не здесь" + link to «Настройки» → «Архив переписок» |

Counts cover only the loaded page of projects, and the text says so when more
exist. A filled marker means data; a hollow one, empty; a dashed one, records
that live on another page. Canvas labels never go under the hover/legend/hint
overlay. On phones the long legend notes are hidden; they are in the region list.
Not covered: per-client event instrumentation (next slice) and memory-record
counts per region.
