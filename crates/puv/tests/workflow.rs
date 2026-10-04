use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use sha1::{Digest, Sha1};
use sha2::Sha256;

struct World {
    _root: tempfile::TempDir,
    cache: PathBuf,
    data: PathBuf,
    bins: PathBuf,
    index: PathBuf,
    registry: String,
    zip_hits: Arc<AtomicUsize>,
}

fn puv() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_puv"))
}

impl World {
    fn output(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new(puv())
            .current_dir(dir)
            .args(args)
            .env("PUV_CACHE_DIR", &self.cache)
            .env("PUV_DATA_DIR", &self.data)
            .env("PUV_BIN_DIR", &self.bins)
            .env("PUV_RUNTIME_INDEX", &self.index)
            .env("PUV_REGISTRY_URL", &self.registry)
            .env("RUST_LOG", "warn")
            .output()
            .unwrap()
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        let output = self.output(dir, args);
        if !output.status.success() {
            eprintln!("cmd {args:?} failed");
            eprintln!("stdout {}", String::from_utf8_lossy(&output.stdout));
            eprintln!("stderr {}", String::from_utf8_lossy(&output.stderr));
        }
        output
    }
}

fn write_fake_runtime(archive: &Path) {
    let dir = archive.parent().unwrap().join("runtime-src");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("php"),
        r#"#!/bin/sh
version=8.4.23
prepend=""
code=""
while [ $# -gt 0 ]; do
  case "$1" in
    -d)
      shift
      case "$1" in
        auto_prepend_file=*) prepend="${1#auto_prepend_file=}" ;;
      esac
      shift
      ;;
    -r)
      shift
      code="$1"
      shift
      ;;
    -v|--version)
      printf 'PHP %s (cli) (built: puv-test) (NTS)\n' "$version"
      exit 0
      ;;
    *)
      file="$1"
      shift
      if [ -n "$prepend" ] && [ -x /usr/bin/php ]; then
        exec /usr/bin/php -d "auto_prepend_file=$prepend" "$file" "$@"
      fi
      if [ -x /usr/bin/php ]; then
        exec /usr/bin/php "$file" "$@"
      fi
      printf 'ran %s\n' "$file"
      exit 0
      ;;
  esac
done
if [ -n "$code" ]; then
  case "$code" in
    *PHP_VERSION*) printf '%s\n' "$version" ;;
    *"2 + 2"*) printf '4\n' ;;
    *) printf '%s\n' "$code" ;;
  esac
fi
"#,
    )
    .unwrap();
    let status = Command::new("tar")
        .arg("-C")
        .arg(&dir)
        .arg("-czf")
        .arg(archive)
        .arg("php")
        .status()
        .unwrap();
    assert!(status.success());
}

fn zip_bytes(files: &[(&str, &str)]) -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default()
            .last_modified_time(zip::DateTime::from_date_and_time(2000, 1, 1, 0, 0, 0).unwrap())
            .unix_permissions(0o644);
        for (name, body) in files {
            writer.start_file(*name, options).unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
    }
    cursor.into_inner()
}

fn sha1_hex(bytes: &[u8]) -> String {
    hex::encode(Sha1::digest(bytes))
}

fn metadata(name: &str, versions: &[serde_json::Value]) -> Vec<u8> {
    serde_json::json!({
        "minified": "composer/2.0",
        "packages": { name: versions }
    })
    .to_string()
    .into_bytes()
}

fn release(
    name: &str,
    version: &str,
    require: serde_json::Value,
    url: &str,
    shasum: &str,
    prefix: &str,
    bin: Option<&str>,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "name": name,
        "version": version,
        "version_normalized": format!("{version}.0"),
        "require": require,
        "description": "Fixture package",
        "keywords": ["fixture"],
        "homepage": "https://example.test/package",
        "license": ["MIT"],
        "type": "library",
        "time": "2024-01-02T00:00:00+00:00",
        "authors": [{"name": "Ada", "email": "ada@example.test"}],
        "autoload": {"psr-4": {prefix: "src/"}},
        "dist": {"type": "zip", "url": url, "shasum": shasum}
    });
    if let Some(bin) = bin {
        value["bin"] = serde_json::json!([bin]);
    }
    value
}

