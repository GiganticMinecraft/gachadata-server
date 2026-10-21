//! OTel メトリクスの instrument 定義。
//!
//! instrument は [`opentelemetry::global::meter`] から作るため、
//! [`crate::telemetry::init_providers`] で meter provider を登録した後に作ること
//! (登録前に作った instrument は no-op のままになる)。
//! OTel が無効 (endpoint 未設定 / `OTEL_SDK_DISABLED=true`) の場合も no-op になる。

use axum::extract::{MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter, ObservableGauge};
use std::time::{Duration, Instant, SystemTime};

const METER_NAME: &str = "gachadata-server";

/// OTel semantic conventions の `http.server.request.duration` 推奨バケット (秒)
const HTTP_DURATION_BOUNDARIES: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

/// mariadb-dump の所要時間のバケット (秒)。上限は DUMP_TIMEOUT (120s)
const DUMP_DURATION_BOUNDARIES: [f64; 11] =
    [0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0];

pub fn meter() -> Meter {
    opentelemetry::global::meter(METER_NAME)
}

/// `http.server.request.duration` (OTel semantic conventions) を記録する axum middleware 用の状態
#[derive(Clone, Debug)]
pub struct HttpServerMetrics {
    duration: Histogram<f64>,
}

impl HttpServerMetrics {
    pub fn new(meter: &Meter) -> Self {
        Self {
            duration: meter
                .f64_histogram("http.server.request.duration")
                .with_unit("s")
                .with_description("Duration of HTTP server requests.")
                .with_boundaries(HTTP_DURATION_BOUNDARIES.to_vec())
                .build(),
        }
    }
}

/// semconv に従い、既知のメソッド以外は `_OTHER` にまとめる (任意の文字列で系列が増えるのを防ぐ)
fn normalized_method(method: &axum::http::Method) -> &'static str {
    use axum::http::Method;
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        Method::CONNECT => "CONNECT",
        Method::OPTIONS => "OPTIONS",
        Method::TRACE => "TRACE",
        Method::PATCH => "PATCH",
        _ => "_OTHER",
    }
}

/// リクエストの処理時間を `http.server.request.duration` に記録する middleware。
/// どの route にもマッチしなかったリクエストには `http.route` を付けない
/// (生のパスを属性にすると系列数が際限なく増えるため)
pub async fn record_http_server_duration(
    State(metrics): State<HttpServerMetrics>,
    request: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let method = normalized_method(request.method());
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned());

    let response = next.run(request).await;

    let mut attributes = vec![
        KeyValue::new("http.request.method", method),
        KeyValue::new(
            "http.response.status_code",
            i64::from(response.status().as_u16()),
        ),
    ];
    if let Some(route) = route {
        attributes.push(KeyValue::new("http.route", route));
    }
    metrics
        .duration
        .record(started.elapsed().as_secs_f64(), &attributes);
    response
}

/// mariadb-dump の失敗理由 (`gachadata.dump.failures` の `reason` 属性)
#[derive(Clone, Copy, Debug)]
pub enum DumpFailureReason {
    Spawn,
    Timeout,
    ExitStatus,
    EmptyOutput,
}

impl DumpFailureReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Timeout => "timeout",
            Self::ExitStatus => "exit_status",
            Self::EmptyOutput => "empty_output",
        }
    }
}

/// mariadb-dump の実行 1 回ごとに記録する instrument
#[derive(Clone, Debug)]
pub struct DumpMetrics {
    duration: Histogram<f64>,
    failures: Counter<u64>,
}

impl DumpMetrics {
    pub fn new(meter: &Meter) -> Self {
        Self {
            duration: meter
                .f64_histogram("gachadata.dump.duration")
                .with_unit("s")
                .with_description("Duration of mariadb-dump runs.")
                .with_boundaries(DUMP_DURATION_BOUNDARIES.to_vec())
                .build(),
            failures: meter
                .u64_counter("gachadata.dump.failures")
                .with_unit("{failure}")
                .with_description("Number of failed mariadb-dump runs.")
                .build(),
        }
    }

