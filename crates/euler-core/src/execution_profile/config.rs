//! `profiles.json` (user profiles and per-context defaults) and the project
//! overlay `.euler/profile-overlay.json` (ADR 0026 section 5). Unknown
//! fields, unknown values, and contradictory combinations are errors that
//! name the file and field, never warnings: a dropped field could leave a
//! profile looser than written.

use super::{
    ApprovalsSetting, BackgroundJobsSetting, CachesSetting, CredentialsSetting, EscalationSetting,
    ExecutionProfile, NetworkSetting, Preset, ProfileError, ProfileFields, SandboxSetting,
    Tightness, TmpSetting,
};
use crate::guardian::PermissionReviewer;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The user profile file, in the Euler home.
pub const PROFILES_FILE: &str = "profiles.json";
/// The one tightening overlay a repository may hold, relative to its root.
pub const PROJECT_OVERLAY_PATH: &str = ".euler/profile-overlay.json";

/// `defaults.subagent` value meaning the parent's resolved profile.
const INHERIT: &str = "inherit";
/// Built-in `defaults.interactive` is `full`; `defaults.headless` is
/// `workspace` (ADR 0026 resolved question 11).
const BUILT_IN_INTERACTIVE: Preset = Preset::Full;
const BUILT_IN_HEADLESS: Preset = Preset::Workspace;

const PROFILE_FIELDS: &str = "sandbox, network, writable_paths, tmp, caches, credentials, \
                              approvals, escalation, background_jobs, reviewer";

/// Which context default selects the profile when `--profile` is absent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileContext {
    /// `euler` and `euler tui`.
    Interactive,
    /// `euler exec`.
    Headless,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfilesFile {
    #[serde(default)]
    defaults: ProfileDefaults,
    #[serde(default)]
    profiles: UniqueKeys<UniqueKeys<Value>>,
}

/// A JSON object whose duplicate keys are an error rather than last-wins,
/// so a repeated field cannot silently replace the one a reader sees first.
#[derive(Debug, Default)]
struct UniqueKeys<T>(BTreeMap<String, T>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for UniqueKeys<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueKeysVisitor<T>(std::marker::PhantomData<T>);

        impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for UniqueKeysVisitor<T> {
            type Value = UniqueKeys<T>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut access: A,
            ) -> Result<Self::Value, A::Error> {
                let mut entries = BTreeMap::new();
                while let Some((key, value)) = access.next_entry::<String, T>()? {
                    if entries.contains_key(&key) {
                        return Err(serde::de::Error::custom(format!("duplicate key `{key}`")));
                    }
                    entries.insert(key, value);
                }
                Ok(UniqueKeys(entries))
            }
        }

        deserializer.deserialize_map(UniqueKeysVisitor(std::marker::PhantomData))
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileDefaults {
    interactive: Option<String>,
    headless: Option<String>,
    subagent: Option<String>,
}

/// Fields a user profile or the project overlay sets; `None` keeps the
/// value it is applied to.
#[derive(Clone, Debug, Default)]
pub(super) struct ProfileOverrides {
    pub(super) sandbox: Option<SandboxSetting>,
    pub(super) network: Option<NetworkSetting>,
    pub(super) writable_paths: Option<Vec<String>>,
    pub(super) tmp: Option<TmpSetting>,
    pub(super) caches: Option<CachesSetting>,
    pub(super) credentials: Option<CredentialsSetting>,
    pub(super) approvals: Option<ApprovalsSetting>,
    pub(super) escalation: Option<EscalationSetting>,
    pub(super) background_jobs: Option<BackgroundJobsSetting>,
    pub(super) reviewer: Option<PermissionReviewer>,
}

