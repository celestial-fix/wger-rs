//! Process environment. Nothing here is read from a file.

use std::env;

use axum::http::Uri;
use chrono_tz::Tz;

/// A fixed upstream destination. Only `http` origins with no path, query, or
/// userinfo are accepted, so the proxy can never be pointed somewhere else by
/// a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpstreamUrl {
    pub host: String,
    pub port: u16,
    /// `host[:port]` exactly as configured, used only as a last-resort Host.
    pub authority: String,
}

impl UpstreamUrl {
    pub fn parse(var: &str, raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        let uri: Uri = raw
            .parse()
            .map_err(|_| format!("{var} is not a valid URL: {raw}"))?;
        match uri.scheme_str() {
            Some("http") => {}
            Some("https") => {
                return Err(format!(
                    "{var} must use http, not https (TLS terminates at nginx-proxy)"
                ))
            }
            Some(other) => return Err(format!("{var} uses unsupported scheme {other}")),
            None => return Err(format!("{var} must be an absolute http:// URL")),
        }
        let authority = uri
            .authority()
            .ok_or_else(|| format!("{var} has no host"))?;
        if authority.as_str().contains('@') {
            return Err(format!("{var} must not contain userinfo"));
        }
        if authority.host().is_empty() {
            return Err(format!("{var} has no host"));
        }
        if let Some(query) = uri.query() {
            return Err(format!("{var} must not contain a query ({query})"));
        }
        let path = uri.path();
        if !(path.is_empty() || path == "/") {
            return Err(format!("{var} must not contain a path ({path})"));
        }
        Ok(Self {
            host: authority
                .host()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string(),
            port: authority.port_u16().unwrap_or(80),
            authority: authority.as_str().to_string(),
        })
    }

    fn optional(var: &str) -> Result<Option<Self>, String> {
        match env::var(var).ok().filter(|value| !value.is_empty()) {
            Some(raw) => Self::parse(var, &raw).map(Some),
            None => Ok(None),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub bind: String,
    pub site_url: String,
    pub time_zone: Tz,
    pub static_root: String,
    pub media_root: String,
    pub jwt_private_key: String,
    pub jwt_public_key: String,
    pub powersync_url: Option<String>,
    pub powersync_url_path: String,
    /// Internal PowerSync address used by the `/ps/` proxy. Distinct from the
    /// public `POWERSYNC_URL` handed to clients.
    pub powersync_upstream_url: Option<UpstreamUrl>,
    /// Internal Django UI address. Unset means every Django route is 502.
    pub django_ui_url: Option<UpstreamUrl>,
    /// Comma-separated IPs, CIDRs, and DNS hostnames allowed to set
    /// `X-Forwarded-*`. Empty trusts nobody.
    pub trusted_proxies: Vec<String>,
    pub powersync_token_lifetime: i64,
    pub access_token_lifetime: i64,
    pub refresh_token_lifetime: i64,
    pub min_account_age_days: i64,
    pub version: String,
    pub min_app_version: String,
    pub min_server_version: String,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let database_url = env::var("DATABASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(database_url_from_django)
            .ok_or_else(|| {
                "DATABASE_URL or DJANGO_DB_USER/DJANGO_DB_PASSWORD/DJANGO_DB_HOST/DJANGO_DB_DATABASE is required"
                    .to_string()
            })?;
        let time_zone_name = env_or("TIME_ZONE", "Europe/Berlin");
        let time_zone = time_zone_name
            .parse::<Tz>()
            .map_err(|_| format!("TIME_ZONE {time_zone_name} is not a known zone"))?;
        let site_url = env_or("SITE_URL", "http://localhost:8000")
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            database_url,
            bind: env_or("BIND", "0.0.0.0:8000"),
            site_url,
            time_zone,
            static_root: env_or("STATIC_ROOT", "/static"),
            media_root: env_or("MEDIA_ROOT", "/media"),
            jwt_private_key: env::var("JWT_PRIVATE_KEY")
                .map_err(|_| "JWT_PRIVATE_KEY is required".to_string())?,
            jwt_public_key: env::var("JWT_PUBLIC_KEY")
                .map_err(|_| "JWT_PUBLIC_KEY is required".to_string())?,
            powersync_url: env::var("POWERSYNC_URL").ok().filter(|s| !s.is_empty()),
            powersync_url_path: env_or("POWERSYNC_URL_PATH", "ps"),
            powersync_upstream_url: UpstreamUrl::optional("POWERSYNC_UPSTREAM_URL")?,
            django_ui_url: UpstreamUrl::optional("DJANGO_UI_URL")?,
            trusted_proxies: env::var("TRUSTED_PROXIES")
                .ok()
                .map(|value| {
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|item| !item.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            powersync_token_lifetime: env_i64("POWERSYNC_TOKEN_LIFETIME", 600),
            access_token_lifetime: env_i64("JWT_ACCESS_TOKEN_LIFETIME", 300),
            refresh_token_lifetime: env_i64("JWT_REFRESH_TOKEN_LIFETIME", 86400),
            min_account_age_days: env_i64("MIN_ACCOUNT_AGE_TO_TRUST", 21),
            version: env_or("WGER_VERSION", "2.7.0"),
            min_app_version: env_or("MIN_APP_VERSION", "2.1.0"),
            min_server_version: env_or("MIN_SERVER_VERSION", "2.5.0"),
        })
    }

    pub fn powersync_public_url(&self) -> String {
        if let Some(url) = &self.powersync_url {
            return url.clone();
        }
        let path = self.powersync_url_path.trim_matches('/');
        format!("{}/{path}/", self.site_url)
    }
}

fn database_url_from_django() -> Option<String> {
    let name = env::var("DJANGO_DB_DATABASE").ok()?;
    let user = env::var("DJANGO_DB_USER").unwrap_or_else(|_| "wger".into());
    let password = env::var("DJANGO_DB_PASSWORD").unwrap_or_default();
    let host = env::var("DJANGO_DB_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port = env::var("DJANGO_DB_PORT").unwrap_or_else(|_| "5432".into());
    Some(format!("postgres://{user}:{password}@{host}:{port}/{name}"))
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn env_i64(key: &str, default: i64) -> i64 {
    env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_accepts_plain_http_origins() {
        let upstream = UpstreamUrl::parse("DJANGO_UI_URL", "http://django-ui:8000").unwrap();
        assert_eq!(upstream.host, "django-ui");
        assert_eq!(upstream.port, 8000);
        assert_eq!(upstream.authority, "django-ui:8000");

        let local = UpstreamUrl::parse("POWERSYNC_UPSTREAM_URL", "http://127.0.0.1").unwrap();
        assert_eq!(local.port, 80);

        let v6 = UpstreamUrl::parse("X", "http://[::1]:8080/").unwrap();
        assert_eq!(v6.host, "::1");
        assert_eq!(v6.port, 8080);
    }

    #[test]
    fn upstream_rejects_anything_but_a_bare_http_origin() {
        for (value, needle) in [
            ("https://django-ui:8000", "http"),
            ("ftp://django-ui", "scheme"),
            ("django-ui:8000", "absolute"),
            ("http://django-ui:8000/prefix", "path"),
            ("http://django-ui:8000/?x=1", "query"),
            ("http://user:pass@django-ui:8000", "userinfo"),
            ("http:///missing-host", "host"),
        ] {
            let err = UpstreamUrl::parse("DJANGO_UI_URL", value).unwrap_err();
            assert!(err.contains(needle), "{value}: {err}");
        }
    }
}
