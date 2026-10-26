//! Plug-in LLM providers ("compute nodes") for Brain ED: one trait-shaped
//! client for OpenAI-compatible `/v1/chat/completions` — llama.cpp's
//! `llama-server`, Ollama, OpenRouter, Groq, Together, vLLM all speak it.
//!
//! Safety rules:
//! - API keys are never stored in settings or printed: settings hold only the
//!   NAME of an environment variable (`api_key_env`), and the value is read
//!   from the environment at call time;
//! - `http://` is allowed only for loopback hosts; anything else must be
//!   `https://` so prompt text cannot leak over a plaintext connection;
//! - nothing is sent anywhere on its own — a request goes out only when
//!   `brain llm test` (or a future distiller run) asks for it.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// One configured provider: where to talk, what model, which env var holds
/// the key, and its place in the fallback order.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) struct LlmProvider {
    pub name: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub effort: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    /// Environment variable NAME (never the key itself).
    #[serde(default)]
    pub api_key_env: String,
    #[serde(default = "default_priority")]
    pub priority: u32,
    #[serde(default = "default_context_tokens")]
    pub context_tokens: u32,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
}

fn default_timeout_seconds() -> u64 {
    120
}

fn default_context_tokens() -> u32 {
    16_384
}

fn default_priority() -> u32 {
    100
}

/// The shared field parser for `brain llm add` and the settings loader.
///
/// `fields` are (key, value) pairs; unknown keys are refused so a typo never
/// silently creates a provider that would never be used.
pub(crate) fn llm_provider_from_fields(fields: &[(String, String)]) -> Result<LlmProvider, String> {
    let mut command = String::new();
    let mut effort = String::new();
    let mut name = String::new();
    let mut base_url = String::new();
    let mut model = String::new();
    let mut api_key_env = String::new();
    let mut priority: Option<u32> = None;
    let mut context_tokens = default_context_tokens();
    let mut timeout_seconds = default_timeout_seconds();
    for (key, value) in fields {
        match key.as_str() {
            "command" => command = value.clone(),
            "effort" => effort = value.clone(),
            "name" => name = value.clone(),
            "base_url" => base_url = value.clone(),
            "model" => model = value.clone(),
            "api_key_env" => api_key_env = value.clone(),
            "timeout_seconds" => {
                timeout_seconds = value
                    .parse()
                    .map_err(|_| "timeout_seconds must be an integer")?;
            }
            "context_tokens" => {
                context_tokens = value
                    .parse()
                    .map_err(|_| "context_tokens must be an integer")?;
            }
            "priority" => {
                priority =
                    Some(value.parse::<u32>().map_err(|_| {
                        "priority must be a whole number (0..=4294967295)".to_owned()
                    })?);
            }
            other => {
                return Err(format!(
                    "unknown llm field {other:?}; supported: name, command, effort, base_url, model, api_key_env, priority, context_tokens, timeout_seconds"
                ));
            }
        }
    }
    if !command.is_empty() && !fields.iter().any(|(k, _)| k == "context_tokens") {
        context_tokens = 128_000;
    }
    // A subscription model reads a 128k window far slower than 120 s allows
    // (Opus on 37 KB of session data timed out live); keep explicit values.
    if !command.is_empty() && !fields.iter().any(|(k, _)| k == "timeout_seconds") {
        timeout_seconds = 900;
    }
    let provider = LlmProvider {
        command,
        effort,
        name,
        base_url,
        model,
        api_key_env,
        priority: priority.unwrap_or_else(default_priority),
        context_tokens,
        timeout_seconds,
    };
    validate_provider(&provider)?;
    Ok(provider)
}