impl ProfileOverrides {
    /// Parse each key on its own so an error names the field it is about.
    fn from_map<'a>(
        entries: impl IntoIterator<Item = (&'a String, &'a Value)>,
    ) -> Result<Self, String> {
        let mut overrides = Self::default();
        for (key, value) in entries {
            match key.as_str() {
                "sandbox" => overrides.sandbox = Some(field_value(key, value)?),
                "network" => overrides.network = Some(field_value(key, value)?),
                "writable_paths" => {
                    let paths: Vec<String> = field_value(key, value)?;
                    if let Some(path) = paths
                        .iter()
                        .find(|path| !(Path::new(path).is_absolute() || path.starts_with("~/")))
                    {
                        return Err(format!(
                            "writable_paths: \"{path}\" must be an absolute path or start with ~/"
                        ));
                    }
                    overrides.writable_paths = Some(paths);
                }
                "tmp" => overrides.tmp = Some(field_value(key, value)?),
                "caches" => overrides.caches = Some(field_value(key, value)?),
                "credentials" => overrides.credentials = Some(field_value(key, value)?),
                "approvals" => overrides.approvals = Some(field_value(key, value)?),
                "escalation" => overrides.escalation = Some(field_value(key, value)?),
                "background_jobs" => overrides.background_jobs = Some(field_value(key, value)?),
                "reviewer" => overrides.reviewer = Some(field_value(key, value)?),
                _ => {
                    return Err(format!(
                        "unknown field `{key}`; expected one of {PROFILE_FIELDS}"
                    ))
                }
            }
        }
        Ok(overrides)
    }

    /// Apply as a user profile over its preset: listed `writable_paths` add
    /// to the preset's; every other field replaces the preset's value.
    fn extend(&self, fields: &mut ProfileFields) {
        if let Some(paths) = &self.writable_paths {
            for path in paths {
                if !fields.writable_paths.contains(path) {
                    fields.writable_paths.push(path.clone());
                }
            }
        }
        replace(&mut fields.sandbox, self.sandbox);
        replace(&mut fields.network, self.network);
        replace(&mut fields.tmp, self.tmp);
        replace(&mut fields.caches, self.caches);
        replace(&mut fields.credentials, self.credentials);
        replace(&mut fields.approvals, self.approvals);
        replace(&mut fields.escalation, self.escalation);
        replace(&mut fields.background_jobs, self.background_jobs);
        replace(&mut fields.reviewer, self.reviewer);
    }

    /// Apply as the project overlay: each field it sets must be at least as
    /// tight as the resolved value. Returns the first field that loosens.
    pub(super) fn tighten(&self, fields: &mut ProfileFields) -> Result<(), String> {
        if self.reviewer.is_some() {
            return Err("reviewer has no order, so a project overlay cannot set it".to_owned());
        }
        if let Some(paths) = &self.writable_paths {
            if let Some(path) = paths
                .iter()
                .find(|path| !fields.writable_paths.contains(path))
            {
                return Err(format!(
                    "writable_paths: \"{path}\" is not writable in the profile; a project \
                     overlay can only remove paths"
                ));
            }
            fields.writable_paths.clone_from(paths);
        }
        tighten_field("sandbox", &mut fields.sandbox, self.sandbox)?;
        tighten_field("network", &mut fields.network, self.network)?;
        tighten_field("tmp", &mut fields.tmp, self.tmp)?;
        tighten_field("caches", &mut fields.caches, self.caches)?;
        tighten_field("credentials", &mut fields.credentials, self.credentials)?;
        tighten_field("approvals", &mut fields.approvals, self.approvals)?;
        tighten_field("escalation", &mut fields.escalation, self.escalation)?;
        tighten_field(
            "background_jobs",
            &mut fields.background_jobs,
            self.background_jobs,
        )
    }
}

fn field_value<T: DeserializeOwned>(key: &str, value: &Value) -> Result<T, String> {
    T::deserialize(value).map_err(|error| format!("{key}: {error}"))
}

fn replace<T>(slot: &mut T, value: Option<T>) {
    if let Some(value) = value {
        *slot = value;
    }
}

fn tighten_field<T: Tightness>(field: &str, slot: &mut T, value: Option<T>) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.rank() < slot.rank() {
        return Err(format!(
            "{field} \"{}\" is looser than the profile's \"{}\"; a project overlay can only tighten",
            value.text(),
            slot.text()
        ));
    }
    *slot = value;
    Ok(())
}

#[derive(Clone, Debug)]
struct UserProfile {
    extends: Preset,
    overrides: ProfileOverrides,
}

/// Parsed `profiles.json`. A user who never writes the file gets
/// [`ProfilesConfig::default`]: the presets and the built-in defaults.
#[derive(Clone, Debug, Default)]
pub struct ProfilesConfig {
    file: PathBuf,
    defaults: ProfileDefaults,
    profiles: BTreeMap<String, UserProfile>,
}

