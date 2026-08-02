mod logging;
mod panic_hook;
mod telemetry;

mod domain {
    use bytes::Bytes;
    use std::fmt::Debug;
    use std::time::SystemTime;

    #[derive(Debug, Clone, Default)]
    pub struct GachadataDump(pub Bytes);

    #[derive(Debug, Clone, Default)]
    pub struct GachadataDumpWithTime {
        pub dump: GachadataDump,
        pub dump_time: Option<SystemTime>,
    }

    #[async_trait::async_trait]
    pub trait GachaDataRepository: Debug + Sync + Send + 'static {
        async fn update_gachadata(&self) -> anyhow::Result<()>;
    }
}

mod infra_repository_impls {
    use crate::config::MySQL;
    use crate::domain::{GachaDataRepository, GachadataDump, GachadataDumpWithTime};
    use anyhow::anyhow;
    use bytes::Bytes;
    use std::ops::Sub;
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime};

    #[derive(Debug, Clone)]
    pub struct MySQLDumpConnection {
        pub connection_information: MySQL,
        pub dump: Arc<Mutex<GachadataDumpWithTime>>,
    }

    impl MySQLDumpConnection {
        #[tracing::instrument]
        pub async fn run_gachadata_dump(&self) -> anyhow::Result<()> {
            let MySQL {
                host: address,
                port,
                user,
                password,
            } = &self.connection_information;

            let output = Command::new("mariadb-dump")
                .args(vec![
                    "--host",
                    address,
                    "--port",
                    port.to_string().as_str(),
                    "--user",
                    user,
                    format!("-p{}", password).as_str(),
                    "seichiassist",
                    "gachadata",
                    "gacha_events",
                ])
                .output()?;

            if let Ok(mut dump) = self.dump.lock() {
                *dump = GachadataDumpWithTime {
                    dump: GachadataDump(Bytes::from(output.stdout)),
                    dump_time: Some(SystemTime::now()),
                }
            } else {
                return Err(anyhow!("Failed to lock gachadata dump."));
            }

            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl GachaDataRepository for MySQLDumpConnection {
        #[tracing::instrument]
        async fn update_gachadata(&self) -> anyhow::Result<()> {
            let is_after_more_than_quarter_hour = match self.dump.lock() {
                Ok(dump) => {
                    let quarter_hour = Duration::from_secs(900);
                    let dump_time = dump.dump_time;

                    let quarter_hour_from_now = SystemTime::now().sub(quarter_hour);

                    match dump_time {
                        Some(dump_time) => quarter_hour_from_now > dump_time,
                        None => true, // dump_timeがNoneになるのは起動して一度も取得されていないときのみ
                    }
                }
                _ => false,
            };

            // 最終dumpの取得から15分以上経過していればGachaDumpを更新する
            if is_after_more_than_quarter_hour {
                self.run_gachadata_dump().await?
            }

            Ok(())
        }
    }
}

mod presentation {
    use crate::domain::GachaDataRepository;
    use crate::infra_repository_impls::MySQLDumpConnection;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::{ErrorResponse, IntoResponse, Response, Result};

    #[tracing::instrument]
    pub async fn get_gachadata_handler(
        State(repository): State<MySQLDumpConnection>,
    ) -> Result<impl IntoResponse> {
        match repository.update_gachadata().await {
            Ok(_) => match repository.dump.lock() {
                Ok(gachadata_dump) if !gachadata_dump.dump.0.is_empty() => Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Disposition", "attachment; filename=gachadata.sql")
                    .header("Content-Type", "application/sql")
                    .body(gachadata_dump.dump.0.to_owned().into_response())
                    .unwrap()),
                Ok(_) => Err(ErrorResponse::from(
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "GachadataDump is empty. \
                        Please contact to administrators.",
                    )
                        .into_response(),
                )),
                Err(err) => {
                    tracing::error!("{}", err);
                    Err(ErrorResponse::from(
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "Failed to lock repository mutex.\
                             Please contact to administrators.",
                        )
                            .into_response(),
                    ))
                }
            },
            Err(err) => {
                tracing::error!("{}", err);
                Err(ErrorResponse::from(
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to update gachadata dump. \
                        Please contact to administrators.",
                    )
                        .into_response(),
                ))
            }
        }
    }
}

mod config {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    pub struct HttpPort {
        pub port: u16,
    }

    #[derive(Debug, Clone, Deserialize)]
    pub struct MySQL {
        pub host: String,
        pub port: u16,
        pub user: String,
        pub password: String,
    }

    pub struct Config {
        pub http_port: HttpPort,
        pub mysql: MySQL,
    }

