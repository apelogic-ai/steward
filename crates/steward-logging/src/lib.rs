//! Shared process-log contract for Steward components.

use std::fmt;
use std::io::{self, Write};

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

pub fn render_event(
    _format: LogFormat,
    _level: LogLevel,
    _component: &str,
    _request_id: Option<&str>,
    message: &str,
) -> String {
    message.to_owned()
}

pub fn write_event(
    writer: &mut impl Write,
    format: LogFormat,
    level: LogLevel,
    component: &str,
    request_id: Option<&str>,
    message: &str,
) -> io::Result<()> {
    writer.write_all(render_event(format, level, component, request_id, message).as_bytes())?;
    writer.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::{InvalidLogFormat, LogFormat, LogLevel, render_event, write_event};

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
    fn sensitive_values_are_redacted_without_dropping_the_event() -> Result<(), String> {
        let cases = [
            ("Authorization: Bearer auth-fixture-value", "auth-fixture-value"),
            ("request failed with Bearer bearer-fixture-value", "bearer-fixture-value"),
            ("provider returned sk-fixture-token-value", "sk-fixture-token-value"),
            (
                "assertion eyJhbGciOiJFUzI1NiJ9.eyJzdWIiOiJhbGljZSJ9.c2lnbmF0dXJl",
                "eyJhbGciOiJFUzI1NiJ9.eyJzdWIiOiJhbGljZSJ9.c2lnbmF0dXJl",
            ),
            (
                "database postgresql://alice:password-fixture@db.example.test/steward failed",
                "password-fixture",
            ),
            ("user alice@example.com was rejected", "alice@example.com"),
            ("api_key=key-fixture-value", "key-fixture-value"),
        ];
        for format in [LogFormat::Plain, LogFormat::Json] {
            for (message, forbidden) in cases {
                let rendered = render_event(
                    format,
                    LogLevel::Error,
                    "steward-apiserver",
                    None,
                    message,
                );
                if rendered.contains(forbidden) {
                    return Err(format!("sensitive value remained in log output: {forbidden}"));
                }
                if !rendered.contains("[redacted]") {
                    return Err("redaction must retain the event with a fixed marker".to_owned());
                }
            }
        }
        Ok(())
    }

    #[test]
    fn invalid_format_error_never_echoes_the_value() -> Result<(), String> {
        let InvalidLogFormat = LogFormat::parse(Some("json-with-secret-fixture"))
            .expect_err("an unsupported format must be refused");
        let message = InvalidLogFormat.to_string();
        if message.contains("secret-fixture") {
            return Err("configuration errors must not echo rejected input".to_owned());
        }
        Ok(())
    }
}
