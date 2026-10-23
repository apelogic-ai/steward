//! Shared process-log contract for Steward components.

use std::env;
use std::fmt;
use std::io::{self, Write};
use std::sync::OnceLock;

use serde::Serialize;

const LOG_FORMAT_ENVIRONMENT: &str = "STEWARD_LOG_FORMAT";
const REDACTED: &str = "[redacted]";

#[derive(Clone, Copy)]
struct ProcessLogConfig {
    format: LogFormat,
    component: &'static str,
}

static PROCESS_LOG_CONFIG: OnceLock<ProcessLogConfig> = OnceLock::new();

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LogFormat {
    #[default]
    Plain,
    Json,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidLogFormat;

impl fmt::Display for InvalidLogFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("STEWARD_LOG_FORMAT must be plain or json")
    }
}

impl std::error::Error for InvalidLogFormat {}

impl LogFormat {
    pub fn parse(value: Option<&str>) -> Result<Self, InvalidLogFormat> {
        match value {
            None | Some("plain") => Ok(Self::Plain),
            Some("json") => Ok(Self::Json),
            Some(_) => Err(InvalidLogFormat),
        }
    }
}

impl LogLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

#[derive(Serialize)]
struct JsonLogEvent<'a> {
    level: &'static str,
    component: &'a str,
    request_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<&'a str>,
    message: &'a str,
}

/// Initializes the process-wide log format and component name.
///
/// Invalid settings fall back to plain text and produce a fixed warning. The
/// rejected value is never copied into logs, and logging cannot prevent the
/// component from starting.
pub fn init(component: &'static str) {
    let configured = env::var(LOG_FORMAT_ENVIRONMENT);
    let parsed = configured
        .as_deref()
        .map(Some)
        .unwrap_or_else(|error| match error {
            env::VarError::NotPresent => None,
            env::VarError::NotUnicode(_) => Some(""),
        });
    let (format, invalid) = match LogFormat::parse(parsed) {
        Ok(format) => (format, false),
        Err(_) => (LogFormat::Plain, true),
    };
    let _ = PROCESS_LOG_CONFIG.set(ProcessLogConfig { format, component });
    if invalid {
        emit(
            LogLevel::Warn,
            None,
            format_args!("invalid log format; using plain text"),
        );
    }
}

pub fn emit(level: LogLevel, request_id: Option<&str>, message: fmt::Arguments<'_>) {
    emit_event(level, request_id, None, message);
}

fn emit_event(
    level: LogLevel,
    request_id: Option<&str>,
    stream: Option<&str>,
    message: fmt::Arguments<'_>,
) {
    let fallback = ProcessLogConfig {
        format: LogFormat::Plain,
        component: "steward",
    };
    let config = PROCESS_LOG_CONFIG.get().copied().unwrap_or(fallback);
    let message = message.to_string();
    let mut stderr = io::stderr().lock();
    let _ = write_event_with_stream(
        &mut stderr,
        config.format,
        level,
        config.component,
        request_id,
        stream,
        &message,
    );
}

pub fn emit_with_request_id(
    level: LogLevel,
    request_id: &impl fmt::Display,
    message: fmt::Arguments<'_>,
) {
    let request_id = request_id.to_string();
    emit(level, Some(&request_id), message);
}

/// Emits a process event with its task output stream as a structured field.
pub fn emit_with_stream(
    level: LogLevel,
    request_id: &impl fmt::Display,
    stream: &str,
    message: fmt::Arguments<'_>,
) {
    let request_id = request_id.to_string();
    emit_event(level, Some(&request_id), Some(stream), message);
}

#[macro_export]
macro_rules! event {
    ($level:expr, request_id = $request_id:expr, $($argument:tt)+) => {
        $crate::emit_with_request_id($level, &$request_id, format_args!($($argument)+))
    };
    ($level:expr, $($argument:tt)+) => {
        $crate::emit($level, None, format_args!($($argument)+))
    };
}

#[macro_export]
macro_rules! info {
    ($($argument:tt)+) => {
        $crate::event!($crate::LogLevel::Info, $($argument)+)
    };
}

#[macro_export]
macro_rules! warn {
    ($($argument:tt)+) => {
        $crate::event!($crate::LogLevel::Warn, $($argument)+)
    };
}

#[macro_export]
macro_rules! error {
    ($($argument:tt)+) => {
        $crate::event!($crate::LogLevel::Error, $($argument)+)
    };
}

