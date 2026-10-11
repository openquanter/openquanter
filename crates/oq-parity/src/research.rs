//! Identified diagnostic evidence and its cumulative research trial ledger.

use std::cell::{Cell, RefCell};

use oq_engine::Tick;
use oq_features::warmup::{WarmupConfig, WarmupPath, WarmupSensitivity, sensitivity};
use oq_hash::sha256_hex;
use oq_stats::trials::TrialRegistry;

use crate::{BaselineStatus, RunManifest};

const SCOPE: &str = "openquanter warmup research evidence v1";

/// The caller supplies the effective feature/decision constructor policy,
/// research question and compatible return-sampling basis. Diagnostic
/// decisions do not provide Sharpe ratios and are recorded as unscored.
pub struct StudyIdentity {
    pub code_commit: String,
    pub question: String,
    pub sampling_basis: String,
    pub policy: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WarmupStudy {
    pub report: WarmupSensitivity,
    pub manifest: RunManifest,
    pub policy_hash: String,
    /// First original input index actually fed to any path.
    pub consumed_start: usize,
    pub trial_ids: Vec<String>,
    pub registry_hash: String,
    pub registry_total_trials: usize,
}

fn known_code(code: &str) -> bool {
    matches!(code.len(), 40 | 64) && code.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parsed immutable attachment metadata and the checksummed result body.
#[derive(Debug, Clone, PartialEq)]
pub struct ResearchAttachment {
    pub manifest: RunManifest,
    pub policy_hash: String,
    pub consumed_start: usize,
    pub consumed_end: usize,
    pub registry_hash: String,
    pub registry_total_trials: usize,
    pub trial_ids: Vec<String>,
    pub body: String,
}

impl ResearchAttachment {
    #[must_use]
    pub fn summary_line(&self) -> String {
        format!(
            "{}: consumed [{}..{}), policy {}, ledger {}, {} cumulative trials, body {} bytes",
            self.manifest.label,
            self.consumed_start,
            self.consumed_end,
            self.policy_hash,
            self.registry_hash,
            self.registry_total_trials,
            self.body.len()
        )
    }
    pub fn verify_registry(&self, registry: &TrialRegistry) -> Result<(), String> {
        if registry.len() != self.registry_total_trials
            || sha256_hex(registry.to_ledger().as_bytes()) != self.registry_hash
            || self
                .trial_ids
                .iter()
                .any(|id| !registry.unscored().contains(id))
        {
            return Err("research attachment needs its exact as-of trial ledger".into());
        }
        Ok(())
    }

    pub fn verify_baseline(&self, expected: &RunManifest) -> Result<BaselineStatus, String> {
        baseline(&self.manifest, expected)
    }
}

fn baseline(manifest: &RunManifest, expected: &RunManifest) -> Result<BaselineStatus, String> {
    if expected.label != SCOPE
        || !known_code(&expected.code_commit)
        || !known_code(&manifest.code_commit)
    {
        return Err("study baseline needs the same scope and known code identity".into());
    }
    let status = expected.compare(manifest);
    if matches!(status, BaselineStatus::Invalidated { .. }) {
        return Err("study inputs or effective policy changed; no behavior conclusion".into());
    }
    Ok(status)
}

/// Strict ordered metadata and exact body checksum; extra/truncated fields
/// are refused rather than returning a successful prefix.
pub fn parse_attachment(text: &str) -> Result<ResearchAttachment, String> {
    let (header, body) = text
        .split_once("\nbody\n")
        .ok_or("research attachment has no result body")?;
    let mut lines = header.lines();
    if lines.next() != Some("openquanter-warmup-evidence 1") {
        return Err("unsupported research attachment version".into());
    }
    let mut get = |key: &str| -> Result<String, String> {
        lines
            .next()
            .and_then(|line| line.strip_prefix(key))
            .map(str::to_string)
            .ok_or_else(|| format!("expected research field {key}"))
    };
    if get("scope=")? != SCOPE {
        return Err("foreign research attachment scope".into());
    }
    let code_commit = get("code_commit=")?;
    if code_commit != "unknown" && !known_code(&code_commit) {
        return Err("invalid research code identity".into());
    }
    let hash = |value: String| -> Result<String, String> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err("invalid research content hash".into());
        }
        Ok(value)
    };
    let number = |value: String| -> Result<usize, String> {
        let n = value
            .parse::<usize>()
            .map_err(|_| "invalid research count")?;
        if n.to_string() != value {
            return Err("non-canonical research count".into());
        }
        Ok(n)
    };
    let data_hash = hash(get("data_hash=")?)?;
    let config_hash = hash(get("config_hash=")?)?;
    let policy_hash = hash(get("policy_hash=")?)?;
    let consumed_start = number(get("consumed_start=")?)?;
    let consumed_end = number(get("consumed_end=")?)?;
    let registry_hash = hash(get("registry_hash=")?)?;
    let registry_total_trials = number(get("registry_total_trials=")?)?;
    let count = number(get("trials=")?)?;
    if count < 2
        || count > header.lines().count()
        || count > registry_total_trials
        || consumed_start >= consumed_end
    {
        return Err("invalid research range or trial count".into());
    }
    let mut trial_ids = Vec::with_capacity(count);
    for _ in 0..count {
        let id = hash(get("trial=")?)?;
        if trial_ids.contains(&id) {
            return Err("duplicate research variant identity".into());
        }
        trial_ids.push(id);
    }
    let body_hash = hash(get("body_hash=")?)?;
    if lines.next().is_some() || body.is_empty() || sha256_hex(body.as_bytes()) != body_hash {
        return Err("research attachment body is changed or metadata is extra".into());
    }
    Ok(ResearchAttachment {
        manifest: RunManifest {
            code_commit,
            data_hash,
            config_hash,
            label: SCOPE.into(),
        },
        policy_hash,
        consumed_start,
        consumed_end,
        registry_hash,
        registry_total_trials,
        trial_ids,
        body: body.into(),
    })
}

