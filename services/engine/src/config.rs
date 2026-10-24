//! `ops.yaml`: the schema, loading (the file, `OPS_*` env overrides, `.env`), secrets and validation.
//!
//! A credential is a literal or `{ env: NAME }` ([`Secret`]); a setting can be either too
//! ([`EnvOr`]). Env values come from the process first, then from `.env` at the workspace root.

use anyhow::{Context as _, Result, anyhow, bail};
use serde::de::{DeserializeOwned, Error as _};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

// ---------------------------------------------------------------------------------------------
// Values that may come from the environment

/// Deserializes `v` as `T`, falling back to parsing its text. `OPS_*` overrides always arrive as
/// strings and YAML gives typed scalars, so `"false"`, `1234` and `"8MiB"` must all work for any
/// `FromStr` type. (`try_parsing` isn't used: it turns a password like `0123` into the number 123.)
fn lenient<T: DeserializeOwned + FromStr>(v: Value) -> Result<T, String>
where
    T::Err: fmt::Display,
{
    let text = match &v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    };
    match serde_json::from_value::<T>(v) {
        Ok(t) => Ok(t),
        Err(e) => match text {
            Some(s) => s
                .parse::<T>()
                .map_err(|pe| format!("invalid value {s:?}: {pe}")),
            None => Err(e.to_string()),
        },
    }
}

/// A setting given literally or read from an env var: `model: all-MiniLM-L6-v2` or
/// `model: { env: EMBEDDINGS_MODEL, default: all-MiniLM-L6-v2 }`. Resolve with [`Ctx::setting`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum EnvOr<T> {
    Literal(T),
    Env {
        env: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        default: Option<T>,
    },
}

impl<'de, T: DeserializeOwned + FromStr> Deserialize<'de> for EnvOr<T>
where
    T::Err: fmt::Display,
{
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        if let Value::Object(m) = &v
            && let Some(env) = m.get("env")
        {
            let env = env
                .as_str()
                .ok_or_else(|| D::Error::custom("`env` must be a variable name"))?
                .to_string();
            let default = match m.get("default") {
                None | Some(Value::Null) => None,
                Some(x) => Some(lenient(x.clone()).map_err(D::Error::custom)?),
            };
            return Ok(EnvOr::Env { env, default });
        }
        lenient(v).map(EnvOr::Literal).map_err(D::Error::custom)
    }
}

/// A credential: a literal, or `{ env: NAME }`. Its `Debug` never shows a literal value.
#[derive(Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Secret {
    Literal(String),
    Env { env: String },
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::String(s) => Ok(Secret::Literal(s)),
            Value::Number(n) => Ok(Secret::Literal(n.to_string())),
            Value::Bool(b) => Ok(Secret::Literal(b.to_string())),
            Value::Object(m) => match m.get("env").and_then(Value::as_str) {
                Some(env) => Ok(Secret::Env {
                    env: env.to_string(),
                }),
                None => Err(D::Error::custom("a secret is a string or { env: NAME }")),
            },
            _ => Err(D::Error::custom("a secret is a string or { env: NAME }")),
        }
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Secret::Literal(_) => write!(f, "Secret(***)"),
            Secret::Env { env } => write!(f, "Secret(env={env})"),
        }
    }
}

impl Secret {
    pub fn resolve(&self, ctx: &Ctx) -> Result<String> {
        match self {
            Secret::Literal(s) => Ok(s.clone()),
            Secret::Env { env } => ctx.env(env).ok_or_else(|| {
                anyhow!("environment variable {env} is not set (checked the process env and .env)")
            }),
        }
    }

    pub fn env_name(&self) -> Option<&str> {
        match self {
            Secret::Literal(_) => None,
            Secret::Env { env } => Some(env),
        }
    }
}

/// A size in bytes, written `1MiB`, `512KiB`, `2GiB`, `100B` or a plain number. 0 means no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize(pub u64);

impl FromStr for ByteSize {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let t = s.trim();
        let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
        let (num, unit) = t.split_at(split);
        let mult: u64 = match unit.trim() {
            "" | "B" => 1,
            "KiB" => 1 << 10,
            "MiB" => 1 << 20,
            "GiB" => 1 << 30,
            _ => 0,
        };
        let bad =
            || format!("invalid size {s:?}: use a number of bytes or B, KiB, MiB, GiB (e.g. 8MiB)");
        if num.is_empty() || mult == 0 {
            return Err(bad());
        }
        num.parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(mult))
            .map(ByteSize)
            .ok_or_else(bad)
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.0;
        match n {
            0 => write!(f, "0"),
            _ if n.is_multiple_of(1 << 30) => write!(f, "{}GiB", n >> 30),
            _ if n.is_multiple_of(1 << 20) => write!(f, "{}MiB", n >> 20),
            _ if n.is_multiple_of(1 << 10) => write!(f, "{}KiB", n >> 10),
            _ => write!(f, "{n}"),
        }
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::Number(n) => n
                .as_u64()
                .map(ByteSize)
                .ok_or_else(|| D::Error::custom(format!("invalid size {n}"))),
            Value::String(s) => s.parse().map_err(D::Error::custom),
            other => Err(D::Error::custom(format!("invalid size {other}"))),
        }
    }
}

