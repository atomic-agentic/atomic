// SPDX-License-Identifier: Apache-2.0

//! Who decides a gate: a person who proves it with a credential.
//!
//! A decision names no one. It carries a [`HumanCredential`], a principal
//! with the secret that principal enrolled, and [`super::decide`] records it
//! only when the gate store's verifier for that principal matches. The store
//! keeps the verifier, never the secret: locally the secret lives in the OS
//! keyring, out of every atom's reach, so an atom's shell that finds atomic's
//! commands still has nothing to approve with.

use super::Principal;
use crate::runtime::{DurableError, DurableRuntime, Param, Result, Tx};
use sha2::{Digest, Sha256};

/// A person's proof of who they are: their principal and the secret they
/// enrolled. Only its holder can make one that verifies.
#[derive(Clone)]
pub struct HumanCredential {
    principal: Principal,
    secret: String,
}

impl HumanCredential {
    /// `principal`, proven by `secret`.
    pub fn new(principal: Principal, secret: impl Into<String>) -> Self {
        Self {
            principal,
            secret: secret.into(),
        }
    }

    /// Who it proves.
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// What the gate store keeps: a digest of the principal and the secret.
    fn verifier(&self) -> String {
        let mut h = Sha256::new();
        for part in [
            "atomic operator credential v1",
            self.principal.as_str(),
            &self.secret,
        ] {
            h.update(part.as_bytes());
            h.update([0]);
        }
        format!("sha256:{:x}", h.finalize())
    }
}

impl std::fmt::Debug for HumanCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HumanCredential")
            .field("principal", &self.principal)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// What [`enroll`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Enrollment {
    /// The principal had no credential; it has this one now.
    Enrolled,
    /// The principal already had this credential.
    AlreadyEnrolled,
    /// The principal's earlier credential was replaced by this one.
    Replaced,
}

/// Why a credential was not enrolled.
#[derive(Debug, thiserror::Error)]
pub enum EnrollError {
    /// The principal is enrolled with another credential.
    #[error("{0} is enrolled with a different credential")]
    Different(Principal),
    /// The store failed; nothing was recorded.
    #[error(transparent)]
    Store(#[from] DurableError),
}

/// How a presented credential compares with the store's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verified {
    /// It matches the enrolled one.
    Yes,
    /// The principal has no credential enrolled.
    NotEnrolled,
    /// It does not match.
    Mismatch,
}

/// Compare two digests in time that does not depend on where they differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// The verifier's database key. A length prefix keeps project/principal
/// pairs distinct even when either name contains punctuation. Local and
/// group principals already have their own, different prefixes.
pub(crate) fn enrollment_key<R: DurableRuntime>(rt: &R, principal: &Principal) -> String {
    match rt.operator_scope() {
        Some(project) => format!("project:{}:{project}{}", project.len(), principal.as_str()),
        None => principal.as_str().to_owned(),
    }
}

/// Check `credential` against the store, in the caller's transaction.
pub(crate) async fn verify(
    tx: &mut Tx<'_, '_>,
    key: &str,
    credential: &HumanCredential,
) -> Result<Verified> {
    let row = tx
        .query_opt(
            "SELECT verifier FROM atomic_operators WHERE principal = ?",
            &[Param::Text(key.to_owned())],
        )
        .await?;
    Ok(match row {
        None => Verified::NotEnrolled,
        Some(row) if same(&row.text("verifier")?, &credential.verifier()) => Verified::Yes,
        Some(_) => Verified::Mismatch,
    })
}

