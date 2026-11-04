# Подготовка к работе над turbo-pancake (новый разработчик)

Что сделать перед тем, как продолжать план. Цель: собрать проект, прогнать все проверки
и продолжить с **этапа S6** (группы и звонки внутри мессенджера). Всё ниже — для Windows.

## 0. Что где лежит

| Файл | Зачем |
|---|---|
| `CLAUDE.md` | инструкция для Claude Code на каждую сессию (что читать, в каком порядке, правила) |
| `docs/PROJECT_STATE.md` | журнал фактов: текущий этап, статус каждого критерия, известные проблемы, **«Exact next task»** |
| `docs/STAGES.md` | этапы S0–S9, их критерии и планы по шагам |
| `docs/ADR/` | принятые решения (почему так, а не иначе) |
| `docs/API.md`, `proto/` | контракты сервера: HTTP API, WebSocket, protobuf |
| `docs/TESTING.md` | какие тесты что доказывают, как запускать испытания |

ТЗ (`TECHNICAL_SPEC_MESSENGER_RU.md`) в репозитории **нет намеренно**: репозиторий
публичный. Его даёт владелец проекта лично (шаг 2).

## 1. Доступ и код

1. Принять приглашение в collaborators (письмо от GitHub или
   `https://github.com/slvx666/turbo-pancake/invitations`).
2. Установить Git и GitHub CLI, войти: `gh auth login` (HTTPS, нужны права `repo`,
   `workflow`).
3. Клонировать в путь **без пробелов и покороче**:
   `git clone https://github.com/slvx666/turbo-pancake C:\src\turbo-pancake`.

## 2. ТЗ

Получить файл ТЗ у владельца (не через публичные места), положить **вне репозитория**,
например `C:\docs\TECHNICAL_SPEC_MESSENGER_RU.md`. Никогда не коммитить ни сам файл, ни
его текст.

## 3. Инструменты

| Что | Как |
|---|---|
| Rust | rustup с <https://rustup.rs>; нужная версия (1.99.1) ставится сама из `rust-toolchain.toml` при первой сборке. Затем `rustup component add llvm-tools` |
| Visual Studio 2022 (Community или Build Tools) | рабочая нагрузка «Разработка классических приложений на C++» + компоненты **MSVC x64**, **Windows SDK**, **C++ Clang tools for Windows** (clang-cl нужен для сборки WebRTC APM), **CMake** |
| Python 3 | для meson/ninja: `python -m venv C:\tp-tools\venv`, затем `C:\tp-tools\venv\Scripts\pip install meson ninja` |
| `nm.exe` | скопировать `llvm-nm.exe` из `<VS>\VC\Tools\Llvm\x64\bin` в `C:\tp-tools\bin\nm.exe` |
| PostgreSQL (только для тестов) | portable zip PostgreSQL 17 для Windows x64 с get.enterprisedb.com (раздел binaries), распаковать, например в `C:\tp-tools\pgsql`. Службу ставить не нужно: тесты сами поднимают временный кластер |

Переменная окружения `TURBO_PANCAKE_TOOLS=C:\tp-tools` (папка, где лежат `venv\Scripts`
и `bin\nm.exe`; её читает `benchmarks/scripts/apm-env.ps1`). Можно вместо этого держать
инструменты в `D:\turbo-pancake-tools` — это путь по умолчанию в скрипте.

## 4. Локальная конфигурация (в git не попадает)

`.cargo/config.toml` в корне репозитория (он в `.gitignore`):

```toml
[build]
# Короткий путь: релизная сборка WebRTC APM упирается в MAX_PATH (LNK1181) в длинных путях.
target-dir = "C:/t"

[env]
# CMake из Visual Studio (поправить путь под свою установку) и явный генератор:
# без него крейт cmake собирает libopus без /O2.
CMAKE = "C:/Program Files/Microsoft Visual Studio/2022/Community/Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin/cmake.exe"
CMAKE_GENERATOR = "Visual Studio 17 2022"
# PostgreSQL для тестов мессенджера (папка с initdb.exe, pg_ctl.exe, createdb.exe).
TURBO_PANCAKE_PG_BIN = "C:/tp-tools/pgsql/bin"
```

`CLAUDE.local.md` в корне репозитория (тоже в `.gitignore`), его читает Claude Code:

```markdown
# Machine-local notes (untracked)

* Spec: `C:\docs\TECHNICAL_SPEC_MESSENGER_RU.md`
* Build directory: `C:/t` (see `.cargo/config.toml`).
* Toolchain: rustup in `%USERPROFILE%\.cargo\bin`; in Bash run
  `export PATH="$HOME/.cargo/bin:$PATH"` before cargo.
* APM build environment: `powershell -File benchmarks/scripts/apm-env.ps1 <command>`
  (TURBO_PANCAKE_TOOLS = `C:\tp-tools`).
* One monitor: GUI tests may use the main monitor and take focus.
* Git: `origin` = https://github.com/slvx666/turbo-pancake, branch `main`; committing and
  pushing is allowed. CI: `gh run list -R slvx666/turbo-pancake`.
```

## 5. Проверка, что всё собирается

Из корня репозитория, в PowerShell (первая сборка долгая: WebRTC APM, libopus, sqlx):

```powershell
cargo fmt --all -- --check
powershell -File benchmarks/scripts/apm-env.ps1 cargo clippy --workspace --all-targets --locked -- -D warnings
powershell -File benchmarks/scripts/apm-env.ps1 cargo test --workspace --locked --no-fail-fast
```