pub fn render_event(
    format: LogFormat,
    level: LogLevel,
    component: &str,
    request_id: Option<&str>,
    message: &str,
) -> String {
    render_event_with_stream(format, level, component, request_id, None, message)
}

pub fn render_event_with_stream(
    format: LogFormat,
    level: LogLevel,
    component: &str,
    request_id: Option<&str>,
    stream: Option<&str>,
    message: &str,
) -> String {
    let message = redact(message);
    let request_id = request_id.map(redact);
    let stream = stream.map(redact);
    match format {
        LogFormat::Plain => message,
        LogFormat::Json => {
            let event = JsonLogEvent {
                level: level.as_str(),
                component,
                request_id: request_id.as_deref(),
                stream: stream.as_deref(),
                message: &message,
            };
            serde_json::to_string(&event).unwrap_or_else(|_| {
                "{\"level\":\"error\",\"component\":\"steward\",\"request_id\":null,\"message\":\"log serialization failed\"}".to_owned()
            })
        }
    }
}

pub fn write_event(
    writer: &mut impl Write,
    format: LogFormat,
    level: LogLevel,
    component: &str,
    request_id: Option<&str>,
    message: &str,
) -> io::Result<()> {
    write_event_with_stream(writer, format, level, component, request_id, None, message)
}

fn write_event_with_stream(
    writer: &mut impl Write,
    format: LogFormat,
    level: LogLevel,
    component: &str,
    request_id: Option<&str>,
    stream: Option<&str>,
    message: &str,
) -> io::Result<()> {
    writer.write_all(
        render_event_with_stream(format, level, component, request_id, stream, message).as_bytes(),
    )?;
    writer.write_all(b"\n")
}

/// Removes credential-shaped values from text before it reaches a log sink.
pub fn redact_text(message: &str) -> String {
    redact(message)
}

#[derive(Clone, Copy)]
enum PendingSensitiveField {
    Authorization,
    Value,
}

fn redact(message: &str) -> String {
    let mut output = String::with_capacity(message.len());
    let mut redact_following = 0_u8;
    let mut pending_field = None;
    for piece in message.split_inclusive(char::is_whitespace) {
        let token = piece.trim_end_matches(char::is_whitespace);
        let whitespace = &piece[token.len()..];
        let (replacement, following) = if redact_following > 0 {
            redact_following -= 1;
            (Some(REDACTED), 0)
        } else if let Some(field) = pending_field {
            if is_field_separator(token) {
                (None, 0)
            } else {
                pending_field = None;
                let following = u8::from(
                    matches!(field, PendingSensitiveField::Authorization)
                        && is_authorization_scheme(token),
                );
                (Some(REDACTED), following)
            }
        } else if let Some(following) = sensitive_field(token) {
            (Some(REDACTED), following)
        } else if let Some(field) = standalone_sensitive_field(token) {
            pending_field = Some(field);
            (None, 0)
        } else if let Some(following) = bearer_value(token) {
            (Some(REDACTED), following)
        } else if contains_secret_key(token)
            || contains_jwt(token)
            || is_password_url(token)
            || is_email(token)
        {
            (Some(REDACTED), 0)
        } else {
            (None, 0)
        };
        redact_following = redact_following.max(following);
        output.push_str(replacement.unwrap_or(token));
        output.push_str(whitespace);
    }
    output
}

fn sensitive_field(token: &str) -> Option<u8> {
    let normalized = token.trim_start_matches(['"', '\'', '{', '[', '(']);
    let separator = normalized.find([':', '='])?;
    let field = normalized[..separator]
        .trim_matches(['"', '\'', '}', ']', ')'])
        .to_ascii_lowercase();
    let field_kind = classify_sensitive_field(&field)?;
    let value = normalized[separator + 1..]
        .trim_matches(['"', '\'', '\\', '{', '}', '[', ']', '(', ')', ',', ';']);
    if value.is_empty() {
        Some(
            if matches!(field_kind, PendingSensitiveField::Authorization) {
                2
            } else {
                1
            },
        )
    } else if matches!(field_kind, PendingSensitiveField::Authorization)
        && is_authorization_scheme(value)
    {
        Some(1)
    } else {
        Some(0)
    }
}

fn standalone_sensitive_field(token: &str) -> Option<PendingSensitiveField> {
    let field = token
        .trim_matches(|character: char| {
            !character.is_ascii_alphanumeric() && !matches!(character, '-' | '_')
        })
        .to_ascii_lowercase();
    classify_sensitive_field(&field)
}

