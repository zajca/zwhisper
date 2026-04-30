# zwhisper — idea & spec (v2, post-review)

> Linuxový desktop tool pro nahrávání PipeWire audia (mic + system output) s
> tray ovládáním, profily a transcription pipelinou napojenou na lokální
> `whisper.cpp` (a volitelně cloud backendy).
>
> **Status: idea-stage, post review v2.** První verze byla over-architected
> a prodávala víc, než reálně umí. Tato verze je honest assessment — co je
> jisté, co je risk, co je úplně otevřené.

---

## 0. Honest status (čti první)

Tohle není hotový design, ale kandidát na walking skeleton. Před commitnutím
do velkého rozsahu fíčur musí klíčové předpoklady projít PoC fází:

| Co | Stav | Co je risk |
|---|---|---|
| **Audio capture (mic + sink monitor)** | hypotéza | Robustnost při default-device switch, formát handoff do whisper.cpp |
| **Whisper.cpp lokálně, post-process** | hypotéza | Cesta k binárce, model management, latence |
| **Daemon + tray rozdělení** | hypotéza | Hardening vs reálný session env, secret-service přes D-Bus |
| **Output delivery bez tray** | hypotéza | CLI/headless flow musí být použitelný i bez tray procesu — viz sekce 5 |
| **Type-at-cursor (dictation)** | **R&D only**, mimo committed roadmap | wtype nefunguje na KWin/Mutter; ydotool nezná focus; libei dozraje časem |
| **Global hotkeys přes xdg-desktop-portal** | KDE primary, ostatní target empirically | GNOME UX neověřený; wlroots nekonzistentní; PTT semantics neexistují |
| **Cloud transcription** | hypotéza | Čistě volitelný remote provider; žádný compliance/consent workflow |

**M0 walking skeleton musí proběhnout dřív**, než se commituje 3-procesová
architektura, settings GUI nebo cloud backendy. Viz sekce 11.

### Co tento dokument *není*

- **Není compliance / privacy spec.** Žádný cloud consent UI, žádný redaction,
  žádný encrypt-at-rest. Jen pragmatické technické safety: file permissions
  `0600`, žádný transcript text v logu, `max_duration_minutes` jako safeguard
  proti runaway recording. Pokud někdy bude potřeba compliance vrstva, je to
  samostatný projekt nad tímhle.
- **Není multi-tenant produkt.** Single-user desktop tool.

---

## 1. Vize a use cases

`zwhisper` je nástroj na pomezí "tape recorder" a "speech-to-text frontend"
pro Linux desktop (Arch + KDE Plasma 6 / GNOME 47, PipeWire, Wayland-first).

### Konkrétní scénáře (s honest disclaimers)

| Scénář | Zdroje | Backend | Výstup | Confidence |
|---|---|---|---|---|
| **Meeting** | mic + system sink monitor | local whisper.cpp NEBO cloud (opt-in) | FLAC + transkript do souboru | high |
| **Voice memo** | mic | whisper.cpp `base` | Opus + text do clipboardu | high |
| **Interview** (long-form) | mic + system (kanálově oddělené) | whisper.cpp `large-v3` post-process | FLAC + transkript | high |
| **Dictation** (push-to-talk → kurzor) | mic | whisper.cpp `small` | text napsaný na kurzor | **low — viz sekce 12** |

**Důležité: "dictation" v plánu zůstává jako research item, ne committed feature.**
Reálně může skončit jako "transkript do clipboardu, user si stiskne Ctrl+V sám".

### Diarization — co skutečně dostáváš

Termín "diarization" znamená v literatuře *speaker diarization*: model identifikuje
"speaker A vs speaker B vs speaker C" embeddingem hlasu. **Whisper.cpp to neumí.**

V tomto projektu rozlišujeme tři různé věci:

1. **True diarization** — jen cloud backendy (Deepgram, AssemblyAI). Embed-based,
   funguje i na single-channel audio.
2. **Channel attribution** — pokud nahráváme mic + sink monitor jako stereo split,
   víme s jistotou: kanál L = lokální user, kanál R = vše ostatní. Není to
   diarization (v pravém kanále jsou všichni vzdálení účastníci pohromadě), ale
   pro single-vs-others use case stačí.
3. **None** — single-channel mic, žádné rozlišení.

Profil musí jasně říkat, která z těchto tří úrovní je v daném záznamu možná.
Žádné "diarization-friendly flow" formulace, které slibují true diarization
a doručí channel attribution.

---

## 2. Architektura

Tři procesy + D-Bus IPC, ale **rozdělení odpovědností je revidované** vůči v1:

