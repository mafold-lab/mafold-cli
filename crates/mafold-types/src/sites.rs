//! A mafold.app site's own Worker — its backend (Workers for Platforms).
//!
//! A site is static files (`deploySite`) plus, optionally, one Worker that
//! answers the paths it claims (`/api/*` by default) with D1 databases of the
//! site owner's bound into it. See `.docs/site-workers-v1.md`.

use serde::{Deserialize, Serialize};

/// One D1 database bound into a site's Worker as `env.<binding>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SiteWorkerBinding {
    pub binding: String,
    pub database_id: String,
    /// The database's name when it was bound (names can change; the id can't).
    pub database: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SiteWorker {
    pub site: String,
    /// Paths the Worker answers. `/api/*` = that prefix; `/x` = exactly `/x`.
    /// Everything else is the site's static files.
    pub routes: Vec<String>,
    pub main_module: String,
    pub compatibility_date: String,
    pub d1: Vec<SiteWorkerBinding>,
    /// Plain-text variables, `env.<NAME>`. Not for secrets.
    #[serde(default)]
    pub vars: std::collections::BTreeMap<String, String>,
    /// The webview app (`owner/slug`) whose launch tokens this Worker verifies:
    /// the platform binds `MAFOLD_APP_ID` and, as a secret, `MAFOLD_APP_SECRET`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    pub bytes: i64,
    pub deployed_by: String,
    pub deployed_at: i64,
}

/// Binding / variable names: `[A-Z][A-Z0-9_]{0,63}`, and never `MAFOLD_*`
/// (the platform's own).
pub fn validate_binding_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let ok = name.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if !ok {
        return Err(format!("{name:?}: a binding name is A-Z, 0-9 and _, starting with a letter"));
    }
    if name.starts_with("MAFOLD_") {
        return Err(format!("{name}: MAFOLD_* names are the platform's"));
    }
    Ok(())
}

/// A route: starts with `/`, at most one `*`, and only at the end.
pub fn validate_route(route: &str) -> Result<(), String> {
    let stars = route.matches('*').count();
    if !route.starts_with('/') || route.len() > 200 || stars > 1 || (stars == 1 && !route.ends_with('*')) {
        return Err(format!("{route:?}: a route is a path like /api/* (one trailing *) or /exact"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_routes() {
        assert!(validate_binding_name("DB").is_ok());
        assert!(validate_binding_name("GARDEN_DB_2").is_ok());
        for bad in ["db", "1DB", "MY-DB", "", "MAFOLD_SITE"] {
            assert!(validate_binding_name(bad).is_err(), "{bad}");
        }
        for ok in ["/api/*", "/", "/healthz", "/*"] {
            assert!(validate_route(ok).is_ok(), "{ok}");
        }
        for bad in ["api/*", "/a/*/b", "/a**", ""] {
            assert!(validate_route(bad).is_err(), "{bad}");
        }
    }
}