/// Structural validation shared by `llm add`, the settings loader and tests.
pub(crate) fn validate_provider(provider: &LlmProvider) -> Result<(), String> {
    if provider.name.trim().is_empty() {
        return Err("llm provider name must not be empty (choose a short name you will pass to --provider, e.g. `local-llama`)".into());
    }
    if !(4096..=1_048_576).contains(&provider.context_tokens) {
        return Err("context_tokens must be between 4096 and 1048576".into());
    }
    if !(10..=3600).contains(&provider.timeout_seconds) {
        return Err("timeout_seconds must be between 10 and 3600".into());
    }
    if !provider.effort.is_empty() && !["low", "medium", "high"].contains(&provider.effort.as_str())
    {
        return Err("effort must be low, medium or high".into());
    }
    if !provider.command.is_empty() {
        if !["claude", "codex"].contains(&provider.command.as_str())
            || !provider.base_url.is_empty()
            || !provider.api_key_env.is_empty()
        {
            return Err(
                "command must be claude or codex and cannot use base_url/api_key_env".into(),
            );
        }
        return Ok(());
    }
    if !provider.effort.is_empty() {
        return Err("effort requires command (set --command first; effort is the reasoning-effort word sent to a command provider)".into());
    }
    let allowed = base_url_allowed(&provider.base_url)?;
    if !allowed {
        return Err(format!(
            "base_url must use https:// or http:// on 127.0.0.1/localhost; refusing {}",
            provider.base_url
        ));
    }
    if provider.model.trim().is_empty() {
        return Err(
            "llm provider model must not be empty (pass --model exactly as the provider names it)"
                .into(),
        );
    }
    if !provider.api_key_env.is_empty()
        && !provider
            .api_key_env
            .starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
        && provider
            .api_key_env
            .chars()
            .any(|c| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
    {
        return Err(
            "api_key_env must look like an environment variable name (UPPER_SNAKE_CASE, e.g. LOCAL_LLM_TOKEN); the secret itself stays in your environment".into(),
        );
    }
    Ok(())
}

/// `https://` to any host, `http://` only to a loopback IP literal (or `localhost`, which
/// [`resolved_base`] pins to 127.0.0.1 without asking DNS). Credentials in the URL
/// (`user:pass@`) are refused outright: they hide the real host from a naive check.
pub(crate) fn base_url_allowed(base_url: &str) -> Result<bool, String> {
    let parsed = url::Url::parse(base_url).map_err(|_| {
        "base_url is not a valid URL; expected https://host/... or http://127.0.0.1:PORT".to_owned()
    })?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(
            "base_url must not contain user:password@; put credentials in api_key_env".into(),
        );
    }
    match parsed.scheme() {
        "https" => Ok(parsed.host().is_some()),
        "http" => Ok(match parsed.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
            None => false,
        }),
        other => Err(format!(
            "base_url scheme {other:?} is not supported; expected https:// or http://127.0.0.1"
        )),
    }
}

/// The base URL actually dialled: `http://localhost` becomes `http://127.0.0.1`, so a
/// hostile resolver cannot send a plaintext request (and its Bearer key) elsewhere.
fn resolved_base(base_url: &str) -> String {
    match url::Url::parse(base_url) {
        Ok(mut parsed)
            if parsed.scheme() == "http"
                && parsed
                    .host_str()
                    .is_some_and(|h| h.eq_ignore_ascii_case("localhost")) =>
        {
            let _ = parsed.set_host(Some("127.0.0.1"));
            parsed.to_string()
        }
        _ => base_url.to_owned(),
    }
}

/// A transport failure in words that cannot carry the request: the library's own
/// messages may quote URLs, headers or redirect targets — and so the key.
fn transport_category(error: &ureq::Error) -> String {
    match error {
        ureq::Error::Status(code, _) => format!("HTTP {code}"),
        ureq::Error::Transport(transport) => format!("transport error: {}", transport.kind()),
    }
}

/// The outcome of one chat attempt against one provider.
#[derive(Clone, Debug)]
#[allow(dead_code)] // base_url is provenance for future consumers of the fallback report
pub(crate) struct ChatOutcome {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub ok: bool,
    pub elapsed_ms: u128,
    pub tokens_per_second: Option<f64>,
    pub json_valid: Option<bool>,
    /// Never the response text — only counts and validity.
    pub error: Option<String>,
    /// Parsed response retained for the distiller; never printed by diagnostics.
    pub response_text: Option<String>,
}

