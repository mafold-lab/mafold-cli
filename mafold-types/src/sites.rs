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
    /// Set while the platform has the backend paused (over its allowance).
    /// Filled on the way out — never part of what was deployed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<SitePause>,
}

// MARK: - Usage and the brakes (`mafold-api/src/site_usage`, `.docs/site-workers-v1.md` §用量)

/// What one person's site backends may use in a calendar month (UTC). The
/// allowance is the PERSON's — a bot's sites count against its owner, the way
/// its databases do — while usage is recorded and shown per site.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SiteAllowance {
    /// Requests that reached a site's Worker.
    pub requests: i64,
    /// CPU time the Workers spent, in milliseconds.
    pub cpu_ms: i64,
    /// D1 rows scanned / written by the person's databases.
    pub rows_read: i64,
    pub rows_written: i64,
    /// D1 storage across the person's live databases.
    pub storage_bytes: i64,
}

/// Counters over a span (a month, a day). D1 rows are the databases', so a
/// site's own totals leave them at zero.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SiteUsageTotals {
    pub requests: i64,
    pub errors: i64,
    pub cpu_ms: i64,
    pub subrequests: i64,
    pub rows_read: i64,
    pub rows_written: i64,
}

/// A site's backend stopped by the platform: the paths its Worker claims answer
/// 503 (`site_paused`) and the Worker is not invoked; static files keep serving,
/// and nothing — code, bindings, data — is touched.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SitePause {
    /// `monthly_requests` · `monthly_cpu` · `monthly_rows_read` ·
    /// `monthly_rows_written` · `storage` · `burst_requests` · `burst_cpu` ·
    /// `burst_rows_written`.
    pub reason: String,
    pub since: i64,
    /// When it comes back by itself (unix seconds). `None`: it doesn't — the
    /// owner frees storage, or resumes by hand after a third burst in a day.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    /// The number that crossed the line, and the line.
    pub used: i64,
    pub limit: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SiteUsageDay {
    /// `YYYY-MM-DD`, UTC.
    pub date: String,
    #[serde(flatten)]
    pub totals: SiteUsageTotals,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SiteUsageSite {
    pub site: String,
    /// This month, this site's Worker.
    pub month: SiteUsageTotals,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<SitePause>,
    /// Day by day — only when one site was asked for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub days: Vec<SiteUsageDay>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SiteUsageDatabase {
    pub id: String,
    pub name: String,
    pub rows_read: i64,
    pub rows_written: i64,
    pub size_bytes: i64,
    /// Sites whose Workers have it bound.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sites: Vec<String>,
}

/// `getSiteUsage` — one person's month.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SiteUsage {
    pub person: String,
    /// `YYYY-MM`, UTC.
    pub month: String,
    /// When the monthly counters start over (unix seconds).
    pub resets_at: i64,
    pub allowance: SiteAllowance,
    /// The person's totals this month: every site's Worker, every database.
    pub used: SiteUsageTotals,
    pub storage_bytes: i64,
    pub sites: Vec<SiteUsageSite>,
    pub databases: Vec<SiteUsageDatabase>,
    /// The newest reading the platform took (unix seconds); `None` before the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured_at: Option<i64>,
    /// Why readings are failing, when they are (numbers above are then stale,
    /// and nothing is paused on a reading that wasn't taken).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measuring_error: Option<String>,
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
