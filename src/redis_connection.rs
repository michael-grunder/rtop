use std::sync::Arc;

use redis::{AsyncConnectionConfig, Client, ErrorKind};

use crate::credentials::CredentialStore;
use crate::model::{RuntimeSettings, Target, TargetProtocol};

/// Open a Redis connection and authenticate it explicitly when credentials are
/// present. Explicit AUTH lets us retry the pre-ACL password-only form when an
/// older Redis server rejects `AUTH default <password>`.
pub async fn connect(
    target: &Target,
    settings: &RuntimeSettings,
) -> redis::RedisResult<Connection> {
    let mut target = target.clone();
    let inner = open(&mut target, settings).await?;
    Ok(Connection {
        target,
        settings: settings.clone(),
        inner: Some(inner),
    })
}

/// Reopen on the next request after a transport failure. Never replay a command:
/// the server may have executed it before the response was lost.
pub struct Connection {
    target: Target,
    settings: RuntimeSettings,
    inner: Option<redis::aio::MultiplexedConnection>,
}

impl Connection {
    pub const fn lazy(target: Target, settings: RuntimeSettings) -> Self {
        Self {
            target,
            settings,
            inner: None,
        }
    }

    pub fn disconnect(&mut self) {
        self.inner = None;
    }

    pub const fn protocol(&self) -> TargetProtocol {
        self.target.protocol
    }

    async fn ready(&mut self) -> redis::RedisResult<&mut redis::aio::MultiplexedConnection> {
        if self.inner.is_none() {
            self.inner = Some(open(&mut self.target, &self.settings).await?);
        }
        Ok(self.inner.as_mut().expect("opened connection"))
    }

    fn check<T>(&mut self, result: &redis::RedisResult<T>) {
        if result.as_ref().is_err_and(redis::RedisError::is_io_error) {
            self.inner = None;
        }
    }
}

impl redis::aio::ConnectionLike for Connection {
    fn req_packed_command<'a>(
        &'a mut self,
        cmd: &'a redis::Cmd,
    ) -> redis::RedisFuture<'a, redis::Value> {
        Box::pin(async move {
            let result = self.ready().await?.req_packed_command(cmd).await;
            self.check(&result);
            result
        })
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        pipe: &'a redis::Pipeline,
        offset: usize,
        count: usize,
    ) -> redis::RedisFuture<'a, Vec<redis::Value>> {
        Box::pin(async move {
            let result = self
                .ready()
                .await?
                .req_packed_commands(pipe, offset, count)
                .await;
            self.check(&result);
            result
        })
    }

    fn get_db(&self) -> i64 {
        0
    }
}

async fn open(
    target: &mut Target,
    settings: &RuntimeSettings,
) -> redis::RedisResult<redis::aio::MultiplexedConnection> {
    let mut authenticated_target = target.clone();
    if let Some(store) = &settings.credential_store {
        store
            .apply(&mut authenticated_target)
            .map_err(|error| redis::RedisError::from(std::io::Error::other(error.to_string())))?;
    }
    let mut connection = match open_transport(target, settings).await {
        Err(error) if target.protocol == TargetProtocol::Tcp && error.is_io_error() => {
            let mut tls_target = target.clone();
            tls_target.protocol = TargetProtocol::Tls;
            let connection = open_transport(&tls_target, settings).await?;
            target.protocol = TargetProtocol::Tls;
            connection
        }
        result => result?,
    };

    authenticated_target.protocol = target.protocol;
    authenticate_and_remember(
        &mut connection,
        &authenticated_target,
        settings.credential_store.as_ref(),
    )
    .await?;
    Ok(connection)
}

async fn open_transport(
    target: &Target,
    settings: &RuntimeSettings,
) -> redis::RedisResult<redis::aio::MultiplexedConnection> {
    let mut url = connection_url(target);
    if target.protocol == TargetProtocol::Tls && settings.insecure_tls.allows(&target.addr) {
        url.push_str("#insecure");
    }
    let client = Client::open(url)?;
    let config = AsyncConnectionConfig::new()
        .set_connection_timeout(Some(settings.connect_timeout))
        .set_response_timeout(Some(settings.command_timeout));
    client
        .get_multiplexed_async_connection_with_config(&config)
        .await
}

