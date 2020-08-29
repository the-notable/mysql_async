// Copyright (c) 2016 Anatoly Ikorsky
//
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. All files in the project carrying such notice may not be copied,
// modified, or distributed except according to those terms.

pub use mysql_common::named_params;

use mysql_common::{
    constants::DEFAULT_MAX_ALLOWED_PACKET,
    crypto,
    packets::{
        parse_auth_switch_request, parse_err_packet, parse_handshake_packet, parse_ok_packet,
        AuthPlugin, AuthSwitchRequest, ErrPacket, HandshakeResponse, OkPacket, OkPacketKind,
        SslRequest,
    },
};

use std::{
    borrow::Cow,
    fmt,
    future::Future,
    mem,
    pin::Pin,
    str::FromStr,
    time::{Duration, Instant},
};

use crate::{
    conn::{pool::Pool, stmt_cache::StmtCache},
    consts::{CapabilityFlags, Command, StatusFlags},
    error::*,
    io::Stream,
    opts::Opts,
    queryable::{
        query_result::{QueryResult, ResultSetMeta},
        transaction::TxStatus,
        BinaryProtocol, Queryable, TextProtocol,
    },
    OptsBuilder,
};
use crate::connection_info::ConnectionInfo;

pub mod pool;
pub mod stmt_cache;

/// Helper that asynchronously disconnects the givent connection on the default tokio executor.
fn disconnect(mut conn: Conn) {
    let disconnected = conn.inner.disconnected;

    // Mark conn as disconnected.
    conn.inner.disconnected = true;

    if !disconnected {
        // We shouldn't call tokio::spawn if unwinding
        if std::thread::panicking() {
            return;
        }

        // Server will report broken connection if spawn fails.
        // this might fail if, say, the runtime is shutting down, but we've done what we could
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Ok(conn) = conn.cleanup_for_pool().await {
                    let _ = conn.disconnect().await;
                }
            });
        }
    }
}

/// Mysql connection
struct ConnInner {
    stream: Option<Stream>,
    id: u32,
    version: (u16, u16, u16),
    socket: Option<String>,
    capabilities: CapabilityFlags,
    status: StatusFlags,
    last_ok_packet: Option<OkPacket<'static>>,
    last_err_packet: Option<ErrPacket<'static>>,
    pool: Option<Pool>,
    pending_result: Option<ResultSetMeta>,
    tx_status: TxStatus,
    opts: Opts,
    last_io: Instant,
    wait_timeout: Duration,
    stmt_cache: StmtCache,
    nonce: Vec<u8>,
    auth_plugin: AuthPlugin<'static>,
    auth_switched: bool,
    /// Connection is already disconnected.
    disconnected: bool,
}

impl fmt::Debug for ConnInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Conn")
            .field("connection id", &self.id)
            .field("server version", &self.version)
            .field("pool", &self.pool)
            .field("pending_result", &self.pending_result)
            .field("tx_status", &self.tx_status)
            .field("stream", &self.stream)
            .field("options", &self.opts)
            .finish()
    }
}

impl ConnInner {
    /// Constructs an empty connection.
    fn empty(opts: Opts) -> ConnInner {
        ConnInner {
            capabilities: opts.get_capabilities(),
            status: StatusFlags::empty(),
            last_ok_packet: None,
            last_err_packet: None,
            stream: None,
            version: (0, 0, 0),
            id: 0,
            pending_result: None,
            pool: None,
            tx_status: TxStatus::None,
            last_io: Instant::now(),
            wait_timeout: Duration::from_secs(0),
            stmt_cache: StmtCache::new(opts.stmt_cache_size()),
            socket: opts.socket().map(Into::into),
            opts,
            nonce: Vec::default(),
            auth_plugin: AuthPlugin::MysqlNativePassword,
            auth_switched: false,
            disconnected: false,
        }
    }

    /// Returns mutable reference to a connection stream.
    ///
    /// Returns `DriverError::ConnectionClosed` if there is no stream.
    fn stream_mut(&mut self) -> Result<&mut Stream> {
        self.stream
            .as_mut()
            .ok_or(DriverError::ConnectionClosed.into())
    }
}

/// MySql server connection.
#[derive(Debug)]
pub struct Conn {
    inner: Box<ConnInner>,
}

impl ConnectionInfo for Conn {
    fn id(&self) -> u32 {
        self.inner.id
    }

    fn last_insert_id(&self) -> Option<u64> {
        self.inner
            .last_ok_packet
            .as_ref()
            .and_then(|ok| ok.last_insert_id())
    }

    fn affected_rows(&self) -> u64 {
        self.inner
            .last_ok_packet
            .as_ref()
            .map(|ok| ok.affected_rows())
            .unwrap_or_default()
    }

    fn info(&self) -> Cow<'_, str> {
        self.inner
            .last_ok_packet
            .as_ref()
            .and_then(|ok| ok.info_str())
            .unwrap_or_else(|| "".into())
    }

    fn get_warnings(&self) -> u16 {
        self.inner
            .last_ok_packet
            .as_ref()
            .map(|ok| ok.warnings())
            .unwrap_or_default()
    }

    fn server_version(&self) -> (u16, u16, u16) {
        self.inner.version
    }

    fn opts(&self) -> &Opts {
        &self.inner.opts
    }
}

impl Conn {

    pub(crate) fn stream_mut(&mut self) -> Result<&mut Stream> {
        self.inner.stream_mut()
    }

    pub(crate) fn capabilities(&self) -> CapabilityFlags {
        self.inner.capabilities
    }

    /// Will update last IO time for this connection.
    pub(crate) fn touch(&mut self) {
        self.inner.last_io = Instant::now();
    }

    /// Will set packet sequence id to `0`.
    pub(crate) fn reset_seq_id(&mut self) {
        if let Some(stream) = self.inner.stream.as_mut() {
            stream.reset_seq_id();
        }
    }

    /// Will syncronize sequence ids between compressed and uncompressed codecs.
    pub(crate) fn sync_seq_id(&mut self) {
        if let Some(stream) = self.inner.stream.as_mut() {
            stream.sync_seq_id();
        }
    }

    /// Handles OK packet.
    pub(crate) fn handle_ok(&mut self, ok_packet: OkPacket<'static>) {
        self.inner.status = ok_packet.status_flags();
        self.inner.last_err_packet = None;
        self.inner.last_ok_packet = Some(ok_packet);
    }

    /// Handles ERR packet.
    pub(crate) fn handle_err(&mut self, err_packet: ErrPacket<'static>) {
        self.inner.status = StatusFlags::empty();
        self.inner.last_ok_packet = None;
        self.inner.last_err_packet = Some(err_packet);
    }

    /// Returns the current transaction status.
    pub(crate) fn get_tx_status(&self) -> TxStatus {
        self.inner.tx_status
    }

    /// Sets the given transaction status for this connection.
    pub(crate) fn set_tx_status(&mut self, tx_status: TxStatus) {
        self.inner.tx_status = tx_status;
    }