fn fixture_world() -> (World, PathBuf) {
    let lib = zip_bytes(&[
        (
            "lib/src/Hello.php",
            "<?php\nnamespace Acme\\Lib;\nclass Hello { public const MSG = \"from-lib\"; }\n",
        ),
        ("lib/composer.json", "{\"name\":\"acme/lib\"}\n"),
    ]);
    let lib_sha = sha1_hex(&lib);
    let shared1 = zip_bytes(&[(
        "shared/src/Version.php",
        "<?php\nnamespace Acme\\Shared;\nclass Version { public const VALUE = \"lib-1\"; }\n",
    )]);
    let shared2 = zip_bytes(&[(
        "shared/src/Version.php",
        "<?php\nnamespace Acme\\Shared;\nclass Version { public const VALUE = \"lib-2\"; }\n",
    )]);
    let one = zip_bytes(&[(
        "one/bin/widget-one",
        "<?php\necho Acme\\Shared\\Version::VALUE, \"\\n\";\n",
    )]);
    let two = zip_bytes(&[(
        "two/bin/widget-two",
        "<?php\necho Acme\\Shared\\Version::VALUE, \"\\n\";\n",
    )]);
    let left = zip_bytes(&[("left/composer.json", "{\"name\":\"acme/left\"}\n")]);
    let right = zip_bytes(&[("right/composer.json", "{\"name\":\"acme/right\"}\n")]);
    let sums = [
        sha1_hex(&lib),
        sha1_hex(&shared1),
        sha1_hex(&shared2),
        sha1_hex(&one),
        sha1_hex(&two),
        sha1_hex(&left),
        sha1_hex(&right),
    ];
    let _ = lib_sha;
    let listener_probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener_probe.local_addr().unwrap().port();
    drop(listener_probe);
    let base = format!("http://127.0.0.1:{port}");
    let urls = [
        format!("{base}/dist/lib.zip"),
        format!("{base}/dist/shared1.zip"),
        format!("{base}/dist/shared2.zip"),
        format!("{base}/dist/one.zip"),
        format!("{base}/dist/two.zip"),
        format!("{base}/dist/left.zip"),
        format!("{base}/dist/right.zip"),
    ];
    let bodies = [lib, shared1, shared2, one, two, left, right];
    let mut routes = HashMap::new();
    for (url, body) in urls.iter().zip(bodies.iter()) {
        let path = url.trim_start_matches(&base).to_string();
        routes.insert(path, body.clone());
    }
    routes.insert(
        "/p2/acme/lib.json".into(),
        metadata(
            "acme/lib",
            &[
                release(
                    "acme/lib",
                    "1.2.0",
                    serde_json::json!({"php": ">=8.1"}),
                    &urls[0],
                    &sums[0],
                    "Acme\\Lib\\",
                    None,
                ),
                release(
                    "acme/lib",
                    "1.0.0",
                    serde_json::json!({"php": ">=8.1"}),
                    &urls[0],
                    &sums[0],
                    "Acme\\Lib\\",
                    None,
                ),
            ],
        ),
    );
    routes.insert(
        "/p2/acme/left.json".into(),
        metadata(
            "acme/left",
            &[release(
                "acme/left",
                "1.0.0",
                serde_json::json!({"php": ">=8.1", "acme/lib": "^1.0"}),
                &urls[5],
                &sums[5],
                "Acme\\Left\\",
                None,
            )],
        ),
    );
    routes.insert(
        "/p2/acme/right.json".into(),
        metadata(
            "acme/right",
            &[release(
                "acme/right",
                "1.0.0",
                serde_json::json!({"php": ">=8.1", "acme/lib": "^2.0"}),
                &urls[6],
                &sums[6],
                "Acme\\Right\\",
                None,
            )],
        ),
    );
    routes.insert(
        "/p2/shared/lib.json".into(),
        metadata(
            "shared/lib",
            &[
                release(
                    "shared/lib",
                    "1.0.0",
                    serde_json::json!({"php": ">=8.1"}),
                    &urls[1],
                    &sums[1],
                    "Acme\\Shared\\",
                    None,
                ),
                release(
                    "shared/lib",
                    "2.0.0",
                    serde_json::json!({"php": ">=8.1"}),
                    &urls[2],
                    &sums[2],
                    "Acme\\Shared\\",
                    None,
                ),
            ],
        ),
    );
    routes.insert(
        "/p2/widget/one.json".into(),
        metadata(
            "widget/one",
            &[release(
                "widget/one",
                "1.0.0",
                serde_json::json!({"php": ">=8.1", "shared/lib": "1.0.0"}),
                &urls[3],
                &sums[3],
                "Acme\\Widget\\",
                Some("bin/widget-one"),
            )],
        ),
    );
    let skeleton = zip_bytes(&[
        (
            "composer.json",
            r#"{"name":"laravel/laravel","require":{"php":">=8.2"}}"#,
        ),
        ("public/index.php", "<?php\necho \"laravel-skeleton\\n\";\n"),
    ]);
    let skeleton_sum = sha1_hex(&skeleton);
    let skeleton_url = format!("{base}/dist/laravel.zip");
    routes.insert("/dist/laravel.zip".into(), skeleton);
    routes.insert(
        "/p2/laravel/laravel.json".into(),
        metadata(
            "laravel/laravel",
            &[release(
                "laravel/laravel",
                "11.0.0",
                serde_json::json!({"php": ">=8.2"}),
                &skeleton_url,
                &skeleton_sum,
                "App\\",
                None,
            )],
        ),
    );
    routes.insert(
        "/api/security-advisories".into(),
        br#"{"advisories":{"acme/lib":[{"advisoryId":"PKSA-test","packageName":"acme/lib","title":"demo hole","link":"https://example.test/advisory","cve":"CVE-2026-1","affectedVersions":"<1.2.0","severity":"high"}]}}"#.to_vec(),
    );
    routes.insert(
        "/p2/widget/two.json".into(),
        metadata(
            "widget/two",
            &[release(
                "widget/two",
                "1.0.0",
                serde_json::json!({"php": ">=8.1", "shared/lib": "2.0.0"}),
                &urls[4],
                &sums[4],
                "Acme\\Widget\\",
                Some("bin/widget-two"),
            )],
        ),
    );
    // The port we probed was closed. serve() binds a new port, so URLs above are wrong.
    // Rebuild by serving on the exact port.
    let world = serve_on(port, routes);
    (world, PathBuf::new())
}

fn serve_on(port: u16, routes: HashMap<String, Vec<u8>>) -> World {
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("cache");
    let data = root.path().join("data");
    let bins = root.path().join("bin");
    let index = root.path().join("index.json");
    let archive = root.path().join("php-8.4.23.tar.gz");
    write_fake_runtime(&archive);
    let sha = hex::encode(Sha256::digest(fs::read(&archive).unwrap()));
    fs::write(
        &index,
        format!(
            r#"[{{"version":"8.4.23","target":"x86_64-unknown-linux-gnu","url":"file://{}","sha256":"{sha}","extensions":["ctype","dom","json","mbstring","phar","tokenizer","xml"]}}]"#,
            archive.display()
        ),
    )
    .unwrap();
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    let zip_hits = Arc::new(AtomicUsize::new(0));
    let hits = Arc::clone(&zip_hits);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            while let Ok(n) = stream.read(&mut tmp) {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|window| window == b"\r\n\r\n") || buf.len() > 16_384 {
                    break;
                }
            }
            let text = String::from_utf8_lossy(&buf);
            let path = text
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/")
                .split('?')
                .next()
                .unwrap_or("/");
            if path.contains("/dist/") {
                hits.fetch_add(1, Ordering::SeqCst);
            }
            if let Some(body) = routes.get(path) {
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(header.as_bytes()).ok();
                stream.write_all(body).ok();
            } else {
                stream
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .ok();
            }
        }
    });
    World {
        _root: root,
        cache,
        data,
        bins,
        index,
        registry: format!("http://127.0.0.1:{port}"),
        zip_hits,
    }
}