/// The JSON body sent to `/v1/chat/completions`.
fn chat_body(
    provider: &LlmProvider,
    prompt: &str,
    max_tokens: u32,
    temperature: f64,
    schema: Option<&Value>,
) -> Value {
    let mut body = json!({
        "model": provider.model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": max_tokens,
        "temperature": temperature,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    if let Some(schema) = schema {
        // llama-server (and OpenAI-compatible servers) accept a JSON-schema
        // response format. `strict: false` explicitly: OpenAI-style gateways
        // default to strict mode, which rejects schemas with optional fields
        // or without `additionalProperties: false`; the answer is validated
        // against the schema here anyway (`json_schema_matches`).
        body["response_format"] = json!({
            "type": "json_schema",
            "json_schema": {"name": "brain_ed_response", "strict": false, "schema": schema},
        });
    }
    body
}

fn endpoint(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    }
}

/// One chat call against one provider. Never prints the response text; the
/// returned value carries counters and validity only.
pub(crate) fn chat_one(
    provider: &LlmProvider,
    prompt: &str,
    max_tokens: u32,
    temperature: f64,
    schema: Option<&Value>,
    timeout: Duration,
) -> ChatOutcome {
    let started = Instant::now();
    let mut outcome = ChatOutcome {
        provider: provider.name.clone(),
        model: provider.model.clone(),
        base_url: provider.base_url.clone(),
        ok: false,
        elapsed_ms: 0,
        tokens_per_second: None,
        json_valid: None,
        error: None,
        response_text: None,
    };
    if !provider.command.is_empty() {
        match crate::llm_command::run(provider, prompt, schema, timeout) {
            Ok(content) => {
                let valid = serde_json::from_str::<Value>(&content)
                    .map(|doc| schema.is_none_or(|s| json_schema_matches(&doc, s)))
                    .unwrap_or(false);
                outcome.json_valid = Some(valid);
                if valid {
                    outcome.ok = true;
                    outcome.response_text = Some(content);
                } else {
                    outcome.error =
                        Some("command response is not valid JSON for requested schema".into());
                }
            }
            Err(error) => outcome.error = Some(error),
        }
        outcome.elapsed_ms = started.elapsed().as_millis();
        return outcome;
    }
    let key = if provider.api_key_env.is_empty() {
        String::new()
    } else {
        std::env::var(&provider.api_key_env).unwrap_or_default()
    };
    if !provider.api_key_env.is_empty() && key.is_empty() {
        outcome.elapsed_ms = started.elapsed().as_millis();
        outcome.error = Some(format!(
            "environment variable {} is not set; the API key is read at call time and never stored",
            provider.api_key_env
        ));
        return outcome;
    }
    if key.chars().any(char::is_control) {
        outcome.elapsed_ms = started.elapsed().as_millis();
        outcome.error = Some(format!(
            "the value of {} contains control characters; refusing to send it",
            provider.api_key_env
        ));
        return outcome;
    }
    if !base_url_allowed(&provider.base_url).unwrap_or(false) {
        outcome.elapsed_ms = started.elapsed().as_millis();
        outcome.error =
            Some("base_url is not allowed (https anywhere, http only to loopback)".into());
        return outcome;
    }
    // No redirects: a 3xx could move the request (and the key) to a host the
    // allow-list never saw, including plain http.
    let agent = ureq::AgentBuilder::new()
        .timeout(timeout)
        .timeout_connect(timeout)
        .redirects(0)
        .build();
    let request = agent
        .post(&endpoint(&resolved_base(&provider.base_url)))
        .set("Content-Type", "application/json");
    let request = if key.is_empty() {
        request
    } else {
        request.set("Authorization", &format!("Bearer {key}"))
    };
    let response = request.send_string(
        &serde_json::to_string(&chat_body(
            provider,
            prompt,
            max_tokens,
            temperature,
            schema,
        ))
        .unwrap_or_default(),
    );
    outcome.elapsed_ms = started.elapsed().as_millis();
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            outcome.error = Some(if started.elapsed() >= timeout {
                format!("request timed out after {} seconds", timeout.as_secs())
            } else {
                transport_category(&error)
            });
            return outcome;
        }
    };
    let body = match response.into_string() {
        Ok(body) => body,
        Err(error) => {
            outcome.error = Some(
                if started.elapsed() >= timeout
                    || matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    )
                {
                    format!("request timed out after {} seconds", timeout.as_secs())
                } else {
                    "response body read failed".into()
                },
            );
            outcome.elapsed_ms = started.elapsed().as_millis();
            return outcome;
        }
    };
    outcome.elapsed_ms = started.elapsed().as_millis();
    let parsed: Value = match serde_json::from_str(&body) {
        Ok(parsed) => parsed,
        Err(_) => {
            outcome.error = Some("response is not JSON".into());
            return outcome;
        }
    };
    let content = parsed
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let tokens = parsed
        .pointer("/usage/completion_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if tokens > 0 {
        // A local server can answer in under a millisecond; clamp to 1 ms so
        // the rate never divides by zero.
        let ms = outcome.elapsed_ms.max(1) as f64;
        outcome.tokens_per_second = Some((tokens as f64) / (ms / 1000.0));
    }
    outcome.response_text = Some(content);
    if let Some(schema) = schema {
        let valid =
            serde_json::from_str::<Value>(outcome.response_text.as_deref().unwrap_or_default())
                .map(|document| json_schema_matches(&document, schema))
                .unwrap_or(false);
        outcome.json_valid = Some(valid);
        if !valid {
            outcome.error = Some("response is not valid JSON for the requested schema".to_owned());
            return outcome;
        }
    } else {
        outcome.json_valid = None;
    }
    outcome.ok = true;
    outcome
}

