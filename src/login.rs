//! Eigener Login für einen Slot, unabhängig vom Login von Claude Code.
//!
//! Dasselbe Verfahren wie `/login` in Claude Code (OAuth mit PKCE, manueller
//! Rückweg): Der Browser zeigt nach der Anmeldung einen Code, der hier
//! eingefügt wird. Jeder Login bekommt einen eigenen Refresh-Token, deshalb
//! kollidiert er weder mit Claude Code noch mit einem anderen PC.
//!
//! Adressen, Client-ID und Scopes stammen aus Claude Code 2.1.300 und können
//! sich mit neueren Versionen ändern.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::slots::Slot;
use crate::usage;

const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
/// Rückweg, auf dem die Anthropic-Seite den Code zum Kopieren anzeigt.
const REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code \
                      user:mcp_servers user:file_upload user:plugins";

/// Ein begonnener Login: der Link für den Browser und die Werte, mit denen
/// der Code danach eingelöst wird.
#[derive(Clone)]
pub struct PendingLogin {
    pub url: String,
    verifier: String,
    state: String,
}

impl PendingLogin {
    pub fn start() -> Result<Self> {
        let verifier = random_token()?;
        let state = random_token()?;
        let challenge = pkce_challenge(&verifier);
        let params = [
            ("code", "true"),
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT_URI),
            ("scope", SCOPES),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
        ];
        let query: Vec<String> = params
            .iter()
            .map(|(key, value)| format!("{key}={}", encode(value)))
            .collect();
        Ok(Self {
            url: format!("{AUTHORIZE_URL}?{}", query.join("&")),
            verifier,
            state,
        })
    }

    /// Löst den eingefügten Code ein und baut daraus einen Slot.
    pub fn finish(&self, pasted: &str) -> Result<Slot> {
        let code = parse_code(pasted, &self.state)?;
        let token = self.exchange(&code)?;
        let access = token
            .get("access_token")
            .and_then(Value::as_str)
            .context("Antwort ohne Access-Token")?;
        // Ohne Profil fehlen nur Anzeigefelder; Claude Code lädt es später selbst nach.
        let profile = fetch_profile(access).ok();
        build_slot(&token, profile.as_ref(), chrono::Utc::now().timestamp_millis())
    }

    fn exchange(&self, code: &str) -> Result<Value> {
        let body = json!({
            "grant_type": "authorization_code",
            "code": code,
            "redirect_uri": REDIRECT_URI,
            "client_id": CLIENT_ID,
            "code_verifier": self.verifier,
            "state": self.state,
        });
        let mut response = usage::agent()
            .post(TOKEN_URL)
            .header("Content-Type", "application/json")
            .header("User-Agent", usage::USER_AGENT)
            .send(body.to_string())
            .map_err(|e| anyhow!("Keine Verbindung zum Anmeldeserver: {e}"))?;
        let status = response.status().as_u16();
        let text = response.body_mut().read_to_string().unwrap_or_default();
        if status != 200 {
            let detail = error_detail(&text);
            if detail.contains("invalid_grant") {
                bail!(
                    "Der Code ist ungültig, abgelaufen oder schon benutzt. \
                     Link neu öffnen und den neuen Code einfügen."
                );
            }
            bail!("Anmeldung fehlgeschlagen (HTTP {status}): {detail}");
        }
        serde_json::from_str(&text).context("Unerwartete Antwort vom Anmeldeserver")
    }
}

fn fetch_profile(access_token: &str) -> Result<Value> {
    let mut response = usage::agent()
        .get(PROFILE_URL)
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("anthropic-beta", usage::OAUTH_BETA)
        .header("User-Agent", usage::USER_AGENT)
        .call()?;
    if response.status().as_u16() != 200 {
        bail!("Profil nicht abrufbar (HTTP {})", response.status().as_u16());
    }
    Ok(serde_json::from_str(&response.body_mut().read_to_string()?)?)
}