#[test]
fn init_creates_a_named_directory() {
    let (world, _) = fixture_world();
    let dir = tempfile::tempdir().unwrap();
    let output = world.run(dir.path(), &["init", "demo"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dir.path().join("demo/puv.toml").is_file());
    assert!(dir.path().join("demo/src/main.php").is_file());
    let manifest = fs::read_to_string(dir.path().join("demo/puv.toml")).unwrap();
    assert!(manifest.contains("name = \"demo\""));
}

#[test]
fn init_refuses_a_second_run_without_force() {
    let (world, _) = fixture_world();
    let dir = tempfile::tempdir().unwrap();
    let output = world.run(dir.path(), &["init"]);
    assert!(output.status.success());
    assert!(dir.path().join("puv.toml").is_file());
    assert!(dir.path().join("src/main.php").is_file());
    assert!(dir.path().join(".gitignore").is_file());
    let again = world.run(dir.path(), &["init"]);
    assert!(!again.status.success());
    let forced = world.run(dir.path(), &["init", "--force"]);
    assert!(forced.status.success());
}

#[test]
fn lock_is_stable_and_conflicts_are_explained() {
    let (world, _) = fixture_world();
    let dir = tempfile::tempdir().unwrap();
    assert!(world.run(dir.path(), &["init"]).status.success());
    fs::write(
        dir.path().join("puv.toml"),
        "[project]\nname = \"demo\"\nphp = \"8.4\"\n\n[dependencies]\n\"acme/lib\" = \"^1.0\"\n",
    )
    .unwrap();
    assert!(world.run(dir.path(), &["lock"]).status.success());
    let first = fs::read(dir.path().join("puv.lock")).unwrap();
    assert!(world.run(dir.path(), &["lock"]).status.success());
    let second = fs::read(dir.path().join("puv.lock")).unwrap();
    assert_eq!(first, second);
    assert!(String::from_utf8_lossy(&first).contains("1.2.0"));

    let conflict = tempfile::tempdir().unwrap();
    assert!(world.run(conflict.path(), &["init"]).status.success());
    fs::write(
        conflict.path().join("puv.toml"),
        "[project]\nname = \"conflict\"\nphp = \"8.4\"\n\n[dependencies]\n\"acme/left\" = \"^1.0\"\n\"acme/right\" = \"^1.0\"\n",
    )
    .unwrap();
    let failed = world.run(conflict.path(), &["lock"]);
    assert!(!failed.status.success());
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        stderr.contains("acme/lib") || stderr.contains("acme/left"),
        "{stderr}"
    );
}

#[test]
fn add_and_sync_share_the_package_cache() {
    let (world, _) = fixture_world();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    assert!(world.run(first.path(), &["init"]).status.success());
    assert!(
        world
            .run(first.path(), &["add", "acme/lib"])
            .status
            .success()
    );
    assert!(world.run(second.path(), &["init"]).status.success());
    assert!(
        world
            .run(second.path(), &["add", "acme/lib"])
            .status
            .success()
    );
    assert_eq!(world.zip_hits.load(Ordering::SeqCst), 1);
    fs::remove_dir_all(first.path().join(".puv")).unwrap();
    assert!(world.run(first.path(), &["sync"]).status.success());
    assert!(first.path().join(".puv/deps/acme/lib").exists());
    assert_eq!(world.zip_hits.load(Ordering::SeqCst), 1);
}

#[test]
fn use_rewrites_the_project_php_shim() {
    let (world, _) = fixture_world();
    let dir = tempfile::tempdir().unwrap();
    assert!(world.run(dir.path(), &["init"]).status.success());
    assert!(world.run(dir.path(), &["use", "8.4"]).status.success());
    let manifest = fs::read_to_string(dir.path().join("puv.toml")).unwrap();
    assert!(manifest.contains("php = \"8.4.23\""), "{manifest}");
    let first = fs::read_to_string(dir.path().join(".puv/bin/php")).unwrap();
    assert!(first.contains("runtimes/8.4.23/php"), "{first}");

    let archive = world.index.parent().unwrap().join("php-8.4.23.tar.gz");
    let sha = hex::encode(Sha256::digest(fs::read(&archive).unwrap()));
    fs::write(
        &world.index,
        format!(
            r#"[{{"version":"8.4.23","target":"x86_64-unknown-linux-gnu","url":"file://{}","sha256":"{sha}","extensions":["ctype","dom","json","mbstring","phar","tokenizer","xml"]}},{{"version":"8.3.1","target":"x86_64-unknown-linux-gnu","url":"file://{}","sha256":"{sha}","extensions":["ctype","dom","json","mbstring","phar","tokenizer","xml"]}}]"#,
            archive.display(),
            archive.display()
        ),
    )
    .unwrap();
    let switched = world.run(dir.path(), &["use", "8.3"]);
    assert!(
        switched.status.success(),
        "{}",
        String::from_utf8_lossy(&switched.stderr)
    );
    let manifest = fs::read_to_string(dir.path().join("puv.toml")).unwrap();
    assert!(manifest.contains("php = \"8.3.1\""), "{manifest}");
    let second = fs::read_to_string(dir.path().join(".puv/bin/php")).unwrap();
    assert!(second.contains("runtimes/8.3.1/php"), "{second}");
    assert!(!second.contains("runtimes/8.4.23/php"));
}

