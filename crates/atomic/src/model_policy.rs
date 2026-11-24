// SPDX-License-Identifier: Apache-2.0

//! Operator-owned model routing for graph coding lanes. Selection is pure so
//! the caller can checkpoint it before starting a model turn.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Endpoint credentials and address selected together for one graph lane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneEndpoint {
    /// OpenAI-compatible API URL, local or remote.
    pub base_url: String,
    /// Model identifier at this endpoint.
    pub name: String,
    /// Name of an operator-provided environment variable, never a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

/// One ordered routing rule. Both selectors, when present, must match.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneRoute {
    /// Exact graph item kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// A match on any one host graph label.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels_any: Vec<String>,
    /// Name in [`LanePolicy::endpoints`].
    pub endpoint: String,
}

/// A route set owned by the operator. Routes are evaluated in written order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LanePolicy {
    /// Named endpoint catalog.
    pub endpoints: BTreeMap<String, LaneEndpoint>,
    /// Ordered item selectors.
    #[serde(default)]
    pub routes: Vec<LaneRoute>,
    /// Required fallback endpoint name.
    pub default: String,
}

/// The complete choice to freeze in a durable lane receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneChoice {
    /// Selected catalog name.
    pub endpoint: String,
    /// Exact endpoint and model used by this lane.
    pub target: LaneEndpoint,
}

/// The admission-time endpoint catalog and the operator file from which only
/// later route edits may be read. A running lane uses its recorded choice.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LanePolicySource {
    /// The operator's own configuration, never a project file.
    pub path: PathBuf,
    /// The endpoint catalog admitted with the run's caps and prices.
    pub endpoints: BTreeMap<String, LaneEndpoint>,
}

impl LaneChoice {
    /// The provider configuration a recorded lane choice gives its turns.
    /// A credential's value is read by the model runtime from the named
    /// environment variable; it is never put in the durable receipt.
    pub fn model_endpoint(&self) -> atomic_core::codex::ModelEndpoint {
        atomic_core::codex::ModelEndpoint {
            base_url: self.target.base_url.clone(),
            model: self.target.name.clone(),
            api_key_env: self.target.api_key_env.clone(),
            sign_in: None,
        }
    }
}

impl LanePolicySource {
    /// Check the admitted endpoint catalog even when the source was restored
    /// from a durable run rather than built from current operator settings.
    pub fn validate_catalog(&self) -> Result<(), String> {
        let default =
            self.endpoints.keys().next().ok_or_else(|| {
                "model.lanes.endpoints must name at least one endpoint".to_owned()
            })?;
        LanePolicy {
            endpoints: self.endpoints.clone(),
            routes: Vec::new(),
            default: default.clone(),
        }
        .validate()
    }

    /// Read the latest operator route declarations for an unstarted lane.
    /// Endpoint changes require a fresh run and fresh budget admission.
    pub fn select<'a>(
        &self,
        kind: &str,
        labels: impl IntoIterator<Item = &'a str>,
    ) -> Result<LaneChoice, String> {
        let text = std::fs::read_to_string(&self.path).map_err(|e| {
            format!(
                "read operator model lanes from {}: {e}",
                self.path.display()
            )
        })?;
        let document = atomic_schema::yaml::load(&self.path, &text).map_err(|e| {
            format!(
                "read operator model lanes from {}: {e}",
                self.path.display()
            )
        })?;
        let value = document
            .value
            .pointer("/model/lanes")
            .ok_or_else(|| "operator model.lanes disappeared during this run".to_owned())?;
        let policy: LanePolicy = serde_json::from_value(value.clone())
            .map_err(|e| format!("operator model.lanes is invalid: {e}"))?;
        policy.validate()?;
        if policy.endpoints != self.endpoints {
            return Err(
                "operator model.lanes endpoint catalog changed; start a new run for new endpoints"
                    .into(),
            );
        }
        Ok(policy.select(kind, labels))
    }
}

