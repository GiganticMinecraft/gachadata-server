mod domain {
    use std::fmt::Debug;
    use tokio::fs::File;

    #[derive(Debug)]
    pub struct GachadataDump(pub File);

    #[async_trait::async_trait]
    pub trait GachaDataRepository: Debug + Sync + Send + 'static {
        async fn get_gachadata(&self) -> anyhow::Result<GachadataDump>;
    }

}

mod infra_repository_impls {
    use std::ops::Sub;
    use std::process::Command;
    use std::time::{Duration, SystemTime};
    use tokio::fs::File;
    use crate::config::MySQL;
    use crate::domain::{GachadataDump, GachaDataRepository};

    #[derive(Debug, Clone)]
    pub struct MySQLDumpConnection {
        pub connection_information: MySQL
    }

    impl MySQLDumpConnection {
        pub async fn run_gachadata_dump(&self) {
            let MySQL {
                address,
                port,
                user,
                password
            } = &self.connection_information;

            Command::new("mysqldump")
                .args(vec!["-u", user, format!("-p{}", password).as_str(), "-h", address, "-P", port.to_string().as_str(), "-t", "seichiassist", "gachadata", ">", "gachadata.sql"])
                .spawn()
                .expect("Failed to run mysqldump.");
        }
    }

    #[async_trait::async_trait]
    impl GachaDataRepository for MySQLDumpConnection {
        async fn get_gachadata(&self) -> anyhow::Result<GachadataDump> {
            let quarter_hour = Duration::from_secs(900);
            let is_after_more_than_quarter_hour = match File::open("gachadata.sql").await {
                Ok(file) => {
                    let last_modified = file.metadata().await?.modified()?;

                    let quarter_hour_from_now = SystemTime::now().sub(quarter_hour);

                    quarter_hour_from_now < last_modified
                },
                Err(_) => true
            };

            if is_after_more_than_quarter_hour {
                self.run_gachadata_dump().await
            }

            Ok(GachadataDump(File::open("gachadata.sql").await?))
        }
    }

}

mod presentation {
    use axum::body::StreamBody;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use tokio_util::io::ReaderStream;
    use crate::domain::GachaDataRepository;
    use crate::infra_repository_impls::MySQLDumpConnection;

    pub async fn get_gachadata_handler(State(repository): State<MySQLDumpConnection>) -> impl IntoResponse {
        match repository.get_gachadata().await {
            Ok(gachadata_dump) => {
                let stream = ReaderStream::new(gachadata_dump.0);
                let body = StreamBody::new(stream);

                (StatusCode::OK, body).into_response()
            },
            Err(err) => {
                tracing::error!("{}", err);
                (StatusCode::INTERNAL_SERVER_ERROR, "Failed to get gachadata.sql. Please contact to administrators.").into_response()
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
        connection_information: config.mysql
    };

    let router = Router::new()
        .route("/", get(get_gachadata_handler))
        .with_state(mysql_dump_connection);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], config.http_port.port));

    axum::Server::bind(&addr).serve(router.into_make_service()).await.unwrap()
}
