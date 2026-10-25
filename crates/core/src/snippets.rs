//! Ready-to-copy connection strings and code for common languages and tools.

use serde::Serialize;
use twinbase_client::DbKind;

#[derive(Debug, Clone, Serialize)]
pub struct Snippet {
    pub id: &'static str,
    pub label: &'static str,
    /// Syntax highlighting language.
    pub language: &'static str,
    pub code: String,
}

pub struct Target<'a> {
    pub kind: DbKind,
    pub host: &'a str,
    pub port: u16,
    pub user: &'a str,
    pub password: &'a str,
    pub database: &'a str,
}

fn s(id: &'static str, label: &'static str, language: &'static str, code: String) -> Snippet {
    Snippet { id, label, language, code }
}

pub fn snippets(t: &Target<'_>) -> Vec<Snippet> {
    let Target { host, port, user, password, database, .. } = *t;
    match t.kind {
        DbKind::Oracle => vec![
            s("easy", "Easy Connect", "text", format!("{host}:{port}/{database}")),
            s("sqlplus", "SQL*Plus / SQLcl", "bash", format!("sql {user}/'{password}'@//{host}:{port}/{database}")),
            s(
                "jdbc",
                "Java (JDBC thin)",
                "java",
                format!(
                    "String url = \"jdbc:oracle:thin:@//{host}:{port}/{database}\";\nConnection conn = DriverManager.getConnection(url, \"{user}\", \"{password}\");"
                ),
            ),
            s(
                "python",
                "Python (python-oracledb, thin mode)",
                "python",
                format!(
                    "import oracledb\n\nconn = oracledb.connect(user=\"{user}\", password=\"{password}\", dsn=\"{host}:{port}/{database}\")\nprint(conn.cursor().execute(\"SELECT banner FROM v$version\").fetchone())"
                ),
            ),
            s(
                "node",
                "Node.js (node-oracledb)",
                "javascript",
                format!(
                    "import oracledb from \"oracledb\";\n\nconst conn = await oracledb.getConnection({{ user: \"{user}\", password: \"{password}\", connectString: \"{host}:{port}/{database}\" }});"
                ),
            ),
            s(
                "go",
                "Go (godror)",
                "go",
                format!(
                    "db, err := sql.Open(\"godror\", `user=\"{user}\" password=\"{password}\" connectString=\"{host}:{port}/{database}\"`)"
                ),
            ),
            s(
                "dotnet",
                ".NET (ODP.NET)",
                "csharp",
                format!("var conn = new OracleConnection(\"User Id={user};Password={password};Data Source={host}:{port}/{database}\");"),
            ),
        ],
        DbKind::Spanner => vec![
            s(
                "env",
                "Emulator environment",
                "bash",
                format!("SPANNER_EMULATOR_HOST={host}:9010  # gRPC port of the emulator; TwinBase uses its REST port {port}"),
            ),
            s("gcloud", "gcloud", "bash", format!("gcloud spanner databases execute-sql {database} --sql='SELECT 1'")),
            s(
                "node",
                "Node.js (@google-cloud/spanner)",
                "javascript",
                format!(
                    "import {{ Spanner }} from \"@google-cloud/spanner\";\n\n// path: \"{database}\" (project/instance/database)\nconst [project, instance, db] = \"{database}\".split(\"/\");\nconst database = new Spanner({{ projectId: project }}).instance(instance).database(db);\nconst [rows] = await database.run(\"SELECT 1\");"
                ),
            ),
            s(
                "python",
                "Python (google-cloud-spanner)",
                "python",
                format!(
                    "from google.cloud import spanner\n\nproject, instance, db = \"{database}\".split(\"/\")\ndatabase = spanner.Client(project=project).instance(instance).database(db)\nwith database.snapshot() as snap:\n    print(list(snap.execute_sql(\"SELECT 1\")))"
                ),
            ),
            s(
                "go",
                "Go (cloud.google.com/go/spanner)",
                "go",
                format!(
                    "client, err := spanner.NewClient(ctx, \"projects/\" + strings.Replace(\"{database}\", \"/\", \"/instances/\", 1)) // then /databases/…"
                ),
            ),
        ],
        DbKind::Bigquery => vec![
            s("bq", "bq CLI", "bash", format!("bq query --project_id={database} --use_legacy_sql=false 'SELECT 1'")),
            s(
                "node",
                "Node.js (@google-cloud/bigquery)",
                "javascript",
                format!(
                    "import {{ BigQuery }} from \"@google-cloud/bigquery\";\n\nconst bigquery = new BigQuery({{ projectId: \"{database}\" }});\nconst [rows] = await bigquery.query({{ query: \"SELECT 1\" }});"
                ),
            ),
            s(
                "python",
                "Python (google-cloud-bigquery)",
                "python",
                format!(
                    "from google.cloud import bigquery\n\nclient = bigquery.Client(project=\"{database}\")\nprint(list(client.query(\"SELECT 1\").result()))"
                ),
            ),
            s("emulator", "Emulator endpoint", "text", format!("http://{host}:{port}  # BigQuery emulator REST endpoint")),
        ],
        DbKind::Cassandra => {
            let auth = if user.is_empty() { String::new() } else { format!(" -u {user} -p '{password}'") };
            vec![
                s("cqlsh", "cqlsh", "bash", format!("cqlsh {host} {port}{auth} -k {database}")),
                s(
                    "node",
                    "Node.js (cassandra-driver)",
                    "javascript",
                    format!(
                        "import cassandra from \"cassandra-driver\";\n\nconst client = new cassandra.Client({{\n  contactPoints: [\"{host}:{port}\"], localDataCenter: \"datacenter1\", keyspace: \"{database}\",\n  credentials: {{ username: \"{user}\", password: \"{password}\" }},\n}});\nconst rs = await client.execute(\"SELECT release_version FROM system.local\");"
                    ),
                ),
                s(
                    "python",
                    "Python (cassandra-driver / scylla-driver)",
                    "python",
                    format!(
                        "from cassandra.cluster import Cluster\nfrom cassandra.auth import PlainTextAuthProvider\n\ncluster = Cluster([\"{host}\"], port={port}, auth_provider=PlainTextAuthProvider(\"{user}\", \"{password}\"))\nsession = cluster.connect(\"{database}\")"
                    ),
                ),
                s(
                    "go",
                    "Go (gocql)",
                    "go",
                    format!(
                        "cluster := gocql.NewCluster(\"{host}:{port}\")\ncluster.Keyspace = \"{database}\"\ncluster.Authenticator = gocql.PasswordAuthenticator{{Username: \"{user}\", Password: \"{password}\"}}\nsession, err := cluster.CreateSession()"
                    ),
                ),
                s(
                    "java",
                    "Java (DataStax driver)",
                    "java",
                    format!(
                        "CqlSession session = CqlSession.builder()\n    .addContactPoint(new InetSocketAddress(\"{host}\", {port}))\n    .withLocalDatacenter(\"datacenter1\")\n    .withAuthCredentials(\"{user}\", \"{password}\")\n    .withKeyspace(\"{database}\")\n    .build();"
                    ),
                ),
            ]
        }
        DbKind::Clickhouse => {
            let url = format!("http://{host}:{port}");
            vec![
                s("url", "HTTP URL", "text", format!("{url}/?database={database}")),
                s(
                    "env",
                    ".env",
                    "bash",
                    format!("CLICKHOUSE_URL={url}\nCLICKHOUSE_USER={user}\nCLICKHOUSE_PASSWORD={password}\nCLICKHOUSE_DB={database}"),
                ),
                s(
                    "curl",
                    "curl",
                    "bash",
                    format!("echo 'SELECT version()' | curl -s -u '{user}:{password}' '{url}/?database={database}' --data-binary @-"),
                ),
                s(
                    "cli",
                    "clickhouse-client",
                    "bash",
                    format!(
                        "clickhouse client --host {host} --user {user} --password '{password}' --database {database}\n# clickhouse-client uses the native protocol (port 9000), not the HTTP port."
                    ),
                ),
                s(
                    "node",
                    "Node.js (@clickhouse/client)",
                    "javascript",
                    format!(
                        "import {{ createClient }} from \"@clickhouse/client\";\n\nconst client = createClient({{ url: \"{url}\", username: \"{user}\", password: \"{password}\", database: \"{database}\" }});\nconst rs = await client.query({{ query: \"SELECT version()\", format: \"JSONEachRow\" }});\nconsole.log(await rs.json());"
                    ),
                ),
                s(
                    "python",
                    "Python (clickhouse-connect)",
                    "python",
                    format!(
                        "import clickhouse_connect\n\nclient = clickhouse_connect.get_client(host=\"{host}\", port={port}, username=\"{user}\", password=\"{password}\", database=\"{database}\")\nprint(client.query(\"SELECT version()\").result_rows)"
                    ),
                ),
                s(
                    "go",
                    "Go (clickhouse-go, HTTP)",
                    "go",
                    format!(
                        "conn := clickhouse.OpenDB(&clickhouse.Options{{\n\tAddr:     []string{{\"{host}:{port}\"}},\n\tProtocol: clickhouse.HTTP,\n\tAuth:     clickhouse.Auth{{Database: \"{database}\", Username: \"{user}\", Password: \"{password}\"}},\n}})"
                    ),
                ),
                s(
                    "jdbc",
                    "Java (JDBC)",
                    "java",
                    format!(
                        "String url = \"jdbc:clickhouse:http://{host}:{port}/{database}\";\nConnection conn = DriverManager.getConnection(url, \"{user}\", \"{password}\");"
                    ),
                ),
            ]
        }
        DbKind::Mssql => {
            let ado = format!("Server={host},{port};Database={database};User Id={user};Password={password};TrustServerCertificate=True");
            let url = format!("sqlserver://{user}:{password}@{host}:{port}?database={database}");
            vec![
                s("ado", "Connection string (.NET)", "text", ado.clone()),
                s("env", ".env", "bash", format!("DATABASE_URL={url}\nMSSQL_CONNECTION_STRING=\"{ado}\"")),
                s("sqlcmd", "sqlcmd", "bash", format!("sqlcmd -S {host},{port} -U {user} -P '{password}' -d {database} -C")),
                s(
                    "dotnet",
                    ".NET (Microsoft.Data.SqlClient)",
                    "csharp",
                    format!(
                        "using Microsoft.Data.SqlClient;\n\nawait using var conn = new SqlConnection(\"{ado}\");\nawait conn.OpenAsync();\nvar cmd = new SqlCommand(\"SELECT SYSUTCDATETIME()\", conn);\nConsole.WriteLine(await cmd.ExecuteScalarAsync());"
                    ),
                ),
                s(
                    "node",
                    "Node.js (mssql)",
                    "javascript",
                    format!(
                        "import sql from \"mssql\";\n\nconst pool = await sql.connect({{\n  server: \"{host}\", port: {port}, database: \"{database}\",\n  user: \"{user}\", password: \"{password}\",\n  options: {{ trustServerCertificate: true }},\n}});\nconst {{ recordset }} = await pool.query`SELECT SYSUTCDATETIME() AS now`;"
                    ),
                ),
                s(
                    "prisma",
                    "Prisma",
                    "prisma",
                    format!(
                        "datasource db {{\n  provider = \"sqlserver\"\n  url      = \"sqlserver://{host}:{port};database={database};user={user};password={password};trustServerCertificate=true\"\n}}"
                    ),
                ),
                s(
                    "python",
                    "Python (pyodbc)",
                    "python",
                    format!(
                        "import pyodbc\n\nconn = pyodbc.connect(\n    \"DRIVER={{ODBC Driver 18 for SQL Server}};SERVER={host},{port};DATABASE={database};\"\n    \"UID={user};PWD={password};TrustServerCertificate=yes\"\n)\nprint(conn.execute(\"SELECT SYSUTCDATETIME()\").fetchone())"
                    ),
                ),
                s(
                    "sqlalchemy",
                    "SQLAlchemy",
                    "python",
                    format!(
                        "from sqlalchemy import create_engine\n\nengine = create_engine(\n    \"mssql+pyodbc://{user}:{password}@{host}:{port}/{database}\"\n    \"?driver=ODBC+Driver+18+for+SQL+Server&TrustServerCertificate=yes\"\n)"
                    ),
                ),
                s(
                    "go",
                    "Go (go-mssqldb)",
                    "go",
                    format!(
                        "import (\n\t\"database/sql\"\n\t_ \"github.com/microsoft/go-mssqldb\"\n)\n\ndb, err := sql.Open(\"sqlserver\", \"{url}&TrustServerCertificate=true\")"
                    ),
                ),
                s(
                    "jdbc",
                    "Java (JDBC)",
                    "java",
                    format!(
                        "String url = \"jdbc:sqlserver://{host}:{port};databaseName={database};user={user};password={password};encrypt=true;trustServerCertificate=true\";\ntry (Connection conn = DriverManager.getConnection(url)) {{\n  // ...\n}}"
                    ),
                ),
                s(
                    "spring",
                    "Spring Boot",
                    "properties",
                    format!(
                        "spring.datasource.url=jdbc:sqlserver://{host}:{port};databaseName={database};encrypt=true;trustServerCertificate=true\nspring.datasource.username={user}\nspring.datasource.password={password}"
                    ),
                ),
                s("efcore", "EF Core", "csharp", format!("builder.Services.AddDbContext<AppDb>(o => o.UseSqlServer(\"{ado}\"));")),
            ]
        }
        DbKind::Postgres => {
            let url = format!("postgresql://{user}:{password}@{host}:{port}/{database}");
            vec![
                s("url", "Connection URL", "text", url.clone()),
                s("env", ".env", "bash", format!("DATABASE_URL={url}")),
                s("psql", "psql", "bash", format!("psql \"{url}\"")),
                s(
                    "node",
                    "Node.js (pg)",
                    "javascript",
                    format!(
                        "import pg from \"pg\";\n\nconst pool = new pg.Pool({{ connectionString: \"{url}\" }});\nconst {{ rows }} = await pool.query(\"SELECT now()\");"
                    ),
                ),
                s("prisma", "Prisma", "prisma", format!("datasource db {{\n  provider = \"postgresql\"\n  url      = \"{url}\"\n}}")),
                s(
                    "python",
                    "Python (psycopg)",
                    "python",
                    format!(
                        "import psycopg\n\nwith psycopg.connect(\"{url}\") as conn:\n    print(conn.execute(\"SELECT now()\").fetchone())"
                    ),
                ),
                s(
                    "sqlalchemy",
                    "SQLAlchemy",
                    "python",
                    format!(
                        "from sqlalchemy import create_engine\n\nengine = create_engine(\"postgresql+psycopg://{user}:{password}@{host}:{port}/{database}\")"
                    ),
                ),
                s(
                    "django",
                    "Django settings",
                    "python",
                    format!(
                        "DATABASES = {{\n    \"default\": {{\n        \"ENGINE\": \"django.db.backends.postgresql\",\n        \"HOST\": \"{host}\",\n        \"PORT\": \"{port}\",\n        \"NAME\": \"{database}\",\n        \"USER\": \"{user}\",\n        \"PASSWORD\": \"{password}\",\n    }}\n}}"
                    ),
                ),
                s("go", "Go (pgx)", "go", format!("conn, err := pgx.Connect(ctx, \"{url}\")")),
                s(
                    "java",
                    "Java (JDBC)",
                    "java",
                    format!(
                        "Connection conn = DriverManager.getConnection(\n    \"jdbc:postgresql://{host}:{port}/{database}\", \"{user}\", \"{password}\");"
                    ),
                ),
                s(
                    "spring",
                    "Spring Boot",
                    "properties",
                    format!(
                        "spring.datasource.url=jdbc:postgresql://{host}:{port}/{database}\nspring.datasource.username={user}\nspring.datasource.password={password}"
                    ),
                ),
                s(
                    "dotnet",
                    ".NET (Npgsql)",
                    "csharp",
                    format!(
                        "var conn = new NpgsqlConnection(\"Host={host};Port={port};Database={database};Username={user};Password={password}\");"
                    ),
                ),
                s("rust", "Rust (sqlx)", "rust", format!("let pool = sqlx::PgPool::connect(\"{url}\").await?;")),
                s(
                    "php",
                    "PHP (PDO)",
                    "php",
                    format!("$pdo = new PDO(\"pgsql:host={host};port={port};dbname={database}\", \"{user}\", \"{password}\");"),
                ),
                s("ruby", "Ruby (pg)", "ruby", format!("conn = PG.connect(\"{url}\")")),
            ]
        }
        DbKind::Mysql => {
            let url = format!("mysql://{user}:{password}@{host}:{port}/{database}");
            vec![
                s("url", "Connection URL", "text", url.clone()),
                s("env", ".env", "bash", format!("DATABASE_URL={url}")),
                s("cli", "mysql CLI", "bash", format!("mysql -h {host} -P {port} -u {user} -p{password} {database}")),
                s(
                    "node",
                    "Node.js (mysql2)",
                    "javascript",
                    format!(
                        "import mysql from \"mysql2/promise\";\n\nconst conn = await mysql.createConnection(\"{url}\");\nconst [rows] = await conn.query(\"SELECT NOW()\");"
                    ),
                ),
                s("prisma", "Prisma", "prisma", format!("datasource db {{\n  provider = \"mysql\"\n  url      = \"{url}\"\n}}")),
                s(
                    "python",
                    "Python (PyMySQL)",
                    "python",
                    format!(
                        "import pymysql\n\nconn = pymysql.connect(host=\"{host}\", port={port}, user=\"{user}\", password=\"{password}\", database=\"{database}\")"
                    ),
                ),
                s(
                    "sqlalchemy",
                    "SQLAlchemy",
                    "python",
                    format!(
                        "from sqlalchemy import create_engine\n\nengine = create_engine(\"mysql+pymysql://{user}:{password}@{host}:{port}/{database}\")"
                    ),
                ),
                s(
                    "go",
                    "Go (go-sql-driver)",
                    "go",
                    format!("db, err := sql.Open(\"mysql\", \"{user}:{password}@tcp({host}:{port})/{database}?parseTime=true\")"),
                ),
                s(
                    "java",
                    "Java (JDBC)",
                    "java",
                    format!(
                        "Connection conn = DriverManager.getConnection(\n    \"jdbc:mysql://{host}:{port}/{database}\", \"{user}\", \"{password}\");"
                    ),
                ),
                s(
                    "spring",
                    "Spring Boot",
                    "properties",
                    format!(
                        "spring.datasource.url=jdbc:mysql://{host}:{port}/{database}\nspring.datasource.username={user}\nspring.datasource.password={password}"
                    ),
                ),
                s(
                    "dotnet",
                    ".NET (MySqlConnector)",
                    "csharp",
                    format!(
                        "var conn = new MySqlConnection(\"Server={host};Port={port};Database={database};User ID={user};Password={password}\");"
                    ),
                ),
                s(
                    "php",
                    "PHP (PDO)",
                    "php",
                    format!("$pdo = new PDO(\"mysql:host={host};port={port};dbname={database}\", \"{user}\", \"{password}\");"),
                ),
                s(
                    "laravel",
                    "Laravel .env",
                    "bash",
                    format!(
                        "DB_CONNECTION=mysql\nDB_HOST={host}\nDB_PORT={port}\nDB_DATABASE={database}\nDB_USERNAME={user}\nDB_PASSWORD={password}"
                    ),
                ),
            ]
        }
        DbKind::Elasticsearch => {
            let url = format!("http://{host}:{port}");
            vec![
                s("url", "URL", "text", url.clone()),
                s("curl", "curl", "bash", format!("curl -u '{user}:{password}' '{url}/_cat/indices?v'")),
                s(
                    "node",
                    "Node.js (@elastic/elasticsearch)",
                    "javascript",
                    format!(
                        "import {{ Client }} from \"@elastic/elasticsearch\";\n\nconst client = new Client({{ node: \"{url}\", auth: {{ username: \"{user}\", password: \"{password}\" }} }});\nconst r = await client.search({{ index: \"products\", query: {{ match_all: {{}} }} }});"
                    ),
                ),
                s(
                    "python",
                    "Python (elasticsearch)",
                    "python",
                    format!(
                        "from elasticsearch import Elasticsearch\n\nes = Elasticsearch(\"{url}\", basic_auth=(\"{user}\", \"{password}\"))\nprint(es.search(index=\"products\", query={{\"match_all\": {{}}}}))"
                    ),
                ),
                s(
                    "opensearch-py",
                    "Python (opensearch-py)",
                    "python",
                    format!(
                        "from opensearchpy import OpenSearch\n\nclient = OpenSearch(hosts=[\"{url}\"], http_auth=(\"{user}\", \"{password}\"))"
                    ),
                ),
            ]
        }
        DbKind::Firestore => {
            let emulator = format!("{host}:{port}");
            vec![
                s("env", "Environment", "bash", format!("FIRESTORE_EMULATOR_HOST={emulator}\nGCLOUD_PROJECT=demo-twinbase")),
                s(
                    "admin-node",
                    "Node.js (firebase-admin)",
                    "javascript",
                    format!(
                        "process.env.FIRESTORE_EMULATOR_HOST = \"{emulator}\";\n\nimport {{ initializeApp }} from \"firebase-admin/app\";\nimport {{ getFirestore }} from \"firebase-admin/firestore\";\n\ninitializeApp({{ projectId: \"demo-twinbase\" }});\nconst snap = await getFirestore().collection(\"users\").get();"
                    ),
                ),
                s(
                    "web",
                    "Web / React Native (firebase)",
                    "javascript",
                    format!(
                        "import {{ initializeApp }} from \"firebase/app\";\nimport {{ getFirestore, connectFirestoreEmulator }} from \"firebase/firestore\";\n\nconst app = initializeApp({{ projectId: \"demo-twinbase\" }});\nconst db = getFirestore(app);\nconnectFirestoreEmulator(db, \"{host}\", {port});"
                    ),
                ),
                s(
                    "python",
                    "Python (google-cloud-firestore)",
                    "python",
                    format!(
                        "import os\nos.environ[\"FIRESTORE_EMULATOR_HOST\"] = \"{emulator}\"\n\nfrom google.cloud import firestore\n\ndb = firestore.Client(project=\"demo-twinbase\")\nprint([d.to_dict() for d in db.collection(\"users\").stream()])"
                    ),
                ),
                s(
                    "go",
                    "Go (cloud.google.com/go/firestore)",
                    "go",
                    format!(
                        "os.Setenv(\"FIRESTORE_EMULATOR_HOST\", \"{emulator}\")\nclient, err := firestore.NewClient(ctx, \"demo-twinbase\")"
                    ),
                ),
                s(
                    "java",
                    "Java (google-cloud-firestore)",
                    "java",
                    format!(
                        "Firestore db = FirestoreOptions.newBuilder()\n    .setProjectId(\"demo-twinbase\")\n    .setEmulatorHost(\"{emulator}\")\n    .build().getService();"
                    ),
                ),
                s(
                    "firebase-json",
                    "firebase.json (emulator suite)",
                    "json",
                    format!("{{\n  \"emulators\": {{ \"firestore\": {{ \"host\": \"{host}\", \"port\": {port} }} }}\n}}"),
                ),
            ]
        }
        DbKind::Dynamodb => {
            let endpoint = format!("http://{host}:{port}");
            vec![
                s("endpoint", "Endpoint", "text", endpoint.clone()),
                s(
                    "env",
                    ".env",
                    "bash",
                    format!(
                        "AWS_ENDPOINT_URL_DYNAMODB={endpoint}\nAWS_REGION=us-east-1\nAWS_ACCESS_KEY_ID={user}\nAWS_SECRET_ACCESS_KEY={password}"
                    ),
                ),
                s("cli", "AWS CLI", "bash", format!("aws dynamodb list-tables --endpoint-url {endpoint} --region us-east-1")),
                s(
                    "node",
                    "Node.js (AWS SDK v3)",
                    "javascript",
                    format!(
                        "import {{ DynamoDBClient, ListTablesCommand }} from \"@aws-sdk/client-dynamodb\";\n\nconst client = new DynamoDBClient({{\n  endpoint: \"{endpoint}\",\n  region: \"us-east-1\",\n  credentials: {{ accessKeyId: \"{user}\", secretAccessKey: \"{password}\" }},\n}});\nconsole.log(await client.send(new ListTablesCommand({{}})));"
                    ),
                ),
                s(
                    "python",
                    "Python (boto3)",
                    "python",
                    format!(
                        "import boto3\n\ndynamodb = boto3.resource(\n    \"dynamodb\",\n    endpoint_url=\"{endpoint}\",\n    region_name=\"us-east-1\",\n    aws_access_key_id=\"{user}\",\n    aws_secret_access_key=\"{password}\",\n)\nprint(list(dynamodb.tables.all()))"
                    ),
                ),
                s(
                    "go",
                    "Go (AWS SDK v2)",
                    "go",
                    format!(
                        "cfg, _ := config.LoadDefaultConfig(ctx,\n    config.WithRegion(\"us-east-1\"),\n    config.WithCredentialsProvider(credentials.NewStaticCredentialsProvider(\"{user}\", \"{password}\", \"\")),\n)\nclient := dynamodb.NewFromConfig(cfg, func(o *dynamodb.Options) {{\n    o.BaseEndpoint = aws.String(\"{endpoint}\")\n}})"
                    ),
                ),
                s(
                    "java",
                    "Java (AWS SDK v2)",
                    "java",
                    format!(
                        "DynamoDbClient client = DynamoDbClient.builder()\n    .endpointOverride(URI.create(\"{endpoint}\"))\n    .region(Region.US_EAST_1)\n    .credentialsProvider(StaticCredentialsProvider.create(AwsBasicCredentials.create(\"{user}\", \"{password}\")))\n    .build();"
                    ),
                ),
            ]
        }
        DbKind::Mongodb => {
            let auth = if user.is_empty() { String::new() } else { format!("{user}:{password}@") };
            let url = format!("mongodb://{auth}{host}:{port}/{database}{}", if user.is_empty() { "" } else { "?authSource=admin" });
            vec![
                s("url", "Connection string", "text", url.clone()),
                s("env", ".env", "bash", format!("MONGODB_URI={url}")),
                s("mongosh", "mongosh", "bash", format!("mongosh \"{url}\"")),
                s(
                    "node",
                    "Node.js (mongodb)",
                    "javascript",
                    format!(
                        "import {{ MongoClient }} from \"mongodb\";\n\nconst client = new MongoClient(\"{url}\");\nconst db = client.db(\"{database}\");\nconst users = await db.collection(\"users\").find({{}}).toArray();"
                    ),
                ),
                s(
                    "mongoose",
                    "Mongoose",
                    "javascript",
                    format!("import mongoose from \"mongoose\";\n\nawait mongoose.connect(\"{url}\");"),
                ),
                s(
                    "python",
                    "Python (pymongo)",
                    "python",
                    format!(
                        "from pymongo import MongoClient\n\nclient = MongoClient(\"{url}\")\nusers = list(client[\"{database}\"].users.find())"
                    ),
                ),
                s(
                    "go",
                    "Go (mongo-driver)",
                    "go",
                    format!(
                        "client, err := mongo.Connect(options.Client().ApplyURI(\"{url}\"))\nusers := client.Database(\"{database}\").Collection(\"users\")"
                    ),
                ),
                s("java", "Java (Spring Boot)", "properties", format!("spring.data.mongodb.uri={url}")),
            ]
        }
        DbKind::Redis => {
            let auth = if password.is_empty() { String::new() } else { format!(":{password}@") };
            let url = format!("redis://{auth}{host}:{port}/{}", if database.is_empty() { "0" } else { database });
            vec![
                s("url", "Connection URL", "text", url.clone()),
                s("env", ".env", "bash", format!("REDIS_URL={url}")),
                s("cli", "redis-cli", "bash", format!("redis-cli -h {host} -p {port}")),
                s(
                    "node",
                    "Node.js (ioredis)",
                    "javascript",
                    format!(
                        "import Redis from \"ioredis\";\n\nconst redis = new Redis(\"{url}\");\nawait redis.set(\"hello\", \"world\");"
                    ),
                ),
                s(
                    "node-redis",
                    "Node.js (redis)",
                    "javascript",
                    format!(
                        "import {{ createClient }} from \"redis\";\n\nconst client = await createClient({{ url: \"{url}\" }}).connect();"
                    ),
                ),
                s(
                    "python",
                    "Python (redis-py)",
                    "python",
                    format!("import redis\n\nr = redis.Redis.from_url(\"{url}\")\nr.set(\"hello\", \"world\")"),
                ),
                s("go", "Go (go-redis)", "go", format!("rdb := redis.NewClient(&redis.Options{{Addr: \"{host}:{port}\"}})")),
                s("java", "Java (Jedis)", "java", format!("Jedis jedis = new Jedis(\"{host}\", {port});")),
                s(
                    "dotnet",
                    ".NET (StackExchange.Redis)",
                    "csharp",
                    format!("var redis = ConnectionMultiplexer.Connect(\"{host}:{port}\");"),
                ),
            ]
        }
        DbKind::Sqlite => vec![s("path", "File path", "text", database.to_owned())],
    }
}
