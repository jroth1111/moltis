//! Telemetry helpers for permissioned browser measurement.
//!
//! These types support safe, owned-site measurement of browser-visible
//! identity and interaction distributions so regressions can be detected
//! before anti-bot changes reach production targets.

use {
    crate::{
        snapshot::sanitize_dom_text,
        types::{BrowserBackendKind, BrowserKind, BrowserPreference},
    },
    reqwest::Url,
    serde::{Deserialize, Serialize},
    std::{
        collections::{BTreeMap, BTreeSet},
        fs,
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
    },
    thiserror::Error,
    time::{OffsetDateTime, format_description::well_known::Rfc3339},
};

#[derive(Debug, Error)]
pub enum TelemetryError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    TimeFormat(#[from] time::error::Format),

    #[error(transparent)]
    Http(#[from] reqwest::Error),

    #[error(transparent)]
    UrlParse(#[from] url::ParseError),

    #[error("invalid tls/ja4 sidecar line {line}: {source}")]
    InvalidJsonLine {
        line: usize,
        #[source]
        source: serde_json::Error,
    },

    #[error("invalid probe origin: {0}")]
    InvalidProbeOrigin(String),

    #[error("probe report missing data: {0}")]
    MissingProbeData(String),

    #[error("tls/ja4 sidecar exited before capture completed: {0}")]
    TlsJa4SidecarExited(String),

    #[error("tls/ja4 sidecar did not produce output at {0}")]
    TlsJa4SidecarNoOutput(PathBuf),

    #[error("tls/ja4 sidecar produced no observations at {0}")]
    TlsJa4SidecarEmptyOutput(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FingerprintScreen {
    pub width: u32,
    pub height: u32,
    pub avail_width: u32,
    pub avail_height: u32,
    pub dpr: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FingerprintSnapshot {
    pub session_id: String,
    pub ts: f64,
    pub url: String,
    pub user_agent: String,
    #[serde(default)]
    pub webdriver: Option<bool>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub languages: Option<Vec<String>>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub screen: Option<FingerprintScreen>,
    #[serde(default)]
    pub hardware_concurrency: Option<u32>,
    #[serde(default)]
    pub device_memory: Option<f64>,
    #[serde(default)]
    pub webgl_vendor: Option<String>,
    #[serde(default)]
    pub webgl_renderer: Option<String>,
    #[serde(default)]
    pub plugins_count: Option<u32>,
}

impl FingerprintSnapshot {
    fn sanitize(&mut self) {
        sanitize_string_field(&mut self.session_id);
        sanitize_string_field(&mut self.url);
        sanitize_string_field(&mut self.user_agent);
        sanitize_optional_string_field(&mut self.platform);
        sanitize_optional_string_field(&mut self.language);
        sanitize_optional_vec_string_field(&mut self.languages);
        sanitize_optional_string_field(&mut self.timezone);
        sanitize_optional_string_field(&mut self.webgl_vendor);
        sanitize_optional_string_field(&mut self.webgl_renderer);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FingerprintHeaders {
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub accept_language: Option<String>,
    #[serde(default)]
    pub sec_ch_ua: Option<String>,
    #[serde(default)]
    pub sec_ch_ua_platform: Option<String>,
    #[serde(default)]
    pub sec_fetch_site: Option<String>,
    #[serde(default)]
    pub x_forwarded_for: Option<String>,
}

impl FingerprintHeaders {
    fn sanitize(&mut self) {
        sanitize_optional_string_field(&mut self.user_agent);
        sanitize_optional_string_field(&mut self.accept_language);
        sanitize_optional_string_field(&mut self.sec_ch_ua);
        sanitize_optional_string_field(&mut self.sec_ch_ua_platform);
        sanitize_optional_string_field(&mut self.sec_fetch_site);
        sanitize_optional_string_field(&mut self.x_forwarded_for);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BehaviorPoint {
    pub t: f64,
    #[serde(rename = "type")]
    pub kind: String,
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub buttons: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BehaviorBatchSummary {
    pub count: usize,
    #[serde(default)]
    pub duration_s: Option<f64>,
    pub path_len_px: f64,
    pub straight_line_px: f64,
    #[serde(default)]
    pub straightness: Option<f64>,
    #[serde(default)]
    pub mean_dt_s: Option<f64>,
    #[serde(default)]
    pub max_idle_gap_s: Option<f64>,
    #[serde(default)]
    pub mean_step_px: Option<f64>,
    #[serde(default)]
    pub mean_speed_px_s: Option<f64>,
    #[serde(default)]
    pub event_rate_hz: Option<f64>,
}

impl BehaviorBatchSummary {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            count: 0,
            duration_s: None,
            path_len_px: 0.0,
            straight_line_px: 0.0,
            straightness: None,
            mean_dt_s: None,
            max_idle_gap_s: None,
            mean_step_px: None,
            mean_speed_px_s: None,
            event_rate_hz: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestSequenceEvent {
    pub run_id: String,
    pub request_index: usize,
    pub request_ts_ms: f64,
    pub path: String,
    pub method: String,
    pub status_code: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestSequenceSummary {
    pub request_count: usize,
    #[serde(default)]
    pub first_path: Option<String>,
    #[serde(default)]
    pub last_path: Option<String>,
    pub distinct_path_count: usize,
    #[serde(default)]
    pub path_sequence: Vec<String>,
    #[serde(default)]
    pub mean_gap_ms: Option<f64>,
    #[serde(default)]
    pub max_gap_ms: Option<f64>,
}

impl RequestSequenceSummary {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            request_count: 0,
            first_path: None,
            last_path: None,
            distinct_path_count: 0,
            path_sequence: Vec::new(),
            mean_gap_ms: None,
            max_gap_ms: None,
        }
    }

    fn sanitize(&mut self) {
        sanitize_optional_string_field(&mut self.first_path);
        sanitize_optional_string_field(&mut self.last_path);
        sanitize_vec_string_field(&mut self.path_sequence);
    }
}

fn sanitize_string_field(value: &mut String) {
    *value = sanitize_dom_text(value).into_owned();
}

fn sanitize_optional_string_field(value: &mut Option<String>) {
    if let Some(inner) = value {
        sanitize_string_field(inner);
    }
}

fn sanitize_vec_string_field(values: &mut Vec<String>) {
    values.iter_mut().for_each(sanitize_string_field);
}

fn sanitize_optional_vec_string_field(values: &mut Option<Vec<String>>) {
    if let Some(inner) = values {
        sanitize_vec_string_field(inner);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeFingerprintReport {
    pub session_id: String,
    pub body: FingerprintSnapshot,
    #[serde(default)]
    pub headers: FingerprintHeaders,
}

impl ProbeFingerprintReport {
    fn sanitize(&mut self) {
        sanitize_string_field(&mut self.session_id);
        self.body.sanitize();
        self.headers.sanitize();
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeBehaviorReport {
    pub session_id: String,
    #[serde(default)]
    pub batches: Vec<BehaviorBatchSummary>,
    pub summary: BehaviorBatchSummary,
}

impl ProbeBehaviorReport {
    fn sanitize(&mut self) {
        sanitize_string_field(&mut self.session_id);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeSequenceReport {
    pub run_id: String,
    pub summary: RequestSequenceSummary,
}

impl ProbeSequenceReport {
    fn sanitize(&mut self) {
        sanitize_string_field(&mut self.run_id);
        self.summary.sanitize();
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeCanarySpec {
    pub origin: String,
    #[serde(default)]
    pub browser: BrowserPreference,
    #[serde(default)]
    pub backends: Vec<BrowserBackendKind>,
    #[serde(default)]
    pub policy: ProbeTelemetryPolicy,
    #[serde(default)]
    pub tls_sidecar: Option<TlsJa4SidecarConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeCanaryVerdict {
    Clean,
    Drifted,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeBackendReport {
    pub backend: BrowserBackendKind,
    pub verdict: ProbeCanaryVerdict,
    #[serde(default)]
    pub evidence: Option<ProbeRunEvidence>,
    #[serde(default)]
    pub baseline: Option<ProbeBaselineUpdate>,
    #[serde(default)]
    pub error: Option<String>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeCanaryReport {
    pub origin: String,
    pub browser: BrowserPreference,
    pub backends: Vec<ProbeBackendReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeBrowserFamily {
    Chrome,
    Chromium,
    Edge,
    Brave,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeProxyMode {
    None,
    Residential,
    Datacenter,
    Socks5,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeRunProfile {
    pub browser_kind: BrowserKind,
    pub browser_family: ProbeBrowserFamily,
    pub browser_version: String,
    pub backend: BrowserBackendKind,
    pub headless: bool,
    pub proxy_mode: ProbeProxyMode,
    pub browser_binary_basename: String,
    pub launch_profile_hash: String,
}

impl ProbeBrowserFamily {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::Chromium => "chromium",
            Self::Edge => "edge",
            Self::Brave => "brave",
            Self::Other => "other",
        }
    }
}

impl ProbeProxyMode {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Residential => "residential",
            Self::Datacenter => "datacenter",
            Self::Socks5 => "socks5",
            Self::Other => "other",
        }
    }
}

impl ProbeCanarySpec {
    #[must_use]
    pub fn effective_backends(&self) -> Vec<BrowserBackendKind> {
        let backends = if self.backends.is_empty() {
            vec![
                BrowserBackendKind::Chromiumoxide,
                BrowserBackendKind::Patchright,
            ]
        } else {
            self.backends.clone()
        };
        let mut deduped = Vec::with_capacity(backends.len());
        for backend in backends {
            if !deduped.contains(&backend) {
                deduped.push(backend);
            }
        }
        deduped
    }

    pub fn validated_origin(&self) -> Result<Url, TelemetryError> {
        let origin = Url::parse(&self.origin)?;
        match origin.scheme() {
            "http" => {
                if self.tls_sidecar.is_some() {
                    return Err(TelemetryError::InvalidProbeOrigin(
                        "tls/ja4 capture requires an https probe origin".to_string(),
                    ));
                }
            },
            "https" => {},
            other => {
                return Err(TelemetryError::InvalidProbeOrigin(format!(
                    "unsupported probe origin scheme '{other}'"
                )));
            },
        }
        Ok(origin)
    }
}

impl ProbeRunProfile {
    #[must_use]
    pub fn storage_key(&self) -> String {
        let headless = if self.headless { "headless" } else { "headed" };
        format!(
            "{}-{}-{}-{}-{}-{}",
            self.browser_kind.as_str(),
            self.browser_family.as_str(),
            sanitize_storage_component(&self.browser_version),
            self.backend,
            headless,
            self.proxy_mode.as_str(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeRunEvidence {
    pub profile: ProbeRunProfile,
    pub fingerprint: FingerprintSnapshot,
    pub headers: FingerprintHeaders,
    pub behavior: BehaviorBatchSummary,
    pub request_sequence: RequestSequenceSummary,
    #[serde(default)]
    pub tls_ja4: Option<TlsJa4Summary>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeBaselineRecord {
    pub key: String,
    pub saved_at: String,
    pub evidence: ProbeRunEvidence,
}

#[derive(Debug, Clone)]
pub struct ProbeBaselineStore {
    root: PathBuf,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsJa4CollectionMode {
    Disabled,
    #[default]
    OnDemand,
    Always,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeTelemetryPolicy {
    pub persist_baselines: bool,
    pub tls_ja4_mode: TlsJa4CollectionMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeCapturePlan {
    pub persist_baseline: bool,
    pub capture_tls_ja4: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeBaselineUpdate {
    #[serde(default)]
    pub previous: Option<ProbeBaselineRecord>,
    pub current: ProbeBaselineRecord,
    #[serde(default)]
    pub drift: Option<ProbeRunDrift>,
}

impl Default for ProbeTelemetryPolicy {
    fn default() -> Self {
        Self {
            persist_baselines: true,
            tls_ja4_mode: TlsJa4CollectionMode::OnDemand,
        }
    }
}

impl Default for ProbeBaselineStore {
    fn default() -> Self {
        Self::new(default_probe_baseline_dir())
    }
}

impl ProbeBaselineStore {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn baseline_path(&self, profile: &ProbeRunProfile) -> PathBuf {
        self.root.join(format!("{}.json", profile.storage_key()))
    }

    pub fn save(&self, evidence: &ProbeRunEvidence) -> Result<ProbeBaselineRecord, TelemetryError> {
        fs::create_dir_all(&self.root)?;
        let record = ProbeBaselineRecord {
            key: evidence.profile.storage_key(),
            saved_at: OffsetDateTime::now_utc().format(&Rfc3339)?,
            evidence: evidence.clone(),
        };
        fs::write(
            self.baseline_path(&evidence.profile),
            serde_json::to_vec_pretty(&record)?,
        )?;
        Ok(record)
    }

    pub fn load(
        &self,
        profile: &ProbeRunProfile,
    ) -> Result<Option<ProbeBaselineRecord>, TelemetryError> {
        let path = self.baseline_path(profile);
        if !path.exists() {
            return Ok(None);
        }

        let record = serde_json::from_slice(&fs::read(path)?)?;
        Ok(Some(record))
    }

    pub fn compare_with_thresholds(
        &self,
        current: &ProbeRunEvidence,
        thresholds: &ProbeDriftThresholds,
    ) -> Result<Option<ProbeRunDrift>, TelemetryError> {
        let Some(baseline) = self.load(&current.profile)? else {
            return Ok(None);
        };

        Ok(Some(compare_probe_run_with_thresholds(
            &baseline.evidence,
            current,
            thresholds,
        )))
    }

    pub fn compare(
        &self,
        current: &ProbeRunEvidence,
    ) -> Result<Option<ProbeRunDrift>, TelemetryError> {
        self.compare_with_thresholds(current, &ProbeDriftThresholds::default())
    }
}

impl ProbeTelemetryPolicy {
    #[must_use]
    pub fn capture_plan(&self, request_tls_ja4: bool) -> ProbeCapturePlan {
        let capture_tls_ja4 = match self.tls_ja4_mode {
            TlsJa4CollectionMode::Disabled => false,
            TlsJa4CollectionMode::OnDemand => request_tls_ja4,
            TlsJa4CollectionMode::Always => true,
        };

        ProbeCapturePlan {
            persist_baseline: self.persist_baselines,
            capture_tls_ja4,
        }
    }

    pub fn persist_and_compare_baseline(
        &self,
        store: &ProbeBaselineStore,
        current: &ProbeRunEvidence,
    ) -> Result<Option<ProbeBaselineUpdate>, TelemetryError> {
        if !self.persist_baselines {
            return Ok(None);
        }

        let previous = store.load(&current.profile)?;
        let drift = previous
            .as_ref()
            .map(|baseline| compare_probe_run(&baseline.evidence, current));
        let current = store.save(current)?;

        Ok(Some(ProbeBaselineUpdate {
            previous,
            current,
            drift,
        }))
    }
}

impl TlsJa4SidecarConfig {
    #[must_use]
    pub fn output_dir(&self) -> PathBuf {
        self.output_dir
            .clone()
            .unwrap_or_else(default_tls_ja4_sidecar_dir)
    }

    #[must_use]
    pub fn output_path(&self, run_id: &str) -> PathBuf {
        self.output_dir()
            .join(format!("{}.jsonl", sanitize_storage_component(run_id)))
    }

    #[must_use]
    pub fn resolved_args(&self, output_path: &Path) -> Vec<String> {
        let output_path = output_path.to_string_lossy();
        self.args
            .iter()
            .map(|arg| arg.replace("{output_path}", &output_path))
            .collect()
    }

    #[must_use]
    pub fn resolved_env(&self, output_path: &Path) -> BTreeMap<String, String> {
        let output_path = output_path.to_string_lossy();
        self.env
            .iter()
            .map(|(key, value)| (key.clone(), value.replace("{output_path}", &output_path)))
            .collect()
    }

    pub fn spawn(&self, run_id: &str) -> Result<TlsJa4SidecarProcess, TelemetryError> {
        let output_path = self.output_path(run_id);
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut command = Command::new(self.command.trim());
        command.args(self.resolved_args(&output_path));
        if let Some(working_dir) = &self.working_dir {
            command.current_dir(working_dir);
        }
        for (key, value) in self.resolved_env(&output_path) {
            command.env(key, value);
        }
        command.env("MOLTIS_TLS_JA4_OUTPUT_PATH", &output_path);
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());

        let child = command.spawn()?;
        Ok(TlsJa4SidecarProcess { child, output_path })
    }
}

impl TlsJa4SidecarProcess {
    #[must_use]
    pub fn output_path(&self) -> &Path {
        &self.output_path
    }

    pub fn stop(mut self) -> Result<(), TelemetryError> {
        if self.child.try_wait()?.is_none() {
            self.child.kill()?;
            let _ = self.child.wait()?;
        }
        Ok(())
    }

    pub fn load_observations(&self) -> Result<Vec<TlsJa4Observation>, TelemetryError> {
        load_tls_ja4_observations(self.output_path())
    }

    pub fn load_summary(&self) -> Result<TlsJa4Summary, TelemetryError> {
        Ok(summarize_tls_ja4_observations(&self.load_observations()?))
    }

    pub fn ensure_running(&mut self) -> Result<(), TelemetryError> {
        if let Some(status) = self.child.try_wait()? {
            return Err(TelemetryError::TlsJa4SidecarExited(status.to_string()));
        }
        Ok(())
    }

    pub fn stop_and_load_summary(mut self) -> Result<TlsJa4Summary, TelemetryError> {
        if self.child.try_wait()?.is_none() {
            self.child.kill()?;
            let _ = self.child.wait()?;
        }
        if !self.output_path.exists() {
            return Err(TelemetryError::TlsJa4SidecarNoOutput(self.output_path));
        }
        let observations = load_tls_ja4_observations(&self.output_path)?;
        if observations.is_empty() {
            return Err(TelemetryError::TlsJa4SidecarEmptyOutput(self.output_path));
        }
        Ok(summarize_tls_ja4_observations(&observations))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeDriftThresholds {
    pub mean_gap_ratio: f64,
    pub max_gap_ratio: f64,
    pub behavior_count_ratio: f64,
    pub behavior_path_ratio: f64,
    pub behavior_straightness_ratio: f64,
    pub behavior_mean_dt_ratio: f64,
    pub behavior_event_rate_ratio: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TlsJa4Observation {
    pub ts_ms: f64,
    #[serde(default)]
    pub ja4: Option<String>,
    #[serde(default)]
    pub ja4s: Option<String>,
    #[serde(default)]
    pub alpn: Option<String>,
    #[serde(default)]
    pub tls_version: Option<String>,
    #[serde(default)]
    pub cipher_suite: Option<String>,
    #[serde(default)]
    pub server_name: Option<String>,
    #[serde(default)]
    pub destination_addr: Option<String>,
    #[serde(default)]
    pub destination_port: Option<u16>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TlsJa4Summary {
    pub event_count: usize,
    #[serde(default)]
    pub distinct_ja4: Vec<String>,
    #[serde(default)]
    pub distinct_ja4s: Vec<String>,
    #[serde(default)]
    pub distinct_alpn: Vec<String>,
    #[serde(default)]
    pub distinct_tls_versions: Vec<String>,
    #[serde(default)]
    pub first_ja4: Option<String>,
    #[serde(default)]
    pub last_ja4: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlsJa4SidecarConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub output_dir: Option<PathBuf>,
}

#[derive(Debug)]
pub struct TlsJa4SidecarProcess {
    child: Child,
    output_path: PathBuf,
}

impl Default for ProbeDriftThresholds {
    fn default() -> Self {
        Self {
            mean_gap_ratio: 0.35,
            max_gap_ratio: 0.50,
            behavior_count_ratio: 0.35,
            behavior_path_ratio: 0.35,
            behavior_straightness_ratio: 0.20,
            behavior_mean_dt_ratio: 0.35,
            behavior_event_rate_ratio: 0.35,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeDriftKind {
    BrowserKindChanged,
    BrowserFamilyChanged,
    BrowserVersionChanged,
    BackendChanged,
    HeadlessChanged,
    ProxyModeChanged,
    BrowserBinaryChanged,
    LaunchProfileHashChanged,
    UserAgentChanged,
    AcceptLanguageChanged,
    WebdriverChanged,
    PlatformChanged,
    TimezoneChanged,
    ScreenChanged,
    HardwareConcurrencyChanged,
    DeviceMemoryChanged,
    BehaviorCountDrift,
    BehaviorPathDrift,
    BehaviorStraightnessDrift,
    BehaviorMeanDtDrift,
    BehaviorEventRateDrift,
    RequestCountChanged,
    PathSequenceChanged,
    MeanGapDrift,
    MaxGapDrift,
    TlsJa4Added,
    TlsJa4Missing,
    TlsJa4Changed,
    TlsJa4sChanged,
    TlsAlpnChanged,
    TlsVersionChanged,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeDriftIssue {
    pub kind: ProbeDriftKind,
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProbeRunDrift {
    pub issues: Vec<ProbeDriftIssue>,
}

impl ProbeRunDrift {
    #[must_use]
    pub fn consistent(&self) -> bool {
        self.issues.is_empty()
    }
}

fn sanitize_storage_component(component: &str) -> String {
    let mut sanitized = String::with_capacity(component.len());
    let mut last_was_separator = false;

    for ch in component.chars() {
        if ch.is_ascii_alphanumeric() {
            sanitized.push(ch.to_ascii_lowercase());
            last_was_separator = false;
        } else if !last_was_separator {
            sanitized.push('_');
            last_was_separator = true;
        }
    }

    sanitized.trim_matches('_').to_string()
}

#[must_use]
pub fn default_probe_baseline_dir() -> PathBuf {
    moltis_config::data_dir()
        .join("browser")
        .join("telemetry")
        .join("probe-baselines")
}

#[must_use]
pub fn default_tls_ja4_sidecar_dir() -> PathBuf {
    moltis_config::data_dir()
        .join("browser")
        .join("telemetry")
        .join("tls-ja4")
}

#[must_use]
pub fn probe_browser_family(kind: BrowserKind) -> ProbeBrowserFamily {
    match kind {
        BrowserKind::Chrome => ProbeBrowserFamily::Chrome,
        BrowserKind::Chromium => ProbeBrowserFamily::Chromium,
        BrowserKind::Edge => ProbeBrowserFamily::Edge,
        BrowserKind::Brave => ProbeBrowserFamily::Brave,
        BrowserKind::Opera | BrowserKind::Vivaldi | BrowserKind::Arc | BrowserKind::Custom => {
            ProbeBrowserFamily::Other
        },
    }
}

#[must_use]
pub fn browser_version_from_user_agent(user_agent: &str) -> String {
    for marker in ["Chrome/", "Edg/", "OPR/", "Version/"] {
        if let Some(index) = user_agent.find(marker) {
            let version = user_agent[index + marker.len()..]
                .split(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
                .next()
                .unwrap_or_default()
                .trim();
            if !version.is_empty() {
                return version.to_string();
            }
        }
    }
    "unknown".to_string()
}

#[must_use]
pub fn stable_hex_hash(input: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[must_use]
pub fn aggregate_behavior_summaries(summaries: &[BehaviorBatchSummary]) -> BehaviorBatchSummary {
    if summaries.is_empty() {
        return BehaviorBatchSummary::empty();
    }

    let count = summaries.iter().map(|summary| summary.count).sum();
    let duration_s = summaries.iter().filter_map(|summary| summary.duration_s).sum::<f64>();
    let path_len_px = summaries
        .iter()
        .map(|summary| summary.path_len_px)
        .sum::<f64>();
    let straight_line_px = summaries
        .iter()
        .map(|summary| summary.straight_line_px)
        .sum::<f64>();
    let weighted = |extract: fn(&BehaviorBatchSummary) -> Option<f64>| -> Option<f64> {
        let mut total_weight = 0.0;
        let mut total = 0.0;
        for summary in summaries {
            let Some(value) = extract(summary) else {
                continue;
            };
            let weight = summary.count.max(1) as f64;
            total += value * weight;
            total_weight += weight;
        }
        (total_weight > 0.0).then_some(total / total_weight)
    };

    BehaviorBatchSummary {
        count,
        duration_s: (duration_s > 0.0).then_some(duration_s),
        path_len_px,
        straight_line_px,
        straightness: (path_len_px > 0.0).then_some(straight_line_px / path_len_px),
        mean_dt_s: weighted(|summary| summary.mean_dt_s),
        max_idle_gap_s: summaries
            .iter()
            .filter_map(|summary| summary.max_idle_gap_s)
            .max_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal)),
        mean_step_px: weighted(|summary| summary.mean_step_px),
        mean_speed_px_s: weighted(|summary| summary.mean_speed_px_s),
        event_rate_hz: weighted(|summary| summary.event_rate_hz),
    }
}

fn probe_report_url(origin: &Url, path: &str) -> Result<Url, TelemetryError> {
    origin.join(path).map_err(TelemetryError::from)
}

async fn fetch_probe_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: Url,
) -> Result<T, TelemetryError> {
    Ok(client.get(url).send().await?.error_for_status()?.json().await?)
}

pub async fn fetch_probe_fingerprint_report(
    client: &reqwest::Client,
    origin: &Url,
    session_id: &str,
) -> Result<ProbeFingerprintReport, TelemetryError> {
    let session_id = url::form_urlencoded::byte_serialize(session_id.as_bytes()).collect::<String>();
    let url = probe_report_url(origin, &format!("/report/{session_id}"))?;
    let mut report: ProbeFingerprintReport = fetch_probe_json(client, url).await?;
    report.sanitize();
    Ok(report)
}

pub async fn fetch_probe_behavior_report(
    client: &reqwest::Client,
    origin: &Url,
    session_id: &str,
) -> Result<ProbeBehaviorReport, TelemetryError> {
    let session_id = url::form_urlencoded::byte_serialize(session_id.as_bytes()).collect::<String>();
    let url = probe_report_url(origin, &format!("/behavior-report/{session_id}"))?;
    let mut report: ProbeBehaviorReport = fetch_probe_json(client, url).await?;
    report.sanitize();
    Ok(report)
}

pub async fn fetch_probe_sequence_report(
    client: &reqwest::Client,
    origin: &Url,
    run_id: &str,
) -> Result<ProbeSequenceReport, TelemetryError> {
    let run_id = url::form_urlencoded::byte_serialize(run_id.as_bytes()).collect::<String>();
    let url = probe_report_url(origin, &format!("/sequence-report/{run_id}"))?;
    let mut report: ProbeSequenceReport = fetch_probe_json(client, url).await?;
    report.sanitize();
    Ok(report)
}

pub fn load_tls_ja4_observations(path: &Path) -> Result<Vec<TlsJa4Observation>, TelemetryError> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    fs::read_to_string(path)?
        .lines()
        .enumerate()
        .filter_map(|(line, content)| {
            let trimmed = content.trim();
            (!trimmed.is_empty()).then_some((line + 1, trimmed))
        })
        .map(|(line, content)| {
            serde_json::from_str(content).map_err(|source| TelemetryError::InvalidJsonLine {
                line,
                source,
            })
        })
        .collect()
}

#[must_use]
pub fn summarize_tls_ja4_observations(observations: &[TlsJa4Observation]) -> TlsJa4Summary {
    let distinct_ja4: BTreeSet<String> = observations
        .iter()
        .filter_map(|observation| observation.ja4.clone())
        .collect();
    let distinct_ja4s: BTreeSet<String> = observations
        .iter()
        .filter_map(|observation| observation.ja4s.clone())
        .collect();
    let distinct_alpn: BTreeSet<String> = observations
        .iter()
        .filter_map(|observation| observation.alpn.clone())
        .collect();
    let distinct_tls_versions: BTreeSet<String> = observations
        .iter()
        .filter_map(|observation| observation.tls_version.clone())
        .collect();

    TlsJa4Summary {
        event_count: observations.len(),
        distinct_ja4: distinct_ja4.into_iter().collect(),
        distinct_ja4s: distinct_ja4s.into_iter().collect(),
        distinct_alpn: distinct_alpn.into_iter().collect(),
        distinct_tls_versions: distinct_tls_versions.into_iter().collect(),
        first_ja4: observations.iter().find_map(|observation| observation.ja4.clone()),
        last_ja4: observations
            .iter()
            .rev()
            .find_map(|observation| observation.ja4.clone()),
    }
}

fn relative_delta(baseline: f64, current: f64) -> f64 {
    let denominator = baseline.abs().max(1.0);
    (current - baseline).abs() / denominator
}

fn push_optional_drift_issue(
    issues: &mut Vec<ProbeDriftIssue>,
    kind: ProbeDriftKind,
    detail: impl Into<String>,
    baseline: Option<f64>,
    current: Option<f64>,
    allowed_ratio: f64,
) {
    let (Some(baseline), Some(current)) = (baseline, current) else {
        return;
    };

    if relative_delta(baseline, current) > allowed_ratio {
        issues.push(ProbeDriftIssue {
            kind,
            detail: detail.into(),
        });
    }
}

#[must_use]
pub fn compare_probe_run_with_thresholds(
    baseline: &ProbeRunEvidence,
    current: &ProbeRunEvidence,
    thresholds: &ProbeDriftThresholds,
) -> ProbeRunDrift {
    let mut issues = Vec::new();

    if baseline.profile.browser_kind != current.profile.browser_kind {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::BrowserKindChanged,
            detail: format!(
                "browser kind changed from {} to {}",
                baseline.profile.browser_kind, current.profile.browser_kind
            ),
        });
    }
    if baseline.profile.browser_family != current.profile.browser_family {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::BrowserFamilyChanged,
            detail: format!(
                "browser family changed from {:?} to {:?}",
                baseline.profile.browser_family, current.profile.browser_family
            ),
        });
    }
    if baseline.profile.browser_version != current.profile.browser_version {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::BrowserVersionChanged,
            detail: format!(
                "browser version changed from {} to {}",
                baseline.profile.browser_version, current.profile.browser_version
            ),
        });
    }
    if baseline.profile.backend != current.profile.backend {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::BackendChanged,
            detail: format!(
                "backend changed from {} to {}",
                baseline.profile.backend, current.profile.backend
            ),
        });
    }
    if baseline.profile.headless != current.profile.headless {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::HeadlessChanged,
            detail: format!(
                "headless changed from {} to {}",
                baseline.profile.headless, current.profile.headless
            ),
        });
    }
    if baseline.profile.proxy_mode != current.profile.proxy_mode {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::ProxyModeChanged,
            detail: format!(
                "proxy mode changed from {:?} to {:?}",
                baseline.profile.proxy_mode, current.profile.proxy_mode
            ),
        });
    }
    if baseline.profile.browser_binary_basename != current.profile.browser_binary_basename {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::BrowserBinaryChanged,
            detail: format!(
                "browser binary changed from {} to {}",
                baseline.profile.browser_binary_basename, current.profile.browser_binary_basename
            ),
        });
    }
    if baseline.profile.launch_profile_hash != current.profile.launch_profile_hash {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::LaunchProfileHashChanged,
            detail: format!(
                "launch profile hash changed from {} to {}",
                baseline.profile.launch_profile_hash, current.profile.launch_profile_hash
            ),
        });
    }

    if baseline.fingerprint.user_agent != current.fingerprint.user_agent {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::UserAgentChanged,
            detail: format!(
                "user agent changed from {} to {}",
                baseline.fingerprint.user_agent, current.fingerprint.user_agent
            ),
        });
    }
    if baseline.headers.accept_language != current.headers.accept_language {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::AcceptLanguageChanged,
            detail: format!(
                "accept-language changed from {:?} to {:?}",
                baseline.headers.accept_language, current.headers.accept_language
            ),
        });
    }
    if baseline.fingerprint.webdriver != current.fingerprint.webdriver {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::WebdriverChanged,
            detail: format!(
                "webdriver changed from {:?} to {:?}",
                baseline.fingerprint.webdriver, current.fingerprint.webdriver
            ),
        });
    }
    if baseline.fingerprint.platform != current.fingerprint.platform {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::PlatformChanged,
            detail: format!(
                "platform changed from {:?} to {:?}",
                baseline.fingerprint.platform, current.fingerprint.platform
            ),
        });
    }
    if baseline.fingerprint.timezone != current.fingerprint.timezone {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::TimezoneChanged,
            detail: format!(
                "timezone changed from {:?} to {:?}",
                baseline.fingerprint.timezone, current.fingerprint.timezone
            ),
        });
    }
    if baseline.fingerprint.screen != current.fingerprint.screen {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::ScreenChanged,
            detail: format!(
                "screen changed from {:?} to {:?}",
                baseline.fingerprint.screen, current.fingerprint.screen
            ),
        });
    }
    if baseline.fingerprint.hardware_concurrency != current.fingerprint.hardware_concurrency {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::HardwareConcurrencyChanged,
            detail: format!(
                "hardware concurrency changed from {:?} to {:?}",
                baseline.fingerprint.hardware_concurrency,
                current.fingerprint.hardware_concurrency
            ),
        });
    }
    if baseline.fingerprint.device_memory != current.fingerprint.device_memory {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::DeviceMemoryChanged,
            detail: format!(
                "device memory changed from {:?} to {:?}",
                baseline.fingerprint.device_memory, current.fingerprint.device_memory
            ),
        });
    }

    push_optional_drift_issue(
        &mut issues,
        ProbeDriftKind::BehaviorCountDrift,
        format!(
            "behavior event count drifted from {} to {}",
            baseline.behavior.count, current.behavior.count
        ),
        Some(baseline.behavior.count as f64),
        Some(current.behavior.count as f64),
        thresholds.behavior_count_ratio,
    );
    push_optional_drift_issue(
        &mut issues,
        ProbeDriftKind::BehaviorPathDrift,
        format!(
            "behavior path length drifted from {} to {}",
            baseline.behavior.path_len_px, current.behavior.path_len_px
        ),
        Some(baseline.behavior.path_len_px),
        Some(current.behavior.path_len_px),
        thresholds.behavior_path_ratio,
    );
    push_optional_drift_issue(
        &mut issues,
        ProbeDriftKind::BehaviorStraightnessDrift,
        format!(
            "behavior straightness drifted from {:?} to {:?}",
            baseline.behavior.straightness, current.behavior.straightness
        ),
        baseline.behavior.straightness,
        current.behavior.straightness,
        thresholds.behavior_straightness_ratio,
    );
    push_optional_drift_issue(
        &mut issues,
        ProbeDriftKind::BehaviorMeanDtDrift,
        format!(
            "behavior mean dt drifted from {:?} to {:?}",
            baseline.behavior.mean_dt_s, current.behavior.mean_dt_s
        ),
        baseline.behavior.mean_dt_s,
        current.behavior.mean_dt_s,
        thresholds.behavior_mean_dt_ratio,
    );
    push_optional_drift_issue(
        &mut issues,
        ProbeDriftKind::BehaviorEventRateDrift,
        format!(
            "behavior event rate drifted from {:?} to {:?}",
            baseline.behavior.event_rate_hz, current.behavior.event_rate_hz
        ),
        baseline.behavior.event_rate_hz,
        current.behavior.event_rate_hz,
        thresholds.behavior_event_rate_ratio,
    );

    if baseline.request_sequence.request_count != current.request_sequence.request_count {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::RequestCountChanged,
            detail: format!(
                "request count changed from {} to {}",
                baseline.request_sequence.request_count, current.request_sequence.request_count
            ),
        });
    }
    if baseline.request_sequence.path_sequence != current.request_sequence.path_sequence {
        issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::PathSequenceChanged,
            detail: format!(
                "path sequence changed from {:?} to {:?}",
                baseline.request_sequence.path_sequence, current.request_sequence.path_sequence
            ),
        });
    }

    match (&baseline.tls_ja4, &current.tls_ja4) {
        (Some(_), None) => issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::TlsJa4Missing,
            detail: "tls/ja4 summary missing from current probe run".to_string(),
        }),
        (None, Some(_)) => issues.push(ProbeDriftIssue {
            kind: ProbeDriftKind::TlsJa4Added,
            detail: "tls/ja4 summary added to current probe run".to_string(),
        }),
        (Some(baseline_tls), Some(current_tls)) => {
            if baseline_tls.distinct_ja4 != current_tls.distinct_ja4 {
                issues.push(ProbeDriftIssue {
                    kind: ProbeDriftKind::TlsJa4Changed,
                    detail: format!(
                        "ja4 values changed from {:?} to {:?}",
                        baseline_tls.distinct_ja4, current_tls.distinct_ja4
                    ),
                });
            }
            if baseline_tls.distinct_ja4s != current_tls.distinct_ja4s {
                issues.push(ProbeDriftIssue {
                    kind: ProbeDriftKind::TlsJa4sChanged,
                    detail: format!(
                        "ja4s values changed from {:?} to {:?}",
                        baseline_tls.distinct_ja4s, current_tls.distinct_ja4s
                    ),
                });
            }
            if baseline_tls.distinct_alpn != current_tls.distinct_alpn {
                issues.push(ProbeDriftIssue {
                    kind: ProbeDriftKind::TlsAlpnChanged,
                    detail: format!(
                        "alpn values changed from {:?} to {:?}",
                        baseline_tls.distinct_alpn, current_tls.distinct_alpn
                    ),
                });
            }
            if baseline_tls.distinct_tls_versions != current_tls.distinct_tls_versions {
                issues.push(ProbeDriftIssue {
                    kind: ProbeDriftKind::TlsVersionChanged,
                    detail: format!(
                        "tls versions changed from {:?} to {:?}",
                        baseline_tls.distinct_tls_versions, current_tls.distinct_tls_versions
                    ),
                });
            }
        }
        (None, None) => {}
    }

    push_optional_drift_issue(
        &mut issues,
        ProbeDriftKind::MeanGapDrift,
        format!(
            "mean gap drifted from {:?} to {:?}",
            baseline.request_sequence.mean_gap_ms, current.request_sequence.mean_gap_ms
        ),
        baseline.request_sequence.mean_gap_ms,
        current.request_sequence.mean_gap_ms,
        thresholds.mean_gap_ratio,
    );
    push_optional_drift_issue(
        &mut issues,
        ProbeDriftKind::MaxGapDrift,
        format!(
            "max gap drifted from {:?} to {:?}",
            baseline.request_sequence.max_gap_ms, current.request_sequence.max_gap_ms
        ),
        baseline.request_sequence.max_gap_ms,
        current.request_sequence.max_gap_ms,
        thresholds.max_gap_ratio,
    );

    ProbeRunDrift { issues }
}