impl WarmupStudy {
    /// Behavior comparability is independent of later ledger appends;
    /// exact artifact reproduction verifies the linked ledger separately.
    pub fn verify_baseline(&self, expected: &RunManifest) -> Result<BaselineStatus, String> {
        baseline(&self.manifest, expected)
    }

    /// The exact as-of cumulative ledger is a separate dependency. Reload
    /// that snapshot, not an edited or subsequently appended ledger.
    pub fn verify_registry(&self, registry: &TrialRegistry) -> Result<(), String> {
        if registry.len() != self.registry_total_trials
            || sha256_hex(registry.to_ledger().as_bytes()) != self.registry_hash
            || self
                .trial_ids
                .iter()
                .any(|id| !registry.unscored().contains(id))
        {
            return Err("research trial ledger differs from the linked snapshot".into());
        }
        Ok(())
    }

    /// Versioned evidence attachment: identity, consumed range, policy,
    /// trial-ledger dependency and checksummed human-readable result.
    #[must_use]
    pub fn artifact_text(&self) -> String {
        let body = self.report.render();
        let mut text = format!(
            "openquanter-warmup-evidence 1\nscope={SCOPE}\ncode_commit={}\ndata_hash={}\nconfig_hash={}\npolicy_hash={}\nconsumed_start={}\nconsumed_end={}\nregistry_hash={}\nregistry_total_trials={}\ntrials={}\n",
            self.manifest.code_commit,
            self.manifest.data_hash,
            self.manifest.config_hash,
            self.policy_hash,
            self.consumed_start,
            self.report.evaluation_end,
            self.registry_hash,
            self.registry_total_trials,
            self.trial_ids.len()
        );
        for id in &self.trial_ids {
            text.push_str(&format!("trial={id}\n"));
        }
        text.push_str(&format!(
            "body_hash={}\nbody\n{body}",
            sha256_hex(body.as_bytes())
        ));
        text
    }
}

fn size(bytes: &mut Vec<u8>, value: usize) -> Result<(), String> {
    bytes.extend(
        u64::try_from(value)
            .map_err(|_| "study size exceeds canonical precision")?
            .to_le_bytes(),
    );
    Ok(())
}

fn blob(bytes: &mut Vec<u8>, value: &[u8]) -> Result<(), String> {
    size(bytes, value.len())?;
    bytes.extend(value);
    Ok(())
}