```
┌──────────────────────────────────┐         ┌────────────────────────────────────────┐
│ zwhisper-tray                    │ D-Bus   │  zwhisperd  (systemd user service)     │
│  - tray indicator (ksni)         │◄───────►│   - PipeWire / GStreamer capture       │
│  - clipboard write (wl-copy)     │         │   - profile manager                    │
│  - notification dispatch         │         │   - transcription orchestrator         │
│                                  │         │   - file sink (recording + transcript) │
└──────────────────────────────────┘         └────────────────────────────────────────┘
                ▲                                              ▲
                │ D-Bus                                        │ D-Bus
                │                                              │
        ┌───────┴────────┐                            ┌────────┴────────────┐
        │ zwhisper-cli   │                            │ zwhisper-settings   │
        │  (start/stop/  │                            │  (rare-use, on-     │
        │   status)      │                            │   demand spawn)     │
        └────────────────┘                            └─────────────────────┘
```

### Kde žijí sinky

Daemon běží jako systemd user service a nemá garantovaný přístup k aktivní
graphical session (WAYLAND_DISPLAY, focus, compositor). Sinky, které session
vyžadují, žijí v procesu, který je *součástí* graphical-session.target —
tj. v `zwhisper-tray`.

| Sink | Žije v | Důvod |
|---|---|---|
| **FileSink** (audio + `.txt`/`.json`) | daemon | čistě I/O, žádná session dep, vždy garantovaný |
| **ClipboardSink** | tray | potřebuje `WAYLAND_DISPLAY` / `DISPLAY`; `wl-copy` musí vidět compositor |
| **NotificationSink** | tray | `org.freedesktop.Notifications` na session bus |

Daemon emituje D-Bus signal s metadaty + cestou; tray si transkript přečte
ze souboru a doručí session-bound sinky. **Type-at-cursor není sink projektu**
— je v R&D queue (sekce 12), default flow je clipboard a user si paste sám.

### D-Bus interface (revidovaný)

```
interface cz.zajca.Zwhisper1.Recorder {
    StartRecording(s profile_name) -> (s session_id);
    StopRecording(s session_id)    -> (s session_id);
    GetStatus()                    -> (s state, s active_profile, x duration_ms);

    // signals — pouze metadata, žádný payload
    StateChanged(s new_state, s session_id);

    // payload se NEPOSÍLÁ přes D-Bus.
    // Konzument si přečte transcript_path (a metadata) ze souboru.
    RecordingComplete(s session_id, s audio_path);
    TranscriptComplete(s session_id, s transcript_path, x bytes, s backend);
}

interface cz.zajca.Zwhisper1.Profiles {
    List() -> (a(ssu));        // [(name, description, schema_version)]
    GetActive() -> (s);
    SetActive(s name);
    Reload();
}
```

D-Bus message size limit je v praxi 128 MB (configurable), ale posílat
megabajty transkriptu přes signal je v každém případě špatný pattern —
spotřebovává paměť všech listenerů, je obtížně resumovatelné, a souběžné
záznamy by se navzájem ovlivňovaly. Cesta k souboru je správná.

---

## 3. Audio capture pipeline

### Zdroje

PipeWire identifikuje dva relevantní typy uzlů:

1. **Audio source** = mikrofon (např. `alsa_input.usb-…`).
2. **Audio sink monitor** = "co slyšíš" = systémový výstup (např.
   `alsa_output.…analog-stereo.monitor`). PipeWire/PulseAudio dávají ke
   každému sinku read-only monitor source — to je "loopback" trik.

### Default device discovery

V1 plán: `pw-metadata` API + fallback `wpctl status`. **Risk:** uživatel může
default zařízení změnit za běhu (přepojení Bluetooth, hot-swap USB mic).
Engine musí na tohle reagovat — buď restartovat pipeline (krátký glitch), nebo
selektovat zařízení jen jednou při startu a explicitně to v UI komunikovat.

Default volba: **lock to specific device at recording start, neměníme za běhu**.
Pokud zařízení zmizí, ukončit nahrávání a notifikovat (lepší než tichá ztráta).

### GStreamer pipeline (PoC cesta)

Pro M0/M1 GStreamer s `pipewiresrc` — bindings (`gstreamer-rs`) jsou zralé,
mixování/encoding/file IO máme zadarmo.

**Mono-mix profile** (pro whisper.cpp):

```
pipewiresrc target-object=<mic>     ! audioconvert ! audioresample ! mix.
pipewiresrc target-object=<monitor> ! audioconvert ! audioresample ! mix.
audiomixer name=mix                 ! audioconvert ! audioresample
                                    ! audio/x-raw,rate=16000,channels=1
                                    ! flacenc ! filesink location=...
```