    pub fn record_success(&self, elapsed: Duration) {
        self.duration.record(
            elapsed.as_secs_f64(),
            &[KeyValue::new("outcome", "success")],
        );
    }

    pub fn record_failure(&self, elapsed: Duration, reason: DumpFailureReason) {
        self.duration.record(
            elapsed.as_secs_f64(),
            &[KeyValue::new("outcome", "failure")],
        );
        self.failures
            .add(1, &[KeyValue::new("reason", reason.as_str())]);
    }
}

/// キャッシュ中の dump の状態を収集のたびに返す observable gauge。
/// 返り値の handle はプロセスの生存期間中保持すること
pub fn register_dump_cache_gauges<F>(
    meter: &Meter,
    process_started: SystemTime,
    cache_state: F,
) -> (ObservableGauge<u64>, ObservableGauge<f64>)
where
    // (最後に dump に成功した時刻, その dump のバイト数)。一度も成功していなければ None
    F: Fn() -> Option<(SystemTime, usize)> + Clone + Send + Sync + 'static,
{
    let size_state = cache_state.clone();
    let size = meter
        .u64_observable_gauge("gachadata.dump.size_bytes")
        .with_unit("By")
        .with_description("Size of the cached (last successful) gachadata dump.")
        .with_callback(move |observer| {
            if let Some((_, len)) = size_state() {
                observer.observe(len as u64, &[]);
            }
        })
        .build();

    let age = meter
        .f64_observable_gauge("gachadata.dump.last_success_age_seconds")
        .with_unit("s")
        .with_description(
            "Seconds since the last successful mariadb-dump \
             (since process start if it has never succeeded).",
        )
        .with_callback(move |observer| {
            // 一度も成功していない場合も起動からの経過で増え続けるようにし、
            // 「起動後ずっと dump できていない」状態も同じ閾値でアラートできるようにする
            let since = cache_state().map_or(process_started, |(dump_time, _)| dump_time);
            // 時計が巻き戻った場合は 0 とする
            let age = since.elapsed().unwrap_or_default();
            observer.observe(age.as_secs_f64(), &[]);
        })
        .build();

    (size, age)
}

#[cfg(test)]
pub mod test_support {
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::data::{
        AggregatedMetrics, HistogramDataPoint, Metric, MetricData, ResourceMetrics,
    };
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    /// テスト用の in-memory meter provider
    pub struct TestMeter {
        pub provider: SdkMeterProvider,
        exporter: InMemoryMetricExporter,
    }

    impl TestMeter {
        pub fn new() -> Self {
            let exporter = InMemoryMetricExporter::default();
            let provider = SdkMeterProvider::builder()
                .with_reader(PeriodicReader::builder(exporter.clone()).build())
                .build();
            Self { provider, exporter }
        }

        pub fn meter(&self) -> opentelemetry::metrics::Meter {
            self.provider.meter("test")
        }

        /// 収集してエクスポートされた最新のメトリクスを返す
        pub fn collect(&self) -> Vec<ResourceMetrics> {
            self.provider.force_flush().expect("flush must succeed");
            self.exporter
                .get_finished_metrics()
                .expect("metrics must be exported")
        }
    }