    /// Returns pending result metadata, if any.
    ///
    /// If `Some(_)`, then result is not yet consumed.
    pub(crate) fn get_pending_result(&self) -> Option<&ResultSetMeta> {
        self.inner.pending_result.as_ref()
    }

    /// Sets the given pening result metadata for this connection. Returns the previous value.
    pub(crate) fn set_pending_result(
        &mut self,
        meta: Option<ResultSetMeta>,
    ) -> Option<ResultSetMeta> {
        std::mem::replace(&mut self.inner.pending_result, meta)
    }

    /// Returns current status flags.
    pub(crate) fn status(&self) -> StatusFlags {
        self.inner.status
    }

    fn take_stream(&mut self) -> Stream {
        self.inner.stream.take().unwrap()
    }

    /// Disconnects this connection from server.
    pub async fn disconnect(mut self) -> Result<()> {
        if !self.inner.disconnected {
            self.inner.disconnected = true;
            self.write_command_data(Command::COM_QUIT, &[]).await?;
            let stream = self.take_stream();
            stream.close().await?;
        }
        Ok(())
    }

    /// Closes the connection.
    async fn close_conn(mut self) -> Result<()> {
        self = self.cleanup_for_pool().await?;
        self.disconnect().await
    }

    /// Returns true if io stream is encrypted.
    fn is_secure(&self) -> bool {
        if let Some(ref stream) = self.inner.stream {
            stream.is_secure()
        } else {
            false
        }
    }

    /// Hacky way to move connection through &mut. `self` becomes unusable.
    fn take(&mut self) -> Conn {
        mem::replace(self, Conn::empty(Default::default()))
    }

    fn empty(opts: Opts) -> Self {
        Self {
            inner: Box::new(ConnInner::empty(opts)),
        }
    }

    /// Set `io::Stream` options as defined in the `Opts` of the connection.
    ///
    /// Requires that self.inner.stream is Some
    fn setup_stream(&mut self) -> Result<()> {
        debug_assert!(self.inner.stream.is_some());
        if let Some(stream) = self.inner.stream.as_mut() {
            stream.set_keepalive_ms(self.inner.opts.tcp_keepalive())?;
            stream.set_tcp_nodelay(self.inner.opts.tcp_nodelay())?;
        }
        Ok(())
    }

    async fn handle_handshake(&mut self) -> Result<()> {
        let packet = self.read_packet().await?;
        let handshake = parse_handshake_packet(&*packet)?;
        self.inner.nonce = {
            let mut nonce = Vec::from(handshake.scramble_1_ref());
            nonce.extend_from_slice(handshake.scramble_2_ref().unwrap_or(&[][..]));
            nonce
        };

        self.inner.capabilities = handshake.capabilities() & self.inner.opts.get_capabilities();
        self.inner.version = handshake.server_version_parsed().unwrap_or((0, 0, 0));
        self.inner.id = handshake.connection_id();
        self.inner.status = handshake.status_flags();
        self.inner.auth_plugin = match handshake.auth_plugin() {
            Some(AuthPlugin::MysqlNativePassword) => AuthPlugin::MysqlNativePassword,
            Some(AuthPlugin::CachingSha2Password) => AuthPlugin::CachingSha2Password,
            Some(AuthPlugin::Other(ref name)) => {
                let name = String::from_utf8_lossy(name).into();
                return Err(DriverError::UnknownAuthPlugin { name }.into());
            }
            None => AuthPlugin::MysqlNativePassword,
        };
        Ok(())
    }

    async fn switch_to_ssl_if_needed(&mut self) -> Result<()> {
        if self
            .inner
            .opts
            .get_capabilities()
            .contains(CapabilityFlags::CLIENT_SSL)
        {
            let ssl_request = SslRequest::new(self.inner.capabilities);
            self.write_packet(ssl_request.as_ref()).await?;
            let conn = self;
            let ssl_opts = conn.opts().ssl_opts().cloned().expect("unreachable");
            let domain = conn.opts().ip_or_hostname().into();
            conn.stream_mut()?.make_secure(domain, ssl_opts).await?;
            Ok(())
        } else {
            Ok(())
        }
    }

    async fn do_handshake_response(&mut self) -> Result<()> {
        let auth_data = self
            .inner
            .auth_plugin
            .gen_data(self.inner.opts.pass(), &*self.inner.nonce);

        let handshake_response = HandshakeResponse::new(
            &auth_data,
            self.inner.version,
            self.inner.opts.user(),
            self.inner.opts.db_name(),
            &self.inner.auth_plugin,
            self.capabilities(),
            &Default::default(), // TODO: Add support
        );

        self.write_packet(handshake_response.as_ref()).await?;
        Ok(())
    }

    async fn perform_auth_switch(
        &mut self,
        auth_switch_request: AuthSwitchRequest<'_>,
    ) -> Result<()> {
        if !self.inner.auth_switched {
            self.inner.auth_switched = true;
            self.inner.nonce = auth_switch_request.plugin_data().into();
            self.inner.auth_plugin = auth_switch_request.auth_plugin().clone().into_owned();
            let plugin_data = self
                .inner
                .auth_plugin
                .gen_data(self.inner.opts.pass(), &*self.inner.nonce)
                .unwrap_or_else(Vec::new);
            self.write_packet(plugin_data).await?;
            self.continue_auth().await?;
            Ok(())
        } else {
            unreachable!("auth_switched flag should be checked by caller")
        }
    }

    fn continue_auth(&mut self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        // NOTE: we need to box this since it may recurse
        // see https://github.com/rust-lang/rust/issues/46415#issuecomment-528099782
        Box::pin(async move {
            match self.inner.auth_plugin {
                AuthPlugin::MysqlNativePassword => {
                    self.continue_mysql_native_password_auth().await?;
                    Ok(())
                }
                AuthPlugin::CachingSha2Password => {
                    self.continue_caching_sha2_password_auth().await?;
                    Ok(())
                }
                AuthPlugin::Other(ref name) => Err(DriverError::UnknownAuthPlugin {
                    name: String::from_utf8_lossy(name.as_ref()).to_string(),
                })?,
            }
        })
    }

    fn switch_to_compression(&mut self) -> Result<()> {
        if self
            .capabilities()
            .contains(CapabilityFlags::CLIENT_COMPRESS)
        {
            if let Some(compression) = self.inner.opts.compression() {
                if let Some(stream) = self.inner.stream.as_mut() {
                    stream.compress(compression);
                }
            }
        }
        Ok(())
    }

