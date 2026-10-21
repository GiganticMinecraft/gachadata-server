mod logging;
mod panic_hook;
mod telemetry;

mod domain {
    use bytes::Bytes;
    use std::fmt::Debug;
    use std::time::SystemTime;

    #[derive(Clone, Default)]
    pub struct GachadataDump(pub Bytes);

    // dump の中身 (SQL 全文) を Debug 出力に含めない
    impl Debug for GachadataDump {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("GachadataDump")
                .field("len_bytes", &self.0.len())
                .finish()
        }
    }

    #[derive(Debug, Clone, Default)]
    pub struct GachadataDumpWithTime {
        pub dump: GachadataDump,
        pub dump_time: Option<SystemTime>,
    }

    #[async_trait::async_trait]
    pub trait GachaDataRepository: Debug + Sync + Send + 'static {
        /// キャッシュが古ければ dump を取り直す。
        /// 失敗してもキャッシュ (前回の正常な dump) は変更しない
        async fn update_gachadata(&self) -> anyhow::Result<()>;

        /// 最後に正常に取得できた dump。一度も成功していなければ None
        fn cached_gachadata(&self) -> Option<GachadataDump>;
    }
}

mod infra_repository_impls {
    use crate::config::MySQL;
    use crate::domain::{GachaDataRepository, GachadataDump, GachadataDumpWithTime};
    use anyhow::{Context, bail};
    use bytes::Bytes;
    use std::ffi::OsString;
    use std::process::Stdio;
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::{Duration, Instant, SystemTime};
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    /// キャッシュの有効期間。最終 dump の取得からこれ以上経過していれば取り直す
    const DUMP_CACHE_TTL: Duration = Duration::from_secs(900);

    /// mariadb-dump 1 回あたりの上限時間。超えたらプロセスを kill して失敗扱いにする
    const DUMP_TIMEOUT: Duration = Duration::from_secs(120);

    /// ログ・span に載せる stderr の上限 (バイト)
    const MAX_STDERR_LOG_BYTES: usize = 4096;

    #[derive(Debug, Clone)]
    pub struct MySQLDumpConnection {
        pub connection_information: MySQL,
        dump: Arc<Mutex<GachadataDumpWithTime>>,
        /// 更新処理の直列化用。中身は直近の dump 試行 (成否問わず) が終わった時刻
        refresh_lock: Arc<tokio::sync::Mutex<Option<Instant>>>,
        program: OsString,
        dump_timeout: Duration,
    }

    impl MySQLDumpConnection {
        pub fn new(connection_information: MySQL) -> Self {
            Self::with_program(connection_information, "mariadb-dump", DUMP_TIMEOUT)
        }

        fn with_program(
            connection_information: MySQL,
            program: impl Into<OsString>,
            dump_timeout: Duration,
        ) -> Self {
            Self {
                connection_information,
                dump: Arc::default(),
                refresh_lock: Arc::default(),
                program: program.into(),
                dump_timeout,
            }
        }

        fn cache(&self) -> std::sync::MutexGuard<'_, GachadataDumpWithTime> {
            // ロック中に panic する処理は無いので、poison されていても中身はそのまま使える
            self.dump.lock().unwrap_or_else(PoisonError::into_inner)
        }

        fn is_cache_stale(&self) -> bool {
            match self.cache().dump_time {
                // 時計が巻き戻って elapsed が Err になった場合も取り直す
                Some(dump_time) => dump_time
                    .elapsed()
                    .map_or(true, |elapsed| elapsed >= DUMP_CACHE_TTL),
                None => true, // 一度も dump に成功していない
            }
        }

