//! The real backend: the VW Group EU Data Act portal (OIDC login + ZIP
//! snapshot download).
//!
//! Ported from the community `HA_VAG-EU-Data-Act` project's `api.py` (MIT
//! licensed), reduced to what this controller needs: only the Volkswagen
//! brand (the user's ID.3), no vehicle-listing/relation endpoints — `vin` is
//! a required config field instead, since the account has exactly one
//! vehicle — and no data-dictionary lookup, since this only ever reads one
//! field (`battery_state_report.soc`), which the portal already labels
//! inline via `dataFieldName`.
//!
//! The login is a full OIDC browser flow against `identity.vwgroup.io`, not
//! a documented API: an email step and a password step, each rendering its
//! hidden form fields (`hmac`, `_csrf`, `relayState`) either as HTML
//! `<input>` elements or inside a `window._IDK.templateModel` JS object,
//! ending on a redirect back to the portal's own
//! `/services/callbacklogin`, which sets the session cookies the rest of
//! this client rides on.

use std::collections::HashMap;
use std::future::Future;
use std::io::Read;
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use reqwest::StatusCode;
use reqwest::header::{
    ACCEPT, ACCEPT_LANGUAGE, CACHE_CONTROL, HeaderMap, HeaderValue, ORIGIN, REFERER,
};
use serde_json::Value;

use super::{CarBatteryError, CarBatterySource};
use crate::units::Soc;

const BASE_URL: &str = "https://eu-data-act.drivesomethinggreater.com";
const OIDC_AUTHORIZE_URL: &str = "https://identity.vwgroup.io/oidc/v1/authorize";
const OIDC_SCOPE: &str = "openid cars profile";
// Volkswagen passenger cars only — see the module doc comment on brand scope.
const VW_CLIENT_ID: &str = "9b58543e-1c15-4193-91d5-8a14145bebb0@apps_vw-dilab_com";
const VW_OIDC_STATE_KEY: &str = "VOLKSWAGEN_PASSENGER_CARS";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36";
const NO_CONTENT_SUFFIX: &str = "_no_content_found.zip";
const SOC_FIELD: &str = "battery_state_report.soc";
/// Bounds the redirect-following loop: a portal outage or a login flow this
/// port has fallen out of step with should fail loudly, not spin forever.
const MAX_REDIRECTS: usize = 10;

/// One authenticated round trip's result, including every URL visited —
/// mirrors aiohttp's `resp.history + [resp]`, which the Python client reads
/// to confirm the redirect chain passed `/services/callbacklogin` even when
/// the final landing page itself 4xx/5xxs for unrelated (locale-CMS) reasons.
struct RedirectResult {
    final_url: String,
    status: u16,
    body: String,
    history: Vec<String>,
}

pub struct VwPortalClient {
    http: reqwest::Client,
    email: String,
    password: String,
    vin: String,
    country: String,
    language: String,
    logged_in: bool,
    /// The data-request identifier the metadata endpoint hands back — cached
    /// across polls, refreshed if a listing ever comes back empty (the
    /// portal can reassign it, e.g. after the subscription was recreated).
    identifier: Option<String>,
}

impl VwPortalClient {
    pub fn new(
        email: String,
        password: String,
        vin: String,
        country: String,
        language: String,
    ) -> Self {
        let http = reqwest::Client::builder()
            .cookie_store(true)
            .timeout(Duration::from_secs(30))
            // `follow_redirects` below walks the `Location` chain by hand to
            // build `history` for `passed_portal_callback`; reqwest's default
            // policy would follow it first and hide every intermediate hop.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("failed to build the VW portal HTTP client");
        VwPortalClient {
            http,
            email,
            password,
            vin,
            country: country.to_lowercase(),
            language: language.to_lowercase(),
            logged_in: false,
            identifier: None,
        }
    }

    async fn ensure_login(&mut self) -> Result<(), CarBatteryError> {
        if !self.logged_in {
            self.do_login().await?;
        }
        Ok(())
    }

