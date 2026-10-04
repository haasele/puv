# PUV

PUV ist eine eigenständige Toolchain und ein Package-Manager für PHP. Das Programm wird als einzelnes Rust-Binary ausgeliefert. Composer ist die Referenz für Package-Namen, Versionsconstraints, Packagist und das Dependency-Modell. Resolver, Cache, Umgebung, Runtime und CLI sind eine eigene Implementierung.

PUV ist kein Composer-Frontend. Die Kompatibilität mit dem bestehenden Ökosystem ist die Eingangsschnittstelle, nicht das Projektmodell.

## Lebenszyklus

```text
PHP Runtime → Environment → Dependencies → Tools → Scripts → Build → Package
```

Ein neues CLI-Projekt:

```text
puv init
puv use 8.4
puv add symfony/console
puv add --dev phpunit/phpunit
puv run src/main.php
puv check
puv build
puv package
```

## CLI

```text
puv
├── init
├── add
├── install
├── remove
├── sync
├── lock
├── run
├── use
├── load
├── tool
├── build
├── package
├── check
├── contain
├── prune
└── php
```

Zusätzlich: `puv -c '...'` und `puv script.php` (gleichbedeutend mit `puv run script.php`).

| Befehl | Verhalten |
| --- | --- |
| `puv init [dir]` | Legt das Projekt im aktuellen Verzeichnis oder in `dir` an |
| `puv create [template] [dir]` | Holt ein Anwendungsgerüst (laravel, codeigniter, symfony, slim). Ohne Namen fragt ein Terminal nach |
| `puv add <pkg>` | `puv.toml` ändern, auflösen, `puv.lock` schreiben, Umgebung synchronisieren |
| `puv add --dev <pkg>` | wie `add`, in `[dev-dependencies]` |
| `puv install <pkg>` | Alias für `add` |
| `puv install` | Alias für `sync` |
| `puv sync` | Stellt exakt `puv.lock` her und löst nicht neu auf |
| `puv lock` | Schreibt den Lock, ohne die Umgebung zwingend neu aufzubauen |
| `puv lock --upgrade` | Löst den gesamten Graphen neu |
| `puv lock <pkg>` | Hebt nur dieses Paket innerhalb seiner Constraint an |
| `puv remove <pkg>` | Austragen, neu locken, synchronisieren |
| `puv use <version>` | Installiert die passende Runtime und schreibt die konkrete Version. `8.1` wird zum neuesten 8.1-Patch, `8.1.33` bleibt 8.1.33. `.puv/bin/php` zeigt darauf |
| `puv use --global <version>` | Setzt den Benutzer-Pin auf die konkrete Runtime |
| `puv php list` | Nur installierte Runtimes, Status `installed` |
| `puv php list --all` | Zusätzlich fehlende Versionen als `installation required` |
| `puv php install 8.3` | Installiert jede verfügbare 8.3.x-Version. `8.3.32` nur diesen Patch. `--all` ohne Version installiert den ganzen Index und ändert neben einer Version nichts |
| `puv php remove 8.3` | Entfernt jede installierte 8.3.x-Version. `8.3.32` nur diesen Patch. `--all` ohne Version entfernt jede von puv installierte Runtime |
| `puv list` | Installierte Pakete, Extensions und ihre Dependencies |
| `puv why <pkg>` | Zeigt, warum ein Paket oder eine Extension installiert ist |
| `puv info <pkg>` | Registry-Informationen zu einem Paket |
| `puv audit` | Schwachstellen, veraltete Pakete und mögliche Upgrades |
| `puv run <args>` | Datei, dann `[scripts]`, dann projektlokale Tools, dann `.puv/bin`, dann der Befehl |
| `puv run build` | Projektscript `build` |
| `puv build` | Pipeline: Lock prüfen, `sync`, dann das Script `build`. Ohne Script wird das Paket nach `dist/` geschrieben |
| `puv load <pkg>` | Isoliertes projektlokales Tool unter `[tool-dependencies]` |
| `puv tool install <pkg>` | Isoliertes globales Tool |
| `puv contain` | Migriert `composer.json` / `composer.lock` ohne die Originale zu löschen |
| `puv contain --clean` | Entfernt die Composer-Dateien nach der Migration |
| `puv prune --global` | Löscht nicht mehr referenzierte Cache-Artefakte |
| `puv prune --global --all` | Leert den Cache. Installierte Tools und Runtimes bleiben |

`puv install` ohne Argumente synchronisiert. `sync` ist der ausdrückliche Befehl dafür.

## Projektdateien

`puv.toml` ist das Manifest. `puv.lock` (TOML) ist die Source of Truth für den aufgelösten Graphen. Ein abgeleitetes `puv.lockb` liegt unter `.puv/lockb`, wird nicht committet und beschleunigt `run` und `sync`. Weicht der Hash von `puv.lock` ab, wird `puv.lockb` verworfen.