#[test]
fn php_list_install_and_remove_follow_the_requested_line() {
    let (world, _) = fixture_world();
    let dir = tempfile::tempdir().unwrap();
    assert!(world.run(dir.path(), &["init"]).status.success());
    must(&world, dir.path(), &["use", "8.4"]);
    let archive = world.index.parent().unwrap().join("php-8.4.23.tar.gz");
    let sha = hex::encode(Sha256::digest(fs::read(&archive).unwrap()));
    let entry = |version: &str| {
        format!(
            r#"{{"version":"{version}","target":"x86_64-unknown-linux-gnu","url":"file://{}","sha256":"{sha}","extensions":["json"]}}"#,
            archive.display()
        )
    };
    fs::write(
        &world.index,
        format!(
            "[{},{},{}]",
            entry("8.4.1"),
            entry("8.4.23"),
            entry("8.3.1")
        ),
    )
    .unwrap();

    let listed = must(&world, dir.path(), &["php", "list"]);
    assert!(listed.contains("8.4.23  installed"), "{listed}");
    assert!(!listed.contains("8.4.1"), "{listed}");
    assert!(!listed.contains("installation required"), "{listed}");

    let all = must(&world, dir.path(), &["php", "list", "--all"]);
    assert!(all.contains("8.4.23  installed"), "{all}");
    assert!(all.contains("8.4.1  installation required"), "{all}");
    assert!(all.contains("8.3.1  installation required"), "{all}");

    let line = must(&world, dir.path(), &["php", "install", "8.4", "--all"]);
    assert!(line.contains("installed php 8.4.1"), "{line}");
    assert!(line.contains("installed php 8.4.23"), "{line}");
    assert!(!line.contains("8.3.1"), "{line}");
    assert!(world.data.join("runtimes/8.4.1/php").is_file());
    assert!(!world.data.join("runtimes/8.3.1/php").exists());

    must(&world, dir.path(), &["php", "install", "--all"]);
    assert!(world.data.join("runtimes/8.3.1/php").is_file());

    let exact = must(&world, dir.path(), &["php", "remove", "8.4.23"]);
    assert_eq!(exact.trim(), "removed php 8.4.23");
    assert!(world.data.join("runtimes/8.4.1/php").is_file());

    let minor = must(&world, dir.path(), &["php", "remove", "8.4"]);
    assert!(minor.contains("removed php 8.4.1"), "{minor}");
    assert!(!minor.contains("8.4.23"), "{minor}");
    assert!(world.data.join("runtimes/8.3.1/php").is_file());
    assert!(!world.data.join("runtimes/8.4.1").exists());

    must(&world, dir.path(), &["php", "remove", "--all"]);
    assert!(!world.data.join("runtimes/8.3.1").exists());
    let empty = must(&world, dir.path(), &["php", "list"]);
    assert!(empty.trim().is_empty(), "{empty}");
    let again = must(&world, dir.path(), &["php", "list", "--all"]);
    assert!(again.contains("8.4.23  installation required"), "{again}");
    assert!(again.contains("8.3.1  installation required"), "{again}");
    assert!(!again.contains("installed"), "{again}");
}

#[test]
fn run_inline_and_direct_execution_use_the_project_runtime() {
    let (world, _) = fixture_world();
    let dir = tempfile::tempdir().unwrap();
    assert!(world.run(dir.path(), &["init"]).status.success());
    assert!(world.run(dir.path(), &["use", "8.4"]).status.success());
    let version = world.run(dir.path(), &["-c", "echo PHP_VERSION;"]);
    assert!(
        version.status.success(),
        "{}",
        String::from_utf8_lossy(&version.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&version.stdout).trim(), "8.4.23");
    let inline = world.run(dir.path(), &["-c", "echo 2 + 2;"]);
    assert_eq!(String::from_utf8_lossy(&inline.stdout).trim(), "4");
    let ran = world.run(dir.path(), &["run", "src/main.php"]);
    assert!(ran.status.success());
    assert!(String::from_utf8_lossy(&ran.stdout).contains("Hello from"));
    let direct = world.run(dir.path(), &["src/main.php"]);
    assert!(direct.status.success());
    assert_eq!(ran.stdout, direct.stdout);
    let nested = world.run(&dir.path().join("src"), &["run", "main.php"]);
    assert!(
        nested.status.success(),
        "{}",
        String::from_utf8_lossy(&nested.stderr)
    );
    assert!(String::from_utf8_lossy(&nested.stdout).contains("Hello from"));
    let managed = world.data.join("runtimes/8.4.23/php");
    let listed = Command::new(&managed).arg("-v").output().unwrap();
    assert!(String::from_utf8_lossy(&listed.stdout).contains("8.4.23"));
}

#[test]
fn global_tools_stay_isolated_and_prune_drops_orphans() {
    let (world, _) = fixture_world();
    assert!(
        world
            .run(std::env::temp_dir().as_path(), &["use", "--global", "8.4"])
            .status
            .success()
    );
    assert!(
        world
            .run(
                std::env::temp_dir().as_path(),
                &["tool", "install", "widget/one"]
            )
            .status
            .success()
    );
    assert!(
        world
            .run(
                std::env::temp_dir().as_path(),
                &["tool", "install", "widget/two"]
            )
            .status
            .success()
    );
    let one = fs::read_to_string(world.data.join("tools/widget-one/puv.lock")).unwrap();
    let two = fs::read_to_string(world.data.join("tools/widget-two/puv.lock")).unwrap();
    assert!(one.contains("version = \"1.0.0\""));
    assert!(two.contains("version = \"2.0.0\""));
    assert_ne!(
        fs::read_link(world.data.join("tools/widget-one/.puv/deps/shared/lib")).unwrap(),
        fs::read_link(world.data.join("tools/widget-two/.puv/deps/shared/lib")).unwrap()
    );
    let orphan = world.cache.join("packages/orphan");
    fs::create_dir_all(&orphan).unwrap();
    fs::write(orphan.join("marker"), b"x").unwrap();
    assert!(
        world
            .run(std::env::temp_dir().as_path(), &["prune", "--global"])
            .status
            .success()
    );
    assert!(!orphan.exists());
    assert!(
        world
            .data
            .join("tools/widget-one/.puv/deps/shared/lib")
            .exists()
    );
}