#[must_use]
pub fn compare_probe_run(
    baseline: &ProbeRunEvidence,
    current: &ProbeRunEvidence,
) -> ProbeRunDrift {
    compare_probe_run_with_thresholds(baseline, current, &ProbeDriftThresholds::default())
}

#[must_use]
pub fn summarize_behavior_points(points: &[BehaviorPoint]) -> BehaviorBatchSummary {
    if points.is_empty() {
        return BehaviorBatchSummary::empty();
    }

    if points.len() == 1 {
        return BehaviorBatchSummary {
            count: 1,
            duration_s: Some(0.0),
            path_len_px: 0.0,
            straight_line_px: 0.0,
            straightness: None,
            mean_dt_s: None,
            max_idle_gap_s: None,
            mean_step_px: None,
            mean_speed_px_s: None,
            event_rate_hz: None,
        };
    }

    let mut total_dt_s = 0.0;
    let mut total_step_px = 0.0;
    let mut total_speed_px_s = 0.0;
    let mut max_idle_gap_s = 0.0_f64;
    let mut segment_count = 0usize;

    for window in points.windows(2) {
        let a = &window[0];
        let b = &window[1];
        let dt_s = ((b.t - a.t) / 1000.0).max(0.0);
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        let step_px = dx.hypot(dy);
        let speed_px_s = if dt_s > 0.0 { step_px / dt_s } else { 0.0 };

        total_dt_s += dt_s;
        total_step_px += step_px;
        total_speed_px_s += speed_px_s;
        max_idle_gap_s = max_idle_gap_s.max(dt_s);
        segment_count += 1;
    }

    let duration_s = ((points.last().map(|point| point.t).unwrap_or(0.0)
        - points.first().map(|point| point.t).unwrap_or(0.0))
        / 1000.0)
        .max(0.0);
    let straight_line_px = (points.last().map(|point| point.x).unwrap_or(0.0)
        - points.first().map(|point| point.x).unwrap_or(0.0))
    .hypot(
        points.last().map(|point| point.y).unwrap_or(0.0)
            - points.first().map(|point| point.y).unwrap_or(0.0),
    );

    BehaviorBatchSummary {
        count: points.len(),
        duration_s: Some(duration_s),
        path_len_px: total_step_px,
        straight_line_px,
        straightness: (total_step_px > 0.0).then_some(straight_line_px / total_step_px),
        mean_dt_s: Some(total_dt_s / segment_count as f64),
        max_idle_gap_s: Some(max_idle_gap_s),
        mean_step_px: Some(total_step_px / segment_count as f64),
        mean_speed_px_s: Some(total_speed_px_s / segment_count as f64),
        event_rate_hz: (duration_s > 0.0).then_some(points.len() as f64 / duration_s),
    }
}