        /// mariadb-dump のコマンドを組み立てる。
        ///
        /// パスワードは argv (/proc/<pid>/cmdline で誰でも読める) には載せず、
        /// stdin から読ませる option file (`--defaults-extra-file=/dev/stdin`) で渡す。
        /// MYSQL_PWD 環境変数だと mariadb-dump が「パスワード無しログイン」とみなして
        /// `--ssl-verify-server-cert` を無効にしてしまうため使わない
        fn dump_command(&self) -> Command {
            let MySQL {
                host, port, user, ..
            } = &self.connection_information;

            let mut command = Command::new(&self.program);
            command
                // --defaults-extra-file は先頭の引数でなければならない
                .arg("--defaults-extra-file=/dev/stdin")
                .arg("--host")
                .arg(host)
                .arg("--port")
                .arg(port.to_string())
                .arg("--user")
                .arg(user)
                .args(["seichiassist", "gachadata", "gacha_events"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                // timeout で future を drop したときに子プロセスを kill する
                .kill_on_drop(true);
            command
        }

        /// stdin で渡す option file の中身。
        /// 値はダブルクォートで囲み、option file のエスケープ規則に従って
        /// `\` と `"` と改行をエスケープする
        fn client_option_file(&self) -> String {
            let mut escaped = String::new();
            for c in self.connection_information.password.chars() {
                match c {
                    '\\' => escaped.push_str("\\\\"),
                    '"' => escaped.push_str("\\\""),
                    '\n' => escaped.push_str("\\n"),
                    '\r' => escaped.push_str("\\r"),
                    c => escaped.push(c),
                }
            }
            format!("[client]\npassword=\"{escaped}\"\n")
        }

        /// mariadb-dump を起動し、option file を stdin に書き込んで終了を待つ
        async fn spawn_dump(&self) -> std::io::Result<std::process::Output> {
            let mut child = self.dump_command().spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                // 書き込みに失敗した場合 (子プロセスが先に終了した等) も、
                // 終了ステータスと stderr で失敗として扱われるので無視してよい
                let _ = stdin.write_all(self.client_option_file().as_bytes()).await;
                // drop で stdin を閉じ、mariadb-dump に EOF を伝える
            }
            child.wait_with_output().await
        }

        /// stderr をログ用に整形する。上限で切り詰め、念のためパスワードを伏せる
        fn sanitize_stderr(&self, stderr: &[u8]) -> String {
            let truncated = &stderr[..stderr.len().min(MAX_STDERR_LOG_BYTES)];
            let text = String::from_utf8_lossy(truncated);
            let text = text.trim();
            let password = &self.connection_information.password;
            if password.is_empty() {
                text.to_owned()
            } else {
                text.replace(password.as_str(), "<redacted>")
            }
        }

        // self を skip しないと Debug 経由でキャッシュ済み dump などが
        // span 属性としてトレース基盤へ送られる
        #[tracing::instrument(skip(self))]
        async fn run_gachadata_dump(&self) -> anyhow::Result<Bytes> {
            let started = Instant::now();
            let output = match tokio::time::timeout(self.dump_timeout, self.spawn_dump()).await {
                Ok(Ok(output)) => output,
                Ok(Err(error)) => {
                    tracing::error!(%error, "failed to spawn mariadb-dump");
                    return Err(error).context("failed to spawn mariadb-dump");
                }
                Err(_) => {
                    tracing::error!(
                        timeout_secs = self.dump_timeout.as_secs(),
                        "mariadb-dump timed out"
                    );
                    bail!(
                        "mariadb-dump timed out after {}s",
                        self.dump_timeout.as_secs_f64()
                    );
                }
            };

            let stderr = self.sanitize_stderr(&output.stderr);
            if !output.status.success() {
                tracing::error!(
                    exit_status = %output.status,
                    stderr = %stderr,
                    "mariadb-dump exited with failure"
                );
                bail!("mariadb-dump exited with {}: {}", output.status, stderr);
            }
            if output.stdout.is_empty() {
                tracing::error!(stderr = %stderr, "mariadb-dump succeeded but produced no output");
                bail!("mariadb-dump produced no output: {}", stderr);
            }
            if !stderr.is_empty() {
                tracing::warn!(stderr = %stderr, "mariadb-dump wrote to stderr");
            }

            tracing::info!(
                len_bytes = output.stdout.len(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "mariadb-dump finished"
            );
            Ok(Bytes::from(output.stdout))
        }
    }

    #[async_trait::async_trait]
    impl GachaDataRepository for MySQLDumpConnection {
        // skip(self): run_gachadata_dump と同じ理由
        #[tracing::instrument(skip(self))]
        async fn update_gachadata(&self) -> anyhow::Result<()> {
            if !self.is_cache_stale() {
                return Ok(());
            }

            let requested_at = Instant::now();
            let mut last_attempt_finished = self.refresh_lock.lock().await;

            // ロック待ちの間に他のリクエストが dump を試行していれば、その結果を使う。
            // 成功していればキャッシュが新しくなっており、失敗していれば直後に
            // 取り直しても同じく失敗する可能性が高いので、待っていた全員で
            // 再実行するのを避ける (次に来たリクエストで再試行される)
            if last_attempt_finished.is_some_and(|finished| finished >= requested_at)
                || !self.is_cache_stale()
            {
                return Ok(());
            }

            let result = self.run_gachadata_dump().await;
            *last_attempt_finished = Some(Instant::now());

            // 失敗時は前回の正常なキャッシュと dump_time をそのまま残す
            let dump = result?;
            *self.cache() = GachadataDumpWithTime {
                dump: GachadataDump(dump),
                dump_time: Some(SystemTime::now()),
            };

            Ok(())
        }

        fn cached_gachadata(&self) -> Option<GachadataDump> {
            let cache = self.cache();
            cache.dump_time.map(|_| cache.dump.clone())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;

        fn mysql() -> MySQL {
            MySQL {
                host: "db.example".to_owned(),
                port: 3306,
                user: "gacha".to_owned(),
                password: "s3cr3t-pass".to_owned(),
            }
        }

        /// テスト用に mariadb-dump の代わりに実行するシェルスクリプトを作る
        fn script(name: &str, body: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "gachadata-server-test-{}-{}",
                std::process::id(),
                name
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("fake-dump");
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        fn connection(program: impl Into<OsString>) -> MySQLDumpConnection {
            MySQLDumpConnection::with_program(mysql(), program, Duration::from_secs(10))
        }

        fn set_cache(connection: &MySQLDumpConnection, body: &'static str, age: Duration) {
            *connection.cache() = GachadataDumpWithTime {
                dump: GachadataDump(Bytes::from_static(body.as_bytes())),
                dump_time: Some(SystemTime::now() - age),
            };
        }

        #[test]
        fn password_is_not_passed_via_argv_or_env() {
            let command = MySQLDumpConnection::new(mysql()).dump_command();
            let command = command.as_std();

            let args: Vec<_> = command.get_args().map(|a| a.to_str().unwrap()).collect();
            assert_eq!(
                args,
                [
                    "--defaults-extra-file=/dev/stdin",
                    "--host",
                    "db.example",
                    "--port",
                    "3306",
                    "--user",
                    "gacha",
                    "seichiassist",
                    "gachadata",
                    "gacha_events",
                ]
            );
            assert!(args.iter().all(|a| !a.contains("s3cr3t-pass")));
            assert!(
                command
                    .get_envs()
                    .all(|(_, value)| value.is_none_or(|v| v.to_str() != Some("s3cr3t-pass")))
            );
        }

        #[test]
        fn option_file_escapes_password() {
            let mut mysql = mysql();
            mysql.password = r#"a"b\c#d'e"#.to_owned() + "\n";
            let connection = MySQLDumpConnection::new(mysql);

            assert_eq!(
                connection.client_option_file(),
                "[client]\npassword=\"a\\\"b\\\\c#d'e\\n\"\n"
            );
        }

        #[tokio::test]
        async fn successful_dump_is_cached() {
            let connection = connection(script("ok", "cat; echo '-- dump'"));

            connection.update_gachadata().await.unwrap();

            let cached = connection.cached_gachadata().expect("cache must be filled");
            assert_eq!(
                &cached.0[..],
                b"[client]\npassword=\"s3cr3t-pass\"\n-- dump\n",
                "option file が stdin で渡される"
            );
        }

        #[tokio::test]
        async fn failure_without_cache_leaves_cache_empty() {
            let connection = connection(script("fail-empty", "echo 'access denied' >&2; exit 2"));

            let error = connection.update_gachadata().await.unwrap_err();

            assert!(format!("{error:#}").contains("access denied"));
            assert!(connection.cached_gachadata().is_none());
        }

        #[tokio::test]
        async fn failure_keeps_previous_cache_and_dump_time() {
            let connection = connection(script("fail-keep", "echo partial; exit 2"));
            set_cache(&connection, "-- previous", DUMP_CACHE_TTL * 2);
            let previous_time = connection.cache().dump_time;

            assert!(connection.update_gachadata().await.is_err());

            assert_eq!(
                &connection.cached_gachadata().unwrap().0[..],
                b"-- previous"
            );
            assert_eq!(connection.cache().dump_time, previous_time);
        }

        #[tokio::test]
        async fn empty_output_is_failure() {
            let connection = connection(script("empty", "exit 0"));
            set_cache(&connection, "-- previous", DUMP_CACHE_TTL * 2);

            assert!(connection.update_gachadata().await.is_err());
            assert_eq!(
                &connection.cached_gachadata().unwrap().0[..],
                b"-- previous"
            );
        }

        #[tokio::test]
        async fn timeout_is_failure() {
            let mut connection = connection(script("timeout", "sleep 10; echo late"));
            connection.dump_timeout = Duration::from_millis(200);

            let error = connection.update_gachadata().await.unwrap_err();

            assert!(format!("{error:#}").contains("timed out"));
            assert!(connection.cached_gachadata().is_none());
        }

        #[tokio::test]
        async fn spawn_failure_is_failure() {
            let connection = connection("/nonexistent/mariadb-dump");

            assert!(connection.update_gachadata().await.is_err());
            assert!(connection.cached_gachadata().is_none());
        }

        #[tokio::test]
        async fn fresh_cache_is_not_refreshed() {
            let connection = connection("/nonexistent/mariadb-dump");
            set_cache(&connection, "-- fresh", Duration::from_secs(1));

            connection.update_gachadata().await.unwrap();
            assert_eq!(&connection.cached_gachadata().unwrap().0[..], b"-- fresh");
        }

        #[tokio::test]
        async fn concurrent_requests_run_dump_once() {
            let counter = script("concurrent", "").with_file_name("count");
            let program = script(
                "concurrent",
                &format!(
                    "echo x >> '{}'; sleep 0.3; echo '-- dump'",
                    counter.display()
                ),
            );
            let _ = std::fs::remove_file(&counter);
            let connection = connection(program);

            let handles: Vec<_> = (0..5)
                .map(|_| {
                    let connection = connection.clone();
                    tokio::spawn(async move { connection.update_gachadata().await })
                })
                .collect();
            for handle in handles {
                handle.await.unwrap().unwrap();
            }

            let runs = std::fs::read_to_string(&counter).unwrap().lines().count();
            assert_eq!(runs, 1);
        }

        #[tokio::test]
        async fn concurrent_failures_run_dump_once() {
            let counter = script("concurrent-fail", "").with_file_name("count");
            let program = script(
                "concurrent-fail",
                &format!("echo x >> '{}'; sleep 0.3; exit 1", counter.display()),
            );
            let _ = std::fs::remove_file(&counter);
            let connection = connection(program);

            let handles: Vec<_> = (0..5)
                .map(|_| {
                    let connection = connection.clone();
                    tokio::spawn(async move { connection.update_gachadata().await })
                })
                .collect();
            for handle in handles {
                // 1 件が dump の失敗を返し、待っていた残りは再試行せず Ok を返す
                let _ = handle.await.unwrap();
            }

            let runs = std::fs::read_to_string(&counter).unwrap().lines().count();
            assert_eq!(runs, 1);
        }

        #[test]
        fn stderr_is_truncated_and_redacted() {
            let connection = MySQLDumpConnection::new(mysql());
            let mut stderr = b"error: s3cr3t-pass rejected\n".to_vec();
            stderr.extend(std::iter::repeat_n(b'a', MAX_STDERR_LOG_BYTES * 2));

            let sanitized = connection.sanitize_stderr(&stderr);

            assert!(!sanitized.contains("s3cr3t-pass"));
            assert!(sanitized.starts_with("error: <redacted> rejected"));
            assert!(sanitized.len() <= MAX_STDERR_LOG_BYTES);
        }
    }
}

mod presentation {
    use crate::domain::GachaDataRepository;
    use crate::infra_repository_impls::MySQLDumpConnection;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};

    // skip(repository): Debug 経由で MySQL 接続情報とキャッシュ済み dump が
    // span 属性に入るのを防ぐ
    #[tracing::instrument(skip(repository))]
    pub async fn get_gachadata_handler(
        State(repository): State<MySQLDumpConnection>,
    ) -> Result<impl IntoResponse, (StatusCode, &'static str)> {
        if let Err(error) = repository.update_gachadata().await {
            // 詳細は run_gachadata_dump 側でログ・span に記録済み
            tracing::warn!(
                error = format!("{error:#}"),
                "Failed to update gachadata dump; serving the previous dump if available"
            );
        }

        match repository.cached_gachadata() {
            Some(gachadata_dump) => Ok(Response::builder()
                .status(StatusCode::OK)
                .header("Content-Disposition", "attachment; filename=gachadata.sql")
                .header("Content-Type", "application/sql")
                .body(gachadata_dump.0.into_response())
                .unwrap()),
            None => Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "Gachadata dump is not available yet. \
                Please retry later or contact administrators.",
            )),
        }
    }
}