    impl Config {
        pub async fn from_environment() -> anyhow::Result<Self> {
            let http_port = envy::prefixed("HTTP_").from_env::<HttpPort>()?;
            let mysql = envy::prefixed("MYSQL_").from_env::<MySQL>()?;

            Ok(Config { http_port, mysql })
        }
    }
}

#[tokio::main]
async fn main() {
    use crate::{
        config::Config, infra_repository_impls::MySQLDumpConnection,
        presentation::get_gachadata_handler,
    };
    use axum::{Router, routing::get};
    use axum_tracing_opentelemetry::middleware::{OtelAxumLayer, OtelInResponseLayer};
    use opentelemetry::trace::TracerProvider as _;
    use pyroscope::backend::{BackendConfig, PprofConfig, pprof_backend};
    use pyroscope::pyroscope::PyroscopeAgentBuilder;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;
    use tower_http::catch_panic::CatchPanicLayer;
    use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};

    // OTel トレーシング (OTLP http/protobuf)。
    // OTEL_EXPORTER_OTLP_ENDPOINT 未設定または OTEL_SDK_DISABLED=true なら無効
    let tracer_provider = telemetry::init_tracer_provider();

    // stdout ログ: 本番は 1 行 JSON (trace_id 注入付き)、ローカル (ENV_NAME=local) は
    // 人間向けフォーマット。LOG_FORMAT=json|pretty で明示上書き可
    let stdout_log_filter = || {
        tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        )
    };
    let json_logs_enabled = logging::json_logs_enabled(
        std::env::var("ENV_NAME").ok().as_deref(),
        std::env::var("LOG_FORMAT").ok().as_deref(),
    );
    let (json_log_layer, pretty_log_layer) = if json_logs_enabled {
        (
            Some(logging::json_log_layer().with_filter(stdout_log_filter())),
            None,
        )
    } else {
        (
            None,
            Some(tracing_subscriber::fmt::layer().with_filter(stdout_log_filter())),
        )
    };

    tracing_subscriber::registry()
        .with(tracer_provider.as_ref().map(|provider| {
            tracing_opentelemetry::layer().with_tracer(provider.tracer("gachadata-server"))
        }))
        .with(json_log_layer)
        .with(pretty_log_layer)
        .init();
    panic_hook::install();

    // 継続プロファイリング (Grafana Pyroscope への push)。
    // PYROSCOPE_SERVER_ADDRESS 未設定なら無効。起動失敗はサーバー本体を止めない。
    // agent はプロセスの生存期間中動かし続けるため束縛だけ保持する
    let _pyroscope_agent = std::env::var("PYROSCOPE_SERVER_ADDRESS")
        .ok()
        .and_then(|server_address| {
            // application 名は game-data-publisher と同じく env で上書き可能にする
            let application_name = std::env::var("PYROSCOPE_APPLICATION_NAME")
                .unwrap_or_else(|_| "gachadata-server".to_owned());
            let started = PyroscopeAgentBuilder::new(
                &server_address,
                &application_name,
                100,
                "pyroscope-rs",
                env!("CARGO_PKG_VERSION"),
                pprof_backend(PprofConfig::default(), BackendConfig::default()),
            )
            .build()
            .and_then(pyroscope::PyroscopeAgent::start);

            match started {
                Ok(agent) => Some(agent),
                Err(error) => {
                    tracing::warn!(%error, "Pyroscope agent の起動に失敗したため、プロファイルなしで続行します");
                    None
                }
            }
        });

    let config = Config::from_environment()
        .await
        .expect("Failed to load config from environment variables.");

    let mysql_dump_connection = MySQLDumpConnection {
        connection_information: config.mysql,
        dump: Arc::new(Mutex::default()),
    };

    let router = Router::new()
        .route("/", get(get_gachadata_handler))
        .with_state(mysql_dump_connection)
        // handler 内 panic で 500 を返し、コネクションを維持する
        // (panic 自体は panic_hook が panic=true 付きでログに残す)
        .layer(CatchPanicLayer::new())
        // レスポンスヘッダーへの trace context 挿入 (OtelAxumLayer より内側に置く)
        .layer(OtelInResponseLayer)
        // リクエストごとの OTel サーバースパン開始
        .layer(OtelAxumLayer::default());

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], config.http_port.port));

    tracing::info!("Listening on {}", config.http_port.port);

    let listener = TcpListener::bind(addr).await.unwrap();

    axum::serve(listener, router).await.unwrap();

    // serve が戻るのはシャットダウン時のみ。バッファ済みスパンを flush する
    if let Some(provider) = tracer_provider {
        let _ = provider.shutdown();
    }
}