impl Serialize for ByteSize {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

// ---------------------------------------------------------------------------------------------
// Schema

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub version: u32,
    pub workspace: Workspace,
    #[serde(default)]
    pub memgraph: MemgraphCfg,
    #[serde(default)]
    pub docker: DockerCfg,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubernetes: Option<K8sCfg>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub shared_libs: Vec<Lib>,
    #[serde(default)]
    pub contracts: Vec<Contract>,
    #[serde(default)]
    pub services: Vec<Service>,
    #[serde(default)]
    pub datastores: Vec<Datastore>,
    #[serde(default)]
    pub semantic: SemanticCfg,
    #[serde(default)]
    pub incidents: IncidentsCfg,
    #[serde(default)]
    pub correlation: CorrelationCfg,
    #[serde(default)]
    pub changes: ChangesCfg,
    #[serde(default)]
    pub indexing: IndexingCfg,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Workspace {
    pub name: String,
    /// `single`, `monorepo` or `multi-repo`.
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct MemgraphCfg {
    pub host: String,
    pub port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<Secret>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<Secret>,
}

impl Default for MemgraphCfg {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 7687,
            username: None,
            password: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct DockerCfg {
    pub compose_files: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct K8sCfg {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubeconfig: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Allow-list: tools refuse every other namespace.
    #[serde(default)]
    pub namespaces: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Python,
    Typescript,
    Javascript,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Lib {
    pub name: String,
    pub path: String,
    pub language: Language,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ContractKind {
    Openapi,
    Library,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Contract {
    pub name: String,
    pub kind: ContractKind,
    /// openapi: the document; library: a `shared_libs` path.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default)]
    pub consumers: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Framework {
    Fastapi,
    Worker,
    Angular,
    None,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Service {
    pub name: String,
    pub path: String,
    pub language: Language,
    pub framework: Framework,
    #[serde(default)]
    pub uses: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<Api>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker: Option<ServiceDocker>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubernetes: Option<ServiceK8s>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs: Option<LogsCfg>,
    #[serde(default)]
    pub datastores: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lsp: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Api {
    pub prefix: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ServiceDocker {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose_service: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServiceK8s {
    pub namespace: String,
    pub selector: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogSourceKind {
    Compose,
    Kubernetes,
    File,
    Command,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LogsCfg {
    pub source: LogSourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DatastoreKind {
    Postgres,
    Redis,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Datastore {
    pub name: String,
    pub kind: DatastoreKind,
    pub url: Secret,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<Secret>,
}

impl Datastore {
    /// The URL with `password` (when set) put into its userinfo, percent-encoded.
    pub fn connection_url(&self, ctx: &Ctx) -> Result<String> {
        let url = self.url.resolve(ctx)?;
        let Some(pw) = &self.password else {
            return Ok(url);
        };
        let pw = percent_encode(&pw.resolve(ctx)?);
        let Some(i) = url.find("://") else {
            bail!("datastore {}: url has no scheme://", self.name)
        };
        let (scheme, rest) = url.split_at(i + 3);
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let (user, host) = match authority.rfind('@') {
            Some(at) => (
                authority[..at].split(':').next().unwrap_or(""),
                &authority[at + 1..],
            ),
            None => ("", authority),
        };
        Ok(format!("{scheme}{user}:{pw}@{host}{path}"))
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '@' => out.push_str("%40"),
            ':' => out.push_str("%3A"),
            '/' => out.push_str("%2F"),
            '%' => out.push_str("%25"),
            '?' => out.push_str("%3F"),
            '#' => out.push_str("%23"),
            ' ' => out.push_str("%20"),
            c => out.push(c),
        }
    }
    out
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SemanticCfg {
    pub embeddings: EmbeddingsCfg,
}

/// Where a local model runs: `cpu`, `cuda`, `all`, `"cuda:1"`, a GPU index, or a list of them.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Device {
    Index(u32),
    List(Vec<u32>),
    Name(String),
}

impl FromStr for Device {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if let Ok(i) = s.parse() {
            return Ok(Device::Index(i));
        }
        if s.starts_with('[') {
            return serde_json::from_str(s)
                .map(Device::List)
                .map_err(|e| format!("invalid device list {s:?}: {e}"));
        }
        Ok(Device::Name(s.to_string()))
    }
}

/// Layer design §4.5(b). Optional fields are `None` when unset (null in YAML).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct EmbeddingsCfg {
    pub enabled: EnvOr<bool>,
    pub model: EnvOr<String>,
    pub device: Option<EnvOr<Device>>,
    pub batch_size: EnvOr<usize>,
    pub chunk_size: EnvOr<usize>,
    pub api_base: Option<EnvOr<String>>,
    pub input_type: EnvOr<String>,
    pub query_input_type: EnvOr<String>,
    pub dimensions: Option<EnvOr<usize>>,
    pub timeout: EnvOr<u64>,
    pub num_retries: EnvOr<u32>,
    pub normalize: EnvOr<bool>,
    pub remote_batch_size: Option<EnvOr<usize>>,
    pub concurrency: EnvOr<usize>,
    pub metric: EnvOr<String>,
    pub scalar_kind: EnvOr<String>,
    pub chunking: ChunkingCfg,
}

impl Default for EmbeddingsCfg {
    fn default() -> Self {
        Self {
            enabled: EnvOr::Literal(true),
            model: EnvOr::Literal("all-MiniLM-L6-v2".into()),
            device: None,
            batch_size: EnvOr::Literal(2000),
            chunk_size: EnvOr::Literal(48),
            api_base: None,
            input_type: EnvOr::Literal("document".into()),
            query_input_type: EnvOr::Literal("query".into()),
            dimensions: None,
            timeout: EnvOr::Literal(60),
            num_retries: EnvOr::Literal(3),
            normalize: EnvOr::Literal(true),
            remote_batch_size: None,
            concurrency: EnvOr::Literal(4),
            metric: EnvOr::Literal("cos".into()),
            scalar_kind: EnvOr::Literal("f32".into()),
            chunking: ChunkingCfg::default(),
        }
    }
}

/// `auto` (the model's own limit) or a token count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxTokens {
    Auto,
    N(usize),
}

impl<'de> Deserialize<'de> for MaxTokens {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::String(s) if s == "auto" => Ok(MaxTokens::Auto),
            Value::String(s) => s.parse().map(MaxTokens::N).map_err(|_| {
                D::Error::custom(format!("max_tokens: {s:?} is not `auto` or a number"))
            }),
            Value::Number(n) => n
                .as_u64()
                .map(|n| MaxTokens::N(n as usize))
                .ok_or_else(|| D::Error::custom("max_tokens must be a positive number")),
            other => Err(D::Error::custom(format!(
                "max_tokens: {other} is not `auto` or a number"
            ))),
        }
    }
}

impl Serialize for MaxTokens {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            MaxTokens::Auto => s.serialize_str("auto"),
            MaxTokens::N(n) => s.serialize_u64(*n as u64),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    #[default]
    Syntax,
    Lines,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Aggregate {
    #[default]
    Max,
    MeanTop2,
}

/// Layer design §4.5.1.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ChunkingCfg {
    pub enabled: bool,
    pub max_tokens: MaxTokens,
    pub remote_max_tokens: usize,
    pub overlap_tokens: usize,
    pub max_chunks_per_symbol: usize,
    pub strategy: Strategy,
    pub aggregate: Aggregate,
}

impl Default for ChunkingCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            max_tokens: MaxTokens::Auto,
            remote_max_tokens: 512,
            overlap_tokens: 32,
            max_chunks_per_symbol: 32,
            strategy: Strategy::Syntax,
            aggregate: Aggregate::Max,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct IncidentsCfg {
    pub dir: String,
}

impl Default for IncidentsCfg {
    fn default() -> Self {
        Self {
            dir: "docs/incidents".into(),
        }
    }
}

/// Flow design §2.1. Each pattern needs a named group `id`; the first match per line wins.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CorrelationCfg {
    pub patterns: Vec<String>,
    /// Cross-service gaps smaller than this are flagged `order_uncertain`.
    pub clock_skew_ms: u64,
}

impl Default for CorrelationCfg {
    fn default() -> Self {
        Self {
            patterns: vec![
                r#"(?i)\b(?:request|trace|correlation)[_-]?id["']?\s*[:=]\s*["']?(?P<id>[A-Za-z0-9-]{8,64})"#.into(),
                r#"(?i)x-request-id["']?\s*[:=]\s*["']?(?P<id>[A-Za-z0-9-]{8,64})"#.into(),
                r#"traceparent["']?\s*[:=]\s*["']?00-(?P<id>[0-9a-f]{32})-"#.into(),
            ],
            clock_skew_ms: 500,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ChangesCfg {
    /// Default look-back for `recent_changes`: `\d+[dhw]`, at most 90 days.
    pub since: String,
}

impl Default for ChangesCfg {
    fn default() -> Self {
        Self { since: "7d".into() }
    }
}

/// Layer design §3.1.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct IndexingCfg {
    pub max_file_bytes: EnvOr<ByteSize>,
    pub skip_minified: bool,
    /// First matching glob (relative to the workspace root) wins.
    pub overrides: Vec<Override>,
}

impl Default for IndexingCfg {
    fn default() -> Self {
        Self {
            max_file_bytes: EnvOr::Literal(ByteSize(1 << 20)),
            skip_minified: true,
            overrides: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Override {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_file_bytes: Option<ByteSize>,
    pub skip: bool,
}

pub mod indexing {
    use super::Ctx;
    use anyhow::{Context as _, Result};

    /// The size limit and skip flag for one file. `max_bytes: None` means no limit.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Limit {
        pub max_bytes: Option<u64>,
        pub skip: bool,
    }

    /// The limit for `relpath` (relative to the workspace root): the first matching override, else
    /// `indexing.max_file_bytes`.
    // ponytail: compiles the override globs on every call; build a GlobSet once if indexing shows it.
    pub fn limit_for(ctx: &Ctx, relpath: &str) -> Result<Limit> {
        let cfg = &ctx.config.indexing;
        let limit = |b: u64| (b > 0).then_some(b);
        for o in &cfg.overrides {
            let glob = super::glob(&o.path)
                .with_context(|| format!("indexing.overrides path {:?}", o.path))?;
            if glob.is_match(relpath) {
                let max = match o.max_file_bytes {
                    Some(b) => b,
                    None => ctx.setting(&cfg.max_file_bytes)?,
                };
                return Ok(Limit {
                    max_bytes: limit(max.0),
                    skip: o.skip,
                });
            }
        }
        Ok(Limit {
            max_bytes: limit(ctx.setting(&cfg.max_file_bytes)?.0),
            skip: false,
        })
    }
}

/// `*` stays within one path segment; `**` crosses segments.
fn glob(pattern: &str) -> Result<globset::GlobMatcher, globset::Error> {
    Ok(globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()?
        .compile_matcher())
}

// ---------------------------------------------------------------------------------------------
// Loading

/// A loaded workspace: its root, its config and the env values it can see.
#[derive(Clone)]
pub struct Ctx {
    pub root: PathBuf,
    pub config: Config,
    process_env: HashMap<String, String>,
    dotenv: HashMap<String, String>,
}

impl fmt::Debug for Ctx {
    // Never print the env maps: they hold credentials.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ctx")
            .field("root", &self.root)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Ctx {
    /// Finds `ops.yaml` in `start` or its nearest ancestor and loads it, with `OPS_*` overrides
    /// from the process env and values from `.env`.
    pub fn load(start: &Path) -> Result<Ctx> {
        Ctx::load_with_env(start, std::env::vars().collect())
    }

    /// [`Ctx::load`] with `env` standing in for the process env (tests; the process env can't be
    /// set safely in Rust 2024).
    pub fn load_with_env(start: &Path, env: HashMap<String, String>) -> Result<Ctx> {
        let root = find_root(start)?;
        let file = root.join("ops.yaml");
        let ops_env: config::Map<String, String> = env
            .iter()
            .filter(|(k, _)| k.starts_with("OPS_"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let config: Config = config::Config::builder()
            .add_source(config::File::from(file.as_path()).format(config::FileFormat::Yaml))
            .add_source(
                config::Environment::with_prefix("OPS")
                    .prefix_separator("_")
                    .separator("__")
                    .source(Some(ops_env)),
            )
            .build()
            .and_then(|c| c.try_deserialize())
            .with_context(|| format!("loading {}", file.display()))?;
        let dotenv = match std::fs::read_to_string(root.join(".env")) {
            Ok(text) => parse_dotenv(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e).context("reading .env"),
        };
        Ok(Ctx {
            root,
            config,
            process_env: env,
            dotenv,
        })
    }

    /// An env value: the process env first, then `.env`.
    pub fn env(&self, name: &str) -> Option<String> {
        self.process_env
            .get(name)
            .or_else(|| self.dotenv.get(name))
            .cloned()
    }

    /// Resolves a setting: its literal, else its env var, else its default.
    pub fn setting<T: FromStr + Clone>(&self, v: &EnvOr<T>) -> Result<T>
    where
        T::Err: fmt::Display,
    {
        match v {
            EnvOr::Literal(t) => Ok(t.clone()),
            EnvOr::Env { env, default } => match self.env(env) {
                Some(s) => s
                    .parse()
                    .map_err(|e| anyhow!("environment variable {env}: {e}")),
                None => default.clone().ok_or_else(|| {
                    anyhow!(
                        "environment variable {env} is not set (checked the process env and .env)"
                    )
                }),
            },
        }
    }

    /// A service by its name or its compose service name.
    pub fn service(&self, name: &str) -> Result<&Service> {
        self.config
            .services
            .iter()
            .find(|s| {
                s.name == name
                    || s.docker.as_ref().and_then(|d| d.compose_service.as_deref()) == Some(name)
            })
            .ok_or_else(|| anyhow!("unknown service {name:?} (not in ops.yaml services)"))
    }

    pub fn datastore(&self, name: &str) -> Result<&Datastore> {
        self.config
            .datastores
            .iter()
            .find(|d| d.name == name)
            .ok_or_else(|| anyhow!("unknown datastore {name:?} (not in ops.yaml datastores)"))
    }

    pub fn lib(&self, name: &str) -> Option<&Lib> {
        self.config.shared_libs.iter().find(|l| l.name == name)
    }

    /// A workspace-relative path made absolute.
    pub fn abs(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

fn find_root(start: &Path) -> Result<PathBuf> {
    let start = if start.is_file() {
        start.parent().unwrap_or(start)
    } else {
        start
    };
    let mut dir = start;
    loop {
        if dir.join("ops.yaml").is_file() {
            return Ok(dir.to_path_buf());
        }
        dir = dir.parent().ok_or_else(|| {
            anyhow!(
                "no ops.yaml in {} or any parent directory (run `ops init`)",
                start.display()
            )
        })?;
    }
}

/// `KEY=VALUE`, `export KEY=VALUE`, `#` comments and quoted values. Never touches the process env.
fn parse_dotenv(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let value = match value.chars().next() {
            Some(q @ ('"' | '\'')) if value.len() >= 2 && value.ends_with(q) => {
                &value[1..value.len() - 1]
            }
            _ => value.split(" #").next().unwrap_or("").trim_end(),
        };
        out.insert(key.trim().to_string(), value.to_string());
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Validation

/// Human-readable problems with `config`; empty when it's consistent.
pub fn validate(config: &Config) -> Vec<String> {
    let mut p = Vec::new();
    if !["single", "monorepo", "multi-repo"].contains(&config.workspace.kind.as_str()) {
        p.push(format!(
            "workspace.kind {:?} must be single, monorepo or multi-repo",
            config.workspace.kind
        ));
    }
    let libs: Vec<&str> = config.shared_libs.iter().map(|l| l.name.as_str()).collect();
    let datastores: Vec<&str> = config.datastores.iter().map(|d| d.name.as_str()).collect();
    for s in &config.services {
        for u in s.uses.iter().filter(|u| !libs.contains(&u.as_str())) {
            p.push(format!("service {}: uses unknown shared lib {u:?}", s.name));
        }
        for d in s
            .datastores
            .iter()
            .filter(|d| !datastores.contains(&d.as_str()))
        {
            p.push(format!("service {}: unknown datastore {d:?}", s.name));
        }
        let ok = match s.framework {
            Framework::Fastapi | Framework::Worker => s.language == Language::Python,
            Framework::Angular => s.language != Language::Python,
            Framework::None => true,
        };
        if !ok {
            p.push(format!(
                "service {}: framework {:?} doesn't fit language {:?}",
                s.name, s.framework, s.language
            ));
        }
    }
    for c in &config.contracts {
        if let Some(prov) = &c.provider
            && !config.services.iter().any(|s| &s.name == prov)
        {
            p.push(format!(
                "contract {}: provider {prov:?} is not a service",
                c.name
            ));
        }
        if c.kind == ContractKind::Library && !config.shared_libs.iter().any(|l| l.path == c.path) {
            p.push(format!(
                "contract {}: library contract path {:?} is not a shared_libs path",
                c.name, c.path
            ));
        }
    }
    let e = &config.semantic.embeddings;
    if let EnvOr::Literal(n) = e.concurrency
        && !(1..=32).contains(&n)
    {
        p.push(format!("semantic.embeddings.concurrency {n} must be 1–32"));
    }
    if let MaxTokens::N(n) = e.chunking.max_tokens
        && e.chunking.overlap_tokens >= n
    {
        p.push(format!(
            "semantic.embeddings.chunking.overlap_tokens {} must be less than max_tokens {n}",
            e.chunking.overlap_tokens
        ));
    }
    for pat in &config.correlation.patterns {
        match regex::Regex::new(pat) {
            Ok(re) if re.capture_names().any(|n| n == Some("id")) => {}
            Ok(_) => p.push(format!(
                "correlation pattern {pat:?} has no named group `id` (?P<id>…)"
            )),
            Err(err) => p.push(format!(
                "correlation pattern {pat:?} doesn't compile: {err}"
            )),
        }
    }
    if let Err(err) = since_days(&config.changes.since) {
        p.push(format!("changes.since: {err}"));
    }
    for o in &config.indexing.overrides {
        if let Err(err) = glob(&o.path) {
            p.push(format!("indexing.overrides path {:?}: {err}", o.path));
        }
    }
    p
}

/// Days in a `\d+[dhw]` look-back, at most 90.
fn since_days(s: &str) -> Result<u64, String> {
    let bad = || format!("{s:?} must look like 7d, 48h or 2w, at most 90 days");
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: u64 = num.parse().map_err(|_| bad())?;
    let days = match unit {
        "d" => n,
        "h" => n.div_ceil(24),
        "w" => n.saturating_mul(7),
        _ => return Err(bad()),
    };
    if days > 90 { Err(bad()) } else { Ok(days) }
}

#[cfg(test)]
mod tests {
    use super::indexing::limit_for;
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The `ops.yaml` example from the base spec, read from the spec itself so the test can't drift.
    fn spec_yaml() -> String {
        let spec = include_str!("../../../docs/superpowers/specs/2026-10-24-ops-support-design.md");
        let section = &spec[spec.find("## Configuration").unwrap()..];
        let start = section.find("```yaml\n").unwrap() + "```yaml\n".len();
        let end = section[start..].find("```").unwrap();
        section[start..start + end].to_string()
    }

    fn tmp_dir() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!(
            "ops-config-test-{}-{}-{nanos}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const MINIMAL: &str = "version: 1\nworkspace: { name: shop, kind: monorepo }\nmemgraph: { host: localhost, port: 7687 }\n";

    /// A workspace in a temp dir with `ops.yaml` (and optionally `.env`), loaded with `env` as the process env.
    fn ctx_with(yaml: &str, dotenv: Option<&str>, env: &[(&str, &str)]) -> Ctx {
        let d = tmp_dir();
        std::fs::write(d.join("ops.yaml"), yaml).unwrap();
        if let Some(e) = dotenv {
            std::fs::write(d.join(".env"), e).unwrap();
        }
        let env: HashMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Ctx::load_with_env(&d, env).unwrap()
    }

    fn ctx_tmp() -> Ctx {
        ctx_with(MINIMAL, None, &[])
    }

    #[test]
    fn parses_spec_example() {
        let c: Config = serde_norway::from_str(&spec_yaml()).unwrap();
        assert_eq!(c.services[0].framework, Framework::Fastapi);
        assert!(matches!(c.datastores[1].url, Secret::Literal(_)));
        assert_eq!(c.kubernetes.as_ref().unwrap().namespaces, vec!["shop"]);
        assert_eq!(c.correlation.clock_skew_ms, 500);
        assert_eq!(c.correlation.patterns.len(), 3); // partial section keeps the default patterns
        assert_eq!(c.semantic.embeddings.chunking.max_chunks_per_symbol, 32); // chunking has 3 of 7 keys
        assert!(!c.indexing.overrides[0].skip); // override without `skip`
        assert!(validate(&c).is_empty(), "{:?}", validate(&c));
    }

    #[test]
    fn spec_example_loads_through_ctx() {
        let ctx = ctx_with(&spec_yaml(), None, &[]);
        assert_eq!(ctx.config.memgraph.port, 7687);
        assert_eq!(
            ctx.config.indexing.overrides[0].max_file_bytes,
            Some(ByteSize(8 << 20))
        );
    }

    #[test]
    fn secret_missing_env_names_var() {
        let err = Secret::Env {
            env: "NOPE_X".into(),
        }
        .resolve(&ctx_tmp())
        .unwrap_err()
        .to_string();
        assert!(err.contains("NOPE_X"), "{err}");
    }

    #[test]
    fn secret_debug_hides_literal() {
        assert!(!format!("{:?}", Secret::Literal("hunter2".into())).contains("hunter2"));
        assert_eq!(
            format!(
                "{:?}",
                Secret::Env {
                    env: "DB_URL".into()
                }
            ),
            "Secret(env=DB_URL)"
        );
        let c = ctx_with(
            &format!(
                "{MINIMAL}datastores:\n  - {{ name: db, kind: postgres, url: \"postgres://u:hunter2@db/x\" }}\n"
            ),
            None,
            &[],
        );
        assert!(!format!("{:?}", c.config).contains("hunter2"));
    }

    #[test]
    fn numeric_literal_secret_is_a_string() {
        let c = ctx_with(
            &format!(
                "{MINIMAL}datastores:\n  - {{ name: c, kind: redis, url: \"redis://cache:6379/0\", password: 1234 }}\n"
            ),
            None,
            &[],
        );
        assert_eq!(
            c.config.datastores[0]
                .password
                .as_ref()
                .unwrap()
                .resolve(&c)
                .unwrap(),
            "1234"
        );
    }

    #[test]
    fn semantic_defaults_and_env() {
        let c = ctx_tmp();
        let e = &c.config.semantic.embeddings;
        assert_eq!(c.setting(&e.model).unwrap(), "all-MiniLM-L6-v2");
        assert_eq!(c.setting(&e.concurrency).unwrap(), 4);
        assert_eq!(c.setting(&e.metric).unwrap(), "cos");
        assert!(c.setting(&e.enabled).unwrap());
        assert!(e.device.is_none() && e.api_base.is_none() && e.dimensions.is_none());
        assert_eq!(c.config.incidents.dir, "docs/incidents");

        let c = ctx_with(
            &format!(
                "{MINIMAL}semantic:\n  embeddings:\n    model: {{ env: EMBEDDINGS_MODEL, default: all-MiniLM-L6-v2 }}\n"
            ),
            Some("EMBEDDINGS_MODEL=openai/text-embedding-3-small\n"),
            &[],
        );
        assert_eq!(
            c.setting(&c.config.semantic.embeddings.model).unwrap(),
            "openai/text-embedding-3-small"
        );

        let c = ctx_with(
            &format!(
                "{MINIMAL}semantic: {{ embeddings: {{ model: {{ env: EMB_X, default: m1 }} }} }}\n"
            ),
            None,
            &[],
        );
        assert_eq!(
            c.setting(&c.config.semantic.embeddings.model).unwrap(),
            "m1"
        ); // default when unset

        let c = ctx_with(
            &format!("{MINIMAL}semantic: {{ embeddings: {{ model: {{ env: EMB_Y }} }} }}\n"),
            None,
            &[],
        );
        assert!(
            c.setting(&c.config.semantic.embeddings.model)
                .unwrap_err()
                .to_string()
                .contains("EMB_Y")
        );

        let c = ctx_with(
            &format!("{MINIMAL}semantic: {{ embeddings: {{ concurrency: 40 }} }}\n"),
            None,
            &[],
        );
        assert!(
            validate(&c.config)
                .iter()
                .any(|p| p.contains("concurrency")),
            "{:?}",
            validate(&c.config)
        );
    }

    #[test]
    fn device_forms() {
        for (yaml, want) in [
            ("cuda", Device::Name("cuda".into())),
            ("1", Device::Index(1)),
            ("\"cuda:1\"", Device::Name("cuda:1".into())),
            ("[0, 1]", Device::List(vec![0, 1])),
        ] {
            let c = ctx_with(
                &format!("{MINIMAL}semantic: {{ embeddings: {{ device: {yaml} }} }}\n"),
                None,
                &[],
            );
            assert_eq!(
                c.setting(c.config.semantic.embeddings.device.as_ref().unwrap())
                    .unwrap(),
                want,
                "{yaml}"
            );
        }
    }

    #[test]
    fn env_override_disables_embeddings() {
        // The core CI job relies on this to run text-only (plan Task 0 / 7c).
        let c = ctx_with(
            MINIMAL,
            None,
            &[("OPS_SEMANTIC__EMBEDDINGS__ENABLED", "false")],
        );
        assert!(!c.setting(&c.config.semantic.embeddings.enabled).unwrap());
    }

    #[test]
    fn byte_size_and_overrides() {
        assert_eq!("8MiB".parse::<ByteSize>().unwrap().0, 8 << 20);
        assert_eq!("512KiB".parse::<ByteSize>().unwrap().0, 512 << 10);
        assert_eq!("2GiB".parse::<ByteSize>().unwrap().0, 2 << 30);
        assert_eq!("1048576".parse::<ByteSize>().unwrap().0, 1 << 20);
        assert_eq!("0".parse::<ByteSize>().unwrap().0, 0);
        assert!("8MB?".parse::<ByteSize>().is_err());

        // YAML string, YAML integer and env string all give the same size.
        let from_str = ctx_with(
            &format!("{MINIMAL}indexing: {{ max_file_bytes: 8MiB }}\n"),
            None,
            &[],
        );
        let from_int = ctx_with(
            &format!("{MINIMAL}indexing: {{ max_file_bytes: 8388608 }}\n"),
            None,
            &[],
        );
        let from_env = ctx_with(
            MINIMAL,
            None,
            &[("OPS_INDEXING__MAX_FILE_BYTES", "8388608")],
        );
        for c in [&from_str, &from_int, &from_env] {
            assert_eq!(
                c.setting(&c.config.indexing.max_file_bytes).unwrap(),
                ByteSize(8 << 20)
            );
        }

        let c = ctx_with(
            &format!(
                "{MINIMAL}indexing:\n  overrides:\n    - {{ path: \"services/legacy/**/*.py\", max_file_bytes: 8MiB }}\n    - {{ path: \"**/generated/**\", skip: true }}\n"
            ),
            None,
            &[],
        );
        assert_eq!(
            limit_for(&c, "services/legacy/a/x.py").unwrap().max_bytes,
            Some(8 << 20)
        );
        assert!(limit_for(&c, "x/generated/y.ts").unwrap().skip);
        assert_eq!(
            limit_for(&c, "services/orders/app/main.py").unwrap(),
            indexing::Limit {
                max_bytes: Some(1 << 20),
                skip: false
            }
        );

        let c = ctx_with(
            &format!("{MINIMAL}indexing: {{ max_file_bytes: 0 }}\n"),
            None,
            &[],
        );
        assert_eq!(limit_for(&c, "a.py").unwrap().max_bytes, None);
    }

    #[test]
    fn bad_byte_size_is_an_error() {
        let d = tmp_dir();
        std::fs::write(
            d.join("ops.yaml"),
            format!("{MINIMAL}indexing: {{ max_file_bytes: 8MB? }}\n"),
        )
        .unwrap();
        let err = format!("{:#}", Ctx::load_with_env(&d, HashMap::new()).unwrap_err());
        assert!(err.contains("8MB?"), "{err}");
    }

    #[test]
    fn correlation_and_changes_defaults() {
        let c = ctx_tmp().config;
        assert_eq!(c.correlation.patterns.len(), 3);
        assert_eq!(c.correlation.clock_skew_ms, 500);
        assert_eq!(c.changes.since, "7d");

        let c = ctx_with(
            &format!("{MINIMAL}correlation: {{ clock_skew_ms: 900 }}\n"),
            None,
            &[],
        )
        .config;
        assert_eq!(
            (c.correlation.clock_skew_ms, c.correlation.patterns.len()),
            (900, 3)
        );

        let c = ctx_with(
            &format!("{MINIMAL}correlation: {{ patterns: [\"req=(\\\\w+)\"] }}\n"),
            None,
            &[],
        )
        .config;
        assert!(
            validate(&c).iter().any(|p| p.contains("id")),
            "{:?}",
            validate(&c)
        );

        for bad in ["1y", "91d", "13w", "x"] {
            let c = ctx_with(
                &format!("{MINIMAL}changes: {{ since: \"{bad}\" }}\n"),
                None,
                &[],
            )
            .config;
            assert!(
                validate(&c).iter().any(|p| p.contains("since")),
                "{bad}: {:?}",
                validate(&c)
            );
        }
        for ok in ["7d", "90d", "12w", "48h"] {
            let c = ctx_with(
                &format!("{MINIMAL}changes: {{ since: \"{ok}\" }}\n"),
                None,
                &[],
            )
            .config;
            assert!(validate(&c).is_empty(), "{ok}: {:?}", validate(&c));
        }

        let tp = regex::Regex::new(&CorrelationCfg::default().patterns[2]).unwrap();
        let caps = tp
            .captures("traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
            .unwrap();
        assert_eq!(&caps["id"], "4bf92f3577b34da6a3ce929d0e0e4736");
        let rid = regex::Regex::new(&CorrelationCfg::default().patterns[0]).unwrap();
        assert_eq!(
            &rid.captures("INFO request_id=abc12345 done").unwrap()["id"],
            "abc12345"
        );
    }

    #[test]
    fn chunking_defaults() {
        let ch = ctx_tmp().config.semantic.embeddings.chunking;
        assert!(ch.enabled);
        assert_eq!(ch.max_tokens, MaxTokens::Auto);
        assert_eq!(
            (
                ch.remote_max_tokens,
                ch.overlap_tokens,
                ch.max_chunks_per_symbol
            ),
            (512, 32, 32)
        );
        assert_eq!(
            (ch.strategy, ch.aggregate),
            (Strategy::Syntax, Aggregate::Max)
        );

        let c = ctx_with(
            &format!("{MINIMAL}semantic: {{ embeddings: {{ chunking: {{ max_tokens: 256, overlap_tokens: 300, strategy: lines, aggregate: mean_top2 }} }} }}\n"),
            None,
            &[],
        )
        .config;
        assert_eq!(c.semantic.embeddings.chunking.max_tokens, MaxTokens::N(256));
        assert_eq!(c.semantic.embeddings.chunking.strategy, Strategy::Lines);
        assert!(
            validate(&c).iter().any(|p| p.contains("overlap_tokens")),
            "{:?}",
            validate(&c)
        );
    }

    #[test]
    fn ops_env_override() {
        let c = ctx_with(MINIMAL, None, &[("OPS_MEMGRAPH__PORT", "7688")]);
        assert_eq!(c.config.memgraph.port, 7688);
    }

    #[test]
    fn env_override_keeps_text_verbatim() {
        // A password like 0123 must not round-trip through a number.
        let c = ctx_with(
            MINIMAL,
            None,
            &[
                ("OPS_MEMGRAPH__PASSWORD", "0123"),
                ("OPS_MEMGRAPH__HOST", "10.0"),
            ],
        );
        assert_eq!(
            c.config
                .memgraph
                .password
                .as_ref()
                .unwrap()
                .resolve(&c)
                .unwrap(),
            "0123"
        );
        assert_eq!(c.config.memgraph.host, "10.0");
    }

    #[test]
    fn dotenv_fallback_and_process_env_wins() {
        let dotenv = "# comment\nFOO=a\nexport BAR=\"b c\"\nBAZ='d'\nQUX=from-file\n";
        let c = ctx_with(MINIMAL, Some(dotenv), &[("QUX", "from-process")]);
        assert_eq!(c.env("FOO").as_deref(), Some("a"));
        assert_eq!(c.env("BAR").as_deref(), Some("b c"));
        assert_eq!(c.env("BAZ").as_deref(), Some("d"));
        assert_eq!(c.env("QUX").as_deref(), Some("from-process"));
        assert_eq!(c.env("NOPE"), None);
    }

    #[test]
    fn load_walks_up_to_ops_yaml() {
        let c = ctx_tmp();
        let sub = c.root.join("services/orders");
        std::fs::create_dir_all(&sub).unwrap();
        let found = Ctx::load_with_env(&sub, HashMap::new()).unwrap();
        assert_eq!(found.root, c.root);
        assert_eq!(found.abs("a/b.py"), c.root.join("a/b.py"));
    }

    #[test]
    fn lookups() {
        let c = ctx_with(
            &format!(
                "{MINIMAL}shared_libs: [{{ name: common, path: libs/common, language: python }}]\nservices:\n  - {{ name: orders-api, path: services/orders, language: python, framework: fastapi, docker: {{ compose_service: orders }} }}\ndatastores:\n  - {{ name: db, kind: postgres, url: {{ env: DB_URL }} }}\n"
            ),
            None,
            &[],
        );
        assert_eq!(c.service("orders").unwrap().name, "orders-api"); // by compose_service
        assert_eq!(c.service("orders-api").unwrap().name, "orders-api");
        assert!(c.service("nope").unwrap_err().to_string().contains("nope"));
        assert_eq!(c.datastore("db").unwrap().kind, DatastoreKind::Postgres);
        assert!(c.datastore("x").is_err());
        assert!(c.lib("common").is_some() && c.lib("x").is_none());
    }

    #[test]
    fn connection_url_injects_password() {
        let c = ctx_tmp();
        let ds = |url: &str, pw: &str| Datastore {
            name: "c".into(),
            kind: DatastoreKind::Redis,
            url: Secret::Literal(url.into()),
            password: Some(Secret::Literal(pw.into())),
        };
        assert_eq!(
            ds("redis://localhost:6379/0", "p@ss")
                .connection_url(&c)
                .unwrap(),
            "redis://:p%40ss@localhost:6379/0"
        );
        assert_eq!(
            ds("postgres://app@db:5432/x", "a b/c?d:e%")
                .connection_url(&c)
                .unwrap(),
            "postgres://app:a%20b%2Fc%3Fd%3Ae%25@db:5432/x"
        );
        assert_eq!(
            ds("postgres://app:old@db/x", "new")
                .connection_url(&c)
                .unwrap(),
            "postgres://app:new@db/x"
        );
        let none = Datastore {
            password: None,
            ..ds("redis://cache:6379/0", "")
        };
        assert_eq!(none.connection_url(&c).unwrap(), "redis://cache:6379/0");
    }

    #[test]
    fn validate_flags_unknown_refs() {
        let yaml = format!(
            "{MINIMAL}shared_libs: [{{ name: common, path: libs/common, language: python }}]
contracts:
  - {{ name: api, kind: openapi, path: api.yaml, provider: ghost }}
  - {{ name: lib, kind: library, path: libs/other }}
services:
  - {{ name: orders, path: services/orders, language: python, framework: fastapi, uses: [nope], datastores: [missing] }}
  - {{ name: web, path: web, language: python, framework: angular }}
"
        );
        let problems = validate(&ctx_with(&yaml, None, &[]).config);
        assert_eq!(problems.len(), 5, "{problems:#?}");
    }
}