    async fn continue_caching_sha2_password_auth(&mut self) -> Result<()> {
        let packet = self.read_packet().await?;
        match packet.get(0) {
            Some(0x00) => {
                // ok packet for empty password
                Ok(())
            }
            Some(0x01) => match packet.get(1) {
                Some(0x03) => {
                    // auth ok
                    self.drop_packet().await
                }
                Some(0x04) => {
                    let mut pass = self.inner.opts.pass().map(Vec::from).unwrap_or_default();
                    pass.push(0);

                    if self.is_secure() {
                        self.write_packet(&*pass).await?;
                    } else {
                        self.write_packet(&[0x02][..]).await?;
                        let packet = self.read_packet().await?;
                        let key = &packet[1..];
                        for (i, byte) in pass.iter_mut().enumerate() {
                            *byte ^= self.inner.nonce[i % self.inner.nonce.len()];
                        }
                        let encrypted_pass = crypto::encrypt(&*pass, key);
                        self.write_packet(&*encrypted_pass).await?;
                    };
                    self.drop_packet().await?;
                    Ok(())
                }
                _ => Err(DriverError::UnexpectedPacket {
                    payload: packet.into(),
                }
                .into()),
            },
            Some(0xfe) if !self.inner.auth_switched => {
                let auth_switch_request = parse_auth_switch_request(&*packet)?.into_owned();
                self.perform_auth_switch(auth_switch_request).await?;
                Ok(())
            }
            _ => Err(DriverError::UnexpectedPacket {
                payload: packet.into(),
            }
            .into()),
        }
    }

    async fn continue_mysql_native_password_auth(&mut self) -> Result<()> {
        let packet = self.read_packet().await?;
        match packet.get(0) {
            Some(0x00) => Ok(()),
            Some(0xfe) if !self.inner.auth_switched => {
                let auth_switch_request = parse_auth_switch_request(packet.as_ref())?.into_owned();
                self.perform_auth_switch(auth_switch_request).await?;
                Ok(())
            }
            _ => Err(DriverError::UnexpectedPacket { payload: packet }.into()),
        }
    }

    fn handle_packet(&mut self, packet: &[u8]) -> Result<()> {
        let kind = if self.get_pending_result().is_some() {
            OkPacketKind::ResultSetTerminator
        } else {
            OkPacketKind::Other
        };

        if let Ok(ok_packet) = parse_ok_packet(&*packet, self.capabilities(), kind) {
            self.handle_ok(ok_packet.into_owned());
        } else if let Ok(err_packet) = parse_err_packet(&*packet, self.capabilities()) {
            self.handle_err(err_packet.clone().into_owned());
            return Err(err_packet.into()).into();
        }

        Ok(())
    }

    pub(crate) async fn read_packet(&mut self) -> Result<Vec<u8>> {
        let packet = crate::io::ReadPacket::new(&mut *self)
            .await
            .map_err(|io_err| {
                self.inner.stream.take();
                self.inner.disconnected = true;
                Error::from(io_err)
            })?;
        self.handle_packet(&*packet)?;
        Ok(packet)
    }

    /// Returns future that reads packets from a server.
    pub(crate) async fn read_packets(&mut self, n: usize) -> Result<Vec<Vec<u8>>> {
        let mut packets = Vec::with_capacity(n);
        for _ in 0..n {
            packets.push(self.read_packet().await?);
        }
        Ok(packets)
    }

    pub(crate) async fn write_packet<T>(&mut self, data: T) -> Result<()>
    where
        T: Into<Vec<u8>>,
    {
        crate::io::WritePacket::new(&mut *self, data.into())
            .await
            .map_err(|io_err| {
                self.inner.stream.take();
                self.inner.disconnected = true;
                From::from(io_err)
            })
    }

    /// Returns future that sends full command body to a server.
    pub(crate) async fn write_command_raw(&mut self, body: Vec<u8>) -> Result<()> {
        debug_assert!(body.len() > 0);
        self.clean_dirty().await?;
        self.reset_seq_id();
        self.write_packet(body).await
    }

    /// Returns future that writes command to a server.
    pub(crate) async fn write_command_data<T>(&mut self, cmd: Command, cmd_data: T) -> Result<()>
    where
        T: AsRef<[u8]>,
    {
        let cmd_data = cmd_data.as_ref();
        let mut body = Vec::with_capacity(1 + cmd_data.len());
        body.push(cmd as u8);
        body.extend_from_slice(cmd_data);
        self.write_command_raw(body).await
    }

    async fn drop_packet(&mut self) -> Result<()> {
        self.read_packet().await?;
        Ok(())
    }

    async fn run_init_commands(&mut self) -> Result<()> {
        let mut init: Vec<_> = self.inner.opts.init().iter().cloned().collect();

        while let Some(query) = init.pop() {
            self.query_drop(query).await?;
        }

        Ok(())
    }

