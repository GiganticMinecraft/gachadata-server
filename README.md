# gachadata-server
整地鯖のガチャデータを公開するためのサーバー

# 前提ソフトウェア
- [mariadb-client](https://mariadb.com/docs/server/clients-and-utilities/mariadb-client) (sql dumpを取得するために必要)

# 環境変数
| 環境変数名      | 説明                                              | 例       | 
| -------------- | ------------------------------------------------- | -------- | 
| HTTP_PORT      | `gachadata-server`が受け付けるHTTPポート            | 80       | 
| MYSQL_HOST     | ゲームデータがあるMYSQLのホスト名                 | db       | 
| MYSQL_PORT     | ゲームデータがあるMYSQLのポート番号               | 3306     | 
| MYSQL_USER     | ゲームデータがあるMYSQLにアクセスできるユーザー名 | user     | 
| MYSQL_PASSWORD | `MYSQL_USER`で指定したユーザーのパスワード        | password | 

# `gachadata.sql`に含まれているデータ
`gachadata.sql`には以下のテーブルのdumpが含まれています
- gachadataテーブル(ガチャ景品データ)
- gacha_eventsテーブル(ガチャ景品のイベントデータ)

# gachadata-serverから`gachadata.sql`をダウンロードする
`http(s)://[gachadata-serverの接続先]/` に対して`GET`リクエストをすることでダウンロードできます。

# 俯瞰図
![overview](./docs/overview.drawio.svg)
