use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use toml_edit::{Array, DocumentMut, Item, Table, Value};

use crate::error::{VdashError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Claude,
    Gemini,
}

impl BackendKind {
    pub const ALL: [BackendKind; 2] = [BackendKind::Claude, BackendKind::Gemini];
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendKind::Claude => write!(f, "claude"),
            BackendKind::Gemini => write!(f, "gemini"),
        }
    }
}

impl FromStr for BackendKind {
    type Err = VdashError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "claude" => Ok(BackendKind::Claude),
            "gemini" => Ok(BackendKind::Gemini),
            other => Err(VdashError::new(2, format!("unknown backend '{other}'"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    SpecWriter,
    Architect,
    Orchestrator,
    TeamLead,
    Integrator,
    Critic,
    Monitor,
    Reviewer,
    Falsifier,
    Worker,
}

impl Role {
    pub const ALL: [Role; 10] = [
        Role::SpecWriter,
        Role::Architect,
        Role::Orchestrator,
        Role::TeamLead,
        Role::Integrator,
        Role::Critic,
        Role::Monitor,
        Role::Reviewer,
        Role::Falsifier,
        Role::Worker,
    ];
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Role::SpecWriter => "spec_writer",
            Role::Architect => "architect",
            Role::Orchestrator => "orchestrator",
            Role::TeamLead => "team_lead",
            Role::Integrator => "integrator",
            Role::Critic => "critic",
            Role::Monitor => "monitor",
            Role::Reviewer => "reviewer",
            Role::Falsifier => "falsifier",
            Role::Worker => "worker",
        };
        write!(f, "{s}")
    }
}

impl FromStr for Role {
    type Err = VdashError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "spec_writer" => Ok(Role::SpecWriter),
            "architect" => Ok(Role::Architect),
            "orchestrator" => Ok(Role::Orchestrator),
            "team_lead" => Ok(Role::TeamLead),
            "integrator" => Ok(Role::Integrator),
            "critic" => Ok(Role::Critic),
            "monitor" => Ok(Role::Monitor),
            "reviewer" => Ok(Role::Reviewer),
            "falsifier" => Ok(Role::Falsifier),
            "worker" => Ok(Role::Worker),
            other => Err(VdashError::new(2, format!("unknown role '{other}'"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleEntry {
    pub backend: BackendKind,
    pub model: Option<String>,
}

impl fmt::Display for RoleEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.model {
            Some(m) => write!(f, "{}:{m}", self.backend),
            None => write!(f, "{}", self.backend),
        }
    }
}

impl FromStr for RoleEntry {
    type Err = VdashError;

    fn from_str(s: &str) -> Result<Self> {
        match s.split_once(':') {
            Some((b, m)) => {
                let backend = BackendKind::from_str(b)?;
                if m.is_empty() {
                    return Err(VdashError::new(
                        2,
                        format!("empty model name in role entry '{s}'"),
                    ));
                }
                Ok(Self {
                    backend,
                    model: Some(m.to_string()),
                })
            }
            None => {
                let backend = BackendKind::from_str(s)?;
                Ok(Self {
                    backend,
                    model: None,
                })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendCfg {
    pub command: Vec<String>,
    pub max_concurrent: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub max_rounds: u32,
    pub max_leads: usize,
    pub max_parallel_lean: usize,
    pub agent_timeout_s: u64,
    pub lean_timeout_s: u64,
    pub integrate_every: u32,
    pub quota_max_wait_s: u64,
    /// Most spec-writer/reviewer rounds one `vdash spec` loop runs before it
    /// stops and hands back to the user.
    pub spec_max_rounds: u32,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_rounds: 20,
            max_leads: 3,
            max_parallel_lean: 4,
            agent_timeout_s: 1800,
            lean_timeout_s: 600,
            integrate_every: 3,
            quota_max_wait_s: 21600,
            spec_max_rounds: 10,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AmendmentPolicy {
    Manual,
    AutoNeutral,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub amendment_policy: AmendmentPolicy,
    pub kernel_recheck: bool,
    pub spillover: bool,
    pub comparator_cmd: String,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            amendment_policy: AmendmentPolicy::Manual,
            kernel_recheck: true,
            spillover: false,
            comparator_cmd: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub id: String,
    pub host_project: PathBuf,
    pub package: String,
    pub use_mathlib: bool,
    pub use_subscription: bool,
    pub backends: BTreeMap<BackendKind, BackendCfg>,
    pub roles: BTreeMap<Role, Vec<RoleEntry>>,
    pub budget: Budget,
    pub policy: Policy,
}

impl Config {
    pub fn entries_for(&self, role: Role) -> &[RoleEntry] {
        self.roles.get(&role).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn backend_cfg(&self, kind: BackendKind) -> &BackendCfg {
        &self.backends[&kind]
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawBackendCfg {
    command: Option<Vec<String>>,
    max_concurrent: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawBackends {
    claude: Option<RawBackendCfg>,
    gemini: Option<RawBackendCfg>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawRoleSpec {
    Single(String),
    List(Vec<String>),
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawRoles {
    spec_writer: Option<RawRoleSpec>,
    architect: Option<RawRoleSpec>,
    orchestrator: Option<RawRoleSpec>,
    team_lead: Option<RawRoleSpec>,
    integrator: Option<RawRoleSpec>,
    critic: Option<RawRoleSpec>,
    monitor: Option<RawRoleSpec>,
    reviewer: Option<RawRoleSpec>,
    falsifier: Option<RawRoleSpec>,
    worker: Option<RawRoleSpec>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawBudget {
    max_rounds: Option<u32>,
    max_leads: Option<usize>,
    max_parallel_lean: Option<usize>,
    agent_timeout_s: Option<u64>,
    lean_timeout_s: Option<u64>,
    integrate_every: Option<u32>,
    quota_max_wait_s: Option<u64>,
    spec_max_rounds: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    amendment_policy: Option<AmendmentPolicy>,
    kernel_recheck: Option<bool>,
    spillover: Option<bool>,
    comparator_cmd: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    id: String,
    host_project: PathBuf,
    package: Option<String>,
    use_mathlib: Option<bool>,
    use_subscription: Option<bool>,
    #[serde(default)]
    backends: RawBackends,
    #[serde(default)]
    roles: RawRoles,
    #[serde(default)]
    budget: RawBudget,
    #[serde(default)]
    policy: RawPolicy,
}

/// The built-in role split (DESIGN §3 table): Claude plans, leads, integrates and reviews
/// amendments; Gemini reviews the spec and falsifies; workers use both.
fn default_role_spec(role: Role) -> &'static [&'static str] {
    match role {
        Role::Reviewer | Role::Falsifier => &["gemini"],
        Role::Worker => &["claude", "gemini"],
        Role::SpecWriter | Role::Architect | Role::Orchestrator | Role::TeamLead | Role::Integrator | Role::Critic | Role::Monitor => &["claude"],
    }
}

/// The entries a role gets when `campaign.toml` does not set it (`/model default`).
pub fn default_role_entries(role: Role) -> Vec<RoleEntry> {
    default_role_spec(role)
        .iter()
        .map(|s| s.parse().expect("built-in role defaults parse"))
        .collect()
}

fn parse_role_spec(role: Role, spec: Option<RawRoleSpec>, default: &[&str]) -> Result<Vec<RoleEntry>> {
    let raw_list: Vec<String> = match spec {
        Some(RawRoleSpec::Single(s)) => vec![s],
        Some(RawRoleSpec::List(v)) => v,
        None => default.iter().map(|s| (*s).to_string()).collect(),
    };
    if raw_list.is_empty() {
        return Err(VdashError::new(
            2,
            format!("role '{role}' must have at least one backend entry"),
        ));
    }
    let mut out = Vec::with_capacity(raw_list.len());
    for item in raw_list {
        out.push(RoleEntry::from_str(&item)?);
    }
    Ok(out)
}

pub fn load(campaign_dir: impl AsRef<Path>) -> Result<Config> {
    let path = campaign_dir.as_ref().join("campaign.toml");
    let content = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(VdashError::new(2, format!("missing {}", path.display())));
        }
        Err(e) => return Err(VdashError::io_path(&path, e)),
    };

    let raw: RawConfig = toml::from_str(&content).map_err(|e| {
        let line_info = e
            .span()
            .map(|span| {
                let line = content[..span.start.min(content.len())]
                    .bytes()
                    .filter(|&b| b == b'\n')
                    .count()
                    + 1;
                format!("line {line}: ")
            })
            .unwrap_or_default();
        VdashError::new(2, format!("{}: {line_info}{}", path.display(), e.message()))
    })?;

    if raw.id.trim().is_empty() {
        return Err(VdashError::new(2, "campaign.toml: 'id' must not be empty"));
    }

    if !raw.host_project.is_dir() {
        return Err(VdashError::new(
            2,
            format!(
                "host_project '{}' is not a directory",
                raw.host_project.display()
            ),
        ));
    }
    if !raw.host_project.join("lean-toolchain").is_file() {
        return Err(VdashError::new(
            2,
            format!(
                "host_project '{}' does not contain lean-toolchain",
                raw.host_project.display()
            ),
        ));
    }

    let claude_raw = raw.backends.claude.unwrap_or_default();
    let claude_cfg = BackendCfg {
        command: claude_raw
            .command
            .unwrap_or_else(|| vec!["claude".to_string()]),
        max_concurrent: claude_raw.max_concurrent.unwrap_or(4),
    };
    if claude_cfg.command.is_empty() || claude_cfg.max_concurrent < 1 {
        return Err(VdashError::new(
            2,
            "backends.claude: command must be non-empty and max_concurrent >= 1",
        ));
    }

    let gemini_raw = raw.backends.gemini.unwrap_or_default();
    let gemini_cfg = BackendCfg {
        command: gemini_raw
            .command
            .unwrap_or_else(|| vec!["agy".to_string()]),
        max_concurrent: gemini_raw.max_concurrent.unwrap_or(4),
    };
    if gemini_cfg.command.is_empty() || gemini_cfg.max_concurrent < 1 {
        return Err(VdashError::new(
            2,
            "backends.gemini: command must be non-empty and max_concurrent >= 1",
        ));
    }

    let mut backends = BTreeMap::new();
    backends.insert(BackendKind::Claude, claude_cfg);
    backends.insert(BackendKind::Gemini, gemini_cfg);

    let mut roles = BTreeMap::new();
    roles.insert(
        Role::SpecWriter,
        parse_role_spec(Role::SpecWriter, raw.roles.spec_writer, default_role_spec(Role::SpecWriter))?,
    );
    roles.insert(
        Role::Architect,
        parse_role_spec(Role::Architect, raw.roles.architect, default_role_spec(Role::Architect))?,
    );
    roles.insert(
        Role::Orchestrator,
        parse_role_spec(Role::Orchestrator, raw.roles.orchestrator, default_role_spec(Role::Orchestrator))?,
    );
    roles.insert(
        Role::TeamLead,
        parse_role_spec(Role::TeamLead, raw.roles.team_lead, default_role_spec(Role::TeamLead))?,
    );
    roles.insert(
        Role::Integrator,
        parse_role_spec(Role::Integrator, raw.roles.integrator, default_role_spec(Role::Integrator))?,
    );
    roles.insert(
        Role::Critic,
        parse_role_spec(Role::Critic, raw.roles.critic, default_role_spec(Role::Critic))?,
    );
    roles.insert(
        Role::Monitor,
        parse_role_spec(Role::Monitor, raw.roles.monitor, default_role_spec(Role::Monitor))?,
    );
    roles.insert(
        Role::Reviewer,
        parse_role_spec(Role::Reviewer, raw.roles.reviewer, default_role_spec(Role::Reviewer))?,
    );
    roles.insert(
        Role::Falsifier,
        parse_role_spec(Role::Falsifier, raw.roles.falsifier, default_role_spec(Role::Falsifier))?,
    );
    roles.insert(
        Role::Worker,
        parse_role_spec(Role::Worker, raw.roles.worker, default_role_spec(Role::Worker))?,
    );

    let def_budget = Budget::default();
    let budget = Budget {
        max_rounds: raw.budget.max_rounds.unwrap_or(def_budget.max_rounds),
        max_leads: raw.budget.max_leads.unwrap_or(def_budget.max_leads),
        max_parallel_lean: raw
            .budget
            .max_parallel_lean
            .unwrap_or(def_budget.max_parallel_lean),
        agent_timeout_s: raw
            .budget
            .agent_timeout_s
            .unwrap_or(def_budget.agent_timeout_s),
        lean_timeout_s: raw
            .budget
            .lean_timeout_s
            .unwrap_or(def_budget.lean_timeout_s),
        integrate_every: raw
            .budget
            .integrate_every
            .unwrap_or(def_budget.integrate_every),
        quota_max_wait_s: raw
            .budget
            .quota_max_wait_s
            .unwrap_or(def_budget.quota_max_wait_s),
        spec_max_rounds: raw.budget.spec_max_rounds.unwrap_or(def_budget.spec_max_rounds),
    };

    if budget.max_rounds < 1
        || budget.max_leads < 1
        || budget.max_parallel_lean < 1
        || budget.agent_timeout_s < 1
        || budget.lean_timeout_s < 1
        || budget.integrate_every < 1
        || budget.quota_max_wait_s < 1
        || budget.spec_max_rounds < 1
    {
        return Err(VdashError::new(2, "all [budget] values must be >= 1"));
    }

    let def_policy = Policy::default();
    let policy = Policy {
        amendment_policy: raw
            .policy
            .amendment_policy
            .unwrap_or(def_policy.amendment_policy),
        kernel_recheck: raw.policy.kernel_recheck.unwrap_or(def_policy.kernel_recheck),
        spillover: raw.policy.spillover.unwrap_or(def_policy.spillover),
        comparator_cmd: raw
            .policy
            .comparator_cmd
            .unwrap_or(def_policy.comparator_cmd),
    };

    let package = raw.package.unwrap_or_else(|| "Apc".to_string());
    if package != "Apc" {
        return Err(VdashError::new(
            2,
            format!("campaign.toml: unsupported package '{package}' (only \"Apc\" is supported)"),
        ));
    }

    Ok(Config {
        id: raw.id,
        host_project: raw.host_project,
        package,
        use_mathlib: raw.use_mathlib.unwrap_or(true),
        use_subscription: raw.use_subscription.unwrap_or(true),
        backends,
        roles,
        budget,
        policy,
    })
}

/// Write the built-in split for every role into `campaign.toml` (`/model default`).
pub fn reset_roles(campaign_dir: impl AsRef<Path>) -> Result<()> {
    for r in Role::ALL {
        set_role(campaign_dir.as_ref(), r, &default_role_entries(r))?;
    }
    Ok(())
}

/// One line per backend naming the roles it serves by default, for `/model default`.
pub fn default_split_summary() -> String {
    let names = |pred: &dyn Fn(&[RoleEntry]) -> bool| {
        Role::ALL
            .into_iter()
            .filter(|&r| pred(&default_role_entries(r)))
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let only = |b: BackendKind| move |es: &[RoleEntry]| es.len() == 1 && es[0].backend == b;
    format!(
        "claude: {}; gemini: {}; both: {}",
        names(&only(BackendKind::Claude)),
        names(&only(BackendKind::Gemini)),
        names(&|es: &[RoleEntry]| es.len() > 1)
    )
}

pub fn set_role(campaign_dir: impl AsRef<Path>, role: Role, entries: &[RoleEntry]) -> Result<()> {
    if entries.is_empty() {
        return Err(VdashError::new(
            2,
            format!("role '{role}' must have at least one backend entry"),
        ));
    }

    let path = campaign_dir.as_ref().join("campaign.toml");
    let content = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(VdashError::new(2, format!("missing {}", path.display())));
        }
        Err(e) => return Err(VdashError::io_path(&path, e)),
    };

    let mut doc: DocumentMut = content
        .parse()
        .map_err(|e| VdashError::new(2, format!("{}: {e}", path.display())))?;

    if !doc.contains_key("roles") {
        doc["roles"] = Item::Table(Table::new());
    }

    let val = if entries.len() == 1 {
        Value::from(entries[0].to_string())
    } else {
        let mut arr = Array::new();
        for entry in entries {
            arr.push(entry.to_string());
        }
        Value::Array(arr)
    };

    doc["roles"][role.to_string()] = Item::Value(val);
    fs::write(&path, doc.to_string()).map_err(|e| VdashError::io_path(&path, e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_host(tmp: &TempDir) -> PathBuf {
        let host = tmp.path().join("host");
        fs::create_dir_all(&host).unwrap();
        fs::write(host.join("lean-toolchain"), "leanprover/lean4:v4.33.0\n").unwrap();
        host
    }

    #[test]
    fn defaults_and_role_parsing() {
        let tmp = TempDir::new().unwrap();
        let host = make_host(&tmp);
        let camp = tmp.path().join("camp");
        fs::create_dir_all(&camp).unwrap();
        fs::write(
            camp.join("campaign.toml"),
            format!(
                "id = \"0123456789abcdef0123456789abcdef\"\nhost_project = \"{}\"\n",
                host.display()
            ),
        )
        .unwrap();

        let cfg = load(&camp).unwrap();
        assert_eq!(cfg.package, "Apc");
        assert!(cfg.use_mathlib);
        assert!(cfg.use_subscription);
        assert_eq!(cfg.backend_cfg(BackendKind::Claude).command, vec!["claude"]);
        assert_eq!(cfg.backend_cfg(BackendKind::Claude).max_concurrent, 4);
        assert_eq!(cfg.backend_cfg(BackendKind::Gemini).command, vec!["agy"]);
        assert_eq!(cfg.backend_cfg(BackendKind::Gemini).max_concurrent, 4);
        assert_eq!(
            cfg.entries_for(Role::SpecWriter),
            &[RoleEntry {
                backend: BackendKind::Claude,
                model: None
            }]
        );
        assert_eq!(
            cfg.entries_for(Role::Reviewer),
            &[RoleEntry {
                backend: BackendKind::Gemini,
                model: None
            }]
        );
        assert_eq!(
            cfg.entries_for(Role::Worker),
            &[
                RoleEntry {
                    backend: BackendKind::Claude,
                    model: None
                },
                RoleEntry {
                    backend: BackendKind::Gemini,
                    model: None
                }
            ]
        );
        assert_eq!(cfg.budget.max_rounds, 20);
        assert_eq!(cfg.budget.max_leads, 3);
        assert_eq!(cfg.budget.max_parallel_lean, 4);
        assert_eq!(cfg.budget.agent_timeout_s, 1800);
        assert_eq!(cfg.budget.lean_timeout_s, 600);
        assert_eq!(cfg.budget.integrate_every, 3);
        assert_eq!(cfg.budget.quota_max_wait_s, 21600);
        assert_eq!(cfg.policy.amendment_policy, AmendmentPolicy::Manual);
        assert!(cfg.policy.kernel_recheck);
        assert!(!cfg.policy.spillover);
        assert_eq!(cfg.policy.comparator_cmd, "");

        let parsed: RoleEntry = "gemini:pro-x".parse().unwrap();
        assert_eq!(
            parsed,
            RoleEntry {
                backend: BackendKind::Gemini,
                model: Some("pro-x".to_string())
            }
        );
    }

    #[test]
    fn string_and_list_role_entries_accepted() {
        let tmp = TempDir::new().unwrap();
        let host = make_host(&tmp);
        let camp = tmp.path().join("camp");
        fs::create_dir_all(&camp).unwrap();
        fs::write(
            camp.join("campaign.toml"),
            format!(
                r#"id = "0123456789abcdef0123456789abcdef"
host_project = "{}"
[roles]
worker = "gemini:pro-x"
reviewer = ["claude:sonnet", "gemini"]
"#,
                host.display()
            ),
        )
        .unwrap();

        let cfg = load(&camp).unwrap();
        assert_eq!(
            cfg.entries_for(Role::Worker),
            &[RoleEntry {
                backend: BackendKind::Gemini,
                model: Some("pro-x".to_string())
            }]
        );
        assert_eq!(
            cfg.entries_for(Role::Reviewer),
            &[
                RoleEntry {
                    backend: BackendKind::Claude,
                    model: Some("sonnet".to_string())
                },
                RoleEntry {
                    backend: BackendKind::Gemini,
                    model: None
                }
            ]
        );
    }

    #[test]
    fn validation_errors_return_code_2() {
        let tmp = TempDir::new().unwrap();
        let host = make_host(&tmp);
        let camp = tmp.path().join("camp");
        fs::create_dir_all(&camp).unwrap();

        // 1. Missing file
        let err = load(&camp).unwrap_err();
        assert_eq!(err.code, 2);

        let write_and_err = |body: &str| -> VdashError {
            fs::write(camp.join("campaign.toml"), body).unwrap();
            let err = load(&camp).unwrap_err();
            assert_eq!(err.code, 2, "expected code 2 for: {body}");
            err
        };

        // 2. TOML syntax error with line
        let err = write_and_err("id = \"abc\"\nthis is not valid toml = = =\n");
        assert!(err.message.contains("line 2"), "msg: {}", err.message);

        // 3. Unknown key (named)
        let err = write_and_err(&format!(
            "id = \"abc\"\nhost_project = \"{}\"\nbogus_key = 1\n",
            host.display()
        ));
        assert!(err.message.contains("bogus_key"), "msg: {}", err.message);

        // 4. Wrong type
        let _ = write_and_err(&format!(
            "id = \"abc\"\nhost_project = \"{}\"\nuse_mathlib = 123\n",
            host.display()
        ));

        // 5. host_project not a dir / lacking lean-toolchain
        let _ = write_and_err("id = \"abc\"\nhost_project = \"/nonexistent/path/xyz\"\n");
        let empty_dir = tmp.path().join("empty_host");
        fs::create_dir_all(&empty_dir).unwrap();
        let _ = write_and_err(&format!(
            "id = \"abc\"\nhost_project = \"{}\"\n",
            empty_dir.display()
        ));

        // 6. Role naming an unknown backend
        let _ = write_and_err(&format!(
            "id = \"abc\"\nhost_project = \"{}\"\n[roles]\nworker = \"openai\"\n",
            host.display()
        ));

        // 7. Empty role list
        let _ = write_and_err(&format!(
            "id = \"abc\"\nhost_project = \"{}\"\n[roles]\nworker = []\n",
            host.display()
        ));

        // 8. amendment_policy not in enum
        let _ = write_and_err(&format!(
            "id = \"abc\"\nhost_project = \"{}\"\n[policy]\namendment_policy = \"yolo\"\n",
            host.display()
        ));

        // 9. Budget value or max_concurrent below 1
        let _ = write_and_err(&format!(
            "id = \"abc\"\nhost_project = \"{}\"\n[budget]\nmax_rounds = 0\n",
            host.display()
        ));
        let _ = write_and_err(&format!(
            "id = \"abc\"\nhost_project = \"{}\"\n[backends.claude]\nmax_concurrent = 0\n",
            host.display()
        ));

        // 10. Unsupported package name
        let err = write_and_err(&format!(
            "id = \"abc\"\nhost_project = \"{}\"\npackage = \"CustomPkg\"\n",
            host.display()
        ));
        assert!(err.message.contains("package"), "msg: {}", err.message);
    }

    #[test]
    fn reset_roles_restores_the_builtin_split() {
        let tmp = TempDir::new().unwrap();
        let host = make_host(&tmp);
        let camp = tmp.path().join("camp");
        fs::create_dir_all(&camp).unwrap();
        let base = format!("id = \"abc\"\nhost_project = \"{}\"\n", host.display());
        fs::write(camp.join("campaign.toml"), &base).unwrap();
        let defaults = load(&camp).unwrap().roles;

        let mut all_claude = format!("# keep me\n{base}\n[roles]\n");
        for r in Role::ALL {
            all_claude.push_str(&format!("{r} = \"claude:opus\"\n"));
        }
        fs::write(camp.join("campaign.toml"), &all_claude).unwrap();
        assert_ne!(load(&camp).unwrap().roles, defaults);

        reset_roles(&camp).unwrap();
        assert_eq!(load(&camp).unwrap().roles, defaults);
        assert!(fs::read_to_string(camp.join("campaign.toml")).unwrap().contains("# keep me"));
        assert_eq!(
            default_split_summary(),
            "claude: spec_writer, architect, orchestrator, team_lead, integrator, critic, monitor; \
             gemini: reviewer, falsifier; both: worker"
        );
    }

    #[test]
    fn set_role_preserves_comments_and_other_keys() {
        let tmp = TempDir::new().unwrap();
        let host = make_host(&tmp);
        let camp = tmp.path().join("camp");
        fs::create_dir_all(&camp).unwrap();
        fs::write(
            camp.join("campaign.toml"),
            format!(
                "# Top-level comment preserved\nid = \"abc\"\nhost_project = \"{}\"\n\n[roles]\n# Role comment preserved\nspec_writer = \"claude\"\n",
                host.display()
            ),
        )
        .unwrap();

        set_role(
            &camp,
            Role::Worker,
            &[
                "gemini:pro-x".parse().unwrap(),
                "claude:sonnet".parse().unwrap(),
            ],
        )
        .unwrap();

        let text = fs::read_to_string(camp.join("campaign.toml")).unwrap();
        assert!(text.contains("# Top-level comment preserved"));
        assert!(text.contains("# Role comment preserved"));

        let cfg = load(&camp).unwrap();
        assert_eq!(
            cfg.entries_for(Role::Worker),
            &[
                RoleEntry {
                    backend: BackendKind::Gemini,
                    model: Some("pro-x".to_string())
                },
                RoleEntry {
                    backend: BackendKind::Claude,
                    model: Some("sonnet".to_string())
                }
            ]
        );
    }
}