impl LanePolicy {
    /// Reject ambiguous or unusable declarations before a graph run starts.
    pub fn validate(&self) -> Result<(), String> {
        if self.endpoints.is_empty() {
            return Err("model.lanes.endpoints must name at least one endpoint".into());
        }
        if !self.endpoints.contains_key(&self.default) {
            return Err(format!(
                "model.lanes.default names unknown endpoint {:?}",
                self.default
            ));
        }
        let mut models = BTreeMap::<&str, &str>::new();
        for (id, endpoint) in &self.endpoints {
            if id.is_empty() || id.trim() != id {
                return Err("model.lanes endpoint names must be nonempty and trimmed".into());
            }
            if endpoint.name.trim().is_empty() {
                return Err(format!("model.lanes endpoint {id:?} needs a model name"));
            }
            if let Some(other) = models.insert(&endpoint.name, id) {
                return Err(format!(
                    "model.lanes endpoints {other:?} and {id:?} share model {:?}; each endpoint needs a distinct model name so its price is unambiguous",
                    endpoint.name
                ));
            }
            let url = url::Url::parse(&endpoint.base_url)
                .map_err(|_| format!("model.lanes endpoint {id:?} has an invalid base_url"))?;
            let local_http = url.scheme() == "http"
                && url.host_str().is_some_and(|host| {
                    host.parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
                });
            if url.scheme() != "https" && !local_http {
                return Err(format!(
                    "model.lanes endpoint {id:?} must use https or loopback http"
                ));
            }
            if !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(format!(
                    "model.lanes endpoint {id:?} base_url must not contain credentials, query or fragment"
                ));
            }
            if let Some(env) = &endpoint.api_key_env
                && (env.is_empty()
                    || !env
                        .bytes()
                        .next()
                        .is_some_and(|b| b == b'_' || b.is_ascii_alphabetic())
                    || !env.bytes().all(|b| b == b'_' || b.is_ascii_alphanumeric()))
            {
                return Err(format!(
                    "model.lanes endpoint {id:?} api_key_env is not an environment variable name"
                ));
            }
        }
        for (index, route) in self.routes.iter().enumerate() {
            if !self.endpoints.contains_key(&route.endpoint) {
                return Err(format!(
                    "model.lanes.routes[{index}] names unknown endpoint {:?}",
                    route.endpoint
                ));
            }
            if route.kind.is_none() && route.labels_any.is_empty() {
                return Err(format!(
                    "model.lanes.routes[{index}] needs kind or labels_any"
                ));
            }
            if route
                .kind
                .as_ref()
                .is_some_and(|kind| kind.trim().is_empty())
                || route.labels_any.iter().any(|label| label.trim().is_empty())
            {
                return Err(format!("model.lanes.routes[{index}] has an empty selector"));
            }
        }
        Ok(())
    }

    /// Select from trusted item properties. Callers must checkpoint the result
    /// before starting the first turn for this lane.
    pub fn select<'a>(&self, kind: &str, labels: impl IntoIterator<Item = &'a str>) -> LaneChoice {
        let labels: BTreeSet<&str> = labels.into_iter().collect();
        let id = self
            .routes
            .iter()
            .find(|route| {
                route.kind.as_deref().is_none_or(|wanted| wanted == kind)
                    && (route.labels_any.is_empty()
                        || route
                            .labels_any
                            .iter()
                            .any(|label| labels.contains(label.as_str())))
            })
            .map_or(self.default.as_str(), |route| route.endpoint.as_str());
        LaneChoice {
            endpoint: id.to_owned(),
            target: self.endpoints[id].clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> LanePolicy {
        serde_json::from_value(serde_json::json!({
            "endpoints": {
                "local": {"base_url":"http://127.0.0.1:11434/v1","name":"qwen"},
                "remote": {"base_url":"https://example.test/v1","name":"frontier","api_key_env":"MODEL_KEY"}
            },
            "routes": [
                {"kind":"task","labels_any":["security"],"endpoint":"remote"},
                {"labels_any":["durability"],"endpoint":"remote"},
                {"kind":"docs","endpoint":"local"}
            ],
            "default":"local"
        }))
        .expect("valid lane policy")
    }

    #[test]
    fn ordered_kind_and_label_routes_select_exact_endpoint() {
        let policy = policy();
        policy.validate().expect("valid policy");
        assert_eq!(policy.select("task", ["security"]).endpoint, "remote");
        assert_eq!(policy.select("task", ["durability"]).endpoint, "remote");
        assert_eq!(policy.select("docs", ["polish"]).target.name, "qwen");
        assert_eq!(policy.select("task", ["polish"]).endpoint, "local");
        assert_eq!(policy.select("docs", ["security"]).endpoint, "local");
    }

    #[test]
    fn refuses_unknown_routes_and_unsafe_endpoint_addresses() {
        let mut unknown = policy();
        unknown.routes[0].endpoint = "missing".into();
        assert!(
            unknown
                .validate()
                .expect_err("unknown route endpoint")
                .contains("unknown endpoint")
        );
        let mut unsafe_url = policy();
        unsafe_url
            .endpoints
            .get_mut("remote")
            .expect("remote endpoint")
            .base_url = "http://outside.test/v1".into();
        assert!(
            unsafe_url
                .validate()
                .expect_err("unsafe endpoint URL")
                .contains("https or loopback")
        );
        let mut invalid_key = policy();
        invalid_key
            .endpoints
            .get_mut("remote")
            .expect("remote endpoint")
            .api_key_env = Some("BAD-NAME".into());
        assert!(
            invalid_key
                .validate()
                .expect_err("invalid key name")
                .contains("environment variable")
        );
        let mut ambiguous_price = policy();
        ambiguous_price
            .endpoints
            .get_mut("remote")
            .expect("remote endpoint")
            .name = "qwen".into();
        assert!(
            ambiguous_price
                .validate()
                .expect_err("same model ID at two endpoints")
                .contains("price is unambiguous")
        );
    }

    #[test]
    fn route_reload_changes_only_later_choices_and_refuses_new_catalog() {
        let policy = policy();
        let path = std::env::temp_dir().join(format!(
            "atomic-lane-policy-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let source = LanePolicySource {
            path: path.clone(),
            endpoints: policy.endpoints.clone(),
        };
        let write = |policy: &LanePolicy| {
            std::fs::write(
                &path,
                serde_json::json!({"model":{"lanes":policy}}).to_string(),
            )
            .expect("write policy");
        };
        write(&policy);
        let first = source.select("task", ["security"]).expect("first choice");
        assert_eq!(first.endpoint, "remote");
        let mut changed = policy.clone();
        changed.routes[0].endpoint = "local".into();
        write(&changed);
        assert_eq!(
            source
                .select("task", ["security"])
                .expect("later choice")
                .endpoint,
            "local"
        );
        assert_eq!(first.endpoint, "remote", "earlier choice stays frozen");
        changed
            .endpoints
            .get_mut("local")
            .expect("local endpoint")
            .name = "different".into();
        write(&changed);
        assert!(
            source
                .select("task", ["security"])
                .expect_err("changed endpoint catalog")
                .contains("catalog changed")
        );
        let _ = std::fs::remove_file(path);
    }
}