    /// Returns a future that resolves to [`Conn`].
    pub fn new<T: Into<Opts>>(opts: T) -> crate::BoxFuture<'static, Conn> {
        let opts = opts.into();
        let fut = Box::pin(async move {
            let mut conn = Conn::empty(opts.clone());

            let stream = if let Some(path) = opts.socket() {
                Stream::connect_socket(path.to_owned()).await?
            } else {
                Stream::connect_tcp(opts.hostport_or_url()).await?
            };

            conn.inner.stream = Some(stream);
            conn.setup_stream()?;
            conn.handle_handshake().await?;
            conn.switch_to_ssl_if_needed().await?;
            conn.do_handshake_response().await?;
            conn.continue_auth().await?;
            conn.switch_to_compression()?;
            conn.read_socket().await?;
            conn.reconnect_via_socket_if_needed().await?;
            conn.read_max_allowed_packet().await?;
            conn.read_wait_timeout().await?;
            conn.run_init_commands().await?;

            Ok(conn)
        });
        crate::BoxFuture(fut)
    }

    /// Returns a future that resolves to [`Conn`].
    pub async fn from_url<T: AsRef<str>>(url: T) -> Result<Conn> {
        Conn::new(Opts::from_str(url.as_ref())?).await
    }

    /// Will try to reconnect via socket using socket address in `self.inner.socket`.
    ///
    /// Won't try to reconnect if socket connection is already enforced in [`Opts`].
    async fn reconnect_via_socket_if_needed(&mut self) -> Result<()> {
        if let Some(socket) = self.inner.socket.as_ref() {
            let opts = self.inner.opts.clone();
            if opts.socket().is_none() {
                let opts = OptsBuilder::from_opts(opts).socket(Some(&**socket));
                match Conn::new(opts).await {
                    Ok(conn) => {
                        let old_conn = std::mem::replace(self, conn);
                        // tidy up the old connection
                        old_conn.close_conn().await?;
                    }
                    Err(_) => (),
                }
            }
        }
        Ok(())
    }

    /// Reads and stores socket address inside the connection.
    ///
    /// Do nothing if socket address is already in [`Opts`] or if `prefer_socket` is `false`.
    async fn read_socket(&mut self) -> Result<()> {
        if self.inner.opts.prefer_socket() && self.inner.socket.is_none() {
            let row_opt = self.query_first("SELECT @@socket").await?;
            self.inner.socket = row_opt.unwrap_or((None,)).0;
        }
        Ok(())
    }

    /// Reads and stores `max_allowed_packet` in the connection.
    async fn read_max_allowed_packet(&mut self) -> Result<()> {
        let row_opt = self.query_first("SELECT @@max_allowed_packet").await?;
        if let Some(stream) = self.inner.stream.as_mut() {
            stream.set_max_allowed_packet(row_opt.unwrap_or((DEFAULT_MAX_ALLOWED_PACKET,)).0);
        }
        Ok(())
    }

    /// Reads and stores `wait_timeout` in the connection.
    async fn read_wait_timeout(&mut self) -> Result<()> {
        let row_opt = self.query_first("SELECT @@wait_timeout").await?;
        let wait_timeout_secs = row_opt.unwrap_or((28800,)).0;
        self.inner.wait_timeout = Duration::from_secs(wait_timeout_secs);
        Ok(())
    }

    /// Returns true if time since last IO exceeds `wait_timeout`
    /// (or `conn_ttl` if specified in opts).
    fn expired(&self) -> bool {
        let ttl = self
            .inner
            .opts
            .conn_ttl()
            .unwrap_or(self.inner.wait_timeout);
        self.idling() > ttl
    }

    /// Returns duration since last IO.
    fn idling(&self) -> Duration {
        self.inner.last_io.elapsed()
    }

    /// Executes `COM_RESET_CONNECTION` on `self`.
    ///
    /// If server version is older than 5.7.2, then it'll reconnect.
    pub async fn reset(&mut self) -> Result<()> {
        let pool = self.inner.pool.clone();

        if self.inner.version > (5, 7, 2) {
            self.write_command_data(Command::COM_RESET_CONNECTION, &[])
                .await?;
            self.read_packet().await?;
        } else {
            let opts = self.inner.opts.clone();
            let old_conn = std::mem::replace(self, Conn::new(opts).await?);
            // tidy up the old connection
            old_conn.close_conn().await?;
        };

        self.inner.stmt_cache.clear();
        self.inner.pool = pool;
        Ok(())
    }

    /// Requires that `self.inner.tx_status != TxStatus::None`
    async fn rollback_transaction(&mut self) -> Result<()> {
        debug_assert_ne!(self.inner.tx_status, TxStatus::None);
        self.inner.tx_status = TxStatus::None;
        self.query_drop("ROLLBACK").await
    }

    /// Returns `true` if `SERVER_MORE_RESULTS_EXISTS` flag is contained
    /// in status flags of the connection.
    pub(crate) fn more_results_exists(&self) -> bool {
        self.status()
            .contains(StatusFlags::SERVER_MORE_RESULTS_EXISTS)
    }

    /// The purpose of this function is to cleanup a pending result set
    /// for prematurely dropeed connection or query result.
    pub(crate) async fn drop_result(&mut self) -> Result<()> {
        match self.inner.pending_result {
            Some(ResultSetMeta::Text(_)) => {
                QueryResult::<'_, '_, TextProtocol>::new(self)
                    .drop_result()
                    .await
            }
            Some(ResultSetMeta::Binary(_)) => {
                QueryResult::<'_, '_, BinaryProtocol>::new(self)
                    .drop_result()
                    .await
            }
            Some(ResultSetMeta::Error(_)) => match self.set_pending_result(None) {
                Some(ResultSetMeta::Error(err)) => Err(err.into()),
                _ => unreachable!(),
            },
            None => Ok(()),
        }
    }

    /// This function will drop pending result and rollback a transaction, if needed.
    ///
    /// The purpose of this function, is to cleanup the connection while returning it to a [`Pool`].
    async fn cleanup_for_pool(mut self) -> Result<Self> {
        loop {
            let result = if self.inner.pending_result.is_some() {
                self.drop_result().await
            } else if self.inner.tx_status != TxStatus::None {
                self.rollback_transaction().await
            } else {
                break;
            };

            // The connection was dropped and we assume that it was dropped intentionally,
            // so we'll ignore non-fatal errors during cleanup (also there is no direct caller
            // to return this error to).
            if let Err(err) = result {
                if err.is_fatal() {
                    // This means that connection is completely broken
                    // and shouldn't return to a pool.
                    return Err(err);
                }
            }
        }
        Ok(self)
    }
}

#[cfg(test)]
mod test {
    use crate::{
        from_row, params, prelude::*, test_misc::get_opts, Conn, Error, OptsBuilder, TxOpts,
        WhiteListFsLocalInfileHandler,
    };

    #[test]
    fn opts_should_satisfy_send_and_sync() {
        struct A<T: Sync + Send>(T);
        A(get_opts());
    }

    #[tokio::test]
    async fn should_connect_without_database() -> super::Result<()> {
        // no database name
        let mut conn: Conn = Conn::new(get_opts().db_name(None::<String>)).await?;
        conn.ping().await?;
        conn.disconnect().await?;

        // empty database name
        let mut conn: Conn = Conn::new(get_opts().db_name(Some(""))).await?;
        conn.ping().await?;
        conn.disconnect().await?;

        Ok(())
    }

    #[tokio::test]
    async fn should_clean_state_if_wrapper_is_dropeed() -> super::Result<()> {
        let mut conn: Conn = Conn::new(get_opts()).await?;

        conn.query_drop("CREATE TEMPORARY TABLE mysql.foo (id SERIAL)")
            .await?;

        // dropped query:
        conn.query_iter("SELECT 1").await?;
        conn.ping().await?;

        // dropped query in dropped transaction:
        let mut tx = conn.start_transaction(Default::default()).await?;
        tx.query_drop("INSERT INTO mysql.foo (id) VALUES (42)")
            .await?;
        tx.exec_iter("SELECT COUNT(*) FROM mysql.foo", ()).await?;
        drop(tx);
        conn.ping().await?;

        let count: u8 = conn
            .query_first("SELECT COUNT(*) FROM mysql.foo")
            .await?
            .unwrap_or_default();

        assert_eq!(count, 0);

        Ok(())
    }

    #[tokio::test]
    async fn should_connect() -> super::Result<()> {
        let mut conn: Conn = Conn::new(get_opts()).await?;
        conn.ping().await?;
        let plugins: Vec<String> = conn
            .query_map("SHOW PLUGINS", |mut row: crate::Row| {
                row.take("Name").unwrap()
            })
            .await?;

        // Should connect with any combination of supported plugin and empty-nonempty password.
        let variants = vec![
            ("caching_sha2_password", 2_u8, "non-empty"),
            ("caching_sha2_password", 2_u8, ""),
            ("mysql_native_password", 0_u8, "non-empty"),
            ("mysql_native_password", 0_u8, ""),
        ]
        .into_iter()
        .filter(|variant| plugins.iter().any(|p| p == variant.0));

        for (plug, val, pass) in variants {
            let _ = conn.query_drop("DROP USER 'test_user'@'%'").await;

            let query = format!("CREATE USER 'test_user'@'%' IDENTIFIED WITH {}", plug);
            conn.query_drop(query).await.unwrap();

            if (8, 0, 11) <= conn.inner.version && conn.inner.version <= (9, 0, 0) {
                conn.query_drop(format!("SET PASSWORD FOR 'test_user'@'%' = '{}'", pass))
                    .await
                    .unwrap();
            } else {
                conn.query_drop(format!("SET old_passwords = {}", val))
                    .await
                    .unwrap();
                conn.query_drop(format!(
                    "SET PASSWORD FOR 'test_user'@'%' = PASSWORD('{}')",
                    pass
                ))
                .await
                .unwrap();
            };

            let opts = get_opts()
                .user(Some("test_user"))
                .pass(Some(pass))
                .db_name(None::<String>);
            let result = Conn::new(opts).await;

            conn.query_drop("DROP USER 'test_user'@'%'").await.unwrap();

            result?.disconnect().await?;
        }

        if crate::test_misc::test_compression() {
            assert!(format!("{:?}", conn).contains("Compression"));
        }

        if crate::test_misc::test_ssl() {
            assert!(format!("{:?}", conn).contains("Tls"));
        }

        conn.disconnect().await?;
        Ok(())
    }