fn classify_sensitive_field(field: &str) -> Option<PendingSensitiveField> {
    match field {
        "authorization" | "proxy-authorization" => Some(PendingSensitiveField::Authorization),
        "token" | "access_token" | "access-token" | "api_key" | "api-key" | "x-api-key"
        | "x_api_key" | "key" | "secret" | "password" | "client_secret" | "client-secret"
        | "private_key" | "private-key" | "database_url" | "database-url" | "id_token"
        | "id-token" | "jwt" | "assertion" => Some(PendingSensitiveField::Value),
        _ => None,
    }
}

fn is_field_separator(token: &str) -> bool {
    !token.is_empty()
        && token
            .bytes()
            .all(|byte| matches!(byte, b':' | b'=' | b'"' | b'\'' | b'\\'))
}

fn is_authorization_scheme(token: &str) -> bool {
    matches!(
        token.trim_matches(|character: char| !character.is_ascii_alphanumeric()),
        value if value.eq_ignore_ascii_case("bearer") || value.eq_ignore_ascii_case("basic")
    )
}

fn bearer_value(token: &str) -> Option<u8> {
    let normalized = token
        .trim_matches(|character: char| {
            !character.is_ascii_alphanumeric() && !matches!(character, ':' | '=')
        })
        .to_ascii_lowercase();
    if normalized == "bearer" {
        Some(1)
    } else if normalized
        .strip_prefix("bearer:")
        .or_else(|| normalized.strip_prefix("bearer="))
        .is_some_and(|value| !value.is_empty())
    {
        Some(0)
    } else {
        None
    }
}

fn contains_secret_key(token: &str) -> bool {
    let normalized = token
        .trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '-')
        .to_ascii_lowercase();
    normalized.starts_with("sk-") && normalized.len() >= 8
}

fn contains_jwt(token: &str) -> bool {
    fn segment_end(bytes: &[u8], mut index: usize) -> usize {
        while bytes
            .get(index)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            index += 1;
        }
        index
    }

    let bytes = token.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        while bytes
            .get(index)
            .is_some_and(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
        {
            index += 1;
        }
        let first_start = index;
        let first_end = segment_end(bytes, first_start);
        if first_end.saturating_sub(first_start) >= 8 && bytes.get(first_end) == Some(&b'.') {
            let second_start = first_end + 1;
            let second_end = segment_end(bytes, second_start);
            if second_end.saturating_sub(second_start) >= 8 && bytes.get(second_end) == Some(&b'.')
            {
                let third_start = second_end + 1;
                let third_end = segment_end(bytes, third_start);
                if third_end.saturating_sub(third_start) >= 8 {
                    return true;
                }
            }
        }
        index = first_end.saturating_add(1);
    }
    false
}

fn is_password_url(token: &str) -> bool {
    let normalized = token.trim_matches(|character: char| {
        matches!(
            character,
            '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'
        )
    });
    let Some((_, rest)) = normalized.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let Some((credentials, _)) = authority.rsplit_once('@') else {
        return false;
    };
    credentials
        .split_once(':')
        .is_some_and(|(user, password)| !user.is_empty() && !password.is_empty())
}

fn is_email(token: &str) -> bool {
    let normalized = token.trim_matches(|character: char| {
        !character.is_ascii_alphanumeric() && !matches!(character, '.' | '_' | '-' | '+' | '@')
    });
    let Some((local, host)) = normalized.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && host.contains('.')
        && !host.ends_with('.')
        && !host.contains('@')
        && normalized.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+' | b'@')
        })
}

#[cfg(test)]
mod tests {
    use super::{
        InvalidLogFormat, LogFormat, LogLevel, render_event, render_event_with_stream, write_event,
    };

    fn assert_redacted_in_both_formats(message: &str, forbidden: &str) -> Result<(), String> {
        for format in [LogFormat::Plain, LogFormat::Json] {
            let rendered =
                render_event(format, LogLevel::Error, "steward-apiserver", None, message);
            if rendered.contains(forbidden) {
                return Err(format!(
                    "{format:?} output retained sensitive fixture {forbidden}: {rendered}"
                ));
            }
            if !rendered.contains("[redacted]") {
                return Err(format!(
                    "{format:?} output did not retain the redaction marker: {rendered}"
                ));
            }
        }
        Ok(())
    }