/// Minimal structural check of the response against the requested schema:
/// required properties present, declared types match, nested objects/arrays
/// recurse. Not a full JSON-Schema validator — enough to trust the shape.
fn json_schema_matches(document: &Value, schema: &Value) -> bool {
    let Some(schema_type) = schema.get("type").and_then(Value::as_str) else {
        return true;
    };
    match schema_type {
        "object" => {
            let Some(document_object) = document.as_object() else {
                return false;
            };
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for key in required {
                    if !document_object.contains_key(key.as_str().unwrap_or_default()) {
                        return false;
                    }
                }
            }
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (key, subschema) in properties {
                    if let Some(value) = document_object.get(key)
                        && !json_schema_matches(value, subschema)
                    {
                        return false;
                    }
                }
            }
            true
        }
        "array" => {
            let Some(items) = document.as_array() else {
                return false;
            };
            let Some(item_schema) = schema.get("items") else {
                return true;
            };
            items
                .iter()
                .all(|item| json_schema_matches(item, item_schema))
        }
        "string" => document.is_string(),
        "integer" | "number" => document.is_number(),
        "boolean" => document.is_boolean(),
        _ => true,
    }
}

/// The document for `brain llm list`.
pub(crate) fn list_document(providers: &[LlmProvider]) -> Value {
    let mut sorted = providers.to_vec();
    sorted.sort_by_key(|provider| provider.priority);
    json!({
        "status": "ok",
        "count": sorted.len(),
        "providers": sorted,
        "note": "API keys are never stored: only the environment variable name is",
    })
}

/// Fallback selection: providers sorted by priority (ascending = tried
/// first), the first successful one wins, failures are reported per provider.
pub(crate) fn chat_with_fallback(
    providers: &[LlmProvider],
    prompt: &str,
    max_tokens: u32,
    temperature: f64,
    schema: Option<&Value>,
    timeout: Option<Duration>,
) -> (Vec<ChatOutcome>, Option<ChatOutcome>) {
    let mut ordered: Vec<&LlmProvider> = providers.iter().collect();
    ordered.sort_by_key(|provider| provider.priority);
    let mut attempts = Vec::new();
    for provider in &ordered {
        let outcome = chat_one(
            provider,
            prompt,
            max_tokens,
            temperature,
            schema,
            timeout.unwrap_or_else(|| Duration::from_secs(provider.timeout_seconds)),
        );
        let limited = outcome.error.as_deref() == Some("provider_limited");
        let ok = outcome.ok;
        attempts.push(outcome);
        // A subscription that hit its usage limit hands over to the next
        // provider; the caller reports `provider_limited` only when none won.
        if limited {
            continue;
        }
        if ok {
            let winner = attempts.last().expect("just pushed").clone();
            return (attempts, Some(winner));
        }
    }
    (attempts, None)
}

#[cfg(test)]
mod tests {
    /// G12: the provider URL table — https anywhere, http only on loopback,
    /// credentials refused, other schemes refused; Ok(false) means parsed
    /// but not allowed.
    #[test]
    fn provider_urls_follow_the_table() {
        let cases: &[(&str, Result<bool, ()>)] = &[
            ("https://api.openai.com/v1", Ok(true)),
            ("https://openai.example:443/v1", Ok(true)),
            ("https://127.0.0.1:9000/v1?x=1#y", Ok(true)),
            ("https://[::1]:9000", Ok(true)),
            ("http://127.0.0.1:8080/v1", Ok(true)),
            ("http://localhost:1234", Ok(true)),
            ("http://LOCALHOST:1234", Ok(true)),
            ("http://[::1]:8000", Ok(true)),
            ("http://192.168.1.5:8080", Ok(false)),
            ("http://10.0.0.1", Ok(false)),
            ("http://0.0.0.0:8000", Ok(false)),
            ("http://[::ffff:127.0.0.1]:8000", Ok(false)),
            ("http://[fe80::1]:8000", Ok(false)),
            ("http://example.com", Ok(false)),
            ("https://", Err(())),
            ("http://", Err(())),
            ("ftp://127.0.0.1", Err(())),
            ("ws://127.0.0.1:8080", Err(())),
            ("127.0.0.1:8080", Err(())),
            ("", Err(())),
            ("not a url", Err(())),
            ("http://user:pass@127.0.0.1:1", Err(())),
            ("https://user@api.example.com", Err(())),
            ("HTTP://127.0.0.1:8080", Ok(true)),
        ];
        for (input, expected) in cases {
            let outcome = base_url_allowed(input).map_err(|_| ());
            assert_eq!(outcome, *expected, "{input:?}");
        }
    }
    use super::*;

