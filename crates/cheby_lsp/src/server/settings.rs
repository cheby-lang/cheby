//! The server's settings (plan §1.9, A §12.3, D-1376).
//!
//! One `cheby` object serves every carrier: `initializationOptions` is the
//! object itself, a `workspace/configuration` answer for the section
//! `cheby` is the object itself, and `didChangeConfiguration` carries it
//! in `params.settings.cheby`.
//!
//! Settings are built in layers: the defaults, then `initializationOptions`,
//! then the latest configuration answer, each key of a later layer winning
//! over the same key of an earlier one. Reading is lenient per key: a value
//! of the wrong type is skipped with a warning, so the key keeps what the
//! earlier layers gave it, its default at the bottom. A key or group this
//! server does not know is skipped with a warning too. A bad setting never
//! stops the server. A `null` value means no opinion, except for
//! `analysis.target`, where it means no target.

use std::time::Duration;

use cheby_driver::analysis::Target;
use serde::Serialize;
use serde_json::{Map, Value};

/// Every setting, with the defaults of D-1376.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Settings {
    pub analysis: AnalysisSettings,
    pub diagnostics: DiagnosticSettings,
    pub completion: CompletionSettings,
    pub inlay_hints: InlaySettings,
    pub lenses: LensSettings,
    pub ownership: OwnershipSettings,
    pub constants: ConstantSettings,
    pub semantic_tokens: SemanticTokenSettings,
    /// The client commands the editor runs, which decide run lenses.
    pub client_commands: Vec<String>,
}

/// `analysis.*`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AnalysisSettings {
    /// Check for this target only, as `cheby check --target` (D-550).
    #[serde(serialize_with = "target_name")]
    pub target: Option<Target>,
    /// The quiet time after an edit before a run, 0 to 1000.
    pub debounce_ms: u64,
    /// How long a request waits for its text's analysis, 100 to 60000.
    pub request_wait_ms: u64,
}

/// `diagnostics.*`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiagnosticSettings {
    pub explain_links: bool,
}

/// `completion.*`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompletionSettings {
    pub auto_import: bool,
    pub type_driven: bool,
}

/// `inlayHints.*`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one switch per kind of hint (D-1376)"
)]
pub(crate) struct InlaySettings {
    pub enable: bool,
    pub let_types: bool,
    pub use_binders: bool,
    pub closure_types: bool,
    pub pipe_stages: bool,
    #[serde(rename = "dyn")]
    pub dyn_: bool,
    pub parameter_names: bool,
    pub constants: bool,
}

/// `lenses.*`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LensSettings {
    pub enable: bool,
    pub interfaces: bool,
    pub provides: bool,
}

/// `ownership.*`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OwnershipSettings {
    pub enabled: bool,
    pub lens: Lens,
    pub inlays: Inlays,
    pub tail: Tail,
}

/// `constants.*`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConstantSettings {
    pub evaluate: bool,
    /// 1 to 3600.
    pub child_timeout_seconds: u64,
}

/// `semanticTokens.*`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SemanticTokenSettings {
    pub enabled: bool,
}

/// `ownership.lens`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Lens {
    Hot,
    All,
    Off,
}

/// `ownership.inlays`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Inlays {
    Misses,
    All,
    Off,
}

/// `ownership.tail`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Tail {
    All,
    #[serde(rename = "notTail")]
    NonTailOnly,
    Off,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            analysis: AnalysisSettings {
                target: None,
                debounce_ms: 40,
                request_wait_ms: 10_000,
            },
            diagnostics: DiagnosticSettings {
                explain_links: true,
            },
            completion: CompletionSettings {
                auto_import: true,
                type_driven: true,
            },
            inlay_hints: InlaySettings {
                enable: true,
                let_types: true,
                use_binders: true,
                closure_types: true,
                pipe_stages: true,
                dyn_: true,
                parameter_names: false,
                constants: false,
            },
            lenses: LensSettings {
                enable: true,
                interfaces: true,
                provides: false,
            },
            ownership: OwnershipSettings {
                enabled: true,
                lens: Lens::Hot,
                inlays: Inlays::Misses,
                tail: Tail::All,
            },
            constants: ConstantSettings {
                evaluate: true,
                child_timeout_seconds: 60,
            },
            semantic_tokens: SemanticTokenSettings { enabled: true },
            client_commands: Vec::new(),
        }
    }
}