impl ProfilesConfig {
    /// Read `file`; a missing file is the built-in configuration.
    pub fn load(file: &Path) -> Result<Self, ProfileError> {
        match fs::read_to_string(file) {
            Ok(text) => Self::parse(file, &text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(ProfileError::Read {
                file: file.to_path_buf(),
                message: error.to_string(),
            }),
        }
    }

    /// Parse and validate `text` as `profiles.json`.
    pub fn parse(file: &Path, text: &str) -> Result<Self, ProfileError> {
        let invalid = |message: String| ProfileError::InvalidFile {
            file: file.to_path_buf(),
            message,
        };
        let parsed: ProfilesFile =
            serde_json::from_str(text).map_err(|error| invalid(error.to_string()))?;
        let mut profiles = BTreeMap::new();
        for (name, body) in parsed.profiles.0 {
            let profile = parse_user_profile(&name, &body)
                .map_err(|message| invalid(format!("profile \"{name}\": {message}")))?;
            profiles.insert(name, profile);
        }
        let config = Self {
            file: file.to_path_buf(),
            defaults: parsed.defaults,
            profiles,
        };
        for (field, name) in [
            ("interactive", &config.defaults.interactive),
            ("headless", &config.defaults.headless),
            ("subagent", &config.defaults.subagent),
        ] {
            let Some(name) = name else { continue };
            let allowed = config.is_defined(name) || (field == "subagent" && name == INHERIT);
            if !allowed {
                return Err(invalid(format!(
                    "defaults.{field}: unknown profile \"{name}\"; defined profiles: {}",
                    config.defined_names().join(", ")
                )));
            }
        }
        Ok(config)
    }

    /// Resolve the profile for a fresh launch or a resume: `requested`
    /// (`--profile`) first, then the context default, then the built-in
    /// default; then the project overlay; then refuse what this build cannot
    /// enforce.
    pub fn resolve(
        &self,
        requested: Option<&str>,
        context: ProfileContext,
        overlay: Option<&ProjectOverlay>,
    ) -> Result<ExecutionProfile, ProfileError> {
        let name = match requested {
            Some(name) => name.to_owned(),
            None => self.default_name(context),
        };
        self.resolve_named(&name, overlay)
    }

    /// Resolve `defaults.subagent` as a launch resolves a profile, overlay
    /// included: `None` when it is unset or `inherit`, so child agents run
    /// under the session's profile. The session tightens the result against
    /// its own profile when a child spawns
    /// ([`ExecutionProfile::child_profile`]).
    pub fn resolve_subagent(
        &self,
        overlay: Option<&ProjectOverlay>,
    ) -> Result<Option<ExecutionProfile>, ProfileError> {
        match self.defaults.subagent.as_deref() {
            None | Some(INHERIT) => Ok(None),
            Some(name) => self.resolve_named(name, overlay).map(Some),
        }
    }

    fn resolve_named(
        &self,
        name: &str,
        overlay: Option<&ProjectOverlay>,
    ) -> Result<ExecutionProfile, ProfileError> {
        let mut profile = self.named(name)?;
        if let Some(overlay) = overlay {
            // Name the overlay when it, not the profile, makes the profile
            // one this build cannot enforce.
            let enforced_before = profile.check_enforced().is_ok();
            overlay.apply(&mut profile)?;
            if enforced_before {
                if let Err(ProfileError::Unenforced { profile, field }) = profile.check_enforced() {
                    return Err(ProfileError::OverlayUnenforced { profile, field });
                }
            }
        }
        profile.check_enforced()?;
        Ok(profile)
    }

    fn default_name(&self, context: ProfileContext) -> String {
        let (configured, built_in) = match context {
            ProfileContext::Interactive => (&self.defaults.interactive, BUILT_IN_INTERACTIVE),
            ProfileContext::Headless => (&self.defaults.headless, BUILT_IN_HEADLESS),
        };
        configured
            .clone()
            .unwrap_or_else(|| built_in.name().to_owned())
    }

    fn named(&self, name: &str) -> Result<ExecutionProfile, ProfileError> {
        let fields = if let Some(preset) = Preset::parse(name) {
            preset.fields()
        } else if let Some(profile) = self.profiles.get(name) {
            let mut fields = profile.extends.fields();
            profile.overrides.extend(&mut fields);
            if let Some(message) = fields.contradiction(profile.overrides.credentials.is_some()) {
                return Err(ProfileError::InvalidFile {
                    file: self.file.clone(),
                    message: format!("profile \"{name}\": {message}"),
                });
            }
            fields
        } else {
            return Err(ProfileError::UnknownProfile {
                name: name.to_owned(),
                defined: self.defined_names(),
            });
        };
        Ok(ExecutionProfile {
            name: name.to_owned(),
            fields,
        })
    }

    /// Whether the user's file names `defaults.interactive`. Without it the
    /// first interactive launch offers the first-run picker.
    pub fn has_interactive_default(&self) -> bool {
        self.defaults.interactive.is_some()
    }

    /// Every profile a user can select: the presets, then user profiles.
    pub fn profile_names(&self) -> Vec<String> {
        self.defined_names()
    }

    fn is_defined(&self, name: &str) -> bool {
        Preset::parse(name).is_some() || self.profiles.contains_key(name)
    }

    fn defined_names(&self) -> Vec<String> {
        Preset::ALL
            .iter()
            .map(|preset| preset.name().to_owned())
            .chain(self.profiles.keys().cloned())
            .collect()
    }
}

/// Write `defaults.interactive` into `file`, keeping every other entry. A
/// missing file is created with only that default. Refuses a file that is
/// not a JSON object rather than replacing what the user wrote. The new
/// file replaces the old by rename, so an Euler process interrupted
/// mid-write cannot leave a truncated file that fails every later launch.
pub fn write_interactive_default(file: &Path, name: &str) -> Result<(), ProfileError> {
    let invalid = |message: String| ProfileError::InvalidFile {
        file: file.to_path_buf(),
        message,
    };
    let mut document = match fs::read_to_string(file) {
        Ok(text) => {
            serde_json::from_str::<Value>(&text).map_err(|error| invalid(error.to_string()))?
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Value::Object(Default::default()),
        Err(error) => {
            return Err(ProfileError::Read {
                file: file.to_path_buf(),
                message: error.to_string(),
            })
        }
    };
    let Value::Object(root) = &mut document else {
        return Err(invalid("expected a JSON object".to_owned()));
    };
    let defaults = root
        .entry("defaults")
        .or_insert_with(|| Value::Object(Default::default()));
    let Value::Object(defaults) = defaults else {
        return Err(invalid("defaults: expected a JSON object".to_owned()));
    };
    defaults.insert("interactive".to_owned(), Value::String(name.to_owned()));
    let write_error = |error: io::Error| ProfileError::Write {
        file: file.to_path_buf(),
        message: error.to_string(),
    };
    let parent = file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(write_error)?;
    let mut text = serde_json::to_string_pretty(&document).expect("JSON value serializes");
    text.push('\n');
    let mut staged = tempfile::NamedTempFile::new_in(parent).map_err(write_error)?;
    io::Write::write_all(&mut staged, text.as_bytes()).map_err(write_error)?;
    staged
        .persist(file)
        .map(|_| ())
        .map_err(|error| write_error(error.error))
}

fn parse_user_profile(name: &str, body: &UniqueKeys<Value>) -> Result<UserProfile, String> {
    let valid_name = !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'));
    if !valid_name {
        return Err("a profile name uses only letters, digits, - and _".to_owned());
    }
    if Preset::parse(name).is_some() || name == INHERIT {
        return Err(format!(
            "\"{name}\" is reserved; choose another profile name"
        ));
    }
    let presets = Preset::ALL.map(Preset::name).join(", ");
    let extends = match body.0.get("extends") {
        None => {
            return Err(format!(
                "extends is required and names a preset ({presets})"
            ))
        }
        Some(Value::String(preset)) => Preset::parse(preset)
            .ok_or_else(|| format!("extends: \"{preset}\" is not a preset ({presets})"))?,
        Some(_) => return Err(format!("extends must name a preset ({presets})")),
    };
    let overrides =
        ProfileOverrides::from_map(body.0.iter().filter(|(key, _)| key.as_str() != "extends"))?;
    let mut fields = extends.fields();
    overrides.extend(&mut fields);
    if let Some(message) = fields.contradiction(overrides.credentials.is_some()) {
        return Err(message);
    }
    Ok(UserProfile { extends, overrides })
}

/// A repository's tightening overlay (`.euler/profile-overlay.json`). It
/// applies to whichever profile resolves and needs no trust acknowledgment,
/// because it can only tighten.
#[derive(Clone, Debug)]
pub struct ProjectOverlay {
    file: PathBuf,
    overrides: ProfileOverrides,
}

impl ProjectOverlay {
    /// Read the overlay under `root`; `None` when the repository has none.
    pub fn load(root: &Path) -> Result<Option<Self>, ProfileError> {
        let file = root.join(PROJECT_OVERLAY_PATH);
        let text = match fs::read_to_string(&file) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(ProfileError::Read {
                    file,
                    message: error.to_string(),
                })
            }
        };
        let invalid = |message: String| ProfileError::InvalidFile {
            file: file.clone(),
            message,
        };
        let body: UniqueKeys<Value> =
            serde_json::from_str(&text).map_err(|error| invalid(error.to_string()))?;
        let overrides = ProfileOverrides::from_map(&body.0).map_err(invalid)?;
        Ok(Some(Self { file, overrides }))
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    pub(super) fn apply(&self, profile: &mut ExecutionProfile) -> Result<(), ProfileError> {
        let invalid = |message: String| ProfileError::InvalidFile {
            file: self.file.clone(),
            message: format!("{message} (profile \"{}\")", profile.name),
        };
        let mut fields = profile.fields.clone();
        self.overrides.tighten(&mut fields).map_err(invalid)?;
        if let Some(message) = fields.contradiction(self.overrides.credentials.is_some()) {
            return Err(invalid(message));
        }
        profile.fields = fields;
        Ok(())
    }
}