    fn authorize_url(&self) -> Result<reqwest::Url, CarBatteryError> {
        let state = format!("{}__{}__{VW_OIDC_STATE_KEY}", self.country, self.language);
        let mut url = reqwest::Url::parse(OIDC_AUTHORIZE_URL)
            .map_err(|e| CarBatteryError::Request(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("client_id", VW_CLIENT_ID)
            .append_pair("response_type", "code")
            .append_pair("scope", OIDC_SCOPE)
            .append_pair("state", &state)
            .append_pair("redirect_uri", &format!("{BASE_URL}/login"))
            .append_pair("prompt", "login");
        Ok(url)
    }

    /// Runs the full OIDC login, populating the client's cookie jar.
    async fn do_login(&mut self) -> Result<(), CarBatteryError> {
        // 1. Start the flow directly at the identity provider's authorize
        // endpoint, built by hand — the portal's own redirect servlet
        // returns HTTP 500 for non-browser clients.
        let authorize_url = self.authorize_url()?;
        let headers1 = login_headers(&self.country, &self.language, Some(&format!("{BASE_URL}/")))?;
        let r1 = self.get_history(authorize_url.as_str(), headers1).await?;

        // 2. POST the email (identifier step).
        let (mut fields, action) = login_fields(&r1.body);
        if !fields.contains_key("hmac") || !fields.contains_key("_csrf") {
            return Err(CarBatteryError::Auth(format!(
                "could not parse the sign-in form (fields found: {:?})",
                fields.keys().collect::<Vec<_>>()
            )));
        }
        fields.insert("email".to_string(), self.email.clone());
        let identifier_action = resolve(&r1.final_url, action.as_deref().unwrap_or(""))?;
        let headers2 = login_headers(&self.country, &self.language, Some(&r1.final_url))?;
        let r2 = self
            .post_form_history(&identifier_action, &fields, headers2)
            .await?;

        // 3. The identifier step lands on the password (authenticate) page,
        // whose hidden fields live in the JS templateModel, not HTML inputs.
        let (mut fields2, _) = login_fields(&r2.body);
        if !fields2.contains_key("hmac") || !fields2.contains_key("_csrf") {
            let err = login_error(&r2.body);
            return Err(CarBatteryError::Auth(err.unwrap_or_else(|| {
                "identity portal did not return the password form - check the email address \
                 (or the login flow changed)"
                    .to_string()
            })));
        }
        fields2.insert("email".to_string(), self.email.clone());
        fields2.insert("password".to_string(), self.password.clone());
        // The page never renders a `<form action>` here (it's client-rendered
        // per the model's `useClientRendering`); the real target is
        // `templateModel.postAction`, e.g. "login/authenticate" — relative not
        // to this page's own URL but to the same signin-service root that
        // `identifierUrl` (also in this model) was relative to when it
        // produced `identifier_action` in step 2. Joining it against the page
        // URL instead double-counts the "login/" segment and gets HTTP 400.
        let authenticate_action =
            resolve_post_action(&r2.body, &identifier_action).ok_or_else(|| {
                CarBatteryError::Auth(
                    "identity portal did not include a postAction for the password step"
                        .to_string(),
                )
            })?;
        let headers3 = login_headers(&self.country, &self.language, Some(&r2.final_url))?;

        // 4. POST credentials; follow the redirect chain back to the portal,
        // which sets the session cookies via /services/callbacklogin.
        let r3 = self
            .post_form_history(&authenticate_action, &fields2, headers3)
            .await?;
        self.finish_login(r3, false).await
    }

    /// Judges the credentials redirect chain and confirms the session.
    /// Recursive (the terms-and-conditions interstitial re-enters once), so
    /// boxed rather than a plain `async fn`.
    fn finish_login(
        &mut self,
        result: RedirectResult,
        after_terms: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), CarBatteryError>> + Send + '_>> {
        Box::pin(async move {
            let landing = result.final_url.clone();
            if result.status >= 400 && !passed_portal_callback(&result.history) {
                let err = login_error(&result.body);
                return Err(CarBatteryError::Auth(err.unwrap_or_else(|| {
                    format!("login rejected (HTTP {})", result.status)
                })));
                // Once the chain has passed callbacklogin, the landing
                // page's own status is ignored — some locales 4xx/5xx there
                // for unrelated (CMS) reasons even after a successful login.
            }

            // IdP terms interstitial — submit the accept form and continue.
            if landing.contains("terms-and-conditions") {
                if after_terms {
                    return Err(CarBatteryError::Auth(
                        "login interrupted: the identity provider still requires accepting \
                         terms and conditions after submission"
                            .to_string(),
                    ));
                }
                let (fields, action) = login_fields(&result.body);
                let missing: Vec<&str> = ["_csrf", "relayState", "hmac"]
                    .into_iter()
                    .filter(|k| !fields.contains_key(*k))
                    .collect();
                if !missing.is_empty() {
                    return Err(CarBatteryError::Auth(format!(
                        "login interrupted: the identity provider requires accepting updated \
                         terms and conditions for this account (missing {} on terms form)",
                        missing.join(", ")
                    )));
                }
                let terms_action = match &action {
                    Some(a) => resolve(&landing, a)?,
                    None => strip_query(&landing),
                };
                let headers = login_headers(&self.country, &self.language, Some(&landing))?;
                let terms_result = self
                    .post_form_history(&terms_action, &fields, headers)
                    .await?;
                return self.finish_login(terms_result, true).await;
            }

            // Positively confirm success: a completed flow lands back on the
            // portal host. Bad credentials re-render the identity sign-in
            // page instead.
            if landing.contains("signin-service") || landing.contains("/error") {
                return Err(CarBatteryError::Auth(
                    "login failed - check email and password".to_string(),
                ));
            }
            let portal_host = host_of(BASE_URL);
            if host_of(&landing) != portal_host {
                return Err(CarBatteryError::Auth(format!(
                    "login did not complete (ended at {landing})"
                )));
            }

            self.logged_in = true;
            Ok(())
        })
    }

    async fn get_history(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<RedirectResult, CarBatteryError> {
        let resp = self
            .http
            .get(url)
            .headers(headers.clone())
            .send()
            .await
            .map_err(|e| CarBatteryError::Request(e.to_string()))?;
        self.follow_redirects(url.to_string(), resp, headers).await
    }

    async fn post_form_history(
        &self,
        url: &str,
        fields: &HashMap<String, String>,
        headers: HeaderMap,
    ) -> Result<RedirectResult, CarBatteryError> {
        let resp = self
            .http
            .post(url)
            .headers(headers.clone())
            .form(fields)
            .send()
            .await
            .map_err(|e| CarBatteryError::Request(e.to_string()))?;
        self.follow_redirects(url.to_string(), resp, headers).await
    }

    /// Follows `Location` redirects by hand (the client itself never does
    /// this automatically) rather than reqwest's built-in policy, so the
    /// full visited-URL history is available to [`passed_portal_callback`] —
    /// the same reason the Python client reads `resp.history`.
    async fn follow_redirects(
        &self,
        mut current_url: String,
        mut resp: reqwest::Response,
        headers: HeaderMap,
    ) -> Result<RedirectResult, CarBatteryError> {
        let mut history = vec![current_url.clone()];
        loop {
            let status = resp.status();
            if status.is_redirection()
                && let Some(loc) = resp.headers().get(reqwest::header::LOCATION)
            {
                let loc = loc
                    .to_str()
                    .map_err(|e| CarBatteryError::Request(e.to_string()))?
                    .to_string();
                current_url = resolve(&current_url, &loc)?;
                if history.len() >= MAX_REDIRECTS {
                    return Err(CarBatteryError::Request(
                        "too many redirects during login".to_string(),
                    ));
                }
                history.push(current_url.clone());
                resp = self
                    .http
                    .get(&current_url)
                    .headers(headers.clone())
                    .send()
                    .await
                    .map_err(|e| CarBatteryError::Request(e.to_string()))?;
                continue;
            }
            let status_code = status.as_u16();
            let final_url = current_url.clone();
            let body = resp
                .text()
                .await
                .map_err(|e| CarBatteryError::Request(e.to_string()))?;
            return Ok(RedirectResult {
                final_url,
                status: status_code,
                body,
                history,
            });
        }
    }

    /// One authenticated GET, re-logging in once on a 401/403 before giving
    /// up — a session can expire between polls, which is normal, not an
    /// error worth surfacing.
    async fn get_authenticated(
        &mut self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<reqwest::Response, CarBatteryError> {
        self.ensure_login().await?;
        let resp = self
            .http
            .get(url)
            .headers(headers.clone())
            .send()
            .await
            .map_err(|e| CarBatteryError::Request(e.to_string()))?;
        if resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN {
            self.logged_in = false;
            self.do_login().await?;
            return self
                .http
                .get(url)
                .headers(headers)
                .send()
                .await
                .map_err(|e| CarBatteryError::Request(e.to_string()));
        }
        Ok(resp)
    }

    async fn get_metadata(&mut self, vin: &str) -> Result<Value, CarBatteryError> {
        let url =
            format!("{BASE_URL}/proxy_api/euda-apim/datarequest/vehicles/{vin}/metadata/partial");
        let resp = self.get_authenticated(&url, HeaderMap::new()).await?;
        json_body(resp).await
    }

    async fn list_datasets(
        &mut self,
        vin: &str,
        identifier: &str,
    ) -> Result<Vec<DatasetEntry>, CarBatteryError> {
        let url =
            format!("{BASE_URL}/proxy_api/euda-apim/datadelivery/vehicles/{vin}/{identifier}/list");
        let mut headers = HeaderMap::new();
        headers.insert("type", HeaderValue::from_static("partial"));
        let resp = self.get_authenticated(&url, headers).await?;
        if resp.status() == StatusCode::NOT_FOUND {
            // No ZIPs delivered yet for this subscription.
            return Ok(Vec::new());
        }
        let value = json_body(resp).await?;
        let files = match &value {
            Value::Array(arr) => arr.clone(),
            Value::Object(obj) => obj
                .get("files")
                .and_then(|f| f.as_array())
                .cloned()
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        Ok(files
            .into_iter()
            .filter_map(|f| {
                let name = f.get("name")?.as_str()?.to_string();
                let created_on = f
                    .get("createdOn")
                    .and_then(|c| c.as_str())
                    .map(String::from);
                Some(DatasetEntry { name, created_on })
            })
            .collect())
    }

    async fn download_dataset_raw(
        &mut self,
        vin: &str,
        identifier: &str,
        name: &str,
    ) -> Result<Vec<u8>, CarBatteryError> {
        let url = format!(
            "{BASE_URL}/proxy_api/euda-apim/datadelivery/vehicles/{vin}/{identifier}/download"
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            "filename",
            HeaderValue::from_str(name).map_err(|e| CarBatteryError::Request(e.to_string()))?,
        );
        headers.insert("type", HeaderValue::from_static("partial"));
        let resp = self.get_authenticated(&url, headers).await?;
        if !resp.status().is_success() {
            return Err(CarBatteryError::Request(format!(
                "download {name} -> HTTP {}",
                resp.status()
            )));
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| CarBatteryError::Request(e.to_string()))
    }

    /// The metadata endpoint's `Identifier`, cached after the first call.
    async fn identifier(&mut self, vin: &str) -> Result<String, CarBatteryError> {
        if let Some(id) = &self.identifier {
            return Ok(id.clone());
        }
        let meta = self.get_metadata(vin).await?;
        let id = identifier_from_metadata(&meta)?;
        self.identifier = Some(id.clone());
        Ok(id)
    }
}

struct DatasetEntry {
    name: String,
    created_on: Option<String>,
}

fn identifier_from_metadata(meta: &Value) -> Result<String, CarBatteryError> {
    meta.get("Identifier")
        .or_else(|| meta.get("identifier"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| CarBatteryError::Parse("metadata response has no Identifier".to_string()))
}

impl CarBatterySource for VwPortalClient {
    async fn poll(&mut self) -> Result<Soc, CarBatteryError> {
        let vin = self.vin.clone();
        let identifier = self.identifier(&vin).await?;
        let mut entries = self.list_datasets(&vin, &identifier).await?;

        // Self-heal a stale identifier once: the portal can assign a new one
        // (e.g. after the subscription was recreated), and an empty listing
        // against the old one looks identical to "nothing delivered yet".
        if entries.is_empty() {
            let meta = self.get_metadata(&vin).await?;
            if let Ok(fresh) = identifier_from_metadata(&meta)
                && fresh != identifier
            {
                self.identifier = Some(fresh.clone());
                entries = self.list_datasets(&vin, &fresh).await?;
            }
        }

        let latest = entries
            .into_iter()
            .filter(|e| !e.name.ends_with(NO_CONTENT_SUFFIX))
            .max_by(|a, b| a.created_on.cmp(&b.created_on))
            .ok_or_else(|| {
                CarBatteryError::Parse("no usable snapshot in the portal's listing yet".to_string())
            })?;

        let identifier = self.identifier.clone().expect("set by identifier() above");
        let raw = self
            .download_dataset_raw(&vin, &identifier, &latest.name)
            .await?;
        extract_soc(&raw)
    }
}

async fn json_body(resp: reqwest::Response) -> Result<Value, CarBatteryError> {
    if !resp.status().is_success() {
        return Err(CarBatteryError::Request(format!("HTTP {}", resp.status())));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| CarBatteryError::Request(e.to_string()))?;
    serde_json::from_str(&text).map_err(|e| CarBatteryError::Parse(format!("invalid JSON: {e}")))
}

/// Unzips one portal snapshot and returns the car's state of charge.
///
/// Free-standing and pure (no I/O beyond the bytes handed in) so it is
/// unit-testable against a fixture with no network round trip — the same
/// reason `zendure::parse_report`/`prediction::solcast::parse_forecast_response`
/// are split out.
pub fn extract_soc(zip_bytes: &[u8]) -> Result<Soc, CarBatteryError> {
    let reader = std::io::Cursor::new(zip_bytes);
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|e| CarBatteryError::Parse(format!("could not read zip: {e}")))?;
    let json_index = (0..archive.len())
        .find(|&i| {
            archive
                .by_index(i)
                .map(|f| f.name().to_ascii_lowercase().ends_with(".json"))
                .unwrap_or(false)
        })
        .ok_or_else(|| {
            CarBatteryError::Parse("no JSON file inside the snapshot zip".to_string())
        })?;
    let mut file = archive.by_index(json_index).map_err(|e| {
        CarBatteryError::Parse(format!("could not open the zip's JSON member: {e}"))
    })?;
    let mut contents = String::new();
    file.read_to_string(&mut contents).map_err(|e| {
        CarBatteryError::Parse(format!("could not read the zip's JSON member: {e}"))
    })?;
    extract_soc_from_json(&contents)
}

fn extract_soc_from_json(json: &str) -> Result<Soc, CarBatteryError> {
    let payload: Value = serde_json::from_str(json)
        .map_err(|e| CarBatteryError::Parse(format!("invalid dataset JSON: {e}")))?;
    let data = payload
        .get("Data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| CarBatteryError::Parse("dataset JSON has no Data array".to_string()))?;
    let point = data
        .iter()
        .find(|item| item.get("dataFieldName").and_then(|f| f.as_str()) == Some(SOC_FIELD))
        .ok_or_else(|| CarBatteryError::Parse(format!("no {SOC_FIELD} entry in this snapshot")))?;
    let raw = point
        .get("value")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CarBatteryError::Parse(format!("{SOC_FIELD} entry has no value")))?;
    // A defensive trim of a trailing '%', in case the portal's own
    // formatting differs from the plain whole-number percent string
    // assumed here — see the parser's own fixture test and the plan's note
    // that this must be checked against a real snapshot (RUST-2: physical
    // quantities aren't guessed).
    let percent: f64 = raw.trim().trim_end_matches('%').parse().map_err(|_| {
        CarBatteryError::Parse(format!("{SOC_FIELD} value {raw:?} is not a number"))
    })?;
    // The parse boundary where this quantity crosses from an untyped wire
    // value into `Soc` — the one named place a cast is allowed to change
    // what the number means (`CLAUDE.md`'s rule on casts). `as u32` on a
    // float saturates (stable Rust float-to-int cast semantics), so an
    // out-of-range or negative reading clamps rather than wrapping.
    Ok(Soc::new(percent.round() as u32))
}

fn strip_query(url: &str) -> String {
    url.split('?').next().unwrap_or(url).to_string()
}

fn resolve(base: &str, relative: &str) -> Result<String, CarBatteryError> {
    if relative.is_empty() {
        return Ok(strip_query(base));
    }
    reqwest::Url::parse(base)
        .and_then(|b| b.join(relative))
        .map(|u| u.to_string())
        .map_err(|e| {
            CarBatteryError::Request(format!(
                "could not resolve URL {relative:?} against {base:?}: {e}"
            ))
        })
}

fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
}

/// `/services/callbacklogin` exchanges the OIDC code and sets the portal's
/// session cookies. Reaching it means authentication completed, regardless
/// of what the final localized CMS landing page returns — some locales
/// 4xx/5xx there even after a successful login.
fn passed_portal_callback(history: &[String]) -> bool {
    let Some(portal_host) = host_of(BASE_URL) else {
        return false;
    };
    history.iter().any(|u| {
        host_of(u) == Some(portal_host.clone())
            && reqwest::Url::parse(u)
                .map(|parsed| parsed.path().starts_with("/services/callbacklogin"))
                .unwrap_or(false)
    })
}

/// Extracts the VW identity `templateModel` JSON embedded in the page.
///
/// The signin/authenticate pages carry their form state (hmac, relayState,
/// prefilled email, postAction, error) in a JS object rather than HTML
/// inputs: `window._IDK = { templateModel: { ... }, csrf_token: '...' }`.
fn extract_template_model(html: &str) -> Value {
    let Some(idx) = html.find("templateModel") else {
        return Value::Null;
    };
    let Some(brace_rel) = html[idx..].find('{') else {
        return Value::Null;
    };
    let start = idx + brace_rel;
    let mut depth: i32 = 0;
    for (i, c) in html[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let end = start + i + c.len_utf8();
                    return serde_json::from_str(&html[start..end]).unwrap_or(Value::Null);
                }
            }
            _ => {}
        }
    }
    Value::Null
}