**Stereo split** (pro channel attribution archiv):

```
pipewiresrc target-object=<mic>     ! audioconvert ! audio/x-raw,channels=1 ! interleave.sink_0
pipewiresrc target-object=<monitor> ! audioconvert ! audio/x-raw,channels=1 ! interleave.sink_1
interleave name=interleave          ! flacenc ! filesink location=...
```

### Co musíme prokázat v M0 (než půjdeme dál)

- pipeline survives 60 minut nepřetržitého záznamu bez memory growth
- po stopnutí je FLAC validní (hlavička, length, žádné truncation)
- žádné dropped samples (kontrola `pipewiresrc` underrun events)
- chování při hot-swap default device (degradace musí být detekovaná, ne tichá)

---

## 4. Transcription backends

```rust
#[async_trait]
trait Transcriber: Send + Sync {
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;  // streaming, true_diarization, languages

    async fn transcribe_file(&self, path: &Path, opts: &TranscribeOpts)
        -> Result<Transcript>;
}
```

### Implementace

| Backend | Modus | True diarization | Vyžaduje connectivity |
|---|---|---|---|
| **WhisperCppLocal** | post-process file | NE | NE |
| **Deepgram** | streaming WS | ANO (embed-based) | ANO |
| **AssemblyAI** | streaming WS | ANO | ANO |
| **OpenAIWhisper** | REST batch | NE | ANO |

### `WhisperCppLocal` — generická detekce, ne hardcode

V1 implicitně předpokládal `/usr/bin/whisper-cli` z autorova vlastního Zen5
balíčku. **Špatně** — projekt se má dát použít na čemkoli, kde je whisper.cpp
nainstalovaný.

Detekce v pořadí:

1. `ZWHISPER_WHISPER_CLI` env var (explicit path)
2. `whisper-cli` v `$PATH`
3. `whisper-cpp` v `$PATH` (jiné distro alias)
4. `~/.local/bin/whisper-cli`
5. Pokud nic — settings UI nabídne "stáhni z github releases" nebo "nainstaluj
   přes package manager", s odkazem na build instructions

Pokud user *má* Zen5-optimized build, je to bonus (rychlejší inference), ale
**není to dependency**. PKGBUILD pro Arch deklaruje `optdepends`, ne `depends`.

### Modely

- Cesta: `~/.local/share/zwhisper/models/ggml-{tiny,base,small,medium,large-v3}.{en.,}bin`
- Settings UI nabídne download + integrita check (SHA256 z huggingface manifestu)
- Profile reference modelu *jménem*, ne cestou (engine resolve)

### Konfigurace API klíčů

V tomto pořadí (první nalezený vyhrává):

1. **secret-service** přes `keyring` crate — `cz.zajca.zwhisper / <backend>`
2. Env var (`ZWHISPER_DEEPGRAM_API_KEY`)
3. `~/.config/zwhisper/secrets.toml` (pouze pokud `chmod 600` a vlastníkem je user)

**Risk:** sandboxing daemonu (viz sekce 9) může blokovat secret-service přístup.
Musí být ověřeno empiricky před commitem do hardenovaného profilu.

---

## 5. Output destinations a delivery model

### FileSink (v daemonu, vždy garantovaný)

- Cesta: `~/Recordings/zwhisper/<profile>/<YYYY-MM-DD_HH-MM-SS>.<ext>`
- Audio: `.flac` / `.opus` / `.wav` podle profilu
- Transkript: stejný basename + `.txt` + `.json` (segmenty s timestampy)
- Pole `retention_days` v profilu (volitelné) — auto-purge starších záznamů
- Permissions: `0600` na audio i transkript

### Session-bound sinky (v tray procesu)

Clipboard a notifications vyžadují živé spojení na compositor session
(`WAYLAND_DISPLAY` / `DISPLAY`, focus, D-Bus session bus s běžícím
notification daemonem). Žijí proto v `zwhisper-tray`, ne v daemonu.

#### Clipboard

- Wayland: `wl-copy < transcript.txt` (subprocess `wl-clipboard` package)
- X11: `xclip -selection clipboard < transcript.txt`
- Auto-detect podle `$XDG_SESSION_TYPE`

#### Notifications

- `notify-rust` crate (volaný z trayu, má session env)

#### Type-at-cursor

**R&D queue, ne committed feature.** Důvody v sekci 12. Default chování
zwhisperu je **clipboard only**. Pokud user chce paste-on-cursor, sám si
ručně stiskne Ctrl+V; pro automatizaci si může nastavit `ydotool` jako
volitelný hack mimo zwhisper, ale ten setup ani jeho podpora nejsou
součástí projektu.

### Delivery model — co se stane, když tray neběží