/// `analysis.target` by its name, as the manifest spells it.
#[expect(
    clippy::ref_option,
    clippy::trivially_copy_pass_by_ref,
    reason = "the signature serde's `serialize_with` calls"
)]
fn target_name<S: serde::Serializer>(
    target: &Option<Target>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match target {
        Some(target) => serializer.serialize_str(target.as_str()),
        None => serializer.serialize_none(),
    }
}

impl Settings {
    /// The defaults with each layer applied in order, and a warning for
    /// each value skipped.
    pub(crate) fn from_layers<'a>(
        layers: impl IntoIterator<Item = &'a Value>,
    ) -> (Self, Vec<String>) {
        let mut settings = Self::default();
        let mut warnings = Vec::new();
        for layer in layers {
            settings.apply(layer, &mut warnings);
        }
        (settings, warnings)
    }

    /// The debounce as a duration.
    pub(crate) const fn debounce(&self) -> Duration {
        Duration::from_millis(self.analysis.debounce_ms)
    }

    /// Applies one layer: a `cheby` object.
    fn apply(&mut self, layer: &Value, warnings: &mut Vec<String>) {
        let Some(object) = layer.as_object() else {
            if !layer.is_null() {
                warnings.push(format!(
                    "settings: expected an object, found {}",
                    kind(layer)
                ));
            }
            return;
        };
        for (key, value) in object {
            if key == "clientCommands" {
                read_strings(key, value, &mut self.client_commands, warnings);
                continue;
            }
            let Some(group) = value.as_object() else {
                if !value.is_null() && GROUPS.contains(&key.as_str()) {
                    warnings.push(format!(
                        "settings: `{key}` must be an object, found {}",
                        kind(value)
                    ));
                } else if !value.is_null() {
                    warnings.push(format!("settings: unknown key `{key}` ignored"));
                }
                continue;
            };
            let mut r = Reader {
                group: key,
                object: group,
                warnings: &mut *warnings,
                known: Vec::new(),
            };
            if self.read_group(key, &mut r) {
                r.finish();
            } else {
                r.warnings
                    .push(format!("settings: unknown key `{key}` ignored"));
            }
        }
    }

    /// Reads the group `key` through `r`. False for a group this server
    /// does not know.
    fn read_group(&mut self, key: &str, r: &mut Reader<'_>) -> bool {
        match key {
            "analysis" => {
                let a = &mut self.analysis;
                r.target("target", &mut a.target);
                r.int("debounceMs", 0, 1000, &mut a.debounce_ms);
                r.int("requestWaitMs", 100, 60_000, &mut a.request_wait_ms);
            }
            "diagnostics" => r.bool("explainLinks", &mut self.diagnostics.explain_links),
            "completion" => {
                let c = &mut self.completion;
                r.bool("autoImport", &mut c.auto_import);
                r.bool("typeDriven", &mut c.type_driven);
            }
            "inlayHints" => {
                let h = &mut self.inlay_hints;
                r.bool("enable", &mut h.enable);
                r.bool("letTypes", &mut h.let_types);
                r.bool("useBinders", &mut h.use_binders);
                r.bool("closureTypes", &mut h.closure_types);
                r.bool("pipeStages", &mut h.pipe_stages);
                r.bool("dyn", &mut h.dyn_);
                r.bool("parameterNames", &mut h.parameter_names);
                r.bool("constants", &mut h.constants);
            }
            "lenses" => {
                let l = &mut self.lenses;
                r.bool("enable", &mut l.enable);
                r.bool("interfaces", &mut l.interfaces);
                r.bool("provides", &mut l.provides);
            }
            "ownership" => {
                let o = &mut self.ownership;
                r.bool("enabled", &mut o.enabled);
                r.choice(
                    "lens",
                    &[("hot", Lens::Hot), ("all", Lens::All), ("off", Lens::Off)],
                    &mut o.lens,
                );
                r.choice(
                    "inlays",
                    &[
                        ("misses", Inlays::Misses),
                        ("all", Inlays::All),
                        ("off", Inlays::Off),
                    ],
                    &mut o.inlays,
                );
                r.choice(
                    "tail",
                    &[
                        ("all", Tail::All),
                        ("notTail", Tail::NonTailOnly),
                        ("off", Tail::Off),
                    ],
                    &mut o.tail,
                );
            }
            "constants" => {
                let c = &mut self.constants;
                r.bool("evaluate", &mut c.evaluate);
                r.int("childTimeoutSeconds", 1, 3600, &mut c.child_timeout_seconds);
            }
            "semanticTokens" => r.bool("enabled", &mut self.semantic_tokens.enabled),
            _ => return false,
        }
        true
    }
}

