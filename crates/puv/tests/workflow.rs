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
    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        let output = Command::new(puv())
            .current_dir(dir)
            .args(args)
            .env("PUV_CACHE_DIR", &self.cache)
            .env("PUV_DATA_DIR", &self.data)
            .env("PUV_BIN_DIR", &self.bins)
            .env("PUV_RUNTIME_INDEX", &self.index)
            .env("PUV_REGISTRY_URL", &self.registry)
            .env("RUST_LOG", "warn")
            .output()
            .unwrap();
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