```toml
[project]
name = "my-tool"
php = "8.4"

[dependencies]
symfony/console = "^7.0"

[dev-dependencies]
phpunit/phpunit = "^11.0"

[tool-dependencies]
phpstan/phpstan = "^2.0"

[scripts]
build = "php build.php"
test = "phpunit"

[package]
format = "phar"
entrypoint = "src/main.php"
```

`php = "8.4"` ist eine Minor-Linie. `puv.lock` hält den konkreten Patch unter `[runtime]`.

Weicht der Content-Hash des Manifests vom Lock ab, endet `puv sync` mit der Aufforderung `puv lock`.

## Umgebung und Cache

```text
~/.cache/puv/          packages, runtimes, metadata, indexes
~/.local/share/puv/    installierte Runtimes, globale Tools, Pins
~/.local/bin/          Shims globaler Tools
```

`PUV_CACHE_DIR`, `PUV_DATA_DIR` und `PUV_BIN_DIR` überschreiben diese Pfade.

Projektumgebung:

```text
.puv/
├── env.json
├── autoload.php
├── bin/
├── deps/          → Symlinks in den content-adressierten Cache
└── tools/<name>/  → isolierte Tool-Umgebungen
```

`puv run` injiziert den Autoloader über `auto_prepend_file`. Es gibt kein `vendor/` als primäres Layout. Scripts aus heruntergeladenen `composer.json`-Dateien werden nicht ausgeführt. Composer-Plugins werden nicht emuliert.

## Runtime

Die Runtime-Quelle ist ein austauschbarer Provider. Voreinstellung sind die glibc-Builds von StaticPHP (`gnu-bulk`) für `x86_64-unknown-linux-gnu` und `aarch64-unknown-linux-gnu`. PHP wird nicht aus dem Quelltext gebaut.

Auswahlreihenfolge: `--php`, dann `[project].php`, dann der globale Pin.

Das Extension-Set kommt aus dem Runtime-Index, ergänzt um immer vorhandene Core-Extensions. Fehlt ein gefordertes `ext-*`, schlägt das Lock fehl.

## Registry und Resolver

Protokoll ist Composer v2: `GET /p2/{vendor}/{package}.json`. Der Metadaten-Minifier wird in Rust expandiert. Antworten werden mit `ETag` / `Last-Modified` gecacht.

Der Resolver ist PubGrub. Gewählt wird die höchste stabile Version; Versionen aus einem vorhandenen Lock werden bevorzugt. `php`, `ext-*`, `lib-*`, `composer-plugin-api` und `composer-runtime-api` sind virtuelle Pakete. `provide` / `replace` / `conflict` werden abgebildet. Minimum-Stability im MVP ist `stable`. Dev-Branches sind kein Bestandteil dieser Version.

Integrität: ist `dist.shasum` gesetzt (SHA-1 oder SHA-256, erkannt an der Länge), wird sie geprüft. Zusätzlich speichert der Lock den SHA-256 des Archivs.

## Ausführung

`puv run` setzt die Projekt-Runtime, `PATH` inklusive `.puv/bin` und der Tool-Bins, und das Projektverzeichnis als Working Directory. `php` in Scripts ist ein Shim auf die verwaltete Runtime.

`puv build` exportiert `PUV_BUILD=1`. `puv run build` tut das nicht.

## Packaging

`puv package` schreibt nach `dist/`. Das Default-Format ist PHAR, erzeugt von einem Rust-Writer. Dependencies werden eingepackt. Die PHP-Runtime bleibt außerhalb. `--format zip` und `--format tar` schreiben denselben Dateibaum. Einträge sind sortiert und tragen feste Zeitstempel.

## Checks

`puv check` parst Projektquellen mit tree-sitter-php und prüft die Projektkonsistenz: veralteter Lock, fehlende Runtime, fehlende Extensions, fehlender Entrypoint, fehlende Script-Ziele. Darüber liegen Regeln für entfernte oder veraltete Sprachkonstrukte der gewählten PHP-Minor-Linie, leere Attribute und NUL-Bytes.

## Migration

`puv contain` übernimmt `require`, `require-dev` und, falls vorhanden, den bereits gelockten Graphen aus `composer.lock`, ohne neu aufzulösen. Fehlt `composer.lock`, werden die Abhängigkeiten aus `composer.json` aufgelöst und nach `puv.lock` geschrieben. Einfache String-Scripts werden kopiert. PHP-Callables, `@composer`-Aufrufe und Plugin-Events werden mit einer Warnung übersprungen. Die Composer-Dateien bleiben, bis `puv contain --clean` sie entfernt.

## Grenzen

PUV startet Composer nicht als Subprozess, bettet Composer nicht als Library ein und übernimmt Composer-Dateiformate nicht als primäres Projektmodell. Plugins, Composer-Scripts und historische Sonderfälle werden nicht nachgebaut. Ziel der Migration sind gewöhnliche dependency-basierte Projekte.
