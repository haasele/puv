use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::Value;

use crate::model::{PackageRelease, releases_from_v2};
use crate::{Error, Result};

pub trait MetadataProvider: Send + Sync {
    fn releases(&self, name: &str) -> Result<Vec<PackageRelease>>;

    /// Names this provider can enumerate without a network round-trip.
    fn known_names(&self) -> Option<Vec<String>> {
        None
    }
}

pub struct HttpRegistry {
    base: String,
    cache_dir: PathBuf,
    client: reqwest::blocking::Client,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    gate: Arc<Gate>,
}

struct Slot {
    state: Mutex<SlotState>,
    cv: Condvar,
}

enum SlotState {
    Empty,
    Ready(std::result::Result<Vec<PackageRelease>, String>),
}

struct Gate {
    state: Mutex<usize>,
    cv: Condvar,
    limit: usize,
}

impl Gate {
    fn new(limit: usize) -> Self {
        Self {
            state: Mutex::new(0),
            cv: Condvar::new(),
            limit,
        }
    }

    fn enter(&self) {
        let mut held = self.state.lock().expect("gate lock");
        while *held >= self.limit {
            held = self.cv.wait(held).expect("gate wait");
        }
        *held += 1;
    }

    fn leave(&self) {
        let mut held = self.state.lock().expect("gate lock");
        *held = held.saturating_sub(1);
        self.cv.notify_one();
    }
}

impl HttpRegistry {
    pub fn new(cache_dir: impl Into<PathBuf>, base: impl Into<String>) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .user_agent("puv/0.1.0")
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .map_err(|err| Error::new(format!("failed to build http client: {err}")))?;
        Ok(Self {
            base: base.into().trim_end_matches('/').to_string(),
            cache_dir: cache_dir.into(),
            client,
            slots: Mutex::new(HashMap::new()),
            gate: Arc::new(Gate::new(8)),
        })
    }

    pub fn packagist(cache_dir: impl Into<PathBuf>) -> Result<Self> {
        Self::new(cache_dir, "https://repo.packagist.org")
    }

    fn slot(&self, name: &str) -> Arc<Slot> {
        let mut slots = self.slots.lock().expect("registry slots");
        slots
            .entry(name.to_string())
            .or_insert_with(|| {
                Arc::new(Slot {
                    state: Mutex::new(SlotState::Empty),
                    cv: Condvar::new(),
                })
            })
            .clone()
    }

    fn prefetch(&self, name: &str) {
        let name = puv_core::normalize_name(name);
        if is_virtual(&name) {
            return;
        }
        let slot = self.slot(&name);
        {
            let state = slot.state.lock().expect("slot");
            if !matches!(*state, SlotState::Empty) {
                return;
            }
        }
        let base = self.base.clone();
        let cache_dir = self.cache_dir.clone();
        let client = self.client.clone();
        let gate = Arc::clone(&self.gate);
        let slot = Arc::clone(&slot);
        thread::spawn(move || {
            gate.enter();
            let result =
                fetch_releases(&client, &base, &cache_dir, &name).map_err(|err| err.to_string());
            gate.leave();
            let mut state = slot.state.lock().expect("slot");
            *state = SlotState::Ready(result);
            slot.cv.notify_all();
        });
    }
}

impl MetadataProvider for HttpRegistry {
    fn releases(&self, name: &str) -> Result<Vec<PackageRelease>> {
        let name = puv_core::normalize_name(name);
        if is_virtual(&name) {
            return Ok(Vec::new());
        }
        let slot = self.slot(&name);
        {
            let mut state = slot.state.lock().expect("slot");
            if matches!(*state, SlotState::Empty) {
                self.gate.enter();
                let result = fetch_releases(&self.client, &self.base, &self.cache_dir, &name);
                self.gate.leave();
                *state = SlotState::Ready(result.map_err(|err| err.to_string()));
                slot.cv.notify_all();
            }
            while matches!(*state, SlotState::Empty) {
                state = slot.cv.wait(state).expect("slot wait");
            }
            match &*state {
                SlotState::Ready(Ok(releases)) => {
                    let releases = releases.clone();
                    drop(state);
                    for release in &releases {
                        for dependency in release.dependencies.keys() {
                            self.prefetch(dependency);
                        }
                    }
                    Ok(releases)
                }
                SlotState::Ready(Err(err)) => Err(Error::new(err.clone())),
                SlotState::Empty => unreachable!("slot filled before read"),
            }
        }
    }
}

fn is_virtual(name: &str) -> bool {
    name == "php"
        || name.starts_with("ext-")
        || name.starts_with("lib-")
        || name == "composer-plugin-api"
        || name == "composer-runtime-api"
}