/// Run one bound warm-up study. Every constructed path, including the
/// reference, is appended to the existing trial registry as unscored.
/// A failure after path construction began retains those attempts.
/// Invalid configuration or foreign registry basis refuses before trials.
pub fn warmup_study<P: WarmupPath>(
    ticks: &[Tick],
    config: &WarmupConfig,
    identity: &StudyIdentity,
    registry: &mut TrialRegistry,
    factory: impl Fn() -> P,
) -> Result<WarmupStudy, String> {
    let mut warmups = config.warmup_ticks.clone();
    warmups.sort_unstable();
    if config.evaluation_start >= config.evaluation_end
        || config.evaluation_end > ticks.len()
        || !config.tolerance.is_finite()
        || config.tolerance < 0.0
        || warmups.len() < 2
        || warmups.windows(2).any(|pair| pair[0] == pair[1])
        || warmups
            .last()
            .is_some_and(|warmup| *warmup > config.evaluation_start)
    {
        return Err("invalid warm-up study configuration".into());
    }
    if (identity.code_commit != "unknown" && !known_code(&identity.code_commit))
        || identity.policy.is_empty()
        || [&identity.question, &identity.sampling_basis]
            .iter()
            .any(|text| text.trim().is_empty() || text.contains(['\n', '\r', '\t']))
    {
        return Err(
            "study needs explicit code identity, question, sampling basis and policy".into(),
        );
    }
    let question_hash = sha256_hex(identity.question.as_bytes());
    let basis = format!(
        "question-sha256={question_hash}; sampling={}",
        identity.sampling_basis
    );
    match registry.basis() {
        Some(prior) if prior != basis => {
            return Err("research registry question or sampling basis differs".into());
        }
        None if !registry.is_empty() => {
            return Err("non-empty research registry has no declared basis".into());
        }
        _ => {}
    }
    let consumed_start = config.evaluation_start - warmups.last().expect("checked");
    let mut input = b"openquanter consumed warmup ticks v1\0".to_vec();
    for tick in &ticks[consumed_start..config.evaluation_end] {
        for value in [
            tick.stamp.exch.0,
            tick.stamp.local.0,
            tick.last.0,
            tick.high.0,
            tick.low.0,
            tick.bid.0,
            tick.ask.0,
            tick.volume.0,
        ] {
            input.extend(value.to_le_bytes());
        }
    }
    let mut policy = Vec::new();
    blob(&mut policy, SCOPE.as_bytes())?;
    blob(&mut policy, identity.question.as_bytes())?;
    blob(&mut policy, identity.sampling_basis.as_bytes())?;
    blob(&mut policy, &identity.policy)?;
    size(&mut policy, config.evaluation_start)?;
    size(&mut policy, config.evaluation_end)?;
    size(&mut policy, warmups.len())?;
    for warmup in &warmups {
        size(&mut policy, *warmup)?;
    }
    policy.extend(config.tolerance.to_bits().to_le_bytes());
    let manifest = RunManifest::from_content(
        identity.code_commit.to_ascii_lowercase(),
        &input,
        &policy,
        SCOPE,
    );
    let policy_hash = sha256_hex(&identity.policy);
    registry.set_basis(basis);
    let registry = RefCell::new(registry);
    let trial_ids = RefCell::new(Vec::new());
    let next = Cell::new(0usize);
    let report = sensitivity(ticks, config, || {
        let warmup = warmups[next.get()];
        next.set(next.get() + 1);
        let id = sha256_hex(
            format!(
                "{SCOPE};code={};data={};config={};warmup={warmup}",
                manifest.code_commit, manifest.data_hash, manifest.config_hash
            )
            .as_bytes(),
        );
        registry.borrow_mut().record_unscored(id.clone());
        trial_ids.borrow_mut().push(id);
        factory()
    })?;
    let registry = registry.borrow();
    Ok(WarmupStudy {
        report,
        manifest,
        policy_hash,
        consumed_start,
        trial_ids: trial_ids.into_inner(),
        registry_hash: sha256_hex(registry.to_ledger().as_bytes()),
        registry_total_trials: registry.len(),
    })
}
