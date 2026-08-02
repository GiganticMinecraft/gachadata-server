use tracing::Subscriber;
use tracing_subscriber::registry::LookupSpan;

/// stdout ログを JSON にするかどうかを判定します。
///
/// `LOG_FORMAT` 環境変数 (`json` / `pretty`) が設定されていればそれに従い、
/// 未設定なら `ENV_NAME=local` (ローカル開発) のときだけ人間向けフォーマットにします。
/// `ENV_NAME` も未設定の場合は本番想定で JSON にします。
pub fn json_logs_enabled(env_name: Option<&str>, log_format: Option<&str>) -> bool {
    match log_format {
        Some(format) => format.eq_ignore_ascii_case("json"),
        None => env_name != Some("local"),
    }
}

/// stdout ログを 1 行 JSON で出力するレイヤーを作ります。
///
/// `tracing-subscriber` 標準の JSON フォーマッタは OTel の trace_id を出力できないため、
/// [`json_subscriber`] を使う。
///
/// - OTel の span コンテキストが有効な場合、`openTelemetry.traceId` / `openTelemetry.spanId`
///   フィールドが付く (Tempo の tracesToLogsV2 でトレース→ログ相関に使う。
///   このフィールド名は seichi_infra 側の Grafana/Loki 設定との契約であり、
///   変更する場合は両方直すこと)
/// - イベントのフィールドはトップレベルへフラットに出力される
///   (`panic=true` のような LogQL の `| json` パースを前提としたフィールドの契約を保つ)
/// - span のフィールドはスパン属性として Tempo 側へ送られるため、ログ行には出力しない
///
/// (seichi-portal-backend の logging.rs と同じ構成)
pub fn json_log_layer<S>() -> json_subscriber::fmt::Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    json_subscriber::layer()
        .flatten_event(true)
        .with_current_span(false)
        .with_span_list(false)
        .with_opentelemetry_ids(true)
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{Arc, Mutex},
    };

    use opentelemetry::trace::TracerProvider as _;
    use tracing::info;
    use tracing_subscriber::{fmt::MakeWriter, layer::SubscriberExt};

    use super::{json_log_layer, json_logs_enabled};

    #[test]
    fn json_logs_are_enabled_outside_local_unless_overridden() {
        assert!(json_logs_enabled(Some("production"), None));
        assert!(!json_logs_enabled(Some("local"), None));
        assert!(json_logs_enabled(None, None), "ENV_NAME 未設定は本番扱い");
        assert!(json_logs_enabled(Some("local"), Some("json")));
        assert!(!json_logs_enabled(Some("production"), Some("pretty")));
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Capture {
        type Writer = Capture;

        fn make_writer(&'a self) -> Capture {
            self.clone()
        }
    }

    fn captured_json(capture: &Capture) -> serde_json::Value {
        let bytes = capture.0.lock().unwrap();
        let text = std::str::from_utf8(&bytes).expect("log output must be valid UTF-8");
        serde_json::from_str(text.lines().next().expect("a log line must be written"))
            .expect("log line must be valid JSON")
    }

    #[test]
    fn formats_event_as_single_line_json() {
        let capture = Capture::default();
        let subscriber =
            tracing_subscriber::registry().with(json_log_layer().with_writer(capture.clone()));

        tracing::subscriber::with_default(subscriber, || {
            info!(dump_age_secs = 42, "hello");
        });

        let json = captured_json(&capture);
        assert_eq!(json["message"], "hello");
        assert_eq!(json["level"], "INFO");
        assert_eq!(
            json["dump_age_secs"], 42,
            "イベントフィールドはトップレベルへフラットに出力される"
        );
        assert!(json.get("timestamp").is_some());
        assert!(
            json.get("openTelemetry").is_none(),
            "OTel の span がなければ traceId は出力しない"
        );
    }

    #[test]
    fn injects_trace_and_span_id_inside_otel_span() {
        let capture = Capture::default();
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")))
            .with(json_log_layer().with_writer(capture.clone()));

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("request");
            let _guard = span.enter();
            info!("with trace");
        });

        let json = captured_json(&capture);
        let trace_id = json["openTelemetry"]["traceId"]
            .as_str()
            .expect("traceId must be present");
        assert_eq!(trace_id.len(), 32);
        assert!(trace_id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(trace_id, "0".repeat(32), "traceId must be valid (non-zero)");

        let span_id = json["openTelemetry"]["spanId"]
            .as_str()
            .expect("spanId must be present");
        assert_eq!(span_id.len(), 16);
    }
}
