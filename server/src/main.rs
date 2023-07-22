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

}


#[tokio::main]
async fn main() {
    println!("Hello, world!");
}