Daemon je single source of truth pro recording lifecycle. Tray je *consumer*
session-bound sinků. Tři možné stavy v okamžiku, kdy daemon dokončí
transkripci:

| Stav | Garantované doručení |
|---|---|
| Tray běží a poslouchá | FileSink + clipboard + notify |
| Tray neběží / spadl | **Pouze FileSink.** Clipboard ani notifikace se neuplatní. |
| Tray nastartuje pozdě (po recording) | Pouze FileSink (tento záznam). Tray po startu jen ukáže poslední transkript v menu "Open last recording". |
| CLI-only flow (žádný tray vůbec) | Pouze FileSink. CLI může explicitně doručit do clipboardu, viz níže. |

**Žádný persistent outbox / retry queue.** Reasoning: clipboard je transient
session state (ne perzistentní zpráva), notifikace má smysl jen real-time.
Replay 30 minut staré clipboard injection do nějaké náhodně otevřené aplikace
by byl horší než chybějící doručení. Single source of truth zůstává soubor.

### CLI / headless guarantees

CLI je first-class scriptovatelný klient. Co garantuje:

- **`zwhisper record …`** — vždy funguje bez tray, FileSink garantovaný
- **`zwhisper transcribe <file>`** — funguje bez tray
- **`zwhisper status`, `start`, `stop`** — funguje proti běžícímu daemonu

Co je session-bound (CLI proxyfikuje, ale efekt vyžaduje session env):

- **`zwhisper output last --to clipboard`** — CLI sama spustí `wl-copy` /
  `xclip` v aktuálním shell session. Funguje, pokud shell má `WAYLAND_DISPLAY`
  / `DISPLAY` (typicky z terminálu uvnitř session). Best effort, ne podporované
  v ssh / cron / čistě headless kontextu.
- **`zwhisper output last --to notify`** — CLI mluví na session bus
  `org.freedesktop.Notifications`. Best effort.

**Žádné CLI volání nesahá do tray procesu**, aby se odlišily zodpovědnosti.
Výhoda: skripty fungují i když tray spadl nebo není instalovaný.

---

## 6. Profile systém

### Lokace

- Shipped templates v `/usr/share/zwhisper/profiles/*.toml` (vendored)
- User overrides v `~/.config/zwhisper/profiles/*.toml`
- **Schema versioning + migration** (revidováno vůči v1):

### Schema versioning

Každý profile soubor má povinné `schema_version = N`. zwhisperd při loadu:

- `schema_version` chybí → reject s clear error message ("legacy profile, run `zwhisper config migrate`")
- `schema_version > current` → reject ("profile from newer zwhisper, please upgrade")
- `schema_version < current` → run in-place migration (chain of migration funcs), backupne starý soubor jako `.toml.bak.<timestamp>`
- `schema_version == current` → load

Migrations are idempotent. CHANGELOG eviduje breaking schema changes per major version.

### Merge vs replace strategy

V1 řekl "user profile přebije shipped, žádný merge". To je křehké — pokud
shipped přidá v upgrade nové pole, user override ho nemá a default v kódu
to musí ošetřit. Lepší:

- **Shipped templates jsou starting point**, user je do `~/.config/...`
  *kopíruje* (CLI příkaz `zwhisper profile clone meeting my-meeting`)
- **User overrides jsou plné, ne partial.** Žádné dědění, žádný merge.
  Předvídatelné. Pokud shipped přidá nové pole, user své profily nedotčené;
  defaulty v kódu zajistí backward compat.
- Settings UI / CLI ukazují diff mezi user profilem a shipped templatem,
  ať user vidí, co chybí.

### Schéma (TOML, schema_version = 1)

```toml
schema_version = 1
name           = "Meeting"
description    = "Záznam hovoru s channel attribution"

[sources]
mic           = "default"          # nebo konkrétní node name
system_output = "default"          # default sink monitor
mode          = "stereo_split"     # mono_mix | stereo_split

[recording]
codec       = "flac"               # flac | opus | wav
sample_rate = 48000
max_duration_minutes = 180         # safety: auto-stop, aby se neztratil
                                   # zapomenutý záznam přes noc

[transcription]
backend     = "whisper_cpp"        # whisper_cpp | deepgram | assemblyai | openai
model       = "small"              # backend-specific identifier
language    = "cs"
auto        = true                 # spustit po stop nahrávání

[[output]]
type = "file"
path = "~/Recordings/zwhisper/meeting/{timestamp}"

[[output]]
type = "notification"

[hotkey]
toggle = "Super+Shift+R"           # přes xdg-desktop-portal — risk, viz sekce 12
```