    #[test]
    fn should_not_panic_if_dropped_without_tokio_runtime() {
        let fut = Conn::new(get_opts());
        let mut runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            fut.await.unwrap();
        });
        // connection will drop here
    }

    #[tokio::test]
    async fn should_execute_init_queries_on_new_connection() -> super::Result<()> {
        let opts = OptsBuilder::from_opts(get_opts()).init(vec!["SET @a = 42", "SET @b = 'foo'"]);
        let mut conn = Conn::new(opts).await?;
        let result: Vec<(u8, String)> = conn.query("SELECT @a, @b").await?;
        conn.disconnect().await?;
        assert_eq!(result, vec![(42, "foo".into())]);
        Ok(())
    }

    #[tokio::test]
    async fn should_reset_the_connection() -> super::Result<()> {
        let mut conn = Conn::new(get_opts()).await?;
        conn.exec_drop("SELECT ?", (1_u8,)).await?;
        conn.reset().await?;
        conn.exec_drop("SELECT ?", (1_u8,)).await?;
        conn.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn should_not_cache_statements_if_stmt_cache_size_is_zero() -> super::Result<()> {
        let opts = OptsBuilder::from_opts(get_opts()).stmt_cache_size(0);

        let mut conn = Conn::new(opts).await?;
        conn.exec_drop("DO ?", (1_u8,)).await?;

        let stmt = conn.prep("DO 2").await?;
        conn.exec_drop(&stmt, ()).await?;
        conn.exec_drop(&stmt, ()).await?;
        conn.close(stmt).await?;

        conn.exec_drop("DO 3", ()).await?;
        conn.exec_batch("DO 4", vec![(), ()]).await?;
        conn.exec_first::<u8, _, _>("DO 5", ()).await?;
        let row: Option<(crate::Value, usize)> = conn
            .query_first("SHOW SESSION STATUS LIKE 'Com_stmt_close';")
            .await?;

        assert_eq!(row.unwrap().1, 1);
        assert_eq!(conn.inner.stmt_cache.len(), 0);

        conn.disconnect().await?;

        Ok(())
    }

    #[tokio::test]
    async fn should_hold_stmt_cache_size_bound() -> super::Result<()> {
        let opts = OptsBuilder::from_opts(get_opts()).stmt_cache_size(3);
        let mut conn = Conn::new(opts).await?;
        conn.exec_drop("DO 1", ()).await?;
        conn.exec_drop("DO 2", ()).await?;
        conn.exec_drop("DO 3", ()).await?;
        conn.exec_drop("DO 1", ()).await?;
        conn.exec_drop("DO 4", ()).await?;
        conn.exec_drop("DO 3", ()).await?;
        conn.exec_drop("DO 5", ()).await?;
        conn.exec_drop("DO 6", ()).await?;
        let row_opt = conn
            .query_first("SHOW SESSION STATUS LIKE 'Com_stmt_close';")
            .await?;
        let (_, count): (String, usize) = row_opt.unwrap();
        assert_eq!(count, 3);
        let order = conn
            .stmt_cache_ref()
            .iter()
            .map(|item| item.1.query.0.as_ref())
            .collect::<Vec<&str>>();
        assert_eq!(order, &["DO 6", "DO 5", "DO 3"]);
        conn.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn should_perform_queries() -> super::Result<()> {
        let long_string = ::std::iter::repeat('A')
            .take(18 * 1024 * 1024)
            .collect::<String>();
        let mut conn = Conn::new(get_opts()).await?;
        let result: Vec<(String, u8)> = conn
            .query(format!(r"SELECT '{}', 231", long_string))
            .await?;
        conn.disconnect().await?;
        assert_eq!((long_string, 231_u8), result[0]);
        Ok(())
    }

    #[tokio::test]
    async fn should_query_drop() -> super::Result<()> {
        let mut conn = Conn::new(get_opts()).await?;
        conn.query_drop("CREATE TEMPORARY TABLE tmp (id int DEFAULT 10, name text)")
            .await?;
        conn.query_drop("INSERT INTO tmp VALUES (1, 'foo')").await?;
        let result: Option<u8> = conn.query_first("SELECT COUNT(*) FROM tmp").await?;
        conn.disconnect().await?;
        assert_eq!(result, Some(1_u8));
        Ok(())
    }

    #[tokio::test]
    async fn dropped_query_result_should_emit_errors_on_cleanup() -> super::Result<()> {
        use crate::{Error::Server, ServerError};
        let mut conn = Conn::new(get_opts()).await?;
        conn.query_iter("SELECT '1'; BLABLA;").await?;
        assert!(matches!(
            conn.query_drop("DO 42;").await.unwrap_err(),
            Server(ServerError { code: 1064, .. })
        ));
        conn.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn should_try_collect() -> super::Result<()> {
        let mut conn = Conn::new(get_opts()).await?;
        let mut result = conn
            .query_iter(
                r"SELECT 'hello', 123
                    UNION ALL
                    SELECT 'world', 'bar'
                    UNION ALL
                    SELECT 'hello', 123
                ",
            )
            .await?;
        let mut rows = result.try_collect::<(String, u8)>().await?;
        assert!(rows.pop().unwrap().is_ok());
        assert!(rows.pop().unwrap().is_err());
        assert!(rows.pop().unwrap().is_ok());
        result.drop_result().await?;
        conn.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn should_try_collect_and_drop() -> super::Result<()> {
        let mut conn = Conn::new(get_opts()).await?;
        let mut rows = conn
            .query_iter(
                r"SELECT 'hello', 123
                    UNION ALL
                    SELECT 'world', 'bar'
                    UNION ALL
                    SELECT 'hello', 123;
                    SELECT 'foo', 255;
                ",
            )
            .await?
            .try_collect_and_drop::<(String, u8)>()
            .await?;
        assert!(rows.pop().unwrap().is_ok());
        assert!(rows.pop().unwrap().is_err());
        assert!(rows.pop().unwrap().is_ok());
        conn.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn should_handle_mutliresult_set() -> super::Result<()> {
        let mut conn = Conn::new(get_opts()).await?;
        let mut result = conn
            .query_iter(
                r"SELECT 'hello', 123
                    UNION ALL
                    SELECT 'world', 231;
                    SELECT 'foo', 255;
                ",
            )
            .await?;
        let rows_1 = result.collect::<(String, u8)>().await?;
        let rows_2 = result.collect_and_drop().await?;
        conn.disconnect().await?;

        assert_eq!((String::from("hello"), 123), rows_1[0]);
        assert_eq!((String::from("world"), 231), rows_1[1]);
        assert_eq!((String::from("foo"), 255), rows_2[0]);
        Ok(())
    }

    #[tokio::test]
    async fn should_map_resultset() -> super::Result<()> {
        let mut conn = Conn::new(get_opts()).await?;
        let mut result = conn
            .query_iter(
                r"
                    SELECT 'hello', 123
                    UNION ALL
                    SELECT 'world', 231;
                    SELECT 'foo', 255;
                ",
            )
            .await?;

        let rows_1 = result.map(|row| from_row::<(String, u8)>(row)).await?;
        let rows_2 = result.map_and_drop(from_row).await?;
        conn.disconnect().await?;

        assert_eq!((String::from("hello"), 123), rows_1[0]);
        assert_eq!((String::from("world"), 231), rows_1[1]);
        assert_eq!((String::from("foo"), 255), rows_2[0]);
        Ok(())
    }

    #[tokio::test]
    async fn should_reduce_resultset() -> super::Result<()> {
        let mut conn = Conn::new(get_opts()).await?;
        let mut result = conn
            .query_iter(
                r"SELECT 5
                    UNION ALL
                    SELECT 6;
                    SELECT 7;",
            )
            .await?;
        let reduced = result
            .reduce(0, |mut acc, row| {
                acc += from_row::<i32>(row);
                acc
            })
            .await?;
        let rows_2 = result.collect_and_drop::<i32>().await?;
        conn.disconnect().await?;
        assert_eq!(11, reduced);
        assert_eq!(7, rows_2[0]);
        Ok(())
    }

    #[tokio::test]
    async fn should_handle_multi_result_sets_where_some_results_have_no_output() -> super::Result<()>
    {
        const QUERY: &str = r"SELECT 1;
            UPDATE time_zone SET Time_zone_id = 1 WHERE Time_zone_id = 1;
            SELECT 2;
            SELECT 3;
            UPDATE time_zone SET Time_zone_id = 1 WHERE Time_zone_id = 1;
            UPDATE time_zone SET Time_zone_id = 1 WHERE Time_zone_id = 1;
            SELECT 4;";

        let mut c = Conn::new(get_opts()).await?;
        c.query_drop("CREATE TEMPORARY TABLE time_zone (Time_zone_id INT)")
            .await
            .unwrap();
        let mut t = c.start_transaction(TxOpts::new()).await?;
        t.query_drop(QUERY).await?;
        let r = t.query_iter(QUERY).await?;
        let out = r.collect_and_drop::<u8>().await?;
        assert_eq!(vec![1], out);
        let r = t.query_iter(QUERY).await?;
        r.for_each_and_drop(|x| assert_eq!(from_row::<u8>(x), 1))
            .await?;
        let r = t.query_iter(QUERY).await?;
        let out = r.map_and_drop(|row| from_row::<u8>(row)).await?;
        assert_eq!(vec![1], out);
        let r = t.query_iter(QUERY).await?;
        let out = r
            .reduce_and_drop(0u8, |acc, x| acc + from_row::<u8>(x))
            .await?;
        assert_eq!(1, out);
        t.query_drop(QUERY).await?;
        t.commit().await?;
        let result = c.exec_first("SELECT 1", ()).await?;
        c.disconnect().await?;
        assert_eq!(result, Some(1_u8));
        Ok(())
    }

    #[tokio::test]
    async fn should_iterate_over_resultset() -> super::Result<()> {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let acc = Arc::new(AtomicUsize::new(0));

        let mut conn = Conn::new(get_opts()).await?;
        let mut result = conn
            .query_iter(
                r"SELECT 2
                    UNION ALL
                    SELECT 3;
                    SELECT 5;",
            )
            .await?;
        result
            .for_each({
                let acc = acc.clone();
                move |row| {
                    acc.fetch_add(from_row::<usize>(row), Ordering::SeqCst);
                }
            })
            .await?;
        result
            .for_each_and_drop({
                let acc = acc.clone();
                move |row| {
                    acc.fetch_add(from_row::<usize>(row), Ordering::SeqCst);
                }
            })
            .await?;
        conn.disconnect().await?;
        assert_eq!(acc.load(Ordering::SeqCst), 10);
        Ok(())
    }

    #[tokio::test]
    async fn should_prepare_statement() -> super::Result<()> {
        let mut conn = Conn::new(get_opts()).await?;
        let stmt = conn.prep(r"SELECT ?").await?;
        conn.close(stmt).await?;
        conn.disconnect().await?;

        let mut conn = Conn::new(get_opts()).await?;
        let stmt = conn.prep(r"SELECT :foo").await?;

        {
            let query = String::from("SELECT ?, ?");
            let stmt = conn.prep(&*query).await?;
            conn.close(stmt).await?;
            {
                let mut conn = Conn::new(get_opts()).await?;
                let stmt = conn.prep(&*query).await?;
                conn.close(stmt).await?;
                conn.disconnect().await?;
            }
        }

        conn.close(stmt).await?;
        conn.disconnect().await?;

        Ok(())
    }

    #[tokio::test]
    async fn should_execute_statement() -> super::Result<()> {
        let long_string = ::std::iter::repeat('A')
            .take(18 * 1024 * 1024)
            .collect::<String>();
        let mut conn = Conn::new(get_opts()).await?;
        let stmt = conn.prep(r"SELECT ?").await?;
        let result = conn.exec_iter(&stmt, (&long_string,)).await?;
        let mut mapped = result
            .map_and_drop(|row| from_row::<(String,)>(row))
            .await?;
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped.pop(), Some((long_string,)));
        let result = conn.exec_iter(&stmt, (42_u8,)).await?;
        let collected = result.collect_and_drop::<(u8,)>().await?;
        assert_eq!(collected, vec![(42u8,)]);
        let result = conn.exec_iter(&stmt, (8_u8,)).await?;
        let reduced = result
            .reduce_and_drop(2, |mut acc, row| {
                acc += from_row::<i32>(row);
                acc
            })
            .await?;
        conn.close(stmt).await?;
        conn.disconnect().await?;
        assert_eq!(reduced, 10);

        let mut conn = Conn::new(get_opts()).await?;
        let stmt = conn.prep(r"SELECT :foo, :bar, :foo, 3").await?;
        let result = conn
            .exec_iter(&stmt, params! { "foo" => "quux", "bar" => "baz" })
            .await?;
        let mut mapped = result
            .map_and_drop(|row| from_row::<(String, String, String, u8)>(row))
            .await?;
        assert_eq!(mapped.len(), 1);
        assert_eq!(
            mapped.pop(),
            Some(("quux".into(), "baz".into(), "quux".into(), 3))
        );
        let result = conn
            .exec_iter(&stmt, params! { "foo" => 2, "bar" => 3 })
            .await?;
        let collected = result.collect_and_drop::<(u8, u8, u8, u8)>().await?;
        assert_eq!(collected, vec![(2, 3, 2, 3)]);
        let result = conn
            .exec_iter(&stmt, params! { "foo" => 2, "bar" => 3 })
            .await?;
        let reduced = result
            .reduce_and_drop(0, |acc, row| {
                let (a, b, c, d): (u8, u8, u8, u8) = from_row(row);
                acc + a + b + c + d
            })
            .await?;
        conn.close(stmt).await?;
        conn.disconnect().await?;
        assert_eq!(reduced, 10);
        Ok(())
    }

    #[tokio::test]
    async fn should_prep_exec_statement() -> super::Result<()> {
        let mut conn = Conn::new(get_opts()).await?;
        let result = conn
            .exec_iter(r"SELECT :a, :b, :a", params! { "a" => 2, "b" => 3 })
            .await?;
        let output = result
            .map_and_drop(|row| {
                let (a, b, c): (u8, u8, u8) = from_row(row);
                a * b * c
            })
            .await?;
        conn.disconnect().await?;
        assert_eq!(output[0], 12u8);
        Ok(())
    }

    #[tokio::test]
    async fn should_first_exec_statement() -> super::Result<()> {
        let mut conn = Conn::new(get_opts()).await?;
        let output = conn
            .exec_first(
                r"SELECT :a UNION ALL SELECT :b",
                params! { "a" => 2, "b" => 3 },
            )
            .await?;
        conn.disconnect().await?;
        assert_eq!(output, Some(2u8));
        Ok(())
    }

    #[tokio::test]
    async fn issue_107() -> super::Result<()> {
        let mut conn = Conn::new(get_opts()).await?;
        conn.query_drop(
            r"CREATE TEMPORARY TABLE mysql.issue (
                    a BIGINT(20) UNSIGNED,
                    b VARBINARY(16),
                    c BINARY(32),
                    d BIGINT(20) UNSIGNED,
                    e BINARY(32)
                )",
        )
        .await?;
        conn.query_drop(
            r"INSERT INTO mysql.issue VALUES (
                    0,
                    0xC066F966B0860000,
                    0x7939DA98E524C5F969FC2DE8D905FD9501EBC6F20001B0A9C941E0BE6D50CF44,
                    0,
                    ''
                ), (
                    1,
                    '',
                    0x076311DF4D407B0854371BA13A5F3FB1A4555AC22B361375FD47B263F31822F2,
                    0,
                    ''
                )",
        )
        .await?;

        let q = "SELECT b, c, d, e FROM mysql.issue";
        let result = conn.query_iter(q).await?;

        let loaded_structs = result
            .map_and_drop(|row| crate::from_row::<(Vec<u8>, Vec<u8>, u64, Vec<u8>)>(row))
            .await?;

        conn.disconnect().await?;

        assert_eq!(loaded_structs.len(), 2);

        Ok(())
    }

    #[tokio::test]
    async fn should_run_transactions() -> super::Result<()> {
        let mut conn = Conn::new(get_opts()).await?;
        conn.query_drop("CREATE TEMPORARY TABLE tmp (id INT, name TEXT)")
            .await?;
        let mut transaction = conn.start_transaction(Default::default()).await?;
        transaction
            .query_drop("INSERT INTO tmp VALUES (1, 'foo'), (2, 'bar')")
            .await?;
        transaction.commit().await?;
        let output_opt = conn.query_first("SELECT COUNT(*) FROM tmp").await?;
        assert_eq!(output_opt, Some((2u8,)));
        let mut transaction = conn.start_transaction(Default::default()).await?;
        transaction
            .query_drop("INSERT INTO tmp VALUES (3, 'baz'), (4, 'quux')")
            .await?;
        let output_opt = transaction
            .exec_first("SELECT COUNT(*) FROM tmp", ())
            .await?;
        assert_eq!(output_opt, Some((4u8,)));
        transaction.rollback().await?;
        let output_opt = conn.query_first("SELECT COUNT(*) FROM tmp").await?;
        assert_eq!(output_opt, Some((2u8,)));

        let mut transaction = conn.start_transaction(Default::default()).await?;
        transaction
            .query_drop("INSERT INTO tmp VALUES (3, 'baz')")
            .await?;
        drop(transaction); // implicit rollback
        let output_opt = conn.query_first("SELECT COUNT(*) FROM tmp").await?;
        assert_eq!(output_opt, Some((2u8,)));

        conn.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn should_handle_multiresult_set_with_error() -> super::Result<()> {
        const QUERY_FIRST: &str = "SELECT * FROM tmp; SELECT 1; SELECT 2;";
        const QUERY_MIDDLE: &str = "SELECT 1; SELECT * FROM tmp; SELECT 2";
        let mut conn = Conn::new(get_opts()).await.unwrap();

        // if error is in the first result set, then query should return it immediately.
        let result = QUERY_FIRST.run(&mut conn).await;
        assert!(matches!(result, Err(Error::Server(_))));

        let mut result = QUERY_MIDDLE.run(&mut conn).await.unwrap();

        // first result set will contain one row
        let result_set: Vec<u8> = result.collect().await.unwrap();
        assert_eq!(result_set, vec![1]);

        // second result set will contain an error.
        let result_set: super::Result<Vec<u8>> = result.collect().await;
        assert!(matches!(result_set, Err(Error::Server(_))));

        // there will be no third result set
        assert!(result.is_empty());

        conn.ping().await?;
        conn.disconnect().await?;

        Ok(())
    }

    #[tokio::test]
    async fn should_handle_binary_multiresult_set_with_error() -> super::Result<()> {
        const PROC_DEF_FIRST: &str =
            r#"CREATE PROCEDURE err_first() BEGIN SELECT * FROM tmp; SELECT 1; END"#;
        const PROC_DEF_MIDDLE: &str =
            r#"CREATE PROCEDURE err_middle() BEGIN SELECT 1; SELECT * FROM tmp; SELECT 2; END"#;

        let mut conn = Conn::new(get_opts()).await.unwrap();

        conn.query_drop("DROP PROCEDURE IF EXISTS err_first")
            .await?;
        conn.query_iter(PROC_DEF_FIRST).await?;

        conn.query_drop("DROP PROCEDURE IF EXISTS err_middle")
            .await?;
        conn.query_iter(PROC_DEF_MIDDLE).await?;

        // if error is in the first result set, then query should return it immediately.
        let result = conn.query_iter("CALL err_first()").await;
        assert!(matches!(result, Err(Error::Server(_))));

        let mut result = conn.query_iter("CALL err_middle()").await?;

        // first result set will contain one row
        let result_set: Vec<u8> = result.collect().await.unwrap();
        assert_eq!(result_set, vec![1]);

        // second result set will contain an error.
        let result_set: super::Result<Vec<u8>> = result.collect().await;
        assert!(matches!(result_set, Err(Error::Server(_))));

        // there will be no third result set
        assert!(result.is_empty());

        conn.ping().await?;
        conn.disconnect().await?;

        Ok(())
    }

    #[tokio::test]
    async fn should_handle_multiresult_set_with_local_infile() -> super::Result<()> {
        use std::fs::write;

        let file_path = tempfile::Builder::new().tempfile_in("").unwrap();
        let file_path = file_path.path();
        let file_name = file_path.file_name().unwrap();

        write(file_name, b"AAAAAA\nBBBBBB\nCCCCCC\n")?;

        let opts = get_opts()
            .local_infile_handler(Some(WhiteListFsLocalInfileHandler::new(&[file_name][..])));

        // LOCAL INFILE in the middle of a multi-result set should not break anything.
        let mut conn = Conn::new(opts).await.unwrap();
        "CREATE TEMPORARY TABLE tmp (a TEXT)".run(&mut conn).await?;

        let query = format!(
            r#"SELECT * FROM tmp;
            LOAD DATA LOCAL INFILE "{}" INTO TABLE tmp;
            LOAD DATA LOCAL INFILE "{}" INTO TABLE tmp;
            SELECT * FROM tmp"#,
            file_name.to_str().unwrap(),
            file_name.to_str().unwrap(),
        );

        let mut result = query.run(&mut conn).await?;

        let result_set = result.collect::<String>().await?;
        assert_eq!(result_set.len(), 0);

        let mut no_local_infile = false;

        for _ in 0..2 {
            match result.collect::<String>().await {
                Ok(result_set) => {
                    assert_eq!(result.affected_rows(), 3);
                    assert!(result_set.is_empty())
                }
                Err(Error::Server(ref err)) if err.code == 1148 => {
                    // The used command is not allowed with this MySQL version
                    no_local_infile = true;
                    break;
                }
                Err(Error::Server(ref err)) if err.code == 3948 => {
                    // Loading local data is disabled;
                    // this must be enabled on both the client and server sides
                    no_local_infile = true;
                    break;
                }
                Err(err) => return Err(err),
            }
        }

        if no_local_infile {
            assert!(result.is_empty());
            assert_eq!(result_set.len(), 0);
        } else {
            let result_set = result.collect::<String>().await?;
            assert_eq!(result_set.len(), 6);
            assert_eq!(result_set[0], "AAAAAA");
            assert_eq!(result_set[1], "BBBBBB");
            assert_eq!(result_set[2], "CCCCCC");
            assert_eq!(result_set[3], "AAAAAA");
            assert_eq!(result_set[4], "BBBBBB");
            assert_eq!(result_set[5], "CCCCCC");
        }

        conn.ping().await?;
        conn.disconnect().await?;

        Ok(())
    }

    #[tokio::test]
    async fn should_provide_multiresult_set_metadata() -> super::Result<()> {
        let mut c = Conn::new(get_opts()).await?;
        c.query_drop("CREATE TEMPORARY TABLE tmp (id INT, foo TEXT)")
            .await?;

        let mut result = c
            .query_iter("SELECT 1; SELECT id, foo FROM tmp WHERE 1 = 2; DO 42; SELECT 2;")
            .await?;
        assert_eq!(result.columns().map(|x| x.len()).unwrap_or_default(), 1);

        result.for_each(drop).await?;
        assert_eq!(result.columns().map(|x| x.len()).unwrap_or_default(), 2);

        result.for_each(drop).await?;
        assert_eq!(result.columns().map(|x| x.len()).unwrap_or_default(), 0);

        result.for_each(drop).await?;
        assert_eq!(result.columns().map(|x| x.len()).unwrap_or_default(), 1);

        c.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn should_handle_local_infile() -> super::Result<()> {
        use std::fs::write;

        let file_path = tempfile::Builder::new().tempfile_in("").unwrap();
        let file_path = file_path.path();
        let file_name = file_path.file_name().unwrap();

        write(file_name, b"AAAAAA\nBBBBBB\nCCCCCC\n")?;

        let opts = get_opts()
            .local_infile_handler(Some(WhiteListFsLocalInfileHandler::new(&[file_name][..])));

        let mut conn = Conn::new(opts).await.unwrap();
        conn.query_drop("CREATE TEMPORARY TABLE tmp (a TEXT);")
            .await
            .unwrap();

        match conn
            .query_drop(format!(
                r#"LOAD DATA LOCAL INFILE "{}" INTO TABLE tmp;"#,
                file_name.to_str().unwrap(),
            ))
            .await
        {
            Ok(_) => (),
            Err(super::Error::Server(ref err)) if err.code == 1148 => {
                // The used command is not allowed with this MySQL version
                return Ok(());
            }
            Err(super::Error::Server(ref err)) if err.code == 3948 => {
                // Loading local data is disabled;
                // this must be enabled on both the client and server sides
                return Ok(());
            }
            e @ Err(_) => e.unwrap(),
        };

        let result: Vec<String> = conn.query("SELECT * FROM tmp").await?;
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], "AAAAAA");
        assert_eq!(result[1], "BBBBBB");
        assert_eq!(result[2], "CCCCCC");

        Ok(())
    }

    #[cfg(feature = "nightly")]
    mod bench {
        use crate::{conn::Conn, queryable::Queryable, test_misc::get_opts};

        #[bench]
        fn simple_exec(bencher: &mut test::Bencher) {
            let mut runtime = tokio::runtime::Runtime::new().unwrap();
            let mut conn = runtime.block_on(Conn::new(get_opts())).unwrap();

            bencher.iter(|| {
                runtime.block_on(conn.query_drop("DO 1")).unwrap();
            });

            runtime.block_on(conn.disconnect()).unwrap();
        }

        #[bench]
        fn select_large_string(bencher: &mut test::Bencher) {
            let mut runtime = tokio::runtime::Runtime::new().unwrap();
            let mut conn = runtime.block_on(Conn::new(get_opts())).unwrap();

            bencher.iter(|| {
                runtime
                    .block_on(conn.query_drop("SELECT REPEAT('A', 10000)"))
                    .unwrap();
            });

            runtime.block_on(conn.disconnect()).unwrap();
        }

        #[bench]
        fn prepared_exec(bencher: &mut test::Bencher) {
            let mut runtime = tokio::runtime::Runtime::new().unwrap();
            let mut conn = runtime.block_on(Conn::new(get_opts())).unwrap();
            let stmt = runtime.block_on(conn.prep("DO 1")).unwrap();

            bencher.iter(|| {
                runtime.block_on(conn.exec_drop(&stmt, ())).unwrap();
            });

            runtime.block_on(conn.close(stmt)).unwrap();
            runtime.block_on(conn.disconnect()).unwrap();
        }

        #[bench]
        fn prepare_and_exec(bencher: &mut test::Bencher) {
            let mut runtime = tokio::runtime::Runtime::new().unwrap();
            let mut conn = runtime.block_on(Conn::new(get_opts())).unwrap();

            bencher.iter(|| {
                runtime.block_on(conn.exec_drop("SELECT ?", (0,))).unwrap();
            });

            runtime.block_on(conn.disconnect()).unwrap();
        }
    }
}