/// Token-Objekt und `oauthAccount` so, wie Claude Code sie nach `/login` ablegt.
fn build_slot(token: &Value, profile: Option<&Value>, now_ms: i64) -> Result<Slot> {
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let access = text(token, "access_token").context("Antwort ohne Access-Token")?;
    let refresh = text(token, "refresh_token").context("Antwort ohne Refresh-Token")?;
    let expires_in = token.get("expires_in").and_then(Value::as_i64).unwrap_or(8 * 3600);
    let scopes: Vec<&str> = token
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .split_whitespace()
        .collect();

    let null = Value::Null;
    let account = profile.map_or(&null, |p| &p["account"]);
    let org = profile.map_or(&null, |p| &p["organization"]);
    let token_account = &token["account"];
    let token_org = &token["organization"];

    let subscription = match org["organization_type"].as_str() {
        Some("claude_max") => json!("max"),
        Some("claude_pro") => json!("pro"),
        Some("claude_enterprise") => json!("enterprise"),
        Some("claude_team") => json!("team"),
        _ => Value::Null,
    };
    let mut oauth = Map::new();
    oauth.insert("accessToken".into(), json!(access));
    oauth.insert("refreshToken".into(), json!(refresh));
    oauth.insert("expiresAt".into(), json!(now_ms + expires_in * 1000));
    if let Some(seconds) = token.get("refresh_token_expires_in").and_then(Value::as_i64) {
        oauth.insert("refreshTokenExpiresAt".into(), json!(now_ms + seconds * 1000));
    }
    oauth.insert("scopes".into(), json!(scopes));
    oauth.insert("subscriptionType".into(), subscription);
    oauth.insert("rateLimitTier".into(), org["rate_limit_tier"].clone());

    let account_uuid = text(account, "uuid").or_else(|| text(token_account, "uuid"));
    let email = text(account, "email").or_else(|| text(token_account, "email_address"));
    let org_uuid = text(org, "uuid").or_else(|| text(token_org, "uuid"));
    if account_uuid.is_none() && email.is_none() {
        bail!("Die Anmeldung hat keinen Account geliefert");
    }

    let mut credentials = Map::new();
    credentials.insert("claudeAiOauth".into(), Value::Object(oauth));
    if let Some(org_uuid) = &org_uuid {
        credentials.insert("organizationUuid".into(), json!(org_uuid));
    }

    let mut oauth_account = Map::new();
    let mut put = |key: &str, value: Value| {
        if !value.is_null() {
            oauth_account.insert(key.to_string(), value);
        }
    };
    put("accountUuid", json!(account_uuid));
    put("emailAddress", json!(email));
    put("organizationUuid", json!(org_uuid));
    put("hasExtraUsageEnabled", org["has_extra_usage_enabled"].clone());
    put("billingType", org["billing_type"].clone());
    put("accountCreatedAt", account["created_at"].clone());
    put("subscriptionCreatedAt", org["subscription_created_at"].clone());
    put("ccOnboardingFlags", org["cc_onboarding_flags"].clone());
    put("seatTier", org["seat_tier"].clone());
    put("displayName", account["display_name"].clone());
    put("fullName", account["full_name"].clone());
    put("organizationName", org["name"].clone());
    put("organizationType", org["organization_type"].clone());
    put("organizationRateLimitTier", org["rate_limit_tier"].clone());
    if profile.is_some() {
        put("profileFetchedAt", json!(now_ms));
    }

    Ok(Slot {
        credentials: Value::Object(credentials),
        oauth_account: Value::Object(oauth_account),
        saved_at: chrono::Local::now().to_rfc3339(),
    })
}

/// Die Seite zeigt `code#state`. Akzeptiert auch nur den Code oder die ganze
/// Adresse aus der Adresszeile.
fn parse_code(pasted: &str, expected_state: &str) -> Result<String> {
    let text = pasted.trim();
    if text.is_empty() {
        bail!("Bitte den Code einfügen");
    }
    let (code, state) = if text.starts_with("http") {
        let query = text.split_once('?').map(|(_, q)| q).unwrap_or_default();
        let mut code = None;
        let mut state = None;
        for pair in query.split(['&', '#']) {
            if let Some(value) = pair.strip_prefix("code=") {
                code = Some(decode(value));
            } else if let Some(value) = pair.strip_prefix("state=") {
                state = Some(decode(value));
            }
        }
        (code.context("In der eingefügten Adresse steht kein Code")?, state)
    } else if let Some((code, state)) = text.split_once('#') {
        (code.to_string(), Some(state.to_string()))
    } else {
        (text.to_string(), None)
    };
    if state.is_some_and(|state| state != expected_state) {
        bail!(
            "Der Code gehört zu einem anderen Login-Versuch. \
             Den Link aus diesem Fenster öffnen und erneut anmelden."
        );
    }
    if code.is_empty() || code.contains(char::is_whitespace) {
        bail!("Das sieht nicht nach dem Code von der Anmeldeseite aus");
    }
    Ok(code)
}

