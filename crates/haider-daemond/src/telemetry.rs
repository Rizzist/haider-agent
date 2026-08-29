//! Opt-in, allow-listed daemon timing telemetry.
//!
//! The daemon has many tracing events whose fields include paths, errors, and
//! other user-derived values. This subscriber deliberately enables only the
//! two timing targets audited for release diagnostics and ignores every field
//! outside their frozen numeric/phase allow-list.

use std::fmt::Write as _;
use std::sync::OnceLock;
use tracing::metadata::LevelFilter;
use tracing::subscriber::Interest;

const TRACE_ENV: &str = "HAIDER_DAEMON_TRACE";
const STORE_TARGET: &str = "haider.store";
const RECOVERY_TARGET: &str = "haider.recovery";

static INSTALL_STATE: OnceLock<bool> = OnceLock::new();

pub(super) fn install_opt_in() {
    let Some(value) = std::env::var_os(TRACE_ENV) else {
        return;
    };
    if value != "1" {
        eprintln!("haiderd: {TRACE_ENV} ignored; set it to 1 to enable safe timing traces");
        return;
    }

    let installed = INSTALL_STATE
        .get_or_init(|| tracing::subscriber::set_global_default(SafeTimingSubscriber).is_ok());
    if !installed {
        eprintln!("haiderd: safe timing traces unavailable; a tracing subscriber already exists");
    }
}

struct SafeTimingSubscriber;

impl tracing::Subscriber for SafeTimingSubscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.is_event() && safe_target(metadata.target())
    }

    fn register_callsite(&self, metadata: &'static tracing::Metadata<'static>) -> Interest {
        if self.enabled(metadata) {
            Interest::always()
        } else {
            Interest::never()
        }
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::TRACE)
    }

    fn new_span(&self, _attributes: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let target = event.metadata().target();
        let mut fields = SafeFields::new(target);
        event.record(&mut fields);
        eprintln!(
            "haiderd: trace level={} target={target}{}",
            event.metadata().level(),
            fields.render()
        );
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

fn safe_target(target: &str) -> bool {
    matches!(target, STORE_TARGET | RECOVERY_TARGET)
}

struct SafeFields<'a> {
    target: &'a str,
    queue_wait_micros: Option<String>,
    operation_micros: Option<String>,
    phase: Option<String>,
    recovered_work: Option<String>,
    touched_sessions: Option<String>,
}

impl<'a> SafeFields<'a> {
    fn new(target: &'a str) -> Self {
        Self {
            target,
            queue_wait_micros: None,
            operation_micros: None,
            phase: None,
            recovered_work: None,
            touched_sessions: None,
        }
    }

    fn record_value(&mut self, field: &tracing::field::Field, value: String) {
        match (self.target, field.name()) {
            (STORE_TARGET, "queue_wait_micros") => self.queue_wait_micros = Some(value),
            (STORE_TARGET | RECOVERY_TARGET, "operation_micros") => {
                self.operation_micros = Some(value);
            }
            (RECOVERY_TARGET, "phase") => self.phase = Some(value),
            (RECOVERY_TARGET, "recovered_work") => self.recovered_work = Some(value),
            (RECOVERY_TARGET, "touched_sessions") => self.touched_sessions = Some(value),
            _ => {}
        }
    }

    fn render(&self) -> String {
        let mut rendered = String::new();
        for (name, value) in [
            ("phase", self.phase.as_deref()),
            ("queue_wait_micros", self.queue_wait_micros.as_deref()),
            ("operation_micros", self.operation_micros.as_deref()),
            ("recovered_work", self.recovered_work.as_deref()),
            ("touched_sessions", self.touched_sessions.as_deref()),
        ] {
            if let Some(value) = value {
                let _ = write!(rendered, " {name}={value}");
            }
        }
        rendered
    }
}

impl tracing::field::Visit for SafeFields<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if self.target == RECOVERY_TARGET
            && field.name() == "phase"
            && matches!(value, "effects" | "turns" | "login_receipts")
        {
            self.record_value(field, value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        if safe_numeric_field(self.target, field.name())
            && !rendered.is_empty()
            && rendered.bytes().all(|byte| byte.is_ascii_digit())
        {
            self.record_value(field, rendered);
        }
    }
}

fn safe_numeric_field(target: &str, field: &str) -> bool {
    matches!(
        (target, field),
        (STORE_TARGET, "queue_wait_micros" | "operation_micros")
            | (
                RECOVERY_TARGET,
                "operation_micros" | "recovered_work" | "touched_sessions"
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_allow_list_excludes_user_derived_fields_and_targets() {
        assert!(safe_numeric_field(STORE_TARGET, "queue_wait_micros"));
        assert!(!safe_numeric_field(RECOVERY_TARGET, "phase"));
        for field in ["prompt", "text", "token", "path", "error", "message"] {
            assert!(!safe_numeric_field(STORE_TARGET, field));
            assert!(!safe_numeric_field(RECOVERY_TARGET, field));
        }
        assert!(!safe_target("haider.hooks"));
        assert!(!safe_target("haider.peer"));
    }
}
