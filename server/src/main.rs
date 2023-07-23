use std::sync::{Arc, Mutex};

mod domain {
    use std::fmt::Debug;
    use std::time::SystemTime;
    use bytes::Bytes;

    #[derive(Debug, Clone, Default)]
    pub struct GachadataDump(pub Bytes);

    #[derive(Debug, Clone, Default)]
    pub struct GachadataDumpWithTime {
        pub dump: GachadataDump,
        pub dump_time: Option<SystemTime>
    }

    #[async_trait::async_trait]
    pub trait GachaDataRepository: Debug + Sync + Send + 'static {
        async fn update_gachadata(&self) -> anyhow::Result<()>;
    }

}

mod infra_repository_impls {
    use std::ops::{Deref, Sub};
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime};
    use anyhow::anyhow;
    use bytes::Bytes;
    use crate::config::MySQL;
    use crate::domain::{GachadataDump, GachadataDumpWithTime, GachaDataRepository};

    #[derive(Debug, Clone)]
    pub struct MySQLDumpConnection {
        pub connection_information: MySQL,
        pub dump: Arc<Mutex<GachadataDumpWithTime>>
    }

    impl MySQLDumpConnection {
        pub async fn run_gachadata_dump(&self) -> anyhow::Result<()> {
            let MySQL {
                address,
                port,
                user,
                password
            } = &self.connection_information;

            let output = Command::new("mysqldump")
                .args(vec!["-h", address, "--port", port.to_string().as_str(), "-u", user, format!("-p{}", password).as_str(), "seichiassist", "gachadata"])
                .output()?;

            if let Ok(mut dump) = self.dump.lock() {
                *dump = GachadataDumpWithTime {
                    dump: GachadataDump(Bytes::from(output.stdout)),
                    dump_time: Some(SystemTime::now())
                }
            } else {
                return Err(anyhow!("Failed to lock gachadata dump."))
            }

            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl GachaDataRepository for MySQLDumpConnection {
        async fn update_gachadata(&self) -> anyhow::Result<()> {
            let is_after_more_than_quarter_hour = match self.dump.lock() {
                Ok(dump) => {
                    let quarter_hour = Duration::from_secs(900);
                    let dump_time = dump.dump_time.ok_or(anyhow!("Failed to get last dump time."))?;

                    let quarter_hour_from_now = SystemTime::now().sub(quarter_hour);

                    quarter_hour_from_now < dump_time
                },
                _ => false
            };

            if is_after_more_than_quarter_hour {
                self.run_gachadata_dump().await?
            }

            Ok(())
        }
    }

}

mod presentation {
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::{ErrorResponse, IntoResponse, Response, Result};
    use crate::infra_repository_impls::MySQLDumpConnection;

    pub async fn get_gachadata_handler(State(repository): State<MySQLDumpConnection>) -> Result<impl IntoResponse> {
        match repository.run_gachadata_dump().await {
            Ok(_) => match repository.dump.lock() {
                Ok(gachadata_dump) if !gachadata_dump.dump.0.is_empty() => {

                    Ok(Response::builder()
                        .status(StatusCode::OK)
                        .header("Content-Disposition", "attachment; filename=gachadata.sql")
                        .body(gachadata_dump.dump.0.to_owned().into_response())
                        .unwrap())
                },
                Ok(_) => {
                    Err(ErrorResponse::from((StatusCode::INTERNAL_SERVER_ERROR, "Failed to get gachadata.sql. Please contact to administrators.").into_response()))
                },
                Err(err) => {
                    tracing::error!("{}", err);
                    Err(ErrorResponse::from((StatusCode::INTERNAL_SERVER_ERROR, "Failed to get gachadata.sql. Please contact to administrators.").into_response()))
                }
            }
            Err(err) => {
                tracing::error!("{}", err);
                Err(ErrorResponse::from((StatusCode::INTERNAL_SERVER_ERROR, "Failed to update gachadata dump. Please contact to administrators.").into_response()))
            }
        }

    }

}

mod config {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    pub struct HttpPort {
        pub port: u16
    }

    #[derive(Debug, Clone, Deserialize)]
    pub struct MySQL {
        pub address: String,
        pub port: u16,
        pub user: String,
        pub password: String
    }

    pub struct Config {
        pub http_port: HttpPort,
        pub mysql: MySQL
    }

    impl Config {
        pub async fn from_environment() -> anyhow::Result<Self> {
            let http_port = envy::prefixed("HTTP_").from_env::<HttpPort>()?;
            let mysql = envy::prefixed("MYSQL_").from_env::<MySQL>()?;

            Ok(Config {
                http_port,
                mysql,
            })
        }
    }

}


#[tokio::main]
async fn main() {
    use crate::config::Config;
    use crate::presentation::get_gachadata_handler;
    use axum::Router;
    use axum::routing::get;
    use crate::infra_repository_impls::MySQLDumpConnection;

    let config = Config::from_environment().await.expect("Failed to load config from environment variables.");

    let mysql_dump_connection = MySQLDumpConnection {
        connection_information: config.mysql,
        dump: Arc::new(Mutex::default())
    };

    let router = Router::new()
        .route("/", get(get_gachadata_handler))
        .with_state(mysql_dump_connection);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], config.http_port.port));

    axum::Server::bind(&addr).serve(router.into_make_service()).await.unwrap()
}
