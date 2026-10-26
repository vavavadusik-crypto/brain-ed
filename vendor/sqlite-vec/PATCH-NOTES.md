# Локальная копия sqlite-vec 0.1.9 с одной правкой

## Зачем копия

Апстрим-версия **не компилируется под musl**, а musl нужен для статического бинаря,
который работает на любом Linux без зависимостей:

```
sqlite-vec.c:68: error: unknown type name 'u_int8_t'
```

Причина: BSD-типы `u_int8_t`, `u_int16_t`, `u_int64_t` определены в glibc через
`sys/types.h`, но не в musl. Условная компиляция в апстриме проверяет `_WIN32`,
`__EMSCRIPTEN__`, `__COSMOPOLITAN__` и `__wasi__`, но не различает glibc и musl.

## Что изменено

`sqlite-vec.c`, строки 68–72: блок typedef обёрнут в `#ifdef __GLIBC__`.

## Почему не патч в кэше

Правка в `~/.cargo/registry` исчезает при очистке кэша, не воспроизводится в CI и
не работает у того, кто собирает из исходников. Вендорная копия даёт одинаковую
сборку у всех.

## Что дальше

Однострочный фикс отправляется в апстрим. Когда он выйдет в релизе, эта копия
удаляется, а зависимость возвращается на crates.io.

Public export: upstream URI examples in sqlite3.h use /example/fred instead of
a home-directory prefix. Comments only; SQLite behavior is unchanged.