Cloud backendy (`deepgram`, `assemblyai`, `openai`) jsou *čistě volitelný
remote provider*. Profile používající cloud backend je vizuálně označený
(☁ prefix v tray menu), aby user věděl, že audio půjde mimo stroj. Žádný
"consent" workflow ani modální dialog — user volí profile vědomě.

---

## 7. Technical safety (minimum)

Tohle není privacy/compliance vrstva (viz sekce 0). Jen pragmatické
zábrany proti vlastním chybám:

- **`max_duration_minutes`** v profilu (default 180) — daemon auto-stop,
  chrání před zapomenutým záznamem přes noc nebo runaway profilem.
- **File permissions** — audio + transcript `0600`, recording dir `0700`.
  Default chování, ne config.
- **Žádný transcript text v logu, nikdy.** Žádné API klíče v logu. Log
  obsahuje jen události (start/stop/profile/backend/exit code).
- **Cloud backend visual marker** — profile s remote providerem má v tray
  menu `☁` prefix. Jeden vizuální cue, žádný workflow.
- **Recording indicator** — tray ikona se mění během záznamu (idle vs
  recording). Pokud tray neběží, indikátor neexistuje — to je vědomé
  trade-off, ne fíčura k vyřešení.

---

## 8. Hotkeys (target-aware)

V1 prezentovala xdg-desktop-portal GlobalShortcuts jako vyřešený cross-desktop
mechanism. Realita je targetová:

- **KDE Plasma 6** (primary target) — portal GlobalShortcuts funguje
- **GNOME 47+** — portal implementace existuje, ale **management UX musí
  být ověřen empiricky** na cílové verzi (registrace shortcut, viditelnost
  v Settings → Keyboard, runtime rebind). Treat as untested, ne broken.
- **wlroots** (Sway/Hyprland) — `xdg-desktop-portal-wlr` GlobalShortcuts
  implementaci má, ale je nekonzistentní mezi compository. Typicky shortcut
  musí být definován v compositor configu a portal jen forwarduje activation.
- **X11** — funguje přes implementation portál, případně starý mechanismus
  (X grab) jako fallback.

### PTT (push-to-talk) semantics

Portal API exponuje `Activated` signál — to je "klávesa byla stisknuta",
ne "down/up". Pravý PTT (drž = nahrávej, pusť = stop) **přes portal nelze**.
Dvě cesty:

1. **Toggle místo PTT** (default plán) — stiskneš = start, znovu = stop.
   Akceptovatelné UX, řeší 90 % use cases. Tohle je v M4.
2. **`libei`** (ne `ydotool`) — modern input emulation/capture framework
   adopted KDE 6+ a GNOME 46+. Umí key down/up. Ale Rust binding (`reis-rs`)
   je rané. Tohle je M8+ research.

### Plán pro M-fáze

- M3: hotkey via portal — *toggle* start/stop pro standardní profily.
- M4: dictation as **transient nahrávání s explicit start/stop**, žádný PTT.
  User stiskne hotkey, mluví, znovu stiskne hotkey, transkript se objeví.
- Pravý PTT odložen na neurčito.

---

## 9. Systemd integrace (s honest disclaimers)

### Daemon unit

`/usr/lib/systemd/user/zwhisperd.service`:

```ini
[Unit]
Description=zwhisper recording daemon
After=pipewire.service wireplumber.service
Requires=pipewire.service

[Service]
Type=dbus
BusName=cz.zajca.Zwhisper1
ExecStart=/usr/bin/zwhisperd
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
```

### Hardening — co zkusíme, co se může pokazit

V1 měl agresivní `ProtectHome=read-only`, `ProtectSystem=strict`, `PrivateTmp=yes`.
Reálně to může konfliktovat s:

- **PipeWire socket** (`$XDG_RUNTIME_DIR/pipewire-0`) — měl by být OK protože
  runtime dir není pod `ProtectHome`, ale ověřit
- **Secret-service** přes D-Bus — sandbox blokuje session bus přes pipe na
  jiný proces? Záleží na nastavení.
- **`~/Recordings/`, `~/.config/zwhisper/`, `~/.local/share/zwhisper/`** —
  musí být v `ReadWritePaths`, jinak daemon nenahraje a nenačte modely.
- **GStreamer plugins** v `~/.local/lib/gstreamer-1.0/` (kdyby user měl
  custom plugin) — `ProtectSystem=strict` je odřízne.
- **`whisper-cli` subprocess** — pokud daemon má `NoNewPrivileges=yes`,
  je to fine; pokud má `MemoryDenyWriteExecute`, JIT v whisper.cpp
  (kdyby měl) by mohl spadnout.