async fn authenticate_and_remember(
    connection: &mut impl redis::aio::ConnectionLike,
    target: &Target,
    store: Option<&Arc<CredentialStore>>,
) -> redis::RedisResult<()> {
    authenticate(connection, target).await?;
    if let Some(store) = store.filter(|_| target.password.is_some()) {
        let store = Arc::clone(store);
        let target = target.clone();
        // Disk access and the cross-process file lock must not block the async runtime.
        match tokio::task::spawn_blocking(move || store.remember_authenticated(&target)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("warning: could not remember authentication: {error:#}"),
            Err(error) => eprintln!("warning: credential persistence task failed: {error}"),
        }
    }
    Ok(())
}

async fn authenticate(
    connection: &mut impl redis::aio::ConnectionLike,
    target: &Target,
) -> redis::RedisResult<()> {
    let Some(password) = target.password.as_deref() else {
        return Ok(());
    };
    let username = target.username.as_deref().unwrap_or("default");

    let acl_result = redis::cmd("AUTH")
        .arg(username)
        .arg(password)
        .query_async::<String>(connection)
        .await;

    match acl_result {
        Ok(_) => Ok(()),
        Err(error) if username == "default" && should_try_legacy_auth(&error) => redis::cmd("AUTH")
            .arg(password)
            .query_async::<String>(connection)
            .await
            .map(|_| ()),
        Err(error) => Err(error),
    }
}

fn should_try_legacy_auth(error: &redis::RedisError) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    matches!(
        error.kind(),
        ErrorKind::Server(redis::ServerErrorKind::ResponseError) | ErrorKind::Extension
    ) && (message.contains("wrong number of arguments") || message.contains("syntax error"))
}