    #[test]
    fn plain_text_remains_the_default() -> Result<(), String> {
        if LogFormat::parse(None).map_err(|error| error.to_string())? != LogFormat::Plain {
            return Err("an absent setting must retain plain text logging".to_owned());
        }
        let rendered = render_event(
            LogFormat::Plain,
            LogLevel::Info,
            "steward-apiserver",
            None,
            "service started",
        );
        if rendered != "service started" {
            return Err("plain text must preserve a safe message".to_owned());
        }
        Ok(())
    }

    #[test]
    fn json_is_exactly_one_object_with_stable_fields() -> Result<(), String> {
        let mut output = Vec::new();
        write_event(
            &mut output,
            LogFormat::Json,
            LogLevel::Error,
            "steward-controller",
            Some("request-123"),
            "first line\nsecond line",
        )
        .map_err(|error| error.to_string())?;
        let output = String::from_utf8(output).map_err(|error| error.to_string())?;
        if output.lines().count() != 1 {
            return Err("one event must occupy exactly one line".to_owned());
        }
        let value: serde_json::Value =
            serde_json::from_str(output.trim_end()).map_err(|error| error.to_string())?;
        let expected = serde_json::json!({
            "level": "error",
            "component": "steward-controller",
            "request_id": "request-123",
            "message": "first line\nsecond line",
        });
        if value != expected {
            return Err(format!("unexpected JSON log contract: {value}"));
        }
        Ok(())
    }

    #[test]
    fn absent_request_id_is_json_null() -> Result<(), String> {
        let rendered = render_event(
            LogFormat::Json,
            LogLevel::Warn,
            "steward-mint",
            None,
            "credential lookup failed",
        );
        let value: serde_json::Value =
            serde_json::from_str(&rendered).map_err(|error| error.to_string())?;
        if value.get("request_id") != Some(&serde_json::Value::Null) {
            return Err("an absent request id must be JSON null".to_owned());
        }
        Ok(())
    }

    #[test]
    fn task_output_stream_is_a_structured_json_field() -> Result<(), String> {
        let rendered = render_event_with_stream(
            LogFormat::Json,
            LogLevel::Info,
            "steward-controller",
            Some("runtime-123"),
            Some("stdout"),
            "task output",
        );
        let value: serde_json::Value =
            serde_json::from_str(&rendered).map_err(|error| error.to_string())?;
        if value.get("stream").and_then(serde_json::Value::as_str) != Some("stdout") {
            return Err(format!("task output stream is not structured: {value}"));
        }
        Ok(())
    }

    #[test]
    fn sensitive_values_are_redacted_without_dropping_the_event() -> Result<(), String> {
        let password_url = [
            "database postgresql://alice:",
            "password-fixture",
            "@db.example.test/steward failed",
        ]
        .concat();
        let cases = [
            (
                "Authorization: Bearer auth-fixture-value",
                "auth-fixture-value",
            ),
            (
                "request failed with Bearer bearer-fixture-value",
                "bearer-fixture-value",
            ),
            (
                "provider returned sk-fixture-token-value",
                "sk-fixture-token-value",
            ),
            (
                "assertion eyJhbGciOiJFUzI1NiJ9.eyJzdWIiOiJhbGljZSJ9.c2lnbmF0dXJl",
                "eyJhbGciOiJFUzI1NiJ9.eyJzdWIiOiJhbGljZSJ9.c2lnbmF0dXJl",
            ),
            (password_url.as_str(), "password-fixture"),
            ("user alice@example.com was rejected", "alice@example.com"),
            ("api_key=key-fixture-value", "key-fixture-value"),
        ];
        for format in [LogFormat::Plain, LogFormat::Json] {
            for (message, forbidden) in cases {
                let rendered =
                    render_event(format, LogLevel::Error, "steward-apiserver", None, message);
                if rendered.contains(forbidden) {
                    return Err(format!(
                        "sensitive value remained in log output: {forbidden}"
                    ));
                }
                if !rendered.contains("[redacted]") {
                    return Err("redaction must retain the event with a fixed marker".to_owned());
                }
            }
        }
        let rendered = render_event(
            LogFormat::Json,
            LogLevel::Error,
            "steward-controller",
            Some("Bearer request-id-fixture"),
            "request failed",
        );
        if rendered.contains("request-id-fixture") || !rendered.contains("[redacted]") {
            return Err("request identifiers must use the same redaction contract".to_owned());
        }
        Ok(())
    }