**Plán:** začít s žádným hardeningem v M0, postupně přidávat directives
a po každé directive otestovat full happy path. Hardening jako last-mile,
ne first-day.

### D-Bus activation

`/usr/share/dbus-1/services/cz.zajca.Zwhisper1.service`:

```ini
[D-BUS Service]
Name=cz.zajca.Zwhisper1
Exec=/usr/bin/zwhisperd
SystemdService=zwhisperd.service
```

**Risk:** D-Bus activation se na user-bus chová různě podle distra a verze
dbus. Test: spawning přes `systemctl --user start zwhisperd` musí dát
identický výsledek jako spontánní activation přes prvního D-Bus klienta,
včetně env var setupu (zejména `XDG_RUNTIME_DIR`, `WAYLAND_DISPLAY`).

### Tray unit

```ini
[Unit]
Description=zwhisper tray indicator
After=graphical-session.target
PartOf=graphical-session.target

[Service]
ExecStart=/usr/bin/zwhisper-tray
Restart=on-failure

[Install]
WantedBy=graphical-session.target
```

Tray *záměrně* není D-Bus aktivovaný — má být součást graphical session,
spouštět se s ní, padat s ní.

---

## 10. GUI volba (zhuštěno)

Detail benchmark je v Apendixu A. Stručná verze:

**Tray (always-on)**: crate `ksni` — pure Rust StatusNotifierItem, žádné
GTK runtime. KDE nativně, GNOME přes `gnome-shell-extension-appindicator`.

**Settings GUI (rare-use, on-demand spawn)**: **`fltk-rs`** s features
`["fltk-bundled", "use-wayland"]`. Důvod: ~1 MB binárka, nejnižší RAM,
mature codebase, hybrid X11/Wayland backend od FLTK 1.4 (2024). Trade-off:
build chain potřebuje cmake + g++. Fallback **Slint software renderer**
pokud FLTK selže na fractional scaling KDE Plasma 6.

**Performance budget** (cíle, ne hard limity):

| Komponenta | Binary | RAM idle |
|---|---|---|
| `zwhisperd` | < 8 MB | < 30 MB |
| `zwhisper-tray` | < 5 MB | < 20 MB |
| `zwhisper-settings` (on-demand) | < 8 MB | < 60 MB během běhu |
| `zwhisper` (CLI) | < 4 MB | n/a |

**Total always-resident** (daemon + tray): cíl < 50 MB RAM. Pro srovnání:
typický Electron tray ~150–250 MB RAM.

---

## 11. Roadmap

Walking skeleton první, pak postupně přidávat. Žádné committed milestones
na fíčury, které stojí na nezralých externích závislostech (libei, true PTT).

| M | Cíl | Definition of done |
|---|---|---|
| **M0 — Walking skeleton** | Mic + sink monitor → FLAC, jednoprocesový bin | `zwhisper record --mic default --monitor default --output x.flac --duration 60`, FLAC validní, žádný drop, 60min run bez memory leaku |
| **M1 — Whisper.cpp post-process** | Po nahrávce → transkript do `.txt`/`.json` | `zwhisper record … --transcribe whisper-cpp --model small --lang cs`, transcript validní; whisper-cli detekce z PATH |
| **M2 — Profiles** | TOML profily, `schema_version=1`, validation, migrations | `zwhisper record --profile meeting`, schema versioning enforced, replace-not-merge |
| **M3 — Daemon + CLI** | Rozdělit na `zwhisperd` + `zwhisper-cli`, D-Bus IPC | start přes CLI, daemon nahrává nezávisle, signal `TranscriptComplete` doručí cestu; CLI funguje bez tray |
| **M4 — Tray (ksni)** | Tray ovládá daemon, profile switching, session-bound sinky | menu funkční, recording indicator viditelný; clipboard a notify deliver z tray, FileSink nezávislý |
| **M5 — Cloud backend (Deepgram)** | První remote transcriber, secret-service integrace | API key v keyring, streaming, ☁ marker v tray menu |
| **M6 — Hotkey toggle (portal)** | xdg-desktop-portal GlobalShortcuts, toggle start/stop | KDE Plasma 6 funguje; GNOME a wlroots empirically tested, dokumentovaný stav |
| **M7 — Settings GUI (FLTK)** | Profile editor, model downloader | rare-use, on-demand spawn |
| **M8 — Packaging** | Arch PKGBUILD, systemd units, systémové testy | install + enable na čistém Arch dává funkční daemon |
| **R&D queue** | type-at-cursor / true PTT (libei), AssemblyAI, OpenAI batch | research, ne committed milestones |