mod config {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    pub struct HttpPort {
        pub port: u16,
    }

    #[derive(Clone, Deserialize)]
    pub struct MySQL {
        pub host: String,
        pub port: u16,
        pub user: String,
        pub password: String,
    }

    // パスワードを Debug 出力に含めない (span/ログへ誤って載せた場合の多層防御)
    impl std::fmt::Debug for MySQL {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MySQL")
                .field("host", &self.host)
                .field("port", &self.port)
                .field("user", &self.user)
                .field("password", &"<redacted>")
                .finish()
        }
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
    // agent はプロセスの生存期間中動かし続け、graceful shutdown 時に flush する
    let pyroscope_agent = std::env::var("PYROSCOPE_SERVER_ADDRESS")
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

    let mysql_dump_connection = MySQLDumpConnection::new(config.mysql);

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

    // SIGTERM (Kubernetes の Pod 停止) / Ctrl-C で serve を抜け、
    // 下の flush 処理へ到達させる
    let shutdown_signal = async {
        let ctrl_c = async {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to install Ctrl-C handler");
        };
        let terminate = async {
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler")
                .recv()
                .await;
        };
        tokio::select! {
            () = ctrl_c => {},
            _ = terminate => {},
        }
        tracing::info!("shutdown signal received");
    };

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal)
        .await
        .unwrap();

    // 終了前に未送信のプロファイル・スパンを flush する
    if let Some(agent) = pyroscope_agent {
        match agent.stop() {
            Ok(agent) => agent.shutdown(),
            Err(error) => tracing::warn!(%error, "Pyroscope agent の停止に失敗しました"),
        }
    }
    if let Some(provider) = tracer_provider {
        let _ = provider.shutdown();
    }
}
