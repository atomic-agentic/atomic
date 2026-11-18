// SPDX-License-Identifier: Apache-2.0

//! Who may decide a gate.
//!
//! Approvers are resolved from two things only: what the workflow file
//! declares for the gate, and the deployment the project runs in. Nothing a
//! model writes, nothing a caller passes as input and nothing in the work
//! graph is an argument here, and [`Approvers`] has no other constructor and
//! cannot be deserialized, so none of those can select who approves.
//!
//! - **Local**, one developer on one machine: the only person is the
//!   authenticated local operator, and every gate resolves to them, whatever
//!   it declares.
//! - **Server**, a shared deployment: a gate names approver groups, each of
//!   which project policy must allow, or it falls back to the policy's
//!   default group. With neither, the workflow fails to load.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A person or group that can decide a gate: `local:<user>` for the local
/// operator, `group:<name>` for a policy group.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Principal(String);

impl Principal {
    /// The local operator, by their account name.
    pub fn local(user: &str) -> Self {
        Self(format!("local:{user}"))
    }

    /// A policy group.
    pub fn group(name: &str) -> Self {
        Self(format!("group:{name}"))
    }

    /// A principal as the gate store recorded it.
    pub(crate) fn stored(text: String) -> Self {
        Self(text)
    }

    /// Its text form.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where the project runs, which decides how approvers resolve. Built from
/// project configuration, never from a run's input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Deployment {
    /// One developer on one machine.
    Local {
        /// The authenticated local operator.
        operator: Principal,
    },
    /// A shared deployment.
    Server {
        /// The approver groups project policy allows a gate to name.
        allowed: Vec<String>,
        /// The group a gate that names none falls back to, if policy sets one.
        default: Option<String>,
    },
}

/// The resolved approvers of one gate. Only [`resolve`] makes one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Approvers(Vec<Principal>);

impl Approvers {
    /// Who they are.
    pub fn principals(&self) -> &[Principal] {
        &self.0
    }
}

/// Why a gate's approvers do not resolve: the workflow fails to load.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ApproverError {
    /// Server mode, and the gate names no group and policy sets no default.
    #[error(
        "gate {gate}: a shared deployment needs approvers; name an allowed group or set a \
         default approver group in project policy"
    )]
    NoGroup {
        /// The gate.
        gate: String,
    },
    /// Server mode, and the gate names a group policy does not allow.
    #[error("gate {gate}: {group} is not an approver group project policy allows")]
    NotAllowed {
        /// The gate.
        gate: String,
        /// The group it named.
        group: String,
    },
}

/// Resolve the approvers of gate `gate`, which declares `declared` (`None`
/// when the workflow file omits `approvers`), in `deployment`.
pub fn resolve(
    gate: &str,
    declared: Option<&[String]>,
    deployment: &Deployment,
) -> Result<Approvers, ApproverError> {
    match deployment {
        Deployment::Local { operator } => Ok(Approvers(vec![operator.clone()])),
        Deployment::Server { allowed, default } => {
            let groups: Vec<String> = match declared {
                Some(groups) if !groups.is_empty() => groups.to_vec(),
                _ => match default {
                    Some(group) => vec![group.clone()],
                    None => {
                        return Err(ApproverError::NoGroup {
                            gate: gate.to_owned(),
                        });
                    }
                },
            };
            let mut principals = Vec::with_capacity(groups.len());
            for group in groups {
                // The policy default is allowed by being the default.
                let is_default = default.as_ref() == Some(&group);
                if !is_default && !allowed.contains(&group) {
                    return Err(ApproverError::NotAllowed {
                        gate: gate.to_owned(),
                        group,
                    });
                }
                principals.push(Principal::group(&group));
            }
            principals.sort();
            principals.dedup();
            Ok(Approvers(principals))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> Deployment {
        Deployment::Local {
            operator: Principal::local("dev"),
        }
    }

    #[test]
    fn omitted_approvers_select_only_the_local_operator() {
        let a = resolve("accept", None, &local()).expect("resolves");
        assert_eq!(a.principals(), [Principal::local("dev")]);
    }

    #[test]
    fn a_declared_group_in_local_mode_still_selects_only_the_local_operator() {
        let declared = vec!["release".to_owned()];
        let a = resolve("accept", Some(&declared), &local()).expect("resolves");
        assert_eq!(a.principals(), [Principal::local("dev")]);
    }

    #[test]
    fn server_mode_without_an_allowed_group_fails_load() {
        let server = Deployment::Server {
            allowed: vec!["release".into()],
            default: None,
        };
        assert_eq!(
            resolve("accept", None, &server),
            Err(ApproverError::NoGroup {
                gate: "accept".into()
            })
        );
        assert_eq!(
            resolve("accept", Some(&[]), &server),
            Err(ApproverError::NoGroup {
                gate: "accept".into()
            })
        );
        assert_eq!(
            resolve("accept", Some(&["ops".to_owned()]), &server),
            Err(ApproverError::NotAllowed {
                gate: "accept".into(),
                group: "ops".into()
            })
        );
    }

    #[test]
    fn server_mode_takes_an_allowed_group_or_the_policy_default() {
        let server = Deployment::Server {
            allowed: vec!["release".into(), "security".into()],
            default: Some("leads".into()),
        };
        let named = resolve(
            "accept",
            Some(&["security".to_owned(), "release".to_owned()]),
            &server,
        )
        .expect("resolves");
        assert_eq!(
            named.principals(),
            [Principal::group("release"), Principal::group("security")]
        );
        let fallback = resolve("accept", None, &server).expect("resolves");
        assert_eq!(fallback.principals(), [Principal::group("leads")]);
    }
}
