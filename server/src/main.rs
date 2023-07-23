use anyhow::anyhow;

mod domain {
    use std::fmt::Debug;
    use std::fs::File;

    #[derive(Debug)]
    pub struct GachadataDump(pub File);

    #[async_trait::async_trait]
    pub trait GachaDataRepository: Debug + Sync + Send + 'static {
        async fn get_gachadata(&self) -> anyhow::Result<GachadataDump>;
    }

}

mod infra_repository_impls {
    use std::fs::File;
    use std::ops::Sub;
    use std::process::Command;
    use std::time::{Duration, SystemTime};
    use crate::config::MySQL;
    use crate::domain::{GachadataDump, GachaDataRepository};

    #[derive(Debug)]
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
            } = self;

            Command::new("mysqldump")
                .args(vec!["-u", user, format!("-p{password}"), "-h", address, "-P", port, "-t", "seichiassist", "gachadata", ">", "gachadata.sql"])
                .spawn()
                .expect("Failed to run mysqldump.");
        }
    }

    #[async_trait::async_trait]
    impl GachaDataRepository for MySQLDumpConnection {
        async fn get_gachadata(&self) -> anyhow::Result<GachadataDump> {
            let quarter_hour = Duration::from_secs(900);
            let is_after_more_than_quarter_hour = match File::open("gachadata.sql") {
                Ok(file) => {
                    let last_modified = file.metadata()?.modified()?;

                    let quarter_hour_from_now = SystemTime::now().sub(quarter_hour);

                    quarter_hour_from_now < last_modified
                },
                Err(_) => true
            };

            if is_after_more_than_quarter_hour {
                self.run_gachadata_dump()
            }

            Ok(GachadataDump(File::open("gachadata.sql")?))
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

    pub async fn get_gachadata_handler(State(repository): &State<dyn GachaDataRepository>) -> impl IntoResponse {
        match repository.get_gachadata().await {
            Ok(gachadataDump) => {
                let stream = ReaderStream::new(gachadataDump.0);
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

    pub struct HttpPort {
        pub port: u16
    }

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
        pub fn from_environment() -> anyhow::Result<Self> {
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

    let config = Config::from_environment()
        .map_err(anyhow!("Failed to load config from environment variables."))?;

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], config.http_port.port));

    axum::Server::bind(&addr).await?
}