#[test]
fn contain_keeps_composer_files_and_syncs_the_locked_versions() {
    let (world, _) = fixture_world();
    let dir = tempfile::tempdir().unwrap();
    let lib_url = format!("{}/dist/lib.zip", world.registry);
    fs::write(
        dir.path().join("composer.json"),
        r#"{"name":"acme/demo","require":{"php":">=8.2","acme/lib":"^1.0"},"scripts":{"test":"phpunit","weird":"@composer dump-autoload"}}"#,
    )
    .unwrap();
    let zip = fs::read(world.cache.join("../")).ok();
    let _ = zip;
    // The zip served by the registry is addressed by URL. contain records that URL from the lock.
    // We do not know the sha1 here; compute it by requesting is unnecessary because contain copies
    // the shasum from composer.lock. Use a matching archive built the same way as the server.
    let archive = zip_bytes(&[
        (
            "lib/src/Hello.php",
            "<?php\nnamespace Acme\\Lib;\nclass Hello { public const MSG = \"from-lib\"; }\n",
        ),
        ("lib/composer.json", "{\"name\":\"acme/lib\"}\n"),
    ]);
    let sum = sha1_hex(&archive);
    // The server route was built with the same zip contents, so the checksum matches.
    fs::write(
        dir.path().join("composer.lock"),
        format!(
            r#"{{"packages":[{{"name":"acme/lib","version":"1.2.0","version_normalized":"1.2.0.0","dist":{{"type":"zip","url":"{lib_url}","shasum":"{sum}"}},"require":{{"php":">=8.1"}},"autoload":{{"psr-4":{{"Acme\\\\Lib\\\\":"src/"}}}}}}],"packages-dev":[]}}"#
        ),
    )
    .unwrap();
    let before = fs::read(dir.path().join("composer.json")).unwrap();
    assert!(world.run(dir.path(), &["contain"]).status.success());
    assert_eq!(fs::read(dir.path().join("composer.json")).unwrap(), before);
    let lock = fs::read_to_string(dir.path().join("puv.lock")).unwrap();
    assert!(lock.contains("acme/lib"));
    assert!(lock.contains("1.2.0"));
    assert!(world.run(dir.path(), &["sync"]).status.success());
    assert!(dir.path().join(".puv/deps/acme/lib").exists());
    assert!(dir.path().join("composer.lock").is_file());
}

#[test]
fn contain_resolves_when_composer_lock_is_missing() {
    let (world, _) = fixture_world();
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("composer.json"),
        r#"{"name":"acme/demo","require":{"php":">=8.2","acme/lib":"^1.0"}}"#,
    )
    .unwrap();
    let result = world.run(dir.path(), &["contain"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("no composer.lock"));
    let lock = fs::read_to_string(dir.path().join("puv.lock")).unwrap();
    assert!(lock.contains("acme/lib"));
    assert!(lock.contains("1.2.0"));
    assert!(dir.path().join("composer.json").is_file());
    assert!(!dir.path().join("composer.lock").exists());
    assert!(world.run(dir.path(), &["sync"]).status.success());
    assert!(dir.path().join(".puv/deps/acme/lib").exists());
}

#[test]
fn check_reports_syntax_errors_and_a_drifted_lock() {
    let (world, _) = fixture_world();
    let dir = tempfile::tempdir().unwrap();
    assert!(world.run(dir.path(), &["init"]).status.success());
    let clean = world.run(dir.path(), &["check"]);
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );
    assert!(
        String::from_utf8_lossy(&clean.stdout).contains("no issues"),
        "{}",
        String::from_utf8_lossy(&clean.stdout)
    );
    fs::write(dir.path().join("src/broken.php"), "<?php\nclass {\n").unwrap();
    let broken = world.run(dir.path(), &["check"]);
    assert!(!broken.status.success());
    assert!(String::from_utf8_lossy(&broken.stdout).contains("syntax.error"));
    fs::remove_file(dir.path().join("src/broken.php")).unwrap();
    fs::write(
        dir.path().join("src/old.php"),
        "<?php\n$x = \"${foo}\";\nfunction demo(Foo $foo = null) {}\n",
    )
    .unwrap();
    let warned = world.run(dir.path(), &["check"]);
    assert!(warned.status.success());
    let text = String::from_utf8_lossy(&warned.stdout);
    assert!(text.contains("deprecated.dollar-curly"));
    assert!(text.contains("deprecated.implicit-nullable"));
    assert!(world.run(dir.path(), &["add", "acme/lib"]).status.success());
    fs::write(
        dir.path().join("puv.toml"),
        "[project]\nname = \"demo\"\nphp = \"8.3\"\n\n[dependencies]\n\"acme/lib\" = \"^1.0\"\n",
    )
    .unwrap();
    let drifted = world.run(dir.path(), &["check"]);
    assert!(!drifted.status.success());
    assert!(String::from_utf8_lossy(&drifted.stdout).contains("project.lock-outdated"));
}

#[test]
fn build_pipeline_is_distinct_from_the_project_script() {
    let (world, _) = fixture_world();
    let dir = tempfile::tempdir().unwrap();
    assert!(world.run(dir.path(), &["init"]).status.success());
    assert!(world.run(dir.path(), &["use", "8.4"]).status.success());
    fs::write(
        dir.path().join("build.php"),
        "<?php\necho getenv('PUV_BUILD') ?: \"no\", \"\\n\";\n",
    )
    .unwrap();
    let manifest = fs::read_to_string(dir.path().join("puv.toml")).unwrap();
    let manifest = manifest.replace("[scripts]\n", "[scripts]\nbuild = \"php build.php\"\n");
    fs::write(dir.path().join("puv.toml"), manifest).unwrap();
    let pipeline = world.run(dir.path(), &["build"]);
    assert!(
        pipeline.status.success(),
        "{}",
        String::from_utf8_lossy(&pipeline.stderr)
    );
    assert!(String::from_utf8_lossy(&pipeline.stdout).contains('1'));
    let script = world.run(dir.path(), &["run", "build"]);
    assert!(
        script.status.success(),
        "{}",
        String::from_utf8_lossy(&script.stderr)
    );
    assert!(String::from_utf8_lossy(&script.stdout).contains("no"));
}