**Klíčové: M0–M2 jsou jediný proces.** Tři-procesová architektura se ověří
až M3, ne v idea-stage. Pokud se M0 ukáže jako neřešitelné (např. PipeWire
default-device switching způsobuje constant glitches), celý zbytek se
přehodnocuje.

---

## 12. Rizika a otevřené otázky

### Top blockers

#### xdg-desktop-portal GlobalShortcuts (M6)

- KDE Plasma 6 (primary target): funguje
- GNOME 47+: portal implementace existuje; **UX a management flow musí být
  empiricky ověřen** na cílové verzi. Treat as untested, ne broken.
- wlroots: nekonzistentní mezi compository
- **PTT (key down/up)**: portal API neexponuje, jen `Activated`. Jen toggle
  je realistický pro M6.

#### Wayland input emulation (R&D queue, mimo committed roadmap)

Jen pro úplnost stavu — žádný M nezávisí na tomto:

- **wtype** (virtual-keyboard-v1): funguje na wlroots (Sway, Hyprland), ne
  na KWin ani Mutter pro non-IME klienty.
- **ydotool** (uinput): focus-unaware, vyžaduje setup `ydotoold` + group
  `input`. Není doporučovaný default.
- **libei** (modern framework, KDE 6+ / GNOME 46+): focus-aware, umí key
  down/up. Rust binding ranný. Až dozraje, dá se vrátit k tématu.

### Další risks

- **PipeWire reconnect** při změně default device — engine musí detekovat
  a kontrolovaně ukončit nahrávání (lepší než tichá ztráta)
- **Whisper.cpp model size** — `large-v3` ~3 GB; settings UI musí jasně
  komunikovat při downloadu
- **secret-service v sandboxu** — D-Bus access z hardenovaného daemonu
  může selhat, fallback na env / chmod 600 soubor s warningem (sekce 9)
- **HiDPI fractional scaling KDE Plasma 6** pro FLTK — netesováno na
  1.5×, viz sekce 10
- **systemd hardening konflikty** — sekce 9, hardening jako last-mile
- **Output delivery bez tray** — sekce 5; pokud tray neběží, jen FileSink
  je garantovaný

### Open product questions

- **Diarization u whisper.cpp** — neumí. Cloud backendy umí. UI ukazuje
  profile s `whisper_cpp` jako "channel attribution only", ne "diarization".
- **Mono-mix vs stereo-split default** — per-profile. `meeting` stereo-split,
  `voicememo` mono.
- **Auto-pause při ticho** — explicitní stop default. Auto-pause může vést
  k překvapení.
- **Multi-language autodetect** — out of scope; jazyk per profile.

---

## 13. Out of scope (záměrně)

Aby projekt zůstal udržitelný v M0–M8:

- Video capture
- Editor transkriptů (čistý `.txt`/`.json`, edit někde jinde)
- Cloud upload nahrávek (žádný "sync")
- Vlastní player nahrávek
- Speaker enrollment / voice fingerprinting
- Multi-language auto-detection
- Mobile / non-Linux platformy
- Push-to-talk a type-at-cursor (R&D queue, ne committed feature)
- Privacy / compliance vrstva (consent UI, encrypt-at-rest, redaction)
- Persistent outbox / retry pro session-bound sinky (FileSink je single source of truth)

---

## 14. Project layout

Cargo workspace, jeden git repo. **Layout odpovídá M0 plánu — některé crates
ještě neexistují, vznikají postupně:**

```
zwhisper/
├── Cargo.toml                 # workspace
├── IDEA.md                    # tento dokument
├── README.md
├── LICENSE                    # MIT
├── crates/
│   ├── zwhisper-core/         # lib: audio engine, transcribers, profiles, sinks
│   ├── zwhisper-ipc/          # lib: D-Bus interface (zbus traits, sdílené)
│   ├── zwhisperd/             # bin: daemon (M3+)
│   ├── zwhisper-tray/         # bin: tray + clipboard/notify sinks (M4+)
│   ├── zwhisper-settings/     # bin: FLTK settings (M8+)
│   └── zwhisper-cli/          # bin: CLI; v M0–M2 jediný binary
├── profiles/                  # shipped TOML templates
├── systemd/                   # .service templates (M3+)
├── dbus/                      # D-Bus activation files (M3+)
├── packaging/                 # PKGBUILD pro Arch (M8)
├── docs/
└── tests/
```

V M0 stačí `crates/zwhisper-cli/` jako jednoprocesový binary.
Dělení do daemon/tray/settings vzniká až M3+.

---

## 15. Tech stack — minimum pro M0