/// Enroll `credential` for its principal, so decisions it carries are
/// recorded. Enrolling the same credential again changes nothing; a
/// principal already enrolled with another one is refused, so whoever holds
/// the first keeps the gates.
pub async fn enroll<R: DurableRuntime>(
    rt: &R,
    credential: &HumanCredential,
) -> std::result::Result<Enrollment, EnrollError> {
    let key = enrollment_key(rt, credential.principal());
    let credential = credential.clone();
    let outcome = rt
        .control_transaction(move |mut tx| {
            let credential = credential.clone();
            let key = key.clone();
            Box::pin(async move {
                match verify(&mut tx, &key, &credential).await? {
                    Verified::Yes => return Ok(Ok(Enrollment::AlreadyEnrolled)),
                    Verified::Mismatch => return Ok(Err(credential.principal.clone())),
                    Verified::NotEnrolled => {}
                }
                let now = super::now_ms();
                let inserted = tx
                    .execute(
                        "INSERT INTO atomic_operators (principal, verifier, enrolled_at) \
                         VALUES (?, ?, ?) ON CONFLICT (principal) DO NOTHING",
                        &[
                            Param::Text(key.to_owned()),
                            Param::Text(credential.verifier()),
                            Param::Int(now),
                        ],
                    )
                    .await?;
                if inserted == 1 {
                    return Ok(Ok(Enrollment::Enrolled));
                }
                // Another enrollment raced in first.
                Ok(match verify(&mut tx, &key, &credential).await? {
                    Verified::Yes => Ok(Enrollment::AlreadyEnrolled),
                    _ => Err(credential.principal.clone()),
                })
            })
        })
        .await?;
    outcome.map_err(EnrollError::Different)
}

/// Enroll `credential` for its principal in place of any credential it had,
/// as a person does who rotates theirs or lost it. Only a person's own
/// command does this: the gate store is out of every atom's reach.
pub async fn replace_enrollment<R: DurableRuntime>(
    rt: &R,
    credential: &HumanCredential,
) -> Result<Enrollment> {
    let key = enrollment_key(rt, credential.principal());
    let credential = credential.clone();
    rt.control_transaction(move |mut tx| {
        let credential = credential.clone();
        let key = key.clone();
        Box::pin(async move {
            let before = verify(&mut tx, &key, &credential).await?;
            if before == Verified::Yes {
                return Ok(Enrollment::AlreadyEnrolled);
            }
            let principal = key;
            tx.execute(
                "DELETE FROM atomic_operators WHERE principal = ?",
                &[Param::Text(principal.clone())],
            )
            .await?;
            tx.execute(
                "INSERT INTO atomic_operators (principal, verifier, enrolled_at) VALUES (?, ?, ?)",
                &[
                    Param::Text(principal),
                    Param::Text(credential.verifier()),
                    Param::Int(super::now_ms()),
                ],
            )
            .await?;
            Ok(match before {
                Verified::Mismatch => Enrollment::Replaced,
                _ => Enrollment::Enrolled,
            })
        })
    })
    .await
}

/// Whether `principal` has a credential enrolled.
pub async fn enrolled<R: DurableRuntime>(rt: &R, principal: &Principal) -> Result<bool> {
    let principal = enrollment_key(rt, principal);
    rt.control_transaction(move |mut tx| {
        let principal = principal.clone();
        Box::pin(async move {
            Ok(tx
                .query_opt(
                    "SELECT principal FROM atomic_operators WHERE principal = ?",
                    &[Param::Text(principal)],
                )
                .await?
                .is_some())
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_verifier_binds_the_principal_and_the_secret_and_hides_the_secret() {
        let dev = HumanCredential::new(Principal::local("dev"), "s3cret");
        let v = dev.verifier();
        assert!(!v.contains("s3cret"));
        assert_eq!(
            v,
            HumanCredential::new(Principal::local("dev"), "s3cret").verifier()
        );
        assert_ne!(
            v,
            HumanCredential::new(Principal::local("dev"), "other").verifier()
        );
        assert_ne!(
            v,
            HumanCredential::new(Principal::local("eve"), "s3cret").verifier()
        );
        assert!(!format!("{dev:?}").contains("s3cret"));
    }

    #[test]
    fn digests_compare_equal_only_when_equal() {
        assert!(same("sha256:ab", "sha256:ab"));
        assert!(!same("sha256:ab", "sha256:ac"));
        assert!(!same("sha256:ab", "sha256:abc"));
    }
}
