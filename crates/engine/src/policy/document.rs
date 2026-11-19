//! Policy documents and building providers from them.

use std::sync::Arc;

use interlaken_ir::catalog::Catalog;
use interlaken_ir::security::{CedarProvider, PolicyProvider, RlsProvider, parse_policies};

use crate::error::EngineError;

/// The language a policy set is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyLanguage {
    /// Cedar, the provider whose policies can be proved (ADR-0020).
    Cedar,
    /// The RLS-style sublanguage (ADR-0015).
    Rls,
}

impl PolicyLanguage {
    /// Return the name stored in the policy tables.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cedar => "cedar",
            Self::Rls => "rls",
        }
    }

    /// Parse a stored name.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Catalog`] for an unknown name.
    pub fn from_name(name: &str) -> Result<Self, EngineError> {
        match name {
            "cedar" => Ok(Self::Cedar),
            "rls" => Ok(Self::Rls),
            other => Err(EngineError::Catalog(format!(
                "unknown policy language {other}"
            ))),
        }
    }
}

/// A policy set's text and language.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyDocument {
    /// The language.
    pub language: PolicyLanguage,
    /// The policies' text.
    pub body: String,
}

/// The active policy set as of a transaction's snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivePolicies {
    /// The version, incremented by every activation.
    pub version: i64,
    /// The policies.
    pub document: PolicyDocument,
}

/// Build a provider for `document`, validating it.
///
/// Cedar policies are validated against the schema generated from every
/// entity type in `catalog`, with `app_context` as the type of `context.app`.
///
/// # Errors
///
/// Returns [`EngineError::Invalid`] if the policies do not parse or validate.
pub fn build_provider(
    document: &PolicyDocument,
    catalog: Arc<dyn Catalog>,
    app_context: &str,
) -> Result<Box<dyn PolicyProvider>, EngineError> {
    Ok(match document.language {
        PolicyLanguage::Rls => Box::new(RlsProvider::new(parse_policies(&document.body)?)),
        PolicyLanguage::Cedar => {
            let names: Vec<String> = catalog
                .entities()
                .iter()
                .map(|e| e.name.to_string())
                .collect();
            let types: Vec<&str> = names.iter().map(String::as_str).collect();
            Box::new(
                CedarProvider::new(&document.body, catalog, &types, app_context)
                    .map_err(|e| EngineError::Invalid(e.to_string()))?,
            )
        }
    })
}
