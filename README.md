# puv

puv, the all-in-one php project, package and runtime manager you always wanted.

One Rust binary. It installs the PHP runtime, resolves Packagist packages, and runs the project. Composer package names, version constraints, and the Composer v2 metadata API are the way in. The resolver, cache, environment, and CLI are puv’s own.

puv does not shell out to Composer, and it does not load Composer plugins.

## Install

Linux glibc, `x86_64` and `aarch64`. Runtimes are prebuilt StaticPHP CLI builds, not a compile of php-src.

```shell
cargo install --path crates/puv --locked
```

Rust 1.92 or newer. The binary is `puv`.

Release builds are published from the Actions tab (Release → Run workflow). The installer reads the latest GitHub release:

```text
puv-x86_64-unknown-linux-gnu.tar.gz
puv-aarch64-unknown-linux-gnu.tar.gz
sha256sums.txt
```

Each archive contains a single `puv` binary. `sha256sums.txt` is `sha256sum` output: the hash, two spaces, then the file name.

## A new project

```shell
puv init
puv use 8.4
puv add symfony/console
puv add --dev phpunit/phpunit
puv run src/main.php
puv check
puv build
```

`puv use 8.4` installs the newest 8.4 patch and records that exact version. `puv use 8.4.23` stays on 8.4.23.

`puv init my-app` creates the directory. `puv create` fetches an application skeleton instead:

```shell
puv create laravel blog
puv create codeigniter
puv create symfony
puv create slim
```

In a terminal, `puv create` with no name asks which template to use. A `composer.json` in the skeleton is imported with `contain`.

## An existing Composer project

```shell
puv contain
puv sync
```

`require` and `require-dev` move into `puv.toml`. If `composer.lock` is present, that graph is copied and not resolved again. If it is missing, puv resolves from `composer.json`. Plain shell scripts are copied. PHP callables, `@composer` calls, and plugin events are skipped with a warning. The Composer files stay until `puv contain --clean`.

## Everyday commands

| Command | What it does |
| --- | --- |
| `puv add <pkg>` | Record the dependency, resolve, write `puv.lock`, install |
| `puv add --dev <pkg>` | Same, under `[dev-dependencies]` |
| `puv remove <pkg>` | Drop it, relock, sync |
| `puv install` | Install exactly what `puv.lock` says. No resolve |
| `puv install <pkg>` | Alias of `add` |
| `puv lock` | Resolve and write the lock |
| `puv lock --upgrade` | Resolve again, ignoring locked versions |
| `puv sync` | Same install as `puv install` with no arguments |
| `puv run src/main.php` | Run a file on the project runtime |
| `puv run test` | Run the `test` script from `puv.toml` |
| `puv -c 'echo PHP_VERSION;'` | Run a snippet on the project runtime |
| `puv script.php` | Same as `puv run script.php` |
| `puv load phpstan/phpstan` | Project-local tool, own environment |
| `puv tool install phpstan/phpstan` | Global tool, own environment |
| `puv list` | Installed packages, extensions, and their dependencies |
| `puv why <pkg>` | Why that package or extension is installed |
| `puv info <pkg>` | Version, license, dependencies, dist archive, tags, and authors. `pkg@version` selects one release |
| `puv audit` | Advisories, outdated packages, and upgrades that fit the constraint |
| `puv check` | Syntax and project consistency. Prints a summary even when nothing is wrong |
| `puv build` | Run the `build` script, or write `dist/` when there is no script |
| `puv package` | Write `dist/`. `--format phar`, `zip`, or `tar` |
| `puv php list` | Installed runtimes |
| `puv php list --all` | Installed and not-yet-installed releases |
| `puv php install 8.3` | Every 8.3 patch. `8.3.32` is that patch only. `--all` is the whole index |
| `puv php remove 8.3` | Every installed 8.3 patch. `--all` removes every runtime puv installed |
| `puv prune --global` | Drop cache entries nothing references |

`puv run` sets the project directory as the working directory, puts `.puv/bin` on `PATH`, and prepends the generated autoloader. `vendor/` is not the project layout.

Downloads print a progress line on stderr. A cached archive stays quiet.

## Project files

`puv.toml` is the manifest. `puv.lock` is the resolved graph you commit. `.puv/lockb` is a cache of that lock for faster `run` and `sync`. If `puv.lock` changes, `lockb` is discarded.

```toml
[project]
name = "my-tool"
php = "8.4.23"

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

A drifted manifest stops `puv sync` and tells you to run `puv lock`.

```text
.puv/
├── autoload.php
├── bin/php          project runtime shim
├── deps/            links into the content-addressed cache
└── tools/<name>/    isolated tool environments
```

```text
~/.cache/puv/          packages, runtime archives, metadata
~/.local/share/puv/    installed runtimes, global tools, the user pin
~/.local/bin/          shims for global tools
```

`PUV_CACHE_DIR`, `PUV_DATA_DIR`, and `PUV_BIN_DIR` move those three roots. `PUV_REGISTRY_URL` replaces `https://repo.packagist.org`.

## How resolution works

Metadata is Composer v2: `GET /p2/{vendor}/{package}.json`. The solver is PubGrub. It picks the highest stable release and prefers a version already in the lock. `php`, `ext-*`, `lib-*`, `composer-plugin-api`, and `composer-runtime-api` are virtual. A required extension the runtime does not ship fails the lock by name.

`dist.shasum` is checked when Packagist sends one. The lock also stores the SHA-256 of the archive.

`puv audit` reads Packagist security advisories. A matching advisory exits non-zero. Upgrades that still satisfy the constraint are listed separately from newer releases that fall outside it.

## What this is not

- It does not start `composer`, and it does not link Composer as a library.
- It does not run scripts from a downloaded `composer.json`.
- It does not load Composer plugins.
- It does not compile PHP.
- It does not embed the PHP runtime inside a PHAR. `puv package` packs the application and its dependencies. The runtime stays outside.
- Published runtime targets are Linux glibc only.