fn error_detail(body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let detail = parsed.as_ref().and_then(|v| match &v["error"] {
        Value::String(kind) => Some(match v["error_description"].as_str() {
            Some(description) => format!("{kind}: {description}"),
            None => kind.clone(),
        }),
        Value::Object(error) => error.get("message").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    });
    detail.unwrap_or_else(|| body.chars().take(200).collect())
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("Zufallswerte nicht verfügbar: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
        match (bytes[i], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                i += 1;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc_7636_example() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn login_url_has_all_parameters() {
        let login = PendingLogin::start().unwrap();
        assert!(login.url.starts_with("https://claude.com/cai/oauth/authorize?code=true&"));
        for part in [
            "client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e",
            "response_type=code",
            "redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback",
            "scope=org%3Acreate_api_key%20user%3Aprofile%20user%3Ainference%20",
            "code_challenge_method=S256",
        ] {
            assert!(login.url.contains(part), "{part} fehlt in {}", login.url);
        }
        assert!(login.url.contains(&format!("state={}", login.state)));
        assert_eq!(login.verifier.len(), 43);
        assert_ne!(PendingLogin::start().unwrap().state, login.state);
    }

    #[test]
    fn parses_pasted_code_variants() {
        assert_eq!(parse_code(" abc#S1 ", "S1").unwrap(), "abc");
        assert_eq!(parse_code("abc", "S1").unwrap(), "abc");
        assert!(parse_code("abc#S2", "S1").is_err());
        assert!(parse_code("", "S1").is_err());
        assert!(parse_code("zwei worte", "S1").is_err());
        let url = "https://platform.claude.com/oauth/code/callback?code=ab%2Dc&state=S1";
        assert_eq!(parse_code(url, "S1").unwrap(), "ab-c");
    }

    #[test]
    fn builds_slot_like_claude_code() {
        let token = json!({
            "access_token": "AT", "refresh_token": "RT", "expires_in": 28800,
            "refresh_token_expires_in": 2592000, "scope": "user:profile user:inference",
            "account": { "uuid": "acc-1", "email_address": "a@example.com" },
            "organization": { "uuid": "org-1" }
        });
        let profile = json!({
            "account": { "uuid": "acc-1", "email": "a@example.com", "display_name": "A",
                         "full_name": "A B", "created_at": "2025-01-01T00:00:00Z" },
            "organization": { "uuid": "org-1", "name": "Org", "organization_type": "claude_max",
                              "rate_limit_tier": "default_claude_max_20x", "billing_type": "stripe",
                              "seat_tier": null, "has_extra_usage_enabled": false }
        });
        let slot = build_slot(&token, Some(&profile), 1_000).unwrap();
        let oauth = &slot.credentials["claudeAiOauth"];
        assert_eq!(oauth["accessToken"], "AT");
        assert_eq!(oauth["refreshToken"], "RT");
        assert_eq!(oauth["expiresAt"], 1_000 + 28_800_000);
        assert_eq!(oauth["refreshTokenExpiresAt"], 1_000 + 2_592_000_000i64);
        assert_eq!(oauth["scopes"], json!(["user:profile", "user:inference"]));
        assert_eq!(oauth["subscriptionType"], "max");
        assert_eq!(oauth["rateLimitTier"], "default_claude_max_20x");
        assert_eq!(slot.credentials["organizationUuid"], "org-1");
        assert_eq!(slot.oauth_account["emailAddress"], "a@example.com");
        assert_eq!(slot.oauth_account["organizationName"], "Org");
        assert!(slot.oauth_account.get("seatTier").is_none());
        let id = slot.identity().unwrap();
        assert_eq!((id.account_uuid.as_str(), id.org_uuid.as_str()), ("acc-1", "org-1"));

        // Ohne Profil reicht, was die Token-Antwort liefert.
        let bare = build_slot(&token, None, 1_000).unwrap();
        assert_eq!(bare.oauth_account["emailAddress"], "a@example.com");
        assert_eq!(bare.credentials["claudeAiOauth"]["subscriptionType"], Value::Null);
    }
}
