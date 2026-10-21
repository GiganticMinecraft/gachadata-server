use opentelemetry::{Key, KeyValue, global};
use opentelemetry_otlp::{MetricExporter, Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::{
    Resource, metrics::SdkMeterProvider, propagation::TraceContextPropagator,
    trace::SdkTracerProvider,
};

const SERVICE_NAME: &str = "gachadata-server";

/// `service.version` に使う値を渡す環境変数。
/// コンテナイメージではビルド時の git commit SHA が入る (Dockerfile の `GIT_SHA`)
const VERSION_ENV: &str = "GACHADATA_VERSION";

/// OTel の tracer / meter provider。終了時に [`Providers::shutdown`] で flush する
pub struct Providers {
    pub tracer: SdkTracerProvider,
    pub meter: SdkMeterProvider,
}

impl Providers {
    /// 未送信のスパン・メトリクスを flush して停止し、(tracer, meter) それぞれの結果を返す。
    /// それぞれ exporter のタイムアウトまでかかりうるため、並行して待つ
    pub fn shutdown(self) -> (OTelSdkResult, OTelSdkResult) {
        let Self { tracer, meter } = self;
        std::thread::scope(|scope| {
            let tracer = scope.spawn(move || tracer.shutdown());
            let meter = meter.shutdown();
            (
                tracer.join().expect("tracer shutdown thread panicked"),
                meter,
            )
        })
    }
}

/// `service.version` の値。環境変数が空または未設定ならクレートのバージョン
fn service_version(env_value: Option<&str>) -> String {
    env_value
        .filter(|value| !value.is_empty())
        .unwrap_or(env!("CARGO_PKG_VERSION"))
        .to_owned()
}

/// トレース・メトリクス共通の resource。
///
/// Resource::builder() は OTEL_SERVICE_NAME / OTEL_RESOURCE_ATTRIBUTES を
/// 自動で読むため、service.name / service.version はそれらで指定されていない場合だけ
/// デフォルト値を与える (指定がなければ SDK は service.name を `unknown_service:<exe>` にする)
fn resource() -> Resource {
    let detected = Resource::builder().build();
    let mut defaults = Vec::new();
    let service_name_detected = detected
        .get(&Key::from_static_str("service.name"))
        .is_some_and(|name| !name.as_str().starts_with("unknown_service"));
    if !service_name_detected {
        defaults.push(KeyValue::new("service.name", SERVICE_NAME));
    }
    if detected
        .get(&Key::from_static_str("service.version"))
        .is_none()
    {
        let version = service_version(std::env::var(VERSION_ENV).ok().as_deref());
        defaults.push(KeyValue::new("service.version", version));
    }
    if defaults.is_empty() {
        return detected;
    }
    // 検出をやり直さず、検出済みの属性にデフォルト値を足す
    Resource::builder_empty()
        .with_attributes(
            detected
                .iter()
                .map(|(key, value)| KeyValue::new(key.clone(), value.clone())),
        )
        .with_attributes(defaults)
        .build()
}

/// OpenTelemetry のトレーシングとメトリクスを初期化します。
///
/// `OTEL_SDK_DISABLED=true` または `OTEL_EXPORTER_OTLP_ENDPOINT` 未設定の場合は
/// 初期化をスキップして `None` を返します
/// (`OTEL_SDK_DISABLED` は Rust SDK 未実装のため自前でゲートしています)。
/// このとき global の meter は no-op のままなので、メトリクスも記録されません。
///
/// エクスポートは OTLP http/protobuf で、endpoint やその他の設定
/// (`OTEL_METRIC_EXPORT_INTERVAL` 等) は `OTEL_*` 環境変数から自動で読み込まれます。
/// (seichi-portal-backend の telemetry.rs と同じ構成)
pub fn init_providers() -> Option<Providers> {
    let sdk_disabled =
        std::env::var("OTEL_SDK_DISABLED").is_ok_and(|value| value.eq_ignore_ascii_case("true"));
    let endpoint_configured =
        std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok_and(|value| !value.is_empty());

    if sdk_disabled || !endpoint_configured {
        return None;
    }

    global::set_text_map_propagator(TraceContextPropagator::new());

    let resource = resource();

    let span_exporter = SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .build()
        .expect("failed to build OTLP span exporter");
    let tracer = SdkTracerProvider::builder()
        .with_batch_exporter(span_exporter)
        .with_resource(resource.clone())
        .build();
    global::set_tracer_provider(tracer.clone());

    let metric_exporter = MetricExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .build()
        .expect("failed to build OTLP metric exporter");
    // PeriodicReader の間隔は OTEL_METRIC_EXPORT_INTERVAL (ミリ秒、デフォルト 60s)
    let meter = SdkMeterProvider::builder()
        .with_periodic_exporter(metric_exporter)
        .with_resource(resource)
        .build();
    global::set_meter_provider(meter.clone());

    Some(Providers { tracer, meter })
}

#[cfg(test)]
mod tests {
    use super::{VERSION_ENV, init_providers, resource, service_version};
    use opentelemetry::Key;

    #[test]
    fn service_version_falls_back_to_crate_version() {
        assert_eq!(service_version(Some("0123abc")), "0123abc");
        assert_eq!(service_version(Some("")), env!("CARGO_PKG_VERSION"));
        assert_eq!(service_version(None), env!("CARGO_PKG_VERSION"));
    }

    fn resource_value(key: &'static str) -> Option<String> {
        resource()
            .get(&Key::from_static_str(key))
            .map(|value| value.to_string())
    }

    /// 環境変数の設定はプロセス全体に影響するため、
    /// 競合しないよう 1 つのテストで順に検証する。
    #[test]
    fn providers_and_resource_follow_environment_variables() {
        // SAFETY: このテストバイナリ内で環境変数を読み書きするのはこのテストだけ
        unsafe {
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
            std::env::remove_var("OTEL_SDK_DISABLED");
            std::env::remove_var("OTEL_SERVICE_NAME");
            std::env::remove_var("OTEL_RESOURCE_ATTRIBUTES");
            std::env::set_var(VERSION_ENV, "deadbeef");
        }
        assert_eq!(
            resource_value("service.name").as_deref(),
            Some("gachadata-server")
        );
        assert_eq!(
            resource_value("service.version").as_deref(),
            Some("deadbeef")
        );

        unsafe {
            std::env::set_var(
                "OTEL_RESOURCE_ATTRIBUTES",
                "service.name=from-attrs-name,service.version=from-attrs",
            );
        }
        assert_eq!(
            resource_value("service.version").as_deref(),
            Some("from-attrs"),
            "OTEL_RESOURCE_ATTRIBUTES の service.version を優先する"
        );
        assert_eq!(
            resource_value("service.name").as_deref(),
            Some("from-attrs-name"),
            "OTEL_RESOURCE_ATTRIBUTES の service.name を優先する"
        );

        unsafe {
            std::env::remove_var("OTEL_RESOURCE_ATTRIBUTES");
            std::env::remove_var(VERSION_ENV);
        }

        assert!(
            init_providers().is_none(),
            "OTEL_EXPORTER_OTLP_ENDPOINT 未設定なら初期化をスキップする"
        );

        unsafe {
            std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://localhost:4318");
            std::env::set_var("OTEL_SDK_DISABLED", "true");
        }
        assert!(
            init_providers().is_none(),
            "OTEL_SDK_DISABLED=true なら endpoint が設定されていてもスキップする"
        );

        unsafe {
            std::env::remove_var("OTEL_SDK_DISABLED");
        }
        let providers = init_providers();
        assert!(
            providers.is_some(),
            "endpoint 設定時は tracer / meter provider が初期化される"
        );

        if let Some(providers) = providers {
            let (tracer, meter) = providers.shutdown();
            tracer.expect("tracer provider must shut down cleanly");
            meter.expect("meter provider must shut down cleanly");
        }
        unsafe {
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        }
    }
}