#[test]
fn build_without_a_script_writes_the_package() {
    let (world, _) = fixture_world();
    let dir = tempfile::tempdir().unwrap();
    assert!(world.run(dir.path(), &["init"]).status.success());
    let built = world.run(dir.path(), &["build"]);
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let stdout = String::from_utf8_lossy(&built.stdout);
    assert!(stdout.contains("built "));
    assert!(stdout.contains(".phar"));
    let dist = fs::read_dir(dir.path().join("dist")).unwrap();
    let produced: Vec<_> = dist.map(|entry| entry.unwrap().path()).collect();
    assert_eq!(produced.len(), 1);
    let bytes = fs::read(&produced[0]).unwrap();
    assert!(bytes.windows(4).any(|window| window == b"GBMB"));
    assert!(
        bytes
            .windows(b"Hello from".len())
            .any(|window| window == b"Hello from")
    );
    if Path::new("/usr/bin/php").is_file() {
        let ran = Command::new("/usr/bin/php")
            .arg(&produced[0])
            .output()
            .unwrap();
        assert!(
            ran.status.success(),
            "{}",
            String::from_utf8_lossy(&ran.stderr)
        );
        assert!(String::from_utf8_lossy(&ran.stdout).contains("Hello from"));
    }
}

#[test]
fn package_phar_contains_dependencies_and_runs() {
    let (world, _) = fixture_world();
    let dir = tempfile::tempdir().unwrap();
    assert!(world.run(dir.path(), &["init"]).status.success());
    assert!(world.run(dir.path(), &["add", "acme/lib"]).status.success());
    fs::write(
        dir.path().join("src/main.php"),
        "<?php\ndeclare(strict_types=1);\necho Acme\\Lib\\Hello::MSG, \"\\n\";\n",
    )
    .unwrap();
    let packed = world.run(dir.path(), &["package"]);
    assert!(
        packed.status.success(),
        "{}",
        String::from_utf8_lossy(&packed.stderr)
    );
    let phar = dir.path().join("dist").join(format!(
        "{}.phar",
        dir.path().file_name().unwrap().to_string_lossy()
    ));
    // init names the project from the temp directory name.
    let produced = fs::read_dir(dir.path().join("dist"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let bytes = fs::read(&produced).unwrap();
    assert!(bytes.windows(4).any(|window| window == b"GBMB"));
    assert!(bytes.windows(8).any(|window| window == b"from-lib"));
    if Path::new("/usr/bin/php").is_file() {
        let ran = Command::new("/usr/bin/php")
            .arg(&produced)
            .output()
            .unwrap();
        assert!(
            ran.status.success(),
            "{} {}",
            String::from_utf8_lossy(&ran.stdout),
            String::from_utf8_lossy(&ran.stderr)
        );
        assert!(String::from_utf8_lossy(&ran.stdout).contains("from-lib"));
    }
    let _ = phar;
}

#[test]
fn warm_run_reuses_lockb_until_the_lock_changes() {
    let (world, _) = fixture_world();
    let dir = tempfile::tempdir().unwrap();
    assert!(world.run(dir.path(), &["init"]).status.success());
    assert!(world.run(dir.path(), &["add", "acme/lib"]).status.success());
    fs::remove_file(dir.path().join(".puv/lockb")).unwrap();
    let first = world.run(dir.path(), &["-v", "run", "src/main.php"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(String::from_utf8_lossy(&first.stderr).contains("lock source: toml"));
    let second = world.run(dir.path(), &["-v", "run", "src/main.php"]);
    assert!(String::from_utf8_lossy(&second.stderr).contains("lock source: lockb"));
    let mut lock = fs::read_to_string(dir.path().join("puv.lock")).unwrap();
    lock = lock.replace("1.2.0", "1.2.9");
    fs::write(dir.path().join("puv.lock"), lock).unwrap();
    let third = world.run(dir.path(), &["-v", "run", "src/main.php"]);
    assert!(
        String::from_utf8_lossy(&third.stderr).contains("lock source: toml"),
        "{}",
        String::from_utf8_lossy(&third.stderr)
    );
}

fn must(world: &World, dir: &Path, args: &[&str]) -> String {
    let output = world.run(dir, args);
    assert!(
        output.status.success(),
        "{args:?}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stage(name: &str) {
    eprintln!("\n== {name} ==");
}

/// One pass through the CLI: a Composer project with no PUV files, a project
/// created by `puv init`, a global tool, build and every package format, checks,
/// add/install/remove, load, contain, and prune.
#[test]
fn end_to_end_pipeline() {
    let (world, _) = fixture_world();
    let work = tempfile::tempdir().unwrap();
    let neutral = work.path();

    stage("old composer project, no puv files");
    let legacy = neutral.join("legacy");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(
        legacy.join("composer.json"),
        r#"{"name":"acme/legacy","require":{"php":">=8.2","acme/lib":"^1.0"},"scripts":{"test":"phpunit","weird":"@composer dump-autoload"}}"#,
    )
    .unwrap();
    let archive = zip_bytes(&[
        (
            "lib/src/Hello.php",
            "<?php\nnamespace Acme\\Lib;\nclass Hello { public const MSG = \"from-lib\"; }\n",
        ),
        ("lib/composer.json", "{\"name\":\"acme/lib\"}\n"),
    ]);
    let sum = sha1_hex(&archive);
    let lib_url = format!("{}/dist/lib.zip", world.registry);
    fs::write(
        legacy.join("composer.lock"),
        format!(
            r#"{{"packages":[{{"name":"acme/lib","version":"1.0.0","version_normalized":"1.0.0.0","dist":{{"type":"zip","url":"{lib_url}","shasum":"{sum}"}},"require":{{"php":">=8.1"}},"autoload":{{"psr-4":{{"Acme\\\\Lib\\\\":"src/"}}}}}}],"packages-dev":[]}}"#
        ),
    )
    .unwrap();
    assert!(!legacy.join("puv.toml").exists());

    stage("new project");
    let created = must(&world, neutral, &["init", "app"]);
    assert!(created.contains("initialized app"));
    let app = neutral.join("app");
    assert!(app.join("puv.toml").is_file());
    assert!(app.join("src/main.php").is_file());
    assert!(!app.join("composer.json").exists());

    stage("runtime");
    must(&world, neutral, &["use", "--global", "8.4"]);
    must(&world, &app, &["use", "8.4"]);
    let listed = must(&world, &app, &["php", "list"]);
    assert!(listed.contains("8.4.23  installed"), "{listed}");
    let version = must(&world, &app, &["-c", "echo PHP_VERSION;"]);
    assert_eq!(version.trim(), "8.4.23");

    stage("global tool install, run, uninstall");
    must(&world, neutral, &["tool", "install", "widget/one"]);
    let tools = must(&world, neutral, &["tool", "list"]);
    assert!(tools.contains("widget/one"), "{tools}");
    let shim = world.bins.join("widget-one");
    assert!(shim.is_file(), "missing tool shim {}", shim.display());
    if Path::new("/usr/bin/php").is_file() {
        let ran = Command::new(&shim).output().unwrap();
        assert!(
            ran.status.success(),
            "{}",
            String::from_utf8_lossy(&ran.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&ran.stdout).trim(), "lib-1");
    }
    must(&world, neutral, &["tool", "uninstall", "widget/one"]);
    assert!(!shim.exists());
    assert!(!world.data.join("tools/widget-one").exists());
    let tools = must(&world, neutral, &["tool", "list"]);
    assert!(!tools.contains("widget/one"), "{tools}");

    stage("build and package every format");
    let built = must(&world, &app, &["build"]);
    let phar = app.join("dist/app.phar");
    assert!(built.contains(phar.to_str().unwrap()), "{built}");
    assert!(phar.is_file());
    if Path::new("/usr/bin/php").is_file() {
        let ran = Command::new("/usr/bin/php").arg(&phar).output().unwrap();
        assert!(
            ran.status.success(),
            "{}",
            String::from_utf8_lossy(&ran.stderr)
        );
        assert!(String::from_utf8_lossy(&ran.stdout).contains("Hello from app"));
    }
    let zipped = must(&world, &app, &["package", "--format", "zip"]);
    let zip_path = app.join("dist/app.zip");
    assert!(zipped.contains(zip_path.to_str().unwrap()), "{zipped}");
    let zip_file = fs::File::open(&zip_path).unwrap();
    let mut archive = zip::ZipArchive::new(zip_file).unwrap();
    let names: Vec<String> = (0..archive.len())
        .map(|index| archive.by_index(index).unwrap().name().to_string())
        .collect();
    assert!(names.iter().any(|name| name == "src/main.php"), "{names:?}");
    let packed = must(&world, &app, &["package", "--format", "tar"]);
    let tar_path = app.join("dist/app.tar.gz");
    assert!(packed.contains(tar_path.to_str().unwrap()), "{packed}");
    let listed = Command::new("tar")
        .args(["-tzf"])
        .arg(&tar_path)
        .output()
        .unwrap();
    assert!(listed.status.success());
    assert!(String::from_utf8_lossy(&listed.stdout).contains("src/main.php"));

    stage("check a clean project");
    let report = must(&world, &app, &["check", "--format", "json"]);
    assert!(report.contains("\"diagnostics\": []"), "{report}");

    stage("add, install, remove");
    must(&world, &app, &["add", "acme/lib"]);
    fs::write(
        app.join("src/main.php"),
        "<?php\ndeclare(strict_types=1);\nfwrite(STDOUT, Acme\\Lib\\Hello::MSG . \"\\n\");\n",
    )
    .unwrap();
    if Path::new("/usr/bin/php").is_file() {
        let ran = must(&world, &app, &["run", "src/main.php"]);
        assert!(ran.contains("from-lib"), "{ran}");
    }
    fs::remove_dir_all(app.join(".puv")).unwrap();
    must(&world, &app, &["install"]);
    assert!(app.join(".puv/deps/acme/lib").is_dir());
    must(&world, &app, &["add", "--dev", "widget/one"]);
    let manifest = fs::read_to_string(app.join("puv.toml")).unwrap();
    assert!(manifest.contains("widget/one"), "{manifest}");
    must(&world, &app, &["remove", "widget/one"]);
    let manifest = fs::read_to_string(app.join("puv.toml")).unwrap();
    assert!(!manifest.contains("widget/one"), "{manifest}");
    assert!(manifest.contains("acme/lib"), "{manifest}");

    stage("load a project tool");
    must(&world, &app, &["load", "widget/two"]);
    if Path::new("/usr/bin/php").is_file() {
        let ran = must(&world, &app, &["run", "widget-two"]);
        assert!(ran.contains("lib-2"), "{ran}");
    } else {
        assert!(app.join(".puv/tools").read_dir().unwrap().next().is_some());
    }

    stage("check failures and recovery");
    fs::write(app.join("src/broken.php"), "<?php\nclass {\n").unwrap();
    let broken = world.output(&app, &["check"]);
    assert!(!broken.status.success());
    assert!(String::from_utf8_lossy(&broken.stdout).contains("syntax.error"));
    fs::remove_file(app.join("src/broken.php")).unwrap();
    let saved = fs::read_to_string(app.join("puv.toml")).unwrap();
    assert!(saved.contains("php = \"8.4.23\""), "{saved}");
    let drifted = saved.replace("php = \"8.4.23\"", "php = \"8.3\"");
    fs::write(app.join("puv.toml"), drifted).unwrap();
    let drifted = world.output(&app, &["check"]);
    assert!(!drifted.status.success());
    assert!(String::from_utf8_lossy(&drifted.stdout).contains("project.lock-outdated"));
    fs::write(app.join("puv.toml"), saved).unwrap();
    must(&world, &app, &["check"]);

    stage("contain a locked composer project");
    let contained = must(&world, &legacy, &["contain"]);
    assert!(contained.contains("contained 1 packages"), "{contained}");
    assert!(legacy.join("composer.json").is_file());
    assert!(legacy.join("composer.lock").is_file());
    let lock = fs::read_to_string(legacy.join("puv.lock")).unwrap();
    assert!(lock.contains("1.0.0"), "{lock}");
    assert!(!lock.contains("1.2.0"), "{lock}");
    let manifest = fs::read_to_string(legacy.join("puv.toml")).unwrap();
    assert!(manifest.contains("\"test\" = \"phpunit\""), "{manifest}");
    must(&world, &legacy, &["sync"]);
    assert!(legacy.join(".puv/deps/acme/lib").is_dir());

    stage("contain without a lock, then clean");
    let fresh = neutral.join("fresh");
    fs::create_dir_all(&fresh).unwrap();
    fs::write(
        fresh.join("composer.json"),
        r#"{"name":"acme/fresh","require":{"php":">=8.2","acme/lib":"^1.0"}}"#,
    )
    .unwrap();
    let migrated = world.run(&fresh, &["contain"]);
    assert!(
        migrated.status.success(),
        "{}",
        String::from_utf8_lossy(&migrated.stderr)
    );
    assert!(String::from_utf8_lossy(&migrated.stderr).contains("no composer.lock"));
    let lock = fs::read_to_string(fresh.join("puv.lock")).unwrap();
    assert!(lock.contains("1.2.0"), "{lock}");
    fs::remove_dir_all(&fresh).unwrap();
    fs::create_dir_all(&fresh).unwrap();
    fs::write(
        fresh.join("composer.json"),
        r#"{"name":"acme/fresh","require":{"php":">=8.2"}}"#,
    )
    .unwrap();
    must(&world, &fresh, &["contain", "--clean"]);
    assert!(!fresh.join("composer.json").exists());
    assert!(fresh.join("puv.toml").is_file());

    stage("prune unreferenced cache entries");
    let orphan = world.cache.join("packages/orphan");
    fs::create_dir_all(&orphan).unwrap();
    fs::write(orphan.join("marker"), b"x").unwrap();
    let pruned = must(&world, neutral, &["prune", "--global"]);
    assert!(pruned.contains("removed"), "{pruned}");
    assert!(!orphan.exists());
    assert!(app.join(".puv/deps/acme/lib").is_dir());
    assert!(legacy.join(".puv/deps/acme/lib").is_dir());

    stage("remove the runtime");
    let removed = must(&world, neutral, &["php", "remove", "8.4"]);
    assert!(removed.contains("removed php 8.4.23"), "{removed}");
    let listed = must(&world, neutral, &["php", "list"]);
    assert!(!listed.contains("8.4.23"), "{listed}");
}

#[test]
fn create_builds_a_template_and_contains_it() {
    let (world, _) = fixture_world();
    let dir = tempfile::tempdir().unwrap();
    let created = must(&world, dir.path(), &["create", "laravel", "blog"]);
    assert!(created.contains("laravel/laravel 11.0.0"), "{created}");
    let blog = dir.path().join("blog");
    assert!(blog.join("public/index.php").is_file());
    assert!(blog.join("composer.json").is_file());
    assert!(blog.join("puv.toml").is_file());
    assert!(blog.join("puv.lock").is_file());
    let missing = world.output(dir.path(), &["create"]);
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("template"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
}

#[test]
fn list_why_info_and_audit_describe_the_graph() {
    let (world, _) = fixture_world();
    let dir = tempfile::tempdir().unwrap();
    assert!(world.run(dir.path(), &["init"]).status.success());
    must(&world, dir.path(), &["add", "acme/lib:1.0.0"]);
    let listed = must(&world, dir.path(), &["list"]);
    assert!(listed.contains("php@8.4.23"), "{listed}");
    assert!(listed.contains("ext-json"), "{listed}");
    assert!(listed.contains("acme/lib@1.0.0"), "{listed}");
    let why = must(&world, dir.path(), &["why", "acme/lib"]);
    assert!(why.contains("depends on acme/lib 1.0.0"), "{why}");
    let ext = must(&world, dir.path(), &["why", "ext-json"]);
    assert!(ext.contains("php@8.4.23 provides it"), "{ext}");
    let info = must(&world, dir.path(), &["info", "acme/lib"]);
    assert!(info.contains("acme/lib@1.2.0"), "{info}");
    assert!(info.contains("deps: 1"), "{info}");
    assert!(info.contains("versions: 2"), "{info}");
    assert!(info.contains("Fixture package"), "{info}");
    assert!(info.contains("keywords: fixture"), "{info}");
    assert!(info.contains("installed: 1.0.0"), "{info}");
    assert!(info.contains(".tarball:"), "{info}");
    assert!(info.contains(".shasum:"), "{info}");
    assert!(info.contains("latest: 1.2.0"), "{info}");
    assert!(info.contains("Ada <ada@example.test>"), "{info}");
    assert!(
        info.contains("Published: 2024-01-02T00:00:00+00:00"),
        "{info}"
    );
    let audit = world.output(dir.path(), &["audit"]);
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&audit.stdout),
        String::from_utf8_lossy(&audit.stderr)
    );
    assert!(!audit.status.success(), "{text}");
    assert!(text.contains("CVE-2026-1"), "{text}");
    assert!(text.contains("outdated"), "{text}");
    assert!(text.contains("outside 1.0.0"), "{text}");
}