/// The groups of the `cheby` object.
const GROUPS: &[&str] = &[
    "analysis",
    "diagnostics",
    "completion",
    "inlayHints",
    "lenses",
    "ownership",
    "constants",
    "semanticTokens",
];

/// Reads the keys of one group, each into its slot.
struct Reader<'a> {
    group: &'a str,
    object: &'a Map<String, Value>,
    warnings: &'a mut Vec<String>,
    /// The keys read, so that the rest can be reported as unknown.
    known: Vec<&'static str>,
}

impl Reader<'_> {
    /// The value of `key`, `None` when it is absent or `null`.
    fn value(&mut self, key: &'static str) -> Option<&Value> {
        self.known.push(key);
        self.object.get(key).filter(|v| !v.is_null())
    }

    /// A wrong value of `key`: it keeps what it had.
    fn bad(&mut self, key: &str, wanted: &str, found: &Value) {
        self.warnings.push(format!(
            "settings: `{}.{key}` must be {wanted}, found {}; the value is ignored",
            self.group,
            kind(found)
        ));
    }

    fn bool(&mut self, key: &'static str, slot: &mut bool) {
        let Some(value) = self.value(key).cloned() else {
            return;
        };
        match value.as_bool() {
            Some(b) => *slot = b,
            None => self.bad(key, "true or false", &value),
        }
    }

    /// An integer, clamped to `min..=max`.
    fn int(&mut self, key: &'static str, min: u64, max: u64, slot: &mut u64) {
        let Some(value) = self.value(key).cloned() else {
            return;
        };
        if let Some(n) = value.as_u64() {
            *slot = n.clamp(min, max);
        } else if value.as_i64().is_some() {
            // A negative integer.
            *slot = min;
        } else {
            self.bad(key, "an integer", &value);
        }
    }

    /// One of `options`, by name.
    fn choice<T: Copy>(&mut self, key: &'static str, options: &[(&str, T)], slot: &mut T) {
        let Some(value) = self.value(key).cloned() else {
            return;
        };
        let found = value
            .as_str()
            .and_then(|name| options.iter().find(|(n, _)| *n == name));
        if let Some(&(_, choice)) = found {
            *slot = choice;
        } else {
            let names: Vec<String> = options.iter().map(|(n, _)| format!("\"{n}\"")).collect();
            self.bad(key, &format!("one of {}", names.join(", ")), &value);
        }
    }

    /// `null` or a target's name.
    fn target(&mut self, key: &'static str, slot: &mut Option<Target>) {
        self.known.push(key);
        let Some(value) = self.object.get(key).cloned() else {
            return;
        };
        if value.is_null() {
            *slot = None;
            return;
        }
        let found = value.as_str().and_then(|name| {
            [Target::Native, Target::Js]
                .into_iter()
                .find(|t| t.as_str() == name)
        });
        match found {
            Some(target) => *slot = Some(target),
            None => self.bad(key, "null, \"native\" or \"js\"", &value),
        }
    }

    /// Warns about each key of the group that was not read.
    fn finish(self) {
        for key in self.object.keys() {
            if !self.known.contains(&key.as_str()) {
                self.warnings.push(format!(
                    "settings: unknown key `{}.{key}` ignored",
                    self.group
                ));
            }
        }
    }
}

/// `clientCommands`: a list of strings.
fn read_strings(key: &str, value: &Value, slot: &mut Vec<String>, warnings: &mut Vec<String>) {
    if value.is_null() {
        return;
    }
    let strings = value.as_array().and_then(|items| {
        items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
    });
    match strings {
        Some(strings) => *slot = strings,
        None => warnings.push(format!(
            "settings: `{key}` must be a list of strings, found {}; the value is ignored",
            kind(value)
        )),
    }
}

/// The JSON kind of `value`, for a warning.
fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}
