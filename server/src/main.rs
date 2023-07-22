use anyhow::anyhow;
use crate::config::Config;

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
    let config = Config::from_environment()
        .map_err(anyhow!("Failed to load config from environment."))?;

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], config.http_port.port));

    axum::Server::bind(&addr).await?
}