#[must_use]
pub fn summarize_request_sequence(events: &[RequestSequenceEvent]) -> RequestSequenceSummary {
    if events.is_empty() {
        return RequestSequenceSummary::empty();
    }

    let mut sorted = events.to_vec();
    sorted.sort_by(|left, right| left.request_index.cmp(&right.request_index));

    let path_sequence: Vec<String> = sorted.iter().map(|event| event.path.clone()).collect();
    let mut distinct_paths = BTreeSet::new();
    for path in &path_sequence {
        distinct_paths.insert(path.clone());
    }

    let gaps_ms: Vec<f64> = sorted
        .windows(2)
        .map(|window| (window[1].request_ts_ms - window[0].request_ts_ms).max(0.0))
        .collect();
    let mean_gap_ms = (!gaps_ms.is_empty())
        .then_some(gaps_ms.iter().sum::<f64>() / gaps_ms.len() as f64);
    let max_gap_ms = gaps_ms.iter().copied().reduce(f64::max);

    RequestSequenceSummary {
        request_count: sorted.len(),
        first_path: sorted.first().map(|event| event.path.clone()),
        last_path: sorted.last().map(|event| event.path.clone()),
        distinct_path_count: distinct_paths.len(),
        path_sequence,
        mean_gap_ms,
        max_gap_ms,
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            BrowserManager, patchright_session::PatchrightSession, protection,
            snapshot::sanitize_dom_text,
            types::{BrowserAction, BrowserConfig, BrowserPreference, BrowserRequest},
        },
        axum::{
            Json, Router,
            extract::{Path as AxumPath, State},
            http::{HeaderMap, StatusCode, Uri},
            response::Html,
            routing::{get, post},
        },
        serde_json::json,
        std::{
            collections::{BTreeMap, HashMap},
            path::PathBuf,
            process::Command,
            sync::{Arc, Mutex as StdMutex, OnceLock},
            time::Instant,
        },
        tempfile::tempdir,
        tokio::{
            net::TcpListener,
            sync::{Mutex, OwnedMutexGuard},
            task::JoinHandle,
            time::{Duration, sleep},
        },
    };

    #[derive(Debug, Clone)]
    struct CapturedFingerprint {
        body: FingerprintSnapshot,
        headers: FingerprintHeaders,
    }

    #[derive(Debug, Clone)]
    struct CapturedBehavior {
        summary: BehaviorBatchSummary,
        sample: Vec<BehaviorPoint>,
    }

    #[derive(Clone)]
    struct ProbeState {
        started_at: Instant,
        fingerprints: Arc<StdMutex<HashMap<String, CapturedFingerprint>>>,
        behaviors: Arc<StdMutex<HashMap<String, Vec<CapturedBehavior>>>>,
        requests: Arc<StdMutex<HashMap<String, Vec<RequestSequenceEvent>>>>,
    }

    impl ProbeState {
        fn new() -> Self {
            Self {
                started_at: Instant::now(),
                fingerprints: Arc::new(StdMutex::new(HashMap::new())),
                behaviors: Arc::new(StdMutex::new(HashMap::new())),
                requests: Arc::new(StdMutex::new(HashMap::new())),
            }
        }

        fn fingerprint(&self, session_id: &str) -> Option<CapturedFingerprint> {
            self.fingerprints.lock().unwrap().get(session_id).cloned()
        }

        fn behaviors(&self, session_id: &str) -> Vec<CapturedBehavior> {
            self.behaviors
                .lock()
                .unwrap()
                .get(session_id)
                .cloned()
                .unwrap_or_default()
        }

        fn first_fingerprint_session(&self) -> Option<String> {
            self.fingerprints
                .lock()
                .unwrap()
                .keys()
                .next()
                .cloned()
        }

        fn first_behavior_session(&self) -> Option<String> {
            self.behaviors.lock().unwrap().keys().next().cloned()
        }

        fn requests(&self, run_id: &str) -> Vec<RequestSequenceEvent> {
            self.requests
                .lock()
                .unwrap()
                .get(run_id)
                .cloned()
                .unwrap_or_default()
        }

        fn request_summary(&self, run_id: &str) -> RequestSequenceSummary {
            summarize_request_sequence(&self.requests(run_id))
        }
    }

    #[derive(Debug, Deserialize)]
    struct BehaviorBatch {
        session_id: String,
        #[allow(dead_code)]
        ts: f64,
        points: Vec<BehaviorPoint>,
    }

    fn query_value(uri: &Uri, name: &str) -> Option<String> {
        uri.query()?.split('&').find_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next()?;
            let value = parts.next().unwrap_or_default();
            (key == name).then_some(value.to_string())
        })
    }

    fn record_request(
        state: &ProbeState,
        uri: &Uri,
        method: &str,
        status_code: u16,
    ) {
        let Some(run_id) = query_value(uri, "run_id") else {
            return;
        };

        let request_ts_ms = state.started_at.elapsed().as_secs_f64() * 1000.0;
        let mut requests = state.requests.lock().unwrap();
        let events = requests.entry(run_id.clone()).or_default();
        let request_index = events.len() + 1;
        events.push(RequestSequenceEvent {
            run_id,
            request_index,
            request_ts_ms,
            path: uri.path().to_string(),
            method: method.to_string(),
            status_code,
        });
    }

    async fn new_session(
        State(state): State<ProbeState>,
        uri: Uri,
    ) -> Json<serde_json::Value> {
        record_request(&state, &uri, "GET", 200);
        Json(json!({ "session_id": uuid::Uuid::new_v4().to_string() }))
    }

    async fn collect_fp(
        State(state): State<ProbeState>,
        uri: Uri,
        headers: HeaderMap,
        Json(payload): Json<FingerprintSnapshot>,
    ) -> Json<serde_json::Value> {
        record_request(&state, &uri, "POST", 200);
        let headers = FingerprintHeaders {
            user_agent: headers
                .get("user-agent")
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
            accept_language: headers
                .get("accept-language")
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
            sec_ch_ua: headers
                .get("sec-ch-ua")
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
            sec_ch_ua_platform: headers
                .get("sec-ch-ua-platform")
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
            sec_fetch_site: headers
                .get("sec-fetch-site")
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
            x_forwarded_for: headers
                .get("x-forwarded-for")
                .and_then(|value| value.to_str().ok())
                .map(ToString::to_string),
        };

        state.fingerprints.lock().unwrap().insert(
            payload.session_id.clone(),
            CapturedFingerprint {
                body: payload,
                headers,
            },
        );

        Json(json!({ "ok": true }))
    }

    async fn collect_behavior(
        State(state): State<ProbeState>,
        uri: Uri,
        Json(batch): Json<BehaviorBatch>,
    ) -> Json<serde_json::Value> {
        record_request(&state, &uri, "POST", 200);
        let summary = summarize_behavior_points(&batch.points);
        state
            .behaviors
            .lock()
            .unwrap()
            .entry(batch.session_id)
            .or_default()
            .push(CapturedBehavior {
                summary: summary.clone(),
                sample: batch.points.into_iter().take(10).collect(),
            });

        Json(json!({ "ok": true, "summary": summary }))
    }

    async fn fingerprint_report(
        AxumPath(session_id): AxumPath<String>,
        State(state): State<ProbeState>,
    ) -> Result<Json<ProbeFingerprintReport>, StatusCode> {
        let fingerprint = state
            .fingerprint(&session_id)
            .ok_or(StatusCode::NOT_FOUND)?;
        Ok(Json(ProbeFingerprintReport {
            session_id,
            body: fingerprint.body,
            headers: fingerprint.headers,
        }))
    }

    async fn behavior_report(
        AxumPath(session_id): AxumPath<String>,
        State(state): State<ProbeState>,
    ) -> Json<ProbeBehaviorReport> {
        let batches = state.behaviors(&session_id);
        let summaries = batches
            .iter()
            .map(|batch| batch.summary.clone())
            .collect::<Vec<_>>();
        Json(ProbeBehaviorReport {
            session_id,
            summary: aggregate_behavior_summaries(&summaries),
            batches: summaries,
        })
    }

    async fn sequence_report(
        AxumPath(run_id): AxumPath<String>,
        State(state): State<ProbeState>,
    ) -> Json<ProbeSequenceReport> {
        Json(ProbeSequenceReport {
            run_id: run_id.clone(),
            summary: state.request_summary(&run_id),
        })
    }

    async fn fingerprint_probe_page(
        State(state): State<ProbeState>,
        uri: Uri,
    ) -> Html<&'static str> {
        record_request(&state, &uri, "GET", 200);
        Html(
            r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <title>Fingerprint Probe</title>
  </head>
  <body>
    <h1>Fingerprint Probe</h1>
    <p>
      This owned probe page records browser-visible identity traits and request headers for
      baseline drift detection in permissioned automation tests.
    </p>
    <p id="payload"></p>
    <button id="probe-ready" type="button">Ready</button>
    <script>
      async function getSessionId() {
        const response = await fetch(`/session${location.search}`);
        return (await response.json()).session_id;
      }

      function getWebGLInfo() {
        try {
          const canvas = document.createElement('canvas');
          const gl = canvas.getContext('webgl') || canvas.getContext('experimental-webgl');
          if (!gl) return {};
          const dbg = gl.getExtension('WEBGL_debug_renderer_info');
          return {
            webgl_vendor: dbg ? gl.getParameter(dbg.UNMASKED_VENDOR_WEBGL) : null,
            webgl_renderer: dbg ? gl.getParameter(dbg.UNMASKED_RENDERER_WEBGL) : null,
          };
        } catch {
          return {};
        }
      }

      (async () => {
        const hidden = 'visible\u200B hidden\u2060 prompt\u00AD text\u{E0001}';
        document.getElementById('payload').textContent = hidden;
        const sessionId = await getSessionId();
        window.__probeSessionId = sessionId;
        const payload = {
          session_id: sessionId,
          ts: performance.now(),
          url: location.href,
          user_agent: navigator.userAgent,
          webdriver: navigator.webdriver ?? null,
          platform: navigator.platform ?? null,
          language: navigator.language ?? null,
          languages: navigator.languages ?? null,
          timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
          screen: {
            width: screen.width,
            height: screen.height,
            avail_width: screen.availWidth,
            avail_height: screen.availHeight,
            dpr: window.devicePixelRatio
          },
          hardware_concurrency: navigator.hardwareConcurrency ?? null,
          device_memory: navigator.deviceMemory ?? null,
          plugins_count: navigator.plugins ? navigator.plugins.length : null,
          ...getWebGLInfo()
        };

        await fetch(`/fp${location.search}`, {
          method: 'POST',
          headers: {'content-type': 'application/json'},
          body: JSON.stringify(payload)
        });

        document.body.dataset.probeSessionId = sessionId;
        document.body.dataset.probeFp = 'ready';
      })();
    </script>
  </body>
</html>"#,
        )
    }

    async fn behavior_probe_page(
        State(state): State<ProbeState>,
        uri: Uri,
    ) -> Html<&'static str> {
        record_request(&state, &uri, "GET", 200);
        Html(
            r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <title>Behavior Probe</title>
    <style>
      body { font-family: sans-serif; margin: 0; min-height: 100vh; position: relative; }
      #box-a, #box-b, #box-c {
        position: absolute;
        width: 120px;
        height: 60px;
        border-radius: 8px;
        border: 0;
        color: white;
      }
      #box-a { left: 90px; top: 100px; background: #1d4ed8; }
      #box-b { left: 360px; top: 200px; background: #059669; }
      #box-c { left: 680px; top: 320px; background: #dc2626; }
    </style>
  </head>
  <body>
    <button id="box-a">Alpha</button>
    <button id="box-b">Bravo</button>
    <button id="box-c">Target</button>
    <script>
      let sessionId = null;
      let points = [];
      let lastFlush = performance.now();

      async function init() {
        const response = await fetch(`/session${location.search}`);
        sessionId = (await response.json()).session_id;
        window.__probeSessionId = sessionId;
        document.body.dataset.probeSessionId = sessionId;
        document.body.dataset.behaviorReady = 'true';
      }

      function pushPoint(event, type) {
        points.push({
          t: performance.now(),
          type,
          x: event.clientX,
          y: event.clientY,
          buttons: event.buttons,
        });
      }

      async function flush(force = false) {
        const now = performance.now();
        if (!force && now - lastFlush < 250 && points.length < 6) return false;
        if (!points.length || !sessionId) return false;
        const batch = points;
        points = [];
        lastFlush = now;
        await fetch(`/behavior${location.search}`, {
          method: 'POST',
          headers: {'content-type': 'application/json'},
          body: JSON.stringify({
            session_id: sessionId,
            ts: now,
            points: batch
          })
        });
        return true;
      }

      window.__flushBehavior = (force = true) => flush(force);
      window.addEventListener('mousemove', event => { pushPoint(event, 'move'); void flush(); }, { passive: true });
      window.addEventListener('mousedown', event => { pushPoint(event, 'down'); void flush(true); }, { passive: true });
      window.addEventListener('mouseup', event => { pushPoint(event, 'up'); void flush(true); }, { passive: true });

      void init();
    </script>
  </body>