    #[test]
    fn quoted_json_authorization_value_is_redacted_in_both_formats() -> Result<(), String> {
        assert_redacted_in_both_formats(
            r#"{"authorization":"Bearer json-auth-fixture"}"#,
            "json-auth-fixture",
        )
    }

    #[test]
    fn glued_authorization_scheme_is_redacted_in_both_formats() -> Result<(), String> {
        assert_redacted_in_both_formats(
            "Authorization:Bearer glued-auth-fixture",
            "glued-auth-fixture",
        )
    }

    #[test]
    fn authorization_basic_value_is_redacted_in_both_formats() -> Result<(), String> {
        assert_redacted_in_both_formats(
            "authorization=Basic basic-auth-fixture",
            "basic-auth-fixture",
        )
    }

    #[test]
    fn glued_bearer_value_is_redacted_in_both_formats() -> Result<(), String> {
        assert_redacted_in_both_formats(
            "request failed bearer:bearer-fixture-value",
            "bearer-fixture-value",
        )
    }

    #[test]
    fn spaced_password_value_is_redacted_in_both_formats() -> Result<(), String> {
        let message = ["password", " = ", "password-field-fixture"].concat();
        assert_redacted_in_both_formats(&message, "password-field-fixture")
    }

    #[test]
    fn api_key_header_value_is_redacted_in_both_formats() -> Result<(), String> {
        assert_redacted_in_both_formats("x-api-key: api-key-fixture", "api-key-fixture")
    }

    #[test]
    fn authorization_key_controls_basic_value_redaction() -> Result<(), String> {
        assert_redacted_in_both_formats(
            "authorization: Basic authorization-key-fixture",
            "authorization-key-fixture",
        )
    }

    #[test]
    fn password_key_controls_value_redaction() -> Result<(), String> {
        assert_redacted_in_both_formats("password: password-key-fixture", "password-key-fixture")
    }

    #[test]
    fn token_key_controls_value_redaction() -> Result<(), String> {
        assert_redacted_in_both_formats("token: token-key-fixture", "token-key-fixture")
    }

    #[test]
    fn database_url_keys_control_value_redaction() -> Result<(), String> {
        assert_redacted_in_both_formats(
            "database_url: database-url-key-fixture",
            "database-url-key-fixture",
        )?;
        assert_redacted_in_both_formats(
            "database-url: database-dash-key-fixture",
            "database-dash-key-fixture",
        )
    }

    #[test]
    fn prefixed_jwt_values_are_redacted_in_both_formats() -> Result<(), String> {
        let jwt = "eyJhbGciOiJFUzI1NiJ9.eyJzdWIiOiJhbGljZSJ9.c2lnbmF0dXJl";
        for prefix in ["id_token=", "jwt:", "assertion=\""] {
            let message = format!("{prefix}{jwt}\"");
            assert_redacted_in_both_formats(&message, jwt)?;
        }
        Ok(())
    }

    #[test]
    fn secret_key_prefixes_are_redacted_without_hiding_ordinary_identifiers() -> Result<(), String>
    {
        for secret in [
            "sk-fixture-token-value",
            "sk-ant-fixture-token-value",
            "sk-proj-fixture-token-value",
        ] {
            assert_redacted_in_both_formats(secret, secret)?;
        }
        for identifier in [
            "task-0a1b2c3d4e5f",
            "operation-id-task-0a1b2c3d4e5f",
            "desk-example-identifier",
            "disk-example-identifier",
        ] {
            for format in [LogFormat::Plain, LogFormat::Json] {
                let rendered = render_event(
                    format,
                    LogLevel::Info,
                    "steward-controller",
                    None,
                    identifier,
                );
                if !rendered.contains(identifier) || rendered.contains("[redacted]") {
                    return Err(format!(
                        "{format:?} output hid ordinary identifier {identifier}: {rendered}"
                    ));
                }
            }
        }
        Ok(())
    }

    #[test]
    fn invalid_format_error_never_echoes_the_value() -> Result<(), String> {
        let Err(InvalidLogFormat) = LogFormat::parse(Some("json-with-secret-fixture")) else {
            return Err("an unsupported format must be refused".to_owned());
        };
        let message = InvalidLogFormat.to_string();
        if message.contains("secret-fixture") {
            return Err("configuration errors must not echo rejected input".to_owned());
        }
        Ok(())
    }
}