    #[test]
    fn timeout_defaults_bounds_and_legacy_settings() {
        let fields = vec![
            ("name".into(), "local".into()),
            ("base_url".into(), "http://127.0.0.1:8080".into()),
            ("model".into(), "m".into()),
        ];
        let provider = llm_provider_from_fields(&fields).unwrap();
        assert_eq!(provider.timeout_seconds, 120);
        let mut legacy = serde_json::to_value(&provider).unwrap();
        legacy.as_object_mut().unwrap().remove("timeout_seconds");
        assert_eq!(
            serde_json::from_value::<LlmProvider>(legacy)
                .unwrap()
                .timeout_seconds,
            120
        );
        for seconds in ["10", "900", "3600", "9", "3601", "invalid"] {
            let mut fields = fields.clone();
            fields.push(("timeout_seconds".into(), seconds.into()));
            let result = llm_provider_from_fields(&fields);
            if ["10", "900", "3600"].contains(&seconds) {
                assert_eq!(result.unwrap().timeout_seconds.to_string(), seconds);
            } else {
                assert!(result.is_err());
            }
        }
    }

    #[test]
    fn base_urls_are_parsed_not_matched_by_prefix() {
        for allowed in [
            "https://api.example.com/v1",
            "http://127.0.0.1:8080",
            "http://127.9.9.9:1",
            "http://[::1]:8080/v1",
            "http://localhost:11434",
        ] {
            assert_eq!(base_url_allowed(allowed), Ok(true), "{allowed}");
        }
        for refused in [
            "http://127.0.0.1.evil:8080",
            "http://10.0.0.5:8080",
            "http://example.com",
            "http://[::2]:8080",
        ] {
            assert_eq!(base_url_allowed(refused), Ok(false), "{refused}");
        }
        for invalid in [
            "http://127.0.0.1:1@10.0.0.5:8080",
            "https://user:pass@api.example.com",
            "ftp://127.0.0.1",
            "not a url",
        ] {
            assert!(base_url_allowed(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn localhost_is_pinned_to_loopback_without_dns() {
        assert_eq!(
            resolved_base("http://localhost:11434/v1"),
            "http://127.0.0.1:11434/v1"
        );
        assert_eq!(
            resolved_base("https://localhost:8443"),
            "https://localhost:8443"
        );
        assert_eq!(
            resolved_base("https://api.example.com"),
            "https://api.example.com"
        );
    }

    #[test]
    fn base_url_rules_refuse_plaintext_off_loopback() {
        assert!(base_url_allowed("https://openrouter.ai/api/v1").unwrap());
        assert!(base_url_allowed("http://127.0.0.1:8080/v1").unwrap());
        assert!(base_url_allowed("http://localhost:8080").unwrap());
        assert!(!base_url_allowed("http://10.0.0.5:8080").unwrap());
        assert!(!base_url_allowed("http://example.com/v1").unwrap());
        assert!(base_url_allowed("ftp://example.com").is_err());
        assert!(base_url_allowed("not a url").is_err());
    }

    #[test]
    fn provider_fields_are_validated_and_unknown_refused() {
        let ok = llm_provider_from_fields(&[
            ("name".into(), "local".into()),
            ("base_url".into(), "http://127.0.0.1:8080".into()),
            ("model".into(), "gemma-4-e4b".into()),
            ("priority".into(), "10".into()),
        ])
        .expect("valid");
        assert_eq!(ok.priority, 10);
        assert!(
            llm_provider_from_fields(&[
                ("name".into(), "local".into()),
                ("base_url".into(), "http://10.0.0.5:8080".into()),
                ("model".into(), "m".into()),
            ])
            .is_err(),
            "plaintext off loopback refused"
        );
        assert!(
            llm_provider_from_fields(&[
                ("name".into(), "x".into()),
                ("surprise".into(), "1".into())
            ])
            .is_err()
        );
        assert!(llm_provider_from_fields(&[("name".into(), " ".into())]).is_err());
    }
}