fn connection_url(target: &Target) -> String {
    match target.protocol {
        TargetProtocol::Tcp => format!("redis://{}/", target.addr),
        TargetProtocol::Tls => format!("rediss://{}/", target.addr),
        TargetProtocol::Unix => format!("redis+unix://{}", target.addr),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use redis::{Cmd, ErrorKind, Pipeline, RedisFuture, Value};

    use super::{authenticate, connection_url, should_try_legacy_auth};
    use crate::model::{Target, TargetProtocol};

    fn target(protocol: TargetProtocol, addr: &str) -> Target {
        Target {
            alias: None,
            addr: addr.to_string(),
            protocol,
            username: Some("default".to_string()),
            password: Some("secret:/?".to_string()),
            tags: Vec::new(),
            process_id: None,
        }
    }

    struct MockConnection {
        commands: Vec<Vec<u8>>,
        responses: VecDeque<redis::RedisResult<Value>>,
    }

    impl redis::aio::ConnectionLike for MockConnection {
        fn req_packed_command<'a>(&'a mut self, command: &'a Cmd) -> RedisFuture<'a, Value> {
            self.commands.push(command.get_packed_command());
            let response = self.responses.pop_front().expect("mock response");
            Box::pin(async move { response })
        }

        fn req_packed_commands<'a>(
            &'a mut self,
            _pipeline: &'a Pipeline,
            _offset: usize,
            _count: usize,
        ) -> RedisFuture<'a, Vec<Value>> {
            Box::pin(async { panic!("authentication does not use pipelines") })
        }

        fn get_db(&self) -> i64 {
            0
        }
    }

    #[test]
    fn connection_url_never_contains_credentials() {
        assert_eq!(
            connection_url(&target(TargetProtocol::Tls, "[::1]:16379")),
            "rediss://[::1]:16379/"
        );
        assert_eq!(
            connection_url(&target(TargetProtocol::Tcp, "127.0.0.1:6380")),
            "redis://127.0.0.1:6380/"
        );
        assert_eq!(
            connection_url(&target(TargetProtocol::Unix, "/tmp/redis.sock")),
            "redis+unix:///tmp/redis.sock"
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // Keep isolated server setup and transport assertions together.
    async fn tls_fallback_verification_authentication_and_reconnect() {
        use std::net::TcpListener;
        use std::process::{Command, Stdio};
        use std::time::Duration;

        use crate::config::{InsecureTls, LocalTls, default_settings};

        struct Server(std::process::Child);
        impl Drop for Server {
            fn drop(&mut self) {
                self.0.kill().expect("stop isolated Redis");
                self.0.wait().expect("reap isolated Redis");
            }
        }

        for program in ["redis-server", "openssl"] {
            if Command::new(program)
                .arg("--version")
                .output()
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            {
                eprintln!("skipping TLS integration test: {program} is not installed");
                return;
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        let generated = Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=wrong-host.example",
                "-keyout",
            ])
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .output()
            .unwrap();
        assert!(
            generated.status.success(),
            "{}",
            String::from_utf8_lossy(&generated.stderr)
        );
        let plain_socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let tls_socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let plain_port = plain_socket.local_addr().unwrap().port();
        let tls_port = tls_socket.local_addr().unwrap().port();
        drop((plain_socket, tls_socket));
        let _server = Server(
            Command::new("redis-server")
                .args([
                    "--bind",
                    "127.0.0.1",
                    "--port",
                    &plain_port.to_string(),
                    "--tls-port",
                    &tls_port.to_string(),
                    "--tls-cert-file",
                ])
                .arg(&cert)
                .arg("--tls-key-file")
                .arg(&key)
                .args(["--tls-ca-cert-file"])
                .arg(&cert)
                .args([
                    "--tls-auth-clients",
                    "no",
                    "--requirepass",
                    "secret:/?",
                    "--save",
                    "",
                    "--appendonly",
                    "no",
                ])
                .current_dir(dir.path())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let mut settings = default_settings();
        settings.connect_timeout = Duration::from_secs(1);
        let plain = target(TargetProtocol::Tcp, &format!("127.0.0.1:{plain_port}"));
        let mut connection = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(connection) = super::connect(&plain, &settings).await {
                    break connection;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("isolated Redis ready");
        assert_eq!(connection.protocol(), TargetProtocol::Tcp);
        let mut bad_auth = plain.clone();
        bad_auth.password = Some("wrong".to_owned());
        let error = super::connect(&bad_auth, &settings)
            .await
            .err()
            .expect("bad plaintext auth fails");
        assert!(!error.to_string().contains("TLS"));
        assert_eq!(
            redis::cmd("PING")
                .query_async::<String>(&mut connection)
                .await
                .unwrap(),
            "PONG"
        );
        let mut tls = target(TargetProtocol::Tcp, &format!("127.0.0.1:{tls_port}"));
        assert!(
            super::connect(&tls, &settings).await.is_err(),
            "self-signed certificates must be rejected by default"
        );
        settings.insecure_tls = InsecureTls::Hosts(vec!["localhost".to_owned()]);
        assert!(
            super::connect(&tls, &settings).await.is_err(),
            "a hostname exception must not authorize other addresses"
        );
        settings.insecure_tls = InsecureTls::Local(LocalTls::Local);
        let mut connection = super::connect(&tls, &settings)
            .await
            .expect("plaintext falls back to TLS");
        assert_eq!(connection.protocol(), TargetProtocol::Tls);
        connection.disconnect();
        assert_eq!(
            redis::cmd("PING")
                .query_async::<String>(&mut connection)
                .await
                .unwrap(),
            "PONG"
        );
        assert_eq!(connection.protocol(), TargetProtocol::Tls);
        tls.protocol = TargetProtocol::Tls;
        super::connect(&tls, &settings)
            .await
            .expect("explicit TLS works");
        tls.password = Some("wrong".to_owned());
        let error = super::connect(&tls, &settings)
            .await
            .err()
            .expect("bad auth fails");
        assert!(!error.to_string().contains("plaintext"));
        let explicit_tls_on_plain = Target {
            protocol: TargetProtocol::Tls,
            ..plain
        };
        assert!(
            super::connect(&explicit_tls_on_plain, &settings)
                .await
                .is_err(),
            "explicit TLS must not downgrade to plaintext"
        );
    }

    #[test]
    fn legacy_fallback_is_limited_to_unsupported_acl_syntax() {
        let old_redis = redis::RedisError::from((
            ErrorKind::Server(redis::ServerErrorKind::ResponseError),
            "ERR wrong number of arguments for 'auth' command",
        ));
        let wrong_password = redis::RedisError::from((
            ErrorKind::AuthenticationFailed,
            "WRONGPASS invalid username-password pair",
        ));

        assert!(should_try_legacy_auth(&old_redis));
        assert!(!should_try_legacy_auth(&wrong_password));
    }

    #[tokio::test]
    async fn default_user_retries_with_password_only_for_old_redis() {
        let old_redis = redis::RedisError::from((
            ErrorKind::Server(redis::ServerErrorKind::ResponseError),
            "ERR wrong number of arguments for 'auth' command",
        ));
        let mut connection = MockConnection {
            commands: Vec::new(),
            responses: VecDeque::from([Err(old_redis), Ok(Value::Okay)]),
        };
        let target = target(TargetProtocol::Tcp, "127.0.0.1:6380");

        authenticate(&mut connection, &target)
            .await
            .expect("legacy AUTH should succeed");

        assert_eq!(connection.commands.len(), 2);
        assert_eq!(
            connection.commands[0],
            b"*3\r\n$4\r\nAUTH\r\n$7\r\ndefault\r\n$9\r\nsecret:/?\r\n"
        );
        assert_eq!(
            connection.commands[1],
            b"*2\r\n$4\r\nAUTH\r\n$9\r\nsecret:/?\r\n"
        );
    }

    #[tokio::test]
    async fn persistence_requires_successful_auth_and_opt_in() {
        use crate::credentials::CredentialStore;
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtop-auth.toml");
        let store = Arc::new(CredentialStore::load(path.clone(), &[]).unwrap());
        let target = target(TargetProtocol::Tcp, "localhost:6380");
        let rejected = || redis::RedisError::from((ErrorKind::AuthenticationFailed, "WRONGPASS"));
        let mut connection = MockConnection {
            commands: Vec::new(),
            responses: VecDeque::from([
                Err(rejected()),
                Ok(Value::Okay),
                Ok(Value::Okay),
                Err(rejected()),
            ]),
        };
        assert!(
            super::authenticate_and_remember(&mut connection, &target, Some(&store))
                .await
                .is_err()
        );
        assert!(!path.exists());
        super::authenticate_and_remember(&mut connection, &target, None)
            .await
            .unwrap();
        assert!(!path.exists());
        super::authenticate_and_remember(&mut connection, &target, Some(&store))
            .await
            .unwrap();
        let original = std::fs::read(&path).unwrap();
        let mut incorrect = target.clone();
        incorrect.password = Some("wrong".to_owned());
        assert!(
            super::authenticate_and_remember(&mut connection, &incorrect, Some(&store))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[tokio::test]
    async fn legacy_auth_is_saved_only_after_fallback_succeeds() {
        use crate::credentials::CredentialStore;
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtop-auth.toml");
        let store = Arc::new(CredentialStore::load(path.clone(), &[]).unwrap());
        let unsupported = || {
            redis::RedisError::from((
                ErrorKind::Server(redis::ServerErrorKind::ResponseError),
                "wrong number of arguments",
            ))
        };
        let mut connection = MockConnection {
            commands: Vec::new(),
            responses: VecDeque::from([
                Err(unsupported()),
                Err(redis::RedisError::from((
                    ErrorKind::AuthenticationFailed,
                    "WRONGPASS",
                ))),
                Err(unsupported()),
                Ok(Value::Okay),
            ]),
        };
        let target = target(TargetProtocol::Tcp, "localhost:6380");
        assert!(
            super::authenticate_and_remember(&mut connection, &target, Some(&store))
                .await
                .is_err()
        );
        assert!(!path.exists());
        super::authenticate_and_remember(&mut connection, &target, Some(&store))
            .await
            .unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn persistence_failure_does_not_fail_authentication() {
        use crate::credentials::CredentialStore;
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtop-auth.toml");
        let store = Arc::new(CredentialStore::load(path.clone(), &[]).unwrap());
        std::fs::create_dir(&path).unwrap();
        let mut connection = MockConnection {
            commands: Vec::new(),
            responses: VecDeque::from([Ok(Value::Okay)]),
        };
        super::authenticate_and_remember(
            &mut connection,
            &target(TargetProtocol::Tcp, "localhost:6380"),
            Some(&store),
        )
        .await
        .unwrap();
        assert!(path.is_dir());
    }
}