</html>"#,
        )
    }

    async fn sequence_step(
        State(state): State<ProbeState>,
        uri: Uri,
    ) -> Json<serde_json::Value> {
        record_request(&state, &uri, "GET", 200);
        Json(json!({
            "ok": true,
            "step": query_value(&uri, "step"),
        }))
    }

    async fn sequence_probe_page(
        State(state): State<ProbeState>,
        uri: Uri,
    ) -> Html<&'static str> {
        record_request(&state, &uri, "GET", 200);
        Html(
            r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <title>Sequence Probe</title>
  </head>
  <body>
    <h1>Sequence Probe</h1>
    <p>
      This page triggers a fixed request cadence so the canary can compare path order and timing
      gaps against a stored baseline.
    </p>
    <button id="sequence-anchor" type="button">Sequence Anchor</button>
    <script>
      function withSearch(path) {
        return `${path}${location.search}`;
      }

      async function runStep(step, delayMs) {
        await new Promise(resolve => setTimeout(resolve, delayMs));
        await fetch(`${withSearch('/sequence-step')}&step=${step}`);
      }

      (async () => {
        const response = await fetch(withSearch('/session'));
        const session = await response.json();
        window.__probeSessionId = session.session_id;
        document.body.dataset.probeSessionId = session.session_id;

        await runStep('alpha', 40);
        await runStep('bravo', 70);
        await runStep('target', 110);

        document.body.dataset.sequenceReady = 'true';
      })();
    </script>
  </body>
</html>"#,
        )
    }

    async fn start_probe_server() -> Result<(String, ProbeState, JoinHandle<()>), Box<dyn std::error::Error>> {
        let state = ProbeState::new();
        let app = Router::new()
            .route("/session", get(new_session))
            .route("/fp", post(collect_fp))
            .route("/behavior", post(collect_behavior))
            .route("/sequence-step", get(sequence_step))
            .route("/report/{session_id}", get(fingerprint_report))
            .route("/behavior-report/{session_id}", get(behavior_report))
            .route("/sequence-report/{run_id}", get(sequence_report))
            .route("/fp-probe", get(fingerprint_probe_page))
            .route("/behavior-probe", get(behavior_probe_page))
            .route("/sequence-probe", get(sequence_probe_page))
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .unwrap_or_else(|error| panic!("probe server should run: {error}"));
        });

        Ok((origin, state, server))
    }

    fn test_browser_config() -> BrowserConfig {
        let mut config = BrowserConfig::default();
        config.persist_profile = false;
        config.protection.enabled = true;
        config.protection.timeout_ms = 90_000;
        config.protection.max_retries = 2;
        config
    }

    fn request(session_id: Option<String>, action: BrowserAction, timeout_ms: u64) -> BrowserRequest {
        BrowserRequest {
            session_id,
            action,
            timeout_ms,
            sandbox: Some(false),
            browser: Some(BrowserPreference::Auto),
        }
    }

    fn snapshot_ref(snapshot: &crate::types::DomSnapshot, text: &str) -> u32 {
        snapshot
            .elements
            .iter()
            .find(|element| element.text.as_deref() == Some(text))
            .map(|element| element.ref_)
            .unwrap()
    }

    fn response_string(response: &crate::BrowserResponse) -> String {
        response
            .result
            .as_ref()
            .and_then(|value| value.as_str())
            .map(ToString::to_string)
            .unwrap()
    }

    fn patchright_profile(config: &BrowserConfig) -> protection::PatchrightLaunchProfile {
        let detection = crate::detect::detect_browser(config.chrome_path.as_deref());
        let selected = crate::detect::pick_browser(&detection.browsers, Some(BrowserPreference::Auto));
        protection::build_patchright_launch_profile_for_browser(config, selected.as_ref())
    }

    fn live_browser_test_lock() -> Arc<Mutex<()>> {
        static LOCK: OnceLock<Arc<Mutex<()>>> = OnceLock::new();
        Arc::clone(LOCK.get_or_init(|| Arc::new(Mutex::new(()))))
    }

    async fn acquire_live_browser_test_guard() -> OwnedMutexGuard<()> {
        live_browser_test_lock().lock_owned().await
    }

    async fn wait_for_patchright_session_id(
        state: &ProbeState,
        kind: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        for _ in 0..50 {
            let session_id = match kind {
                "fingerprint" => state.first_fingerprint_session(),
                "behavior" => state.first_behavior_session(),
                _ => None,
            };
            if let Some(session_id) = session_id {
                return Ok(session_id);
            }
            sleep(Duration::from_millis(100)).await;
        }

        Err("probe session id not set".into())
    }

    #[test]
    fn summarize_behavior_points_reports_shape_metrics() {
        let summary = summarize_behavior_points(&[
            BehaviorPoint { t: 0.0, kind: "move".into(), x: 0.0, y: 0.0, buttons: Some(0) },
            BehaviorPoint { t: 16.0, kind: "move".into(), x: 3.0, y: 4.0, buttons: Some(0) },
            BehaviorPoint { t: 32.0, kind: "up".into(), x: 6.0, y: 8.0, buttons: Some(0) },
        ]);

        assert_eq!(summary.count, 3);
        assert_eq!(summary.path_len_px, 10.0);
        assert_eq!(summary.straight_line_px, 10.0);
        assert_eq!(summary.straightness, Some(1.0));
        assert!(summary.mean_speed_px_s.unwrap() > 300.0);
    }

    #[test]
    fn summarize_request_sequence_reports_path_and_gap_metrics() {
        let summary = summarize_request_sequence(&[
            RequestSequenceEvent {
                run_id: "run-1".into(),
                request_index: 3,
                request_ts_ms: 180.0,
                path: "/sequence-step".into(),
                method: "GET".into(),
                status_code: 200,
            },
            RequestSequenceEvent {
                run_id: "run-1".into(),
                request_index: 1,
                request_ts_ms: 20.0,
                path: "/sequence-probe".into(),
                method: "GET".into(),
                status_code: 200,
            },
            RequestSequenceEvent {
                run_id: "run-1".into(),
                request_index: 2,
                request_ts_ms: 70.0,
                path: "/session".into(),
                method: "GET".into(),
                status_code: 200,
            },
            RequestSequenceEvent {
                run_id: "run-1".into(),
                request_index: 4,
                request_ts_ms: 320.0,
                path: "/sequence-step".into(),
                method: "GET".into(),
                status_code: 200,
            },
        ]);

        assert_eq!(summary.request_count, 4);
        assert_eq!(summary.first_path.as_deref(), Some("/sequence-probe"));
        assert_eq!(summary.last_path.as_deref(), Some("/sequence-step"));
        assert_eq!(summary.distinct_path_count, 3);
        assert_eq!(
            summary.path_sequence,
            vec![
                "/sequence-probe".to_string(),
                "/session".to_string(),
                "/sequence-step".to_string(),
                "/sequence-step".to_string(),
            ]
        );
        assert_eq!(summary.mean_gap_ms, Some(100.0));
        assert_eq!(summary.max_gap_ms, Some(140.0));
    }

    fn sample_probe_run_evidence() -> ProbeRunEvidence {
        ProbeRunEvidence {
            profile: ProbeRunProfile {
                browser_kind: BrowserKind::Chrome,
                browser_family: ProbeBrowserFamily::Chrome,
                browser_version: "123.0.0.0".to_string(),
                backend: BrowserBackendKind::Patchright,
                headless: true,
                proxy_mode: ProbeProxyMode::None,
                browser_binary_basename: "Google Chrome".to_string(),
                launch_profile_hash: "abc123def4567890".to_string(),
            },
            fingerprint: FingerprintSnapshot {
                session_id: "session-1".to_string(),
                ts: 12.0,
                url: "https://probe.example/fp".to_string(),
                user_agent: "Mozilla/5.0 Chrome/123".to_string(),
                webdriver: Some(false),
                platform: Some("MacIntel".to_string()),
                language: Some("en-AU".to_string()),
                languages: Some(vec!["en-AU".to_string(), "en".to_string()]),
                timezone: Some("Australia/Melbourne".to_string()),
                screen: Some(FingerprintScreen {
                    width: 2560,
                    height: 1440,
                    avail_width: 2560,
                    avail_height: 1415,
                    dpr: 2.0,
                }),
                hardware_concurrency: Some(8),
                device_memory: Some(8.0),
                webgl_vendor: Some("Google Inc.".to_string()),
                webgl_renderer: Some("ANGLE".to_string()),
                plugins_count: Some(5),
            },
            headers: FingerprintHeaders {
                user_agent: Some("Mozilla/5.0 Chrome/123".to_string()),
                accept_language: Some("en-AU,en;q=0.9".to_string()),
                sec_ch_ua: Some("\"Google Chrome\";v=\"123\"".to_string()),
                sec_ch_ua_platform: Some("\"macOS\"".to_string()),
                sec_fetch_site: Some("same-origin".to_string()),
                x_forwarded_for: None,
            },
            behavior: BehaviorBatchSummary {
                count: 6,
                duration_s: Some(0.4),
                path_len_px: 220.0,
                straight_line_px: 200.0,
                straightness: Some(0.91),
                mean_dt_s: Some(0.08),
                max_idle_gap_s: Some(0.15),
                mean_step_px: Some(44.0),
                mean_speed_px_s: Some(550.0),
                event_rate_hz: Some(15.0),
            },
            request_sequence: RequestSequenceSummary {
                request_count: 5,
                first_path: Some("/sequence-probe".to_string()),
                last_path: Some("/sequence-step".to_string()),
                distinct_path_count: 3,
                path_sequence: vec![
                    "/sequence-probe".to_string(),
                    "/session".to_string(),
                    "/sequence-step".to_string(),
                    "/sequence-step".to_string(),
                    "/sequence-step".to_string(),
                ],
                mean_gap_ms: Some(75.0),
                max_gap_ms: Some(120.0),
            },
            tls_ja4: Some(TlsJa4Summary {
                event_count: 2,
                distinct_ja4: vec!["t13d1516h2_8daaf6152771_b186095e22b6".to_string()],
                distinct_ja4s: vec!["t120200_c030_1".to_string()],
                distinct_alpn: vec!["h2".to_string()],
                distinct_tls_versions: vec!["tls1.3".to_string()],
                first_ja4: Some("t13d1516h2_8daaf6152771_b186095e22b6".to_string()),
                last_ja4: Some("t13d1516h2_8daaf6152771_b186095e22b6".to_string()),
            }),
        }
    }

    #[test]
    fn probe_run_profile_storage_key_is_backend_headless_proxy_specific() {
        let baseline = sample_probe_run_evidence().profile;
        let mut headed = baseline.clone();
        headed.headless = false;
        let mut chromium = baseline.clone();
        chromium.backend = BrowserBackendKind::Chromiumoxide;
        let mut residential = baseline.clone();
        residential.proxy_mode = ProbeProxyMode::Residential;

        assert_eq!(
            baseline.storage_key(),
            "chrome-chrome-123_0_0_0-patchright-headless-none"
        );
        assert_ne!(baseline.storage_key(), headed.storage_key());
        assert_ne!(baseline.storage_key(), chromium.storage_key());
        assert_ne!(baseline.storage_key(), residential.storage_key());
    }

    #[test]
    fn probe_baseline_store_round_trips_saved_evidence() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let store = ProbeBaselineStore::new(dir.path().join("probe-baselines"));
        let evidence = sample_probe_run_evidence();

        let saved = store.save(&evidence)?;
        let loaded = store.load(&evidence.profile)?.unwrap();

        assert_eq!(saved.key, evidence.profile.storage_key());
        assert_eq!(loaded.evidence, evidence);
        assert_eq!(loaded.key, saved.key);
        assert!(store.baseline_path(&loaded.evidence.profile).exists());

        Ok(())
    }

    #[test]
    fn probe_baseline_store_keys_by_profile() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let store = ProbeBaselineStore::new(dir.path().join("probe-baselines"));
        let evidence = sample_probe_run_evidence();
        store.save(&evidence)?;

        let mut missing_profile = evidence.profile.clone();
        missing_profile.proxy_mode = ProbeProxyMode::Residential;

        assert!(store.load(&missing_profile)?.is_none());

        Ok(())
    }

    #[test]
    fn probe_baseline_store_compares_saved_baseline() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let store = ProbeBaselineStore::new(dir.path().join("probe-baselines"));
        let baseline = sample_probe_run_evidence();
        store.save(&baseline)?;

        let mut current = baseline.clone();
        current.headers.accept_language = Some("en-US,en;q=0.9".to_string());

        let drift = store.compare(&current)?.unwrap();

        assert!(!drift.consistent());
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::AcceptLanguageChanged));

        Ok(())
    }

    #[test]
    fn probe_telemetry_policy_defaults_to_routine_baselines_and_opt_in_tls() {
        let policy = ProbeTelemetryPolicy::default();

        assert_eq!(
            policy.capture_plan(false),
            ProbeCapturePlan {
                persist_baseline: true,
                capture_tls_ja4: false,
            }
        );
        assert_eq!(
            policy.capture_plan(true),
            ProbeCapturePlan {
                persist_baseline: true,
                capture_tls_ja4: true,
            }
        );
    }

    #[test]
    fn probe_telemetry_policy_can_disable_or_force_tls_capture() {
        let disabled = ProbeTelemetryPolicy {
            persist_baselines: true,
            tls_ja4_mode: TlsJa4CollectionMode::Disabled,
        };
        let always = ProbeTelemetryPolicy {
            persist_baselines: true,
            tls_ja4_mode: TlsJa4CollectionMode::Always,
        };

        assert!(!disabled.capture_plan(true).capture_tls_ja4);
        assert!(!disabled.capture_plan(false).capture_tls_ja4);
        assert!(always.capture_plan(false).capture_tls_ja4);
        assert!(always.capture_plan(true).capture_tls_ja4);
    }

    #[test]
    fn probe_telemetry_policy_persists_and_compares_baseline() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let store = ProbeBaselineStore::new(dir.path().join("probe-baselines"));
        let policy = ProbeTelemetryPolicy::default();
        let baseline = sample_probe_run_evidence();

        let first = policy
            .persist_and_compare_baseline(&store, &baseline)?
            .unwrap();
        assert!(first.previous.is_none());
        assert!(first.drift.is_none());

        let mut current = baseline.clone();
        current.headers.accept_language = Some("en-US,en;q=0.9".to_string());

        let second = policy
            .persist_and_compare_baseline(&store, &current)?
            .unwrap();
        assert!(second.previous.is_some());
        assert!(second
            .drift
            .as_ref()
            .is_some_and(|drift| !drift.consistent()));
        assert_eq!(
            second.current.evidence.headers.accept_language.as_deref(),
            Some("en-US,en;q=0.9")
        );

        Ok(())
    }

    #[test]
    fn summarize_tls_ja4_observations_reports_distinct_values() {
        let summary = summarize_tls_ja4_observations(&[
            TlsJa4Observation {
                ts_ms: 1.0,
                ja4: Some("ja4-a".to_string()),
                ja4s: Some("ja4s-a".to_string()),
                alpn: Some("h2".to_string()),
                tls_version: Some("tls1.3".to_string()),
                cipher_suite: Some("TLS_AES_128_GCM_SHA256".to_string()),
                server_name: Some("example.com".to_string()),
                destination_addr: Some("203.0.113.10".to_string()),
                destination_port: Some(443),
            },
            TlsJa4Observation {
                ts_ms: 2.0,
                ja4: Some("ja4-a".to_string()),
                ja4s: Some("ja4s-b".to_string()),
                alpn: Some("http/1.1".to_string()),
                tls_version: Some("tls1.2".to_string()),
                cipher_suite: Some("TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256".to_string()),
                server_name: Some("example.com".to_string()),
                destination_addr: Some("203.0.113.10".to_string()),
                destination_port: Some(443),
            },
        ]);

        assert_eq!(summary.event_count, 2);
        assert_eq!(summary.distinct_ja4, vec!["ja4-a".to_string()]);
        assert_eq!(
            summary.distinct_ja4s,
            vec!["ja4s-a".to_string(), "ja4s-b".to_string()]
        );
        assert_eq!(
            summary.distinct_alpn,
            vec!["h2".to_string(), "http/1.1".to_string()]
        );
        assert_eq!(
            summary.distinct_tls_versions,
            vec!["tls1.2".to_string(), "tls1.3".to_string()]
        );
        assert_eq!(summary.first_ja4.as_deref(), Some("ja4-a"));
        assert_eq!(summary.last_ja4.as_deref(), Some("ja4-a"));
    }

    #[test]
    fn tls_ja4_sidecar_config_resolves_output_path_and_placeholders() {
        let config = TlsJa4SidecarConfig {
            command: "ja4-sidecar".to_string(),
            args: vec!["--output".to_string(), "{output_path}".to_string()],
            env: BTreeMap::from([(
                "JA4_OUTPUT".to_string(),
                "{output_path}".to_string(),
            )]),
            working_dir: None,
            output_dir: Some(PathBuf::from("/tmp/moltis-ja4")),
        };

        let output_path = config.output_path("realestate-run");

        assert_eq!(
            output_path,
            PathBuf::from("/tmp/moltis-ja4/realestate_run.jsonl")
        );
        assert_eq!(
            config.resolved_args(&output_path),
            vec![
                "--output".to_string(),
                "/tmp/moltis-ja4/realestate_run.jsonl".to_string(),
            ]
        );
        assert_eq!(
            config.resolved_env(&output_path).get("JA4_OUTPUT"),
            Some(&"/tmp/moltis-ja4/realestate_run.jsonl".to_string())
        );
    }

    #[test]
    fn probe_canary_spec_rejects_http_origin_when_tls_capture_requested() {
        let spec = ProbeCanarySpec {
            origin: "http://127.0.0.1:8080".to_string(),
            browser: BrowserPreference::Auto,
            backends: Vec::new(),
            policy: ProbeTelemetryPolicy::default(),
            tls_sidecar: Some(TlsJa4SidecarConfig {
                command: "ja4-sidecar".to_string(),
                args: Vec::new(),
                env: BTreeMap::new(),
                working_dir: None,
                output_dir: None,
            }),
        };

        assert!(matches!(
            spec.validated_origin(),
            Err(TelemetryError::InvalidProbeOrigin(_))
        ));
    }

    #[test]
    fn load_tls_ja4_observations_reads_jsonl() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("capture.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"ts_ms\":1.0,\"ja4\":\"ja4-a\",\"ja4s\":\"ja4s-a\",\"alpn\":\"h2\",\"tls_version\":\"tls1.3\"}\n",
                "{\"ts_ms\":2.0,\"ja4\":\"ja4-b\",\"ja4s\":\"ja4s-a\",\"alpn\":\"h2\",\"tls_version\":\"tls1.3\"}\n",
            ),
        )?;

        let observations = load_tls_ja4_observations(&path)?;
        let summary = summarize_tls_ja4_observations(&observations);

        assert_eq!(observations.len(), 2);
        assert_eq!(
            summary.distinct_ja4,
            vec!["ja4-a".to_string(), "ja4-b".to_string()]
        );
        assert_eq!(summary.distinct_ja4s, vec!["ja4s-a".to_string()]);

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn tls_ja4_sidecar_process_reports_early_exit() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()?;
        std::thread::sleep(Duration::from_millis(50));
        let mut process = TlsJa4SidecarProcess {
            child,
            output_path: dir.path().join("capture.jsonl"),
        };

        let error = process.ensure_running().unwrap_err();
        assert!(matches!(error, TelemetryError::TlsJa4SidecarExited(_)));

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn tls_ja4_sidecar_process_requires_non_empty_output() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()?;
        let process = TlsJa4SidecarProcess {
            child,
            output_path: dir.path().join("capture.jsonl"),
        };

        let error = process.stop_and_load_summary().unwrap_err();
        assert!(matches!(error, TelemetryError::TlsJa4SidecarNoOutput(_)));

        Ok(())
    }

    #[test]
    fn compare_probe_run_reports_tls_ja4_drift() {
        let baseline = sample_probe_run_evidence();
        let mut current = baseline.clone();
        current.tls_ja4 = Some(TlsJa4Summary {
            event_count: 1,
            distinct_ja4: vec!["ja4-b".to_string()],
            distinct_ja4s: vec!["ja4s-b".to_string()],
            distinct_alpn: vec!["http/1.1".to_string()],
            distinct_tls_versions: vec!["tls1.2".to_string()],
            first_ja4: Some("ja4-b".to_string()),
            last_ja4: Some("ja4-b".to_string()),
        });

        let drift = compare_probe_run(&baseline, &current);

        assert!(!drift.consistent());
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::TlsJa4Changed));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::TlsJa4sChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::TlsAlpnChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::TlsVersionChanged));
    }

    #[test]
    fn compare_probe_run_accepts_matching_evidence() {
        let baseline = sample_probe_run_evidence();
        let current = baseline.clone();

        let drift = compare_probe_run(&baseline, &current);

        assert!(drift.consistent());
        assert!(drift.issues.is_empty());
    }

    #[test]
    fn compare_probe_run_reports_identity_and_profile_drift() {
        let baseline = sample_probe_run_evidence();
        let mut current = baseline.clone();
        current.profile.browser_kind = BrowserKind::Brave;
        current.profile.browser_version = "124.0.0.0".to_string();
        current.profile.backend = BrowserBackendKind::Chromiumoxide;
        current.profile.headless = false;
        current.profile.proxy_mode = ProbeProxyMode::Residential;
        current.profile.browser_binary_basename = "Brave Browser".to_string();
        current.profile.launch_profile_hash = "deadbeefdeadbeef".to_string();
        current.fingerprint.user_agent = "Mozilla/5.0 Chrome/124".to_string();
        current.headers.accept_language = Some("en-US,en;q=0.9".to_string());
        current.fingerprint.platform = Some("Win32".to_string());
        current.fingerprint.timezone = Some("America/New_York".to_string());

        let drift = compare_probe_run(&baseline, &current);

        assert!(!drift.consistent());
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::BrowserKindChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::BrowserVersionChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::BackendChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::HeadlessChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::ProxyModeChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::BrowserBinaryChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::LaunchProfileHashChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::UserAgentChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::AcceptLanguageChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::PlatformChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::TimezoneChanged));
    }

    #[test]
    fn compare_probe_run_reports_behavior_drift() {
        let baseline = sample_probe_run_evidence();
        let mut current = baseline.clone();
        current.behavior.count = 20;
        current.behavior.path_len_px = 480.0;
        current.behavior.straightness = Some(0.1);
        current.behavior.mean_dt_s = Some(0.7);
        current.behavior.event_rate_hz = Some(2.0);

        let drift = compare_probe_run(&baseline, &current);

        assert!(!drift.consistent());
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::BehaviorCountDrift));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::BehaviorPathDrift));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::BehaviorStraightnessDrift));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::BehaviorMeanDtDrift));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::BehaviorEventRateDrift));
    }

    #[test]
    fn probe_reports_sanitize_string_fields() {
        let mut fingerprint = ProbeFingerprintReport {
            session_id: "sess\u{200b}ion".to_string(),
            body: FingerprintSnapshot {
                session_id: "sess\u{2060}ion".to_string(),
                ts: 1.0,
                url: "https://exa\u{200b}mple.com".to_string(),
                user_agent: "Mozi\u{2060}lla".to_string(),
                webdriver: Some(false),
                platform: Some("Mac\u{200b}Intel".to_string()),
                language: Some("en\u{2060}-AU".to_string()),
                languages: Some(vec!["en\u{200b}-AU".to_string()]),
                timezone: Some("Austral\u{2060}ia/Melbourne".to_string()),
                screen: None,
                hardware_concurrency: None,
                device_memory: None,
                webgl_vendor: Some("Ven\u{200b}dor".to_string()),
                webgl_renderer: Some("Rend\u{2060}erer".to_string()),
                plugins_count: None,
            },
            headers: FingerprintHeaders {
                user_agent: Some("Mozi\u{200b}lla".to_string()),
                accept_language: Some("en\u{2060}-AU".to_string()),
                sec_ch_ua: Some("\"Chrom\u{200b}e\"".to_string()),
                sec_ch_ua_platform: Some("\"mac\u{2060}OS\"".to_string()),
                sec_fetch_site: Some("sam\u{200b}e-origin".to_string()),
                x_forwarded_for: Some("127.0.0.\u{2060}1".to_string()),
            },
        };
        fingerprint.sanitize();
        assert_eq!(fingerprint.session_id, "session");
        assert_eq!(fingerprint.body.url, "https://example.com");
        assert_eq!(fingerprint.body.user_agent, "Mozilla");
        assert_eq!(fingerprint.body.platform.as_deref(), Some("MacIntel"));
        assert_eq!(fingerprint.body.languages.as_deref(), Some(&["en-AU".to_string()][..]));
        assert_eq!(fingerprint.headers.accept_language.as_deref(), Some("en-AU"));

        let mut sequence = ProbeSequenceReport {
            run_id: "run\u{200b}id".to_string(),
            summary: RequestSequenceSummary {
                request_count: 2,
                first_path: Some("/fi\u{2060}rst".to_string()),
                last_path: Some("/la\u{200b}st".to_string()),
                distinct_path_count: 2,
                path_sequence: vec!["/fi\u{200b}rst".to_string(), "/la\u{2060}st".to_string()],
                mean_gap_ms: Some(10.0),
                max_gap_ms: Some(12.0),
            },
        };
        sequence.sanitize();
        assert_eq!(sequence.run_id, "runid");
        assert_eq!(sequence.summary.first_path.as_deref(), Some("/first"));
        assert_eq!(sequence.summary.last_path.as_deref(), Some("/last"));
        assert_eq!(sequence.summary.path_sequence, vec!["/first", "/last"]);
    }

    #[test]
    fn compare_probe_run_reports_request_sequence_drift() {
        let baseline = sample_probe_run_evidence();
        let mut current = baseline.clone();
        current.request_sequence.request_count = 6;
        current.request_sequence.path_sequence.push("/fp".to_string());
        current.request_sequence.mean_gap_ms = Some(150.0);
        current.request_sequence.max_gap_ms = Some(250.0);

        let drift = compare_probe_run_with_thresholds(
            &baseline,
            &current,
            &ProbeDriftThresholds {
                mean_gap_ratio: 0.25,
                max_gap_ratio: 0.25,
                ..ProbeDriftThresholds::default()
            },
        );

        assert!(!drift.consistent());
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::RequestCountChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::PathSequenceChanged));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::MeanGapDrift));
        assert!(drift
            .issues
            .iter()
            .any(|issue| issue.kind == ProbeDriftKind::MaxGapDrift));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn browser_manager_probe_captures_identity_behavior_and_sanitizes_snapshot()
    -> Result<(), Box<dyn std::error::Error>> {
        let _guard = acquire_live_browser_test_guard().await;
        let (origin, state, server) = start_probe_server().await?;
        let manager = BrowserManager::new(test_browser_config());

        let navigate = manager
            .handle_request(request(
                None,
                BrowserAction::Navigate {
                    url: format!("{origin}/fp-probe"),
                },
                30_000,
            ))
            .await;
        assert!(navigate.success, "{navigate:?}");

        let session_id = navigate.session_id.clone();
        let wait = manager
            .handle_request(request(
                Some(session_id.clone()),
                BrowserAction::Wait {
                    selector: Some("body[data-probe-fp='ready']".to_string()),
                    ref_: None,
                    timeout_ms: 10_000,
                },
                12_000,
            ))
            .await;
        assert!(wait.success, "{wait:?}");

        let snapshot = manager
            .handle_request(request(Some(session_id.clone()), BrowserAction::Snapshot, 20_000))
            .await;
        assert!(snapshot.success, "{snapshot:?}");

        let fingerprint_session = manager
            .handle_request(request(
                Some(session_id.clone()),
                BrowserAction::Evaluate {
                    code: "window.__probeSessionId".to_string(),
                },
                10_000,
            ))
            .await;
        let fingerprint_session = response_string(&fingerprint_session);

        let behavior_nav = manager
            .handle_request(request(
                Some(session_id.clone()),
                BrowserAction::Navigate {
                    url: format!("{origin}/behavior-probe"),
                },
                30_000,
            ))
            .await;
        assert!(behavior_nav.success, "{behavior_nav:?}");

        let behavior_wait = manager
            .handle_request(request(
                Some(session_id.clone()),
                BrowserAction::Wait {
                    selector: Some("body[data-behavior-ready='true']".to_string()),
                    ref_: None,
                    timeout_ms: 10_000,
                },
                12_000,
            ))
            .await;
        assert!(behavior_wait.success, "{behavior_wait:?}");

        let behavior_snapshot = manager
            .handle_request(request(Some(session_id.clone()), BrowserAction::Snapshot, 20_000))
            .await;
        let behavior_snapshot = behavior_snapshot.snapshot.as_ref().unwrap();
        let alpha = snapshot_ref(behavior_snapshot, "Alpha");
        let bravo = snapshot_ref(behavior_snapshot, "Bravo");
        let target = snapshot_ref(behavior_snapshot, "Target");

        assert!(
            manager
                .handle_request(request(Some(session_id.clone()), BrowserAction::Hover { ref_: alpha }, 10_000))
                .await
                .success
        );
        assert!(
            manager
                .handle_request(request(Some(session_id.clone()), BrowserAction::Hover { ref_: bravo }, 10_000))
                .await
                .success
        );
        assert!(
            manager
                .handle_request(request(Some(session_id.clone()), BrowserAction::Click { ref_: target }, 10_000))
                .await
                .success
        );

        let behavior_flush = manager
            .handle_request(request(
                Some(session_id.clone()),
                BrowserAction::Evaluate {
                    code: "window.__flushBehavior(true).then(() => true)".to_string(),
                },
                10_000,
            ))
            .await;
        assert!(behavior_flush.success, "{behavior_flush:?}");

        let behavior_session = manager
            .handle_request(request(
                Some(session_id),
                BrowserAction::Evaluate {
                    code: "window.__probeSessionId".to_string(),
                },
                10_000,
            ))
            .await;
        let behavior_session = response_string(&behavior_session);

        manager.shutdown().await;
        server.abort();

        let fingerprint = state.fingerprint(&fingerprint_session).unwrap();
        assert_eq!(
            fingerprint.headers.user_agent.as_deref(),
            Some(fingerprint.body.user_agent.as_str())
        );
        assert!(
            fingerprint
                .headers
                .accept_language
                .as_deref()
                .is_some_and(|value| !value.is_empty())
        );
        assert!(
            fingerprint
                .body
                .languages
                .as_ref()
                .is_some_and(|languages| !languages.is_empty())
        );

        let snapshot_content = snapshot
            .snapshot
            .as_ref()
            .and_then(|page| page.content.as_deref())
            .unwrap();
        assert_eq!(sanitize_dom_text(snapshot_content).as_ref(), snapshot_content);
        assert!(!snapshot_content.contains('\u{200B}'));

        let behavior_batches = state.behaviors(&behavior_session);
        assert!(!behavior_batches.is_empty());
        let total_count: usize = behavior_batches.iter().map(|batch| batch.summary.count).sum();
        let total_path_len: f64 = behavior_batches
            .iter()
            .map(|batch| batch.summary.path_len_px)
            .sum();
        assert!(total_count >= 3);
        assert!(total_path_len > 0.0);
        assert!(behavior_batches.iter().any(|batch| batch.summary.mean_dt_s.is_some()));
        assert!(behavior_batches.iter().any(|batch| batch.summary.max_idle_gap_s.is_some()));
        assert!(behavior_batches.iter().any(|batch| batch.summary.event_rate_hz.is_some()));

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patchright_probe_captures_identity_and_behavior() -> Result<(), Box<dyn std::error::Error>> {
        let _guard = acquire_live_browser_test_guard().await;
        let (origin, state, server) = start_probe_server().await?;
        let config = test_browser_config();
        let profile = patchright_profile(&config);
        let mut session = PatchrightSession::start(&config.protection, &profile).await?;

        session.goto(&format!("{origin}/fp-probe")).await?;
        assert!(session.wait_selector("body[data-probe-fp='ready']", 10_000).await?);
        let fingerprint_session = wait_for_patchright_session_id(&state, "fingerprint").await?;

        session.goto(&format!("{origin}/behavior-probe")).await?;
        assert!(session
            .wait_selector("body[data-behavior-ready='true']", 10_000)
            .await?);
        let centers = session
            .evaluate(
                r#"(() => ['box-a', 'box-b', 'box-c'].map(id => {
                    const rect = document.getElementById(id).getBoundingClientRect();
                    return { x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 };
                }))()"#,
            )
            .await?;
        let centers = centers.as_array().unwrap();

        for point in &centers[..2] {
            session
                .mouse_move(
                    point["x"].as_f64().unwrap(),
                    point["y"].as_f64().unwrap(),
                )
                .await?;
        }
        session
            .mouse_click(
                centers[2]["x"].as_f64().unwrap(),
                centers[2]["y"].as_f64().unwrap(),
                1,
            )
            .await?;
        sleep(Duration::from_millis(500)).await;
        let behavior_session = wait_for_patchright_session_id(&state, "behavior").await?;

        session.close().await?;
        server.abort();

        let fingerprint = state.fingerprint(&fingerprint_session).unwrap();
        assert_eq!(
            fingerprint.headers.user_agent.as_deref(),
            Some(fingerprint.body.user_agent.as_str())
        );
        assert!(
            fingerprint
                .headers
                .accept_language
                .as_deref()
                .is_some_and(|value| !value.is_empty())
        );

        let behavior_batches = state.behaviors(&behavior_session);
        assert!(!behavior_batches.is_empty());
        let total_count: usize = behavior_batches.iter().map(|batch| batch.summary.count).sum();
        let total_path_len: f64 = behavior_batches
            .iter()
            .map(|batch| batch.summary.path_len_px)
            .sum();
        assert!(total_count >= 3);
        assert!(total_path_len > 0.0);
        assert!(behavior_batches
            .iter()
            .any(|batch| batch.summary.mean_speed_px_s.is_some()));
        assert!(behavior_batches
            .iter()
            .any(|batch| batch.summary.straightness.is_some()));
        assert!(!behavior_batches[0].sample.is_empty());

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn browser_manager_probe_canary_reports_clean_runs() -> Result<(), Box<dyn std::error::Error>>
    {
        let _guard = acquire_live_browser_test_guard().await;
        let (origin, _state, server) = start_probe_server().await?;
        let manager = BrowserManager::new(test_browser_config());
        let report = manager
            .run_probe_canary(ProbeCanarySpec {
                origin,
                browser: BrowserPreference::Auto,
                backends: vec![
                    BrowserBackendKind::Chromiumoxide,
                    BrowserBackendKind::Patchright,
                ],
                policy: ProbeTelemetryPolicy {
                    persist_baselines: false,
                    ..ProbeTelemetryPolicy::default()
                },
                tls_sidecar: None,
            })
            .await?;

        manager.shutdown().await;
        server.abort();

        assert_eq!(report.backends.len(), 2);
        for backend in &report.backends {
            assert_eq!(backend.verdict, ProbeCanaryVerdict::Clean, "{backend:?}");
            assert!(backend.error.is_none());
            assert!(backend.baseline.is_none());
            let evidence = backend.evidence.as_ref().unwrap();
            assert_eq!(evidence.profile.backend, backend.backend);
            assert!(evidence.behavior.count > 0);
            assert_eq!(evidence.request_sequence.request_count, 5);
        }

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn browser_manager_probe_captures_request_sequence() -> Result<(), Box<dyn std::error::Error>> {
        let _guard = acquire_live_browser_test_guard().await;
        let (origin, state, server) = start_probe_server().await?;
        let manager = BrowserManager::new(test_browser_config());
        let run_id = uuid::Uuid::new_v4().to_string();

        let navigate = manager
            .handle_request(request(
                None,
                BrowserAction::Navigate {
                    url: format!("{origin}/sequence-probe?run_id={run_id}"),
                },
                30_000,
            ))
            .await;
        assert!(navigate.success, "{navigate:?}");

        let wait = manager
            .handle_request(request(
                Some(navigate.session_id),
                BrowserAction::Wait {
                    selector: Some("body[data-sequence-ready='true']".to_string()),
                    ref_: None,
                    timeout_ms: 10_000,
                },
                12_000,
            ))
            .await;
        assert!(wait.success, "{wait:?}");

        manager.shutdown().await;
        server.abort();

        let summary = state.request_summary(&run_id);
        assert_eq!(summary.request_count, 5);
        assert_eq!(summary.first_path.as_deref(), Some("/sequence-probe"));
        assert_eq!(summary.last_path.as_deref(), Some("/sequence-step"));
        assert_eq!(summary.distinct_path_count, 3);
        assert_eq!(
            summary.path_sequence,
            vec![
                "/sequence-probe".to_string(),
                "/session".to_string(),
                "/sequence-step".to_string(),
                "/sequence-step".to_string(),
                "/sequence-step".to_string(),
            ]
        );
        assert!(summary.mean_gap_ms.is_some_and(|gap| gap > 0.0));
        assert!(summary.max_gap_ms.is_some_and(|gap| gap >= 40.0));

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patchright_probe_captures_request_sequence() -> Result<(), Box<dyn std::error::Error>> {
        let _guard = acquire_live_browser_test_guard().await;
        let (origin, state, server) = start_probe_server().await?;
        let config = test_browser_config();
        let profile = patchright_profile(&config);
        let mut session = PatchrightSession::start(&config.protection, &profile).await?;
        let run_id = uuid::Uuid::new_v4().to_string();

        session
            .goto(&format!("{origin}/sequence-probe?run_id={run_id}"))
            .await?;
        assert!(session
            .wait_selector("body[data-sequence-ready='true']", 10_000)
            .await?);

        session.close().await?;
        server.abort();

        let summary = state.request_summary(&run_id);
        assert_eq!(summary.request_count, 5);
        assert_eq!(summary.first_path.as_deref(), Some("/sequence-probe"));
        assert_eq!(summary.last_path.as_deref(), Some("/sequence-step"));
        assert_eq!(summary.distinct_path_count, 3);
        assert_eq!(
            summary.path_sequence,
            vec![
                "/sequence-probe".to_string(),
                "/session".to_string(),
                "/sequence-step".to_string(),
                "/sequence-step".to_string(),
                "/sequence-step".to_string(),
            ]
        );
        assert!(summary.mean_gap_ms.is_some_and(|gap| gap > 0.0));
        assert!(summary.max_gap_ms.is_some_and(|gap| gap >= 40.0));

        Ok(())
    }
}