fn fetch_releases(
    client: &reqwest::blocking::Client,
    base: &str,
    cache_dir: &Path,
    name: &str,
) -> Result<Vec<PackageRelease>> {
    let url = format!("{base}/p2/{name}.json");
    let cache_path = cache_dir.join(format!("{name}.json"));
    let meta_path = cache_dir.join(format!("{name}.http"));
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let mut request = client.get(&url);
    if let Some(meta) = read_meta(&meta_path) {
        if let Some(etag) = meta.get("etag").and_then(Value::as_str) {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        if let Some(modified) = meta.get("last-modified").and_then(Value::as_str) {
            request = request.header(reqwest::header::IF_MODIFIED_SINCE, modified);
        }
    }
    let response = request
        .send()
        .map_err(|err| Error::new(format!("failed to fetch {url}: {err}")))?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        let cached = fs::read_to_string(&cache_path)
            .map_err(|err| Error::new(format!("failed to read metadata cache: {err}")))?;
        return parse_body(&cached);
    }
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(Vec::new());
    }
    if !response.status().is_success() {
        return Err(Error::new(format!(
            "registry returned {} for {name}",
            response.status()
        )));
    }
    let etag = header_string(&response, reqwest::header::ETAG);
    let modified = header_string(&response, reqwest::header::LAST_MODIFIED);
    let text = response
        .text()
        .map_err(|err| Error::new(format!("failed to read {url}: {err}")))?;
    fs::write(&cache_path, &text).ok();
    let mut meta = serde_json::Map::new();
    if let Some(etag) = etag {
        meta.insert("etag".to_string(), Value::String(etag));
    }
    if let Some(modified) = modified {
        meta.insert("last-modified".to_string(), Value::String(modified));
    }
    if !meta.is_empty() {
        fs::write(&meta_path, Value::Object(meta).to_string()).ok();
    }
    parse_body(&text)
}

fn parse_body(text: &str) -> Result<Vec<PackageRelease>> {
    let value: Value = serde_json::from_str(text)
        .map_err(|err| Error::new(format!("invalid package metadata: {err}")))?;
    releases_from_v2(&value)
}

fn header_string(
    response: &reqwest::blocking::Response,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn read_meta(path: &Path) -> Option<serde_json::Map<String, Value>> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str::<Value>(&text)
        .ok()?
        .as_object()
        .cloned()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advisory {
    pub id: String,
    pub package: String,
    pub title: String,
    pub link: Option<String>,
    pub cve: Option<String>,
    pub affected: String,
    pub severity: Option<String>,
}

impl HttpRegistry {
    pub fn advisories(&self, names: &[String]) -> Result<Vec<Advisory>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let url = advisory_url(&self.base, names);
        self.gate.enter();
        let response = self.client.get(&url).send();
        self.gate.leave();
        let response = response
            .map_err(|err| Error::new(format!("failed to fetch security advisories: {err}")))?;
        if !response.status().is_success() {
            return Err(Error::new(format!(
                "security advisories returned {} for {url}",
                response.status()
            )));
        }
        let body: Value = response
            .json()
            .map_err(|err| Error::new(format!("invalid security advisories: {err}")))?;
        Ok(parse_advisories(&body))
    }
}

pub fn parse_advisories(body: &Value) -> Vec<Advisory> {
    let Some(groups) = body.get("advisories").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut advisories = Vec::new();
    for (package, items) in groups {
        let Some(items) = items.as_array() else {
            continue;
        };
        for item in items {
            let package = item
                .get("packageName")
                .and_then(Value::as_str)
                .unwrap_or(package);
            let title = item
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("security advisory")
                .to_string();
            let id = item
                .get("advisoryId")
                .or_else(|| item.get("advisory_id"))
                .and_then(Value::as_str)
                .unwrap_or(&title)
                .to_string();
            advisories.push(Advisory {
                id,
                package: puv_core::normalize_name(package),
                title,
                link: item.get("link").and_then(Value::as_str).map(str::to_string),
                cve: item
                    .get("cve")
                    .and_then(Value::as_str)
                    .filter(|cve| !cve.is_empty())
                    .map(str::to_string),
                affected: item
                    .get("affectedVersions")
                    .or_else(|| item.get("affected_versions"))
                    .and_then(Value::as_str)
                    .unwrap_or("*")
                    .to_string(),
                severity: item
                    .get("severity")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
    }
    advisories.sort_by(|left, right| (&left.package, &left.id).cmp(&(&right.package, &right.id)));
    advisories
}

fn advisory_url(base: &str, names: &[String]) -> String {
    let host = if base.contains("://repo.packagist.org") {
        "https://packagist.org"
    } else {
        base.trim_end_matches('/')
    };
    let mut url = format!("{host}/api/security-advisories?");
    for (index, name) in names.iter().enumerate() {
        if index > 0 {
            url.push('&');
        }
        url.push_str("packages[]=");
        url.push_str(&form_encode(name));
    }
    url
}

fn form_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// In-memory registry used by tests and by `puv contain` fixtures.
pub struct MemoryRegistry {
    pub packages: HashMap<String, Vec<PackageRelease>>,
}

impl MetadataProvider for MemoryRegistry {
    fn releases(&self, name: &str) -> Result<Vec<PackageRelease>> {
        Ok(self
            .packages
            .get(&puv_core::normalize_name(name))
            .cloned()
            .unwrap_or_default())
    }

    fn known_names(&self) -> Option<Vec<String>> {
        Some(self.packages.keys().cloned().collect())
    }
}