    pub fn find_metric<'a>(metrics: &'a [ResourceMetrics], name: &str) -> Option<&'a Metric> {
        metrics
            .iter()
            .rev()
            .flat_map(|rm| rm.scope_metrics())
            .flat_map(|sm| sm.metrics())
            .find(|m| m.name() == name)
    }

    pub fn f64_histogram_points(metric: &Metric) -> Vec<&HistogramDataPoint<f64>> {
        match metric.data() {
            AggregatedMetrics::F64(MetricData::Histogram(histogram)) => {
                histogram.data_points().collect()
            }
            _ => panic!("{} must be an f64 histogram", metric.name()),
        }
    }

    pub fn u64_sum_total(metric: &Metric) -> u64 {
        match metric.data() {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                sum.data_points().map(|point| point.value()).sum()
            }
            _ => panic!("{} must be a u64 sum", metric.name()),
        }
    }

    pub fn f64_gauge_value(metric: &Metric) -> f64 {
        match metric.data() {
            AggregatedMetrics::F64(MetricData::Gauge(gauge)) => gauge
                .data_points()
                .next()
                .expect("gauge must have a data point")
                .value(),
            _ => panic!("{} must be an f64 gauge", metric.name()),
        }
    }

    pub fn u64_gauge_value(metric: &Metric) -> Option<u64> {
        match metric.data() {
            AggregatedMetrics::U64(MetricData::Gauge(gauge)) => {
                gauge.data_points().next().map(|point| point.value())
            }
            _ => panic!("{} must be a u64 gauge", metric.name()),
        }
    }

    pub fn attribute(point: &HistogramDataPoint<f64>, key: &str) -> Option<String> {
        point
            .attributes()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::routing::get;
    use tower::ServiceExt;

    #[tokio::test]
    async fn http_duration_has_route_method_and_status() {
        let test_meter = TestMeter::new();
        let metrics = HttpServerMetrics::new(&test_meter.meter());
        let router = Router::new().route("/", get(|| async { "ok" })).layer(
            axum::middleware::from_fn_with_state(metrics, record_http_server_duration),
        );

        for uri in ["/", "/no-such-path"] {
            let response = router
                .clone()
                .oneshot(Request::get(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let _ = response.status();
        }

        let exported = test_meter.collect();
        let metric = find_metric(&exported, "http.server.request.duration")
            .expect("http.server.request.duration must be exported");
        let points = f64_histogram_points(metric);
        assert_eq!(points.len(), 2);

        let ok = points
            .iter()
            .find(|p| attribute(p, "http.response.status_code").as_deref() == Some("200"))
            .expect("200 の系列がある");
        assert_eq!(attribute(ok, "http.route").as_deref(), Some("/"));
        assert_eq!(attribute(ok, "http.request.method").as_deref(), Some("GET"));
        assert_eq!(ok.count(), 1);

        let not_found = points
            .iter()
            .find(|p| {
                attribute(p, "http.response.status_code").as_deref()
                    == Some(&StatusCode::NOT_FOUND.as_u16().to_string())
            })
            .expect("404 の系列がある");
        assert_eq!(
            attribute(not_found, "http.route"),
            None,
            "マッチしないパスは http.route を付けない"
        );
    }

    #[test]
    fn unknown_method_is_normalized() {
        let method = axum::http::Method::from_bytes(b"FOO").unwrap();
        assert_eq!(normalized_method(&method), "_OTHER");
        assert_eq!(normalized_method(&axum::http::Method::GET), "GET");
    }

    #[test]
    fn cache_gauges_report_size_and_age() {
        let test_meter = TestMeter::new();
        let meter = test_meter.meter();
        let started = SystemTime::now() - Duration::from_secs(100);

        // 一度も成功していない: size は出さず、age は起動からの経過
        let _never = register_dump_cache_gauges(&meter, started, || None);
        let exported = test_meter.collect();
        let size = find_metric(&exported, "gachadata.dump.size_bytes").map(u64_gauge_value);
        assert!(matches!(size, None | Some(None)));
        let age = f64_gauge_value(
            find_metric(&exported, "gachadata.dump.last_success_age_seconds").unwrap(),
        );
        assert!((100.0..110.0).contains(&age), "age = {age}");

        // 成功済み: size と最終成功からの経過
        let test_meter = TestMeter::new();
        let dump_time = SystemTime::now() - Duration::from_secs(5);
        let _ok = register_dump_cache_gauges(&test_meter.meter(), started, move || {
            Some((dump_time, 1234))
        });
        let exported = test_meter.collect();
        assert_eq!(
            u64_gauge_value(find_metric(&exported, "gachadata.dump.size_bytes").unwrap()),
            Some(1234)
        );
        let age = f64_gauge_value(
            find_metric(&exported, "gachadata.dump.last_success_age_seconds").unwrap(),
        );
        assert!((5.0..15.0).contains(&age), "age = {age}");
    }
}