Ожидается: всё зелёное. Нюансы:

* Тесты `media-video` с `hardware_*` и `share.rs` проверяют скорость кадров на GPU: при
  запущенной игре или занятом NVENC они падают по таймингу. Это окружение, не код.
* Тесты мессенджера сами запускают PostgreSQL через `pg_ctl` и гасят его при выходе.
  Без PostgreSQL можно временно поставить `TURBO_PANCAKE_SKIP_PG=1` (тесты пропустятся с
  сообщением), но для работы над S6 PostgreSQL нужен обязательно.
* GUI-испытания (`benchmarks/scripts/*-trial.ps1`) раскладывают окна по координатам
  второго монитора (`--window-pos`). В `s5-trial.ps1` это параметры `-X1 -X2 -Y`, на
  одном мониторе: `-X1 0 -X2 960 -Y 0`; в более старых скриптах координаты (`1940`,
  `2900`) правятся в самом скрипте.

Release-сборка клиента и сервера (для испытаний):
`powershell -File benchmarks/scripts/apm-env.ps1 cargo build --release --locked -p turbo-pancake-desktop -p turbo-pancake-app-server`.

## 6. Важный контекст (прочитать до начала S6)

* **Только Windows (ADR 0012).** С этапа S4 весь продукт делается и принимается на Windows.
  macOS и Linux — отдельный этап **S9** после S8. Не начинать работу под macOS/Linux;
  платформенный код — за `cfg(windows)`, платформенно-нейтральные крейты (protocol,
  сервер, SFU, messenger-client) не должны тянуть Windows-API. CI тоже только Windows.
* **Owner-assisted пункты не блокируют этапы.** То, что требует железа или присутствия
  владельца (RTX 4070 Super из ТЗ, монитор 1440p ≥ 120 Гц, два дома, камера для
  screen-to-screen задержки, проверка на слух), записано в PROJECT_STATE как pending.
* **H.264 — только кодеки ОС/вендора (ADR 0009):** Media Foundation, без своих и без
  OpenH264/FFmpeg.
* **Репозиторий публичный:** не коммитить IP-адреса, личные пути, имена, данные машин и
  сетей, текст ТЗ. Испытания описывать обобщённо («сервер на публичном IP тестера»).
* **S5 (аккаунты и личные сообщения), ADR 0013:**
  * сервер: PostgreSQL + миграции `migrations/`, Argon2id, токены сессий (в БД только
    SHA-256), лента изменений по `change_seq`, idempotent отправка по `client_nonce`,
    блокировки на сервере, presence по живым WebSocket;
  * клиентское ядро `crates/messenger-client`: SQLite-кэш, синхронизация, pending-операции,
    свой авторизованный WebSocket;
  * desktop: UI чата, сессия хранится в Windows Credential Manager;
  * TLS (ADR 0014, `docs/DEPLOY-TLS.md`): сервер с `--tls-cert/--tls-key` отдаёт
    HTTPS + WSS, сертификаты делает `turbo-pancake-app-server gen-cert --name <IP>`,
    клиенты всегда проверяют сертификат (`--server-ca ca.pem` или публичный PKI).
    reqwest собран без встроенного криптопровайдера: любой HTTP-клиент создавайте с
    конфигом из `crates/tls` (`Client::new()` упадёт);
  * **звонки пока не привязаны к аккаунтам:** голосовая комната — это отдельное
    WebSocket-соединение по id комнаты, без токена (разрешено только на loopback-сервере
    или с `--anonymous-calls true`). Привязать звонки к диалогам и пользователям —
    задача S6.
* **Задачи S6 (ТЗ §6, §7, §11, §17):** группы до 100 участников, создатель добавляет и
  удаляет участников и меняет название, приглашение по username; общий звонок внутри
  группы и личный звонок из личного чата, статусы звонка, вход в идущий звонок, один
  активный звонок на пользователя (с учётом нескольких устройств); блокировка запрещает
  личные звонки; события `call.started/participant_joined/participant_left/ended`,
  `conversation.updated`; интеграционный тест на 100 участников группы; переход UI
  чат ↔ звонок без порчи истории и прав. Таблицы уже рассчитаны на группы (`kind`,
  `title`, `role`); планируйте шаги в `docs/STAGES.md` до кода, как для S5.
* **Подводные камни:**
  * `.ps1` сохранять в UTF-8 **с BOM** и CRLF, иначе PowerShell 5.1 ломает кириллицу;
  * `pg_ctl start` нельзя вызывать с перенаправленным выводом: сервер наследует
    дескрипторы, и вызов зависает;
  * в Slint `row` — зарезервированное имя свойства;
  * `cargo audit` в CI игнорирует RUSTSEC-2023-0071: `rsa` есть только в `Cargo.lock`
    через sqlx, но не собирается (PROJECT_STATE, пункт 41).

## 7. Как работать дальше

* Каждая сессия начинается по `CLAUDE.md`: ТЗ целиком → `docs/PROJECT_STATE.md` (раздел
  «Exact next task») → `docs/STAGES.md`, ADR, `git status`, `git log`.
* Этап — DONE только когда у каждого Windows-критерия есть PASS с доказательством; иначе
  `UNVERIFIED`.
* Перед концом сессии: fmt / clippy / тесты, обновить PROJECT_STATE, закоммитить,
  запушить, проверить CI.
* Если над проектом параллельно работает кто-то ещё: заранее договориться, кто что
  делает. `main` и `docs/PROJECT_STATE.md` общие — перед началом `git pull`, коммиты
  небольшие.