| Účel | Crate | Pozn. |
|---|---|---|
| Async runtime | `tokio` | M3+ |
| Audio capture | `gstreamer`, `gstreamer-app` | M0 |
| Config | `serde`, `toml` | M2 |
| CLI | `clap` (derive) | M0 |
| Logy | `tracing`, `tracing-subscriber`, `tracing-appender` | M0, žádný transcript text |
| Error | `thiserror`, `color-eyre` | M0 |
| Time | `chrono` | M0 |
| D-Bus | `zbus` | M3 |
| Tray | `ksni` | M4 |
| Notifications | `notify-rust` | M4 (v tray) |
| GUI (settings) | `fltk` + features `fltk-bundled, use-wayland` | M8 |
| Secrets | `keyring` | M5 |
| HTTP | `reqwest` | M5 |
| WebSocket | `tokio-tungstenite` | M5 |
| JSON | `serde_json` | M1 |

**Externí runtime deps** (přidávají se postupně, ne všechny v M0):

- `pipewire`, `wireplumber` (always)
- `gstreamer`, `gst-plugins-{base,good,bad}` (M0)
- jakékoli `whisper-cli` v PATH (M1) — `optdepends` v PKGBUILD
- `wl-clipboard`, `xclip` (M4 v tray)
- `xdg-desktop-portal` + portal-{kde,gnome,wlr} (M6)
- `libsecret` (M5)

---

## Apendix A — GUI toolkit research data

Detailní data podporující rozhodnutí v sekci 10. Tato tabulka existuje
proto, aby budoucí reviewer (nebo já po půl roce) viděl, na čem rozhodnutí
stojí, a kdy je čas ho přehodnotit.

### Binary size (release + strip + LTO, Linux x86_64)

| Toolkit / backend | Binary | Pozn. |
|---|---|---|
| **FLTK-rs (statické)** | **~1 MB** | nejmenší, oficiální claim |
| iced + glow | 1.5 MB | wgpu varianta 3.1–6 MB |
| egui + eframe (glow) | ~2–4 MB | extrapolace |
| Slint (Winit + FemtoVG) | ~5–8 MB | runtime <300 KiB |
| egui + eframe (wgpu) | ~18 MB | default eframe |
| Tauri | 5 MB + WebKit/WebView | nepoužitelné |

### RAM idle (jednoduché okno)

| Toolkit | RAM idle |
|---|---|
| FLTK-rs | nejmenší (žádný GPU) |
| Slint software renderer | <300 KiB runtime + heap |
| iced + glow | 27 MB |
| iced + wgpu | 76 MB |
| egui 0.28.1 (wgpu) | ~150 MB |
| egui 0.29.1+ (wgpu) | ~300 MB ⚠️ neopravená regrese, [issue #5245](https://github.com/emilk/egui/issues/5245) |

### Tray crate srovnání

| Crate | Závislosti | Pozn. |
|---|---|---|
| **`ksni`** | pure Rust (zbus) | nativně KDE; GNOME potřebuje appindicator extension |
| `tray-icon` (Tauri) | GTK, libxdo, libappindicator | táhne celé GTK |
| `tray-item` | GTK | méně udržované |

### Závěry

1. **FLTK-rs** je primary settings GUI volba. ~1 MB binárka, nejnižší RAM,
   mature codebase. Trade-off: cmake + C++17 v build chain.
2. **Slint** je fallback pokud fractional scaling KDE Plasma 6 selže.
3. **egui** vyloučeno kvůli neopravené RAM regresi #5245 (open late 2024).
4. **wgpu = velký RAM tax** napříč všemi toolkity. Glow nebo software
   renderer šetří desítky MB.
5. **Tauri/Electron** vyloučeno — webview tahá 200+ MB.
6. **Tray musí být sám binárka**, settings spawnuje on-demand. To je
   největší architektonická úspora.

### Zdroje

- [boringcactus 2025 Rust GUI survey](https://www.boringcactus.com/2025/04/13/2025-survey-of-rust-gui-libraries.html)
- [Lukas Kalbertodt: Tauri vs Iced vs egui benchmark](http://lukaskalbertodt.github.io/2023/02/03/tauri-iced-egui-performance-comparison.html)
- [egui RAM regression #5245](https://github.com/emilk/egui/issues/5245)
- [iced binary/RAM discussion #1531](https://github.com/iced-rs/iced/discussions/1531)
- [fltk-rs official site](https://fltk-rs.github.io/fltk-rs/)
- [FLTK 1.4 Wayland + HiDPI (Phoronix)](https://www.phoronix.com/news/FLTK-1.4-Released)
- [FLTK 1.4.5 release (April 2026)](https://github.com/fltk/fltk/releases/tag/release-1.4.5)
- [Slint backends & renderers](https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backends_and_renderers/)
- [ksni crate](https://github.com/iovxw/ksni)