fn extract_csrf(html: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"csrf_token\s*[:=]\s*['"]([^'"]+)['"]"#).expect("valid regex")
    });
    re.captures(html).map(|c| c[1].to_string())
}

/// The first `<form>`'s `action` attribute and every `<input>` inside it —
/// mirrors the Python client's `_FormParser` (an `HTMLParser` subclass);
/// implemented with two small regexes here since a full HTML parser is more
/// than this narrow, known page shape needs.
fn parse_form(html: &str) -> (Option<String>, HashMap<String, String>) {
    static FORM_RE: OnceLock<Regex> = OnceLock::new();
    static INPUT_RE: OnceLock<Regex> = OnceLock::new();
    static ATTR_RE: OnceLock<Regex> = OnceLock::new();

    let form_re = FORM_RE
        .get_or_init(|| Regex::new(r#"(?is)<form\b([^>]*)>(.*?)</form>"#).expect("valid regex"));
    let input_re =
        INPUT_RE.get_or_init(|| Regex::new(r#"(?is)<input\b([^>]*)/?>"#).expect("valid regex"));
    let attr_re = ATTR_RE
        .get_or_init(|| Regex::new(r#"(\w+)\s*=\s*(?:"([^"]*)"|'([^']*)')"#).expect("valid regex"));

    let Some(form_caps) = form_re.captures(html) else {
        return (None, HashMap::new());
    };
    let action = attrs_of(attr_re, &form_caps[1]).remove("action");

    let mut fields = HashMap::new();
    for input_caps in input_re.captures_iter(&form_caps[2]) {
        let mut attrs = attrs_of(attr_re, &input_caps[1]);
        if let Some(name) = attrs.remove("name") {
            fields.insert(name, attrs.remove("value").unwrap_or_default());
        }
    }
    (action, fields)
}

fn attrs_of(attr_re: &Regex, tag_attrs: &str) -> HashMap<String, String> {
    attr_re
        .captures_iter(tag_attrs)
        .map(|c| {
            let key = c[1].to_ascii_lowercase();
            let value = c
                .get(2)
                .or_else(|| c.get(3))
                .map_or("", |m| m.as_str())
                .to_string();
            (key, value)
        })
        .collect()
}

/// The password step's submit URL, built from `templateModel.postAction`
/// (e.g. `"login/authenticate"`) — never a `<form action>`, since this page
/// is client-rendered. `postAction` and the model's own `identifierUrl` are
/// both relative to the same signin-service root, so that root is recovered
/// by stripping `identifierUrl` off the *already-correct* `identifier_action`
/// URL from step 2, rather than resolved against this page's own URL: a
/// standard relative-URL join only drops the last path segment, which
/// double-counts the shared `login/` segment and gets HTTP 400.
fn resolve_post_action(html: &str, identifier_action: &str) -> Option<String> {
    let model = extract_template_model(html);
    let obj = model.as_object()?;
    let post_action = obj.get("postAction").and_then(|v| v.as_str())?;
    let identifier_url = obj
        .get("identifierUrl")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let root = identifier_action
        .strip_suffix(identifier_url)
        .unwrap_or(identifier_action);
    Some(format!("{root}{post_action}"))
}

/// Collects the fields needed to POST a VW identity login step: merges HTML
/// hidden inputs with the JS templateModel/csrf, since the email step
/// renders inputs server-side but the password step's live entirely in JS.
/// Returns `(fields, form_action)`.
fn login_fields(html: &str) -> (HashMap<String, String>, Option<String>) {
    let (action, mut fields) = parse_form(html);
    let model = extract_template_model(html);
    if let Some(obj) = model.as_object() {
        for key in ["hmac", "relayState"] {
            if let Some(v) = obj.get(key).and_then(|v| v.as_str()) {
                fields.insert(key.to_string(), v.to_string());
            }
        }
        if let Some(email) = obj
            .get("emailPasswordForm")
            .and_then(|v| v.get("email"))
            .and_then(|v| v.as_str())
        {
            fields
                .entry("email".to_string())
                .or_insert_with(|| email.to_string());
        }
    }
    if let Some(csrf) = extract_csrf(html) {
        fields.entry("_csrf".to_string()).or_insert(csrf);
    }
    (fields, action)
}

/// A human-readable login error from the page, if present.
fn login_error(html: &str) -> Option<String> {
    let model = extract_template_model(html);
    let obj = model.as_object()?;
    let err = obj.get("error").or_else(|| obj.get("errorCode"))?;
    if let Some(m) = err.as_object() {
        return m
            .get("text")
            .and_then(|v| v.as_str())
            .or_else(|| m.get("errorCode").and_then(|v| v.as_str()))
            .map(String::from);
    }
    err.as_str().map(String::from)
}

fn accept_language(language: &str, country: &str) -> String {
    if language.len() == 2 {
        let region = if country.is_empty() {
            language.to_uppercase()
        } else {
            country.to_uppercase()
        };
        format!("{language}-{region},{language};q=0.9,en;q=0.8")
    } else {
        "en-US,en;q=0.9".to_string()
    }
}

/// Browser-like headers for the VW identity login flow.
fn login_headers(
    country: &str,
    language: &str,
    referer: Option<&str>,
) -> Result<HeaderMap, CarBatteryError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::USER_AGENT,
        HeaderValue::from_static(USER_AGENT),
    );
    headers.insert(
        ACCEPT,
        HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
        ),
    );
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_str(&accept_language(language, country))
            .map_err(|e| CarBatteryError::Request(e.to_string()))?,
    );
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("max-age=0"));
    if let Some(referer) = referer {
        headers.insert(
            REFERER,
            HeaderValue::from_str(referer).map_err(|e| CarBatteryError::Request(e.to_string()))?,
        );
        if let Some(host) = host_of(referer)
            && let Ok(parsed) = reqwest::Url::parse(referer)
        {
            let origin = format!("{}://{host}", parsed.scheme());
            headers.insert(
                ORIGIN,
                HeaderValue::from_str(&origin)
                    .map_err(|e| CarBatteryError::Request(e.to_string()))?,
            );
        }
    }
    Ok(headers)
}

#[cfg(test)]
#[path = "vw_portal_tests.rs"]
mod tests;